//! Encoded image sources resolved before draw-command construction.

use std::fmt;
use std::path::{Component, Path, PathBuf};

use tokio::io::AsyncReadExt;

/// Static images shipped with `rustverse_svg`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum BundledImage {
    DeadlyAssault,
    ShiyuDefense,
    Hollows,
    StarIcon,
}

impl BundledImage {
    pub(crate) const fn id(self) -> &'static str {
        match self {
            Self::DeadlyAssault => "rustverse-bundled-da.webp",
            Self::ShiyuDefense => "rustverse-bundled-shiyu.webp",
            Self::Hollows => "rustverse-bundled-hollows.png",
            Self::StarIcon => "image/star-icon.png",
        }
    }

    pub(crate) const fn encoded(self) -> &'static [u8] {
        match self {
            Self::DeadlyAssault => include_bytes!("../../../image/da.webp"),
            Self::ShiyuDefense => include_bytes!("../../../image/shiyu.webp"),
            Self::Hollows => include_bytes!("../../../image/hollows.png"),
            Self::StarIcon => include_bytes!("../../../image/star-icon.png"),
        }
    }

    pub(crate) fn from_id(id: &str) -> Option<Self> {
        match id {
            "rustverse-bundled-da.webp" => Some(Self::DeadlyAssault),
            "rustverse-bundled-shiyu.webp" => Some(Self::ShiyuDefense),
            "rustverse-bundled-hollows.png" => Some(Self::Hollows),
            "image/star-icon.png" | "rustverse-bundled-star-icon.png" => Some(Self::StarIcon),
            _ => None,
        }
    }

    pub(crate) const fn all() -> [Self; 4] {
        [
            Self::DeadlyAssault,
            Self::ShiyuDefense,
            Self::Hollows,
            Self::StarIcon,
        ]
    }
}

/// A traversal-safe file inside the configured remote-image cache.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct CacheImageSource {
    cache_root: PathBuf,
    path: PathBuf,
}

