//! Public startup asset manifest and bounded image preparation.
//!
//! Paths and encoded bytes are resolved here, before scene construction. The
//! renderer-facing result contains only stable keys and canonical decoded
//! pixels ready for atlas packing.

use std::{
    collections::BTreeSet,
    error::Error,
    fmt,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use anyhow::Context as _;
use tokio::{sync::Semaphore, task::JoinSet};

use super::atlas::{
    decode::{DecodeLimits, DecodedImage, decode_image},
    source::{BundledImage, CacheImageSource},
    types::AssetKey,
};

/// A static image shipped with `rustverse_svg`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum BundledAsset {
    DeadlyAssault,
    ShiyuDefense,
    Hollows,
    StarIcon,
}

impl BundledAsset {
    pub const fn all() -> [Self; 4] {
        [
            Self::DeadlyAssault,
            Self::ShiyuDefense,
            Self::Hollows,
            Self::StarIcon,
        ]
    }

    /// Stable logical identity used by startup manifests and atlas lookups.
    pub const fn stable_key(self) -> &'static str {
        match self {
            Self::DeadlyAssault => "rustverse-bundled-da.webp",
            Self::ShiyuDefense => "rustverse-bundled-shiyu.webp",
            Self::Hollows => "rustverse-bundled-hollows.png",
            Self::StarIcon => "image/star-icon.png",
        }
    }

    pub(crate) const fn internal(self) -> BundledImage {
        match self {
            Self::DeadlyAssault => BundledImage::DeadlyAssault,
            Self::ShiyuDefense => BundledImage::ShiyuDefense,
            Self::Hollows => BundledImage::Hollows,
            Self::StarIcon => BundledImage::StarIcon,
        }
    }
}

/// Encoded source resolved only during startup preparation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartupAssetSource {
    Bundled(BundledAsset),
    Cache {
        cache_root: PathBuf,
        relative_path: PathBuf,
    },
}

/// One stable asset identity and its preparation-only source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupAssetEntry {
    key: String,
    source: StartupAssetSource,
}

impl StartupAssetEntry {
    pub fn bundled(asset: BundledAsset) -> Self {
        Self {
            key: asset.stable_key().to_owned(),
            source: StartupAssetSource::Bundled(asset),
        }
    }

    pub fn cached(
        key: impl Into<String>,
        cache_root: impl Into<PathBuf>,
        relative_path: impl Into<PathBuf>,
    ) -> Result<Self, StartupManifestError> {
        let key = validate_key(key.into())?;
        let relative_path = relative_path.into();
        validate_relative_path(&relative_path)?;
        Ok(Self {
            key,
            source: StartupAssetSource::Cache {
                cache_root: cache_root.into(),
                relative_path,
            },
        })
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn source(&self) -> &StartupAssetSource {
        &self.source
    }
}

/// Deterministic startup image inventory and its decode parallelism bound.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupAssetManifest {
    entries: Vec<StartupAssetEntry>,
    decode_concurrency: usize,
}

impl StartupAssetManifest {
    pub fn new(
        entries: impl IntoIterator<Item = StartupAssetEntry>,
        decode_concurrency: usize,
    ) -> Result<Self, StartupManifestError> {
        if decode_concurrency == 0 {
            return Err(StartupManifestError::ZeroDecodeConcurrency);
        }
        let entries: Vec<_> = entries.into_iter().collect();
        let mut keys = BTreeSet::new();
        for entry in &entries {
            validate_key(entry.key.clone())?;
            if !keys.insert(entry.key.clone()) {
                return Err(StartupManifestError::DuplicateKey(entry.key.clone()));
            }
        }
        Ok(Self {
            entries,
            decode_concurrency,
        })
    }

    pub fn entries(&self) -> &[StartupAssetEntry] {
        &self.entries
    }

    pub const fn decode_concurrency(&self) -> usize {
        self.decode_concurrency
    }
}