impl CacheImageSource {
    pub(crate) fn new(cache_root: &Path, relative_path: &Path) -> Result<Self, CacheSourceError> {
        if relative_path.as_os_str().is_empty()
            || relative_path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(CacheSourceError::InvalidRelativePath(
                relative_path.to_path_buf(),
            ));
        }
        Ok(Self {
            cache_root: cache_root.to_path_buf(),
            path: cache_root.join(relative_path),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) async fn read(&self, max_encoded_bytes: usize) -> Result<Vec<u8>, CacheSourceError> {
        let root_metadata = tokio::fs::symlink_metadata(&self.cache_root)
            .await
            .map_err(|source| CacheSourceError::Io {
                path: self.cache_root.clone(),
                source,
            })?;
        if root_metadata.file_type().is_symlink() {
            return Err(CacheSourceError::SymlinkNotAllowed(self.cache_root.clone()));
        }
        let canonical_root = tokio::fs::canonicalize(&self.cache_root)
            .await
            .map_err(|source| CacheSourceError::Io {
                path: self.cache_root.clone(),
                source,
            })?;
        let mut checked_path = self.cache_root.clone();
        for component in self
            .path
            .strip_prefix(&self.cache_root)
            .expect("constructor always joins cache_root")
            .components()
        {
            checked_path.push(component);
            let metadata = tokio::fs::symlink_metadata(&checked_path)
                .await
                .map_err(|source| CacheSourceError::Io {
                    path: checked_path.clone(),
                    source,
                })?;
            if metadata.file_type().is_symlink() {
                return Err(CacheSourceError::SymlinkNotAllowed(checked_path));
            }
        }
        let canonical_path = tokio::fs::canonicalize(&self.path)
            .await
            .map_err(|source| CacheSourceError::Io {
                path: self.path.clone(),
                source,
            })?;
        if !canonical_path.starts_with(&canonical_root) {
            return Err(CacheSourceError::OutsideCacheRoot {
                path: self.path.clone(),
                cache_root: self.cache_root.clone(),
            });
        }
        // Portable async file APIs do not expose openat2-style resolution.
        // Opening the checked canonical path and validating that same handle
        // prevents final-link and metadata/read mismatches. The cache producer
        // must still avoid concurrently replacing parent directories.
        let file = tokio::fs::File::open(&canonical_path)
            .await
            .map_err(|source| CacheSourceError::Io {
                path: self.path.clone(),
                source,
            })?;
        let metadata = file
            .metadata()
            .await
            .map_err(|source| CacheSourceError::Io {
                path: self.path.clone(),
                source,
            })?;
        if !metadata.is_file() {
            return Err(CacheSourceError::NotAFile(self.path.clone()));
        }
        let actual = metadata.len();
        if actual > max_encoded_bytes as u64 {
            return Err(CacheSourceError::EncodedTooLarge {
                path: self.path.clone(),
                actual,
                limit: max_encoded_bytes,
            });
        }
        let bounded_len = max_encoded_bytes.saturating_add(1);
        let mut bytes = Vec::with_capacity(
            usize::try_from(actual)
                .unwrap_or(max_encoded_bytes)
                .min(bounded_len),
        );
        file.take(bounded_len as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(|source| CacheSourceError::Io {
                path: self.path.clone(),
                source,
            })?;
        if bytes.len() > max_encoded_bytes {
            return Err(CacheSourceError::EncodedTooLarge {
                path: self.path.clone(),
                actual: bytes.len() as u64,
                limit: max_encoded_bytes,
            });
        }
        Ok(bytes)
    }
}

#[derive(Debug)]
pub(crate) enum CacheSourceError {
    InvalidRelativePath(PathBuf),
    SymlinkNotAllowed(PathBuf),
    OutsideCacheRoot {
        path: PathBuf,
        cache_root: PathBuf,
    },
    NotAFile(PathBuf),
    EncodedTooLarge {
        path: PathBuf,
        actual: u64,
        limit: usize,
    },
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for CacheSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRelativePath(path) => {
                write!(
                    formatter,
                    "cache image path must be relative: {}",
                    path.display()
                )
            }
            Self::SymlinkNotAllowed(path) => {
                write!(
                    formatter,
                    "cache image path contains symlink: {}",
                    path.display()
                )
            }
            Self::OutsideCacheRoot { path, cache_root } => write!(
                formatter,
                "cache image {} resolves outside {}",
                path.display(),
                cache_root.display()
            ),
            Self::NotAFile(path) => {
                write!(formatter, "cache image {} is not a file", path.display())
            }
            Self::EncodedTooLarge {
                path,
                actual,
                limit,
            } => write!(
                formatter,
                "cache image {} is {actual} bytes; limit is {limit}",
                path.display()
            ),
            Self::Io { path, source } => {
                write!(
                    formatter,
                    "cannot read cache image {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for CacheSourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_bundled_ids_resolve_and_have_matching_signatures() {
        for image in BundledImage::all() {
            assert_eq!(BundledImage::from_id(image.id()), Some(image));
            let bytes = image.encoded();
            assert!(
                bytes.starts_with(b"\x89PNG\r\n\x1a\n")
                    || (bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP")
            );
        }
    }

    #[test]
    fn all_bundled_images_decode_with_production_limits() {
        for image in BundledImage::all() {
            super::super::decode::decode_image(
                image.encoded(),
                super::super::decode::DecodeLimits::default(),
            )
            .unwrap_or_else(|error| panic!("{} did not decode: {error}", image.id()));
        }
    }

    #[test]
    fn cache_source_rejects_escape_and_absolute_paths() {
        let root = Path::new("/cache");
        for invalid in [
            Path::new(""),
            Path::new("../secret.png"),
            Path::new("nested/../../secret.png"),
            Path::new("/absolute.png"),
        ] {
            assert!(CacheImageSource::new(root, invalid).is_err(), "{invalid:?}");
        }
        let source = CacheImageSource::new(root, Path::new("ab/image.webp")).unwrap();
        assert_eq!(source.path(), Path::new("/cache/ab/image.webp"));
    }

    #[tokio::test]
    async fn cache_read_enforces_encoded_limit() {
        let unique = format!(
            "rustverse-svg-cache-source-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        );
        let root = std::env::temp_dir().join(unique);
        tokio::fs::create_dir_all(&root).await.unwrap();
        let source = CacheImageSource::new(&root, Path::new("image.bin")).unwrap();
        tokio::fs::write(source.path(), b"four").await.unwrap();

        assert_eq!(source.read(4).await.unwrap(), b"four");
        assert!(matches!(
            source.read(3).await.unwrap_err(),
            CacheSourceError::EncodedTooLarge { actual: 4, .. }
        ));

        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cache_read_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let unique = format!("rustverse-svg-cache-symlink-{}", std::process::id());
        let base = std::env::temp_dir().join(unique);
        let root = base.join("cache");
        let outside = base.join("outside");
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::create_dir_all(&outside).await.unwrap();
        tokio::fs::write(outside.join("secret.png"), b"secret")
            .await
            .unwrap();
        symlink(&outside, root.join("escape")).unwrap();
        let source = CacheImageSource::new(&root, Path::new("escape/secret.png")).unwrap();

        assert!(matches!(
            source.read(64).await.unwrap_err(),
            CacheSourceError::SymlinkNotAllowed(_)
        ));

        tokio::fs::remove_dir_all(base).await.unwrap();
    }
}