impl Default for StartupAssetManifest {
    fn default() -> Self {
        Self::new(BundledAsset::all().map(StartupAssetEntry::bundled), 4)
            .expect("default startup manifest is valid")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartupManifestError {
    ZeroDecodeConcurrency,
    EmptyKey,
    DuplicateKey(String),
    InvalidCachePath(PathBuf),
}

impl fmt::Display for StartupManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroDecodeConcurrency => {
                formatter.write_str("startup decode concurrency must be greater than zero")
            }
            Self::EmptyKey => formatter.write_str("startup asset key must not be empty"),
            Self::DuplicateKey(key) => write!(formatter, "duplicate startup asset key {key}"),
            Self::InvalidCachePath(path) => write!(
                formatter,
                "startup cache path must be a non-empty relative path: {}",
                path.display()
            ),
        }
    }
}

impl Error for StartupManifestError {}

fn validate_key(key: String) -> Result<String, StartupManifestError> {
    if key.is_empty() {
        Err(StartupManifestError::EmptyKey)
    } else {
        Ok(key)
    }
}

fn validate_relative_path(path: &Path) -> Result<(), StartupManifestError> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        Err(StartupManifestError::InvalidCachePath(path.to_path_buf()))
    } else {
        Ok(())
    }
}

pub(crate) struct PreparedStartupAsset {
    pub(crate) key: AssetKey,
    pub(crate) decoded: DecodedImage,
}

pub(crate) async fn prepare_startup_manifest(
    manifest: StartupAssetManifest,
    limits: DecodeLimits,
) -> anyhow::Result<Vec<PreparedStartupAsset>> {
    prepare_startup_manifest_inner(manifest, limits, None).await
}

async fn prepare_startup_manifest_inner(
    manifest: StartupAssetManifest,
    limits: DecodeLimits,
    observer: Option<Arc<ConcurrencyObserver>>,
) -> anyhow::Result<Vec<PreparedStartupAsset>> {
    // Revalidate at the async boundary so future internal constructors cannot
    // accidentally bypass the public manifest invariants.
    let manifest = StartupAssetManifest::new(manifest.entries, manifest.decode_concurrency)
        .map_err(anyhow::Error::new)?;
    let semaphore = Arc::new(Semaphore::new(manifest.decode_concurrency));
    let mut tasks = JoinSet::new();

    for entry in manifest.entries {
        let semaphore = Arc::clone(&semaphore);
        let observer = observer.clone();
        tasks.spawn(async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .context("startup decode semaphore was closed")?;
            let _observation = match observer {
                Some(observer) => Some(observer.enter().await),
                None => None,
            };
            let encoded: Arc<[u8]> = match entry.source {
                StartupAssetSource::Bundled(asset) => asset.internal().encoded().into(),
                StartupAssetSource::Cache {
                    cache_root,
                    relative_path,
                } => CacheImageSource::new(&cache_root, &relative_path)
                    .map_err(anyhow::Error::new)?
                    .read(limits.max_encoded_bytes)
                    .await
                    .map_err(anyhow::Error::new)?
                    .into(),
            };
            let decoded = tokio::task::spawn_blocking(move || {
                let _permit = _permit;
                let _observation = _observation;
                decode_image(&encoded, limits)
            })
            .await
            .context("startup image decoder task failed")?
            .map_err(anyhow::Error::new)?;
            Ok::<_, anyhow::Error>(PreparedStartupAsset {
                key: AssetKey::new(entry.key).map_err(anyhow::Error::new)?,
                decoded,
            })
        });
    }

    let mut prepared = Vec::new();
    while let Some(result) = tasks.join_next().await {
        prepared.push(result.context("startup preparation task failed")??);
    }
    prepared.sort_by(|left, right| left.key.cmp(&right.key));
    Ok(prepared)
}

struct ConcurrencyObserver {
    active: std::sync::atomic::AtomicUsize,
    maximum: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    barrier: Option<Arc<tokio::sync::Barrier>>,
}

impl ConcurrencyObserver {
    async fn enter(self: &Arc<Self>) -> ConcurrencyObservation {
        use std::sync::atomic::Ordering;

        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        #[cfg(test)]
        if let Some(barrier) = &self.barrier {
            barrier.wait().await;
        }
        ConcurrencyObservation {
            observer: Arc::clone(self),
        }
    }
}

struct ConcurrencyObservation {
    observer: Arc<ConcurrencyObserver>,
}

impl Drop for ConcurrencyObservation {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;

        self.observer.active.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;

    fn encode_png() -> Vec<u8> {
        let mut encoded = Vec::new();
        let mut encoder = png::Encoder::new(&mut encoded, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[10, 20, 30, 255]).unwrap();
        drop(writer);
        encoded
    }

    fn temporary_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rustverse-svg-startup-{label}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn default_manifest_contains_every_bundled_asset_once() {
        let manifest = StartupAssetManifest::default();
        assert_eq!(manifest.decode_concurrency(), 4);
        assert_eq!(manifest.entries().len(), BundledAsset::all().len());
        for asset in BundledAsset::all() {
            assert!(
                manifest
                    .entries()
                    .contains(&StartupAssetEntry::bundled(asset))
            );
        }
    }

    #[test]
    fn manifest_rejects_zero_concurrency_duplicate_and_empty_keys() {
        assert_eq!(
            StartupAssetManifest::new([], 0).unwrap_err(),
            StartupManifestError::ZeroDecodeConcurrency
        );
        let duplicate = StartupAssetEntry::cached("same", "/cache", "a.png").unwrap();
        assert_eq!(
            StartupAssetManifest::new([duplicate.clone(), duplicate], 1).unwrap_err(),
            StartupManifestError::DuplicateKey("same".to_owned())
        );
        assert_eq!(
            StartupAssetEntry::cached("", "/cache", "a.png").unwrap_err(),
            StartupManifestError::EmptyKey
        );
        assert!(
            StartupAssetManifest::new(
                [
                    StartupAssetEntry::cached("first", "/cache", "a.png").unwrap(),
                    StartupAssetEntry::cached("second", "/cache", "a.png").unwrap(),
                ],
                1,
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn cached_png_is_prepared_and_results_are_key_sorted() {
        let root = temporary_root("cached");
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(root.join("image.png"), encode_png())
            .await
            .unwrap();
        let manifest = StartupAssetManifest::new(
            [
                StartupAssetEntry::cached("z-last", &root, "image.png").unwrap(),
                StartupAssetEntry::bundled(BundledAsset::StarIcon),
                StartupAssetEntry::cached("a-first", &root, "image.png").unwrap(),
            ],
            2,
        )
        .unwrap();

        let prepared = prepare_startup_manifest(manifest, DecodeLimits::default())
            .await
            .unwrap();
        assert_eq!(
            prepared
                .iter()
                .map(|asset| asset.key.to_string())
                .collect::<Vec<_>>(),
            ["a-first", "image/star-icon.png", "z-last"]
        );
        assert_eq!(prepared[0].decoded.rgba8, [10, 20, 30, 255]);

        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn preparation_never_exceeds_decode_concurrency() {
        let root = temporary_root("concurrency");
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(root.join("image.png"), encode_png())
            .await
            .unwrap();
        let entries = (0..4)
            .map(|index| {
                StartupAssetEntry::cached(
                    format!("cached-{index}"),
                    &root,
                    PathBuf::from("image.png"),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let manifest = StartupAssetManifest::new(entries, 2).unwrap();
        let observer = Arc::new(ConcurrencyObserver {
            active: std::sync::atomic::AtomicUsize::new(0),
            maximum: std::sync::atomic::AtomicUsize::new(0),
            barrier: Some(Arc::new(tokio::sync::Barrier::new(2))),
        });

        let prepared = prepare_startup_manifest_inner(
            manifest,
            DecodeLimits::default(),
            Some(observer.clone()),
        )
        .await
        .unwrap();
        assert_eq!(prepared.len(), 4);
        assert_eq!(observer.active.load(Ordering::SeqCst), 0);
        assert_eq!(observer.maximum.load(Ordering::SeqCst), 2);

        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn bad_cache_source_is_reported() {
        let root = temporary_root("missing");
        tokio::fs::create_dir_all(&root).await.unwrap();
        let manifest = StartupAssetManifest::new(
            [StartupAssetEntry::cached("missing", &root, "missing.png").unwrap()],
            1,
        )
        .unwrap();

        assert!(
            prepare_startup_manifest(manifest, DecodeLimits::default())
                .await
                .is_err()
        );
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
