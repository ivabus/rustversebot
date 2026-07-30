use std::{fmt, num::NonZeroU32};

/// Stable logical identity used by manifests and draw commands.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct AssetKey(String);

impl AssetKey {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, AtlasError> {
        let value = value.into();
        if value.is_empty() {
            return Err(AtlasError::EmptyAssetKey);
        }
        Ok(Self(value))
    }
}

impl fmt::Display for AssetKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Hash of canonical decoded pixel content.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ContentHash(pub(crate) [u8; 32]);

impl ContentHash {
    pub(crate) const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

/// Lifetime and packing class of an atlas asset.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum AssetClass {
    /// Preloaded at renderer startup. Pages containing these assets are pinned.
    StartupStatic,
    /// Added after startup to append-only dynamic pages.
    Dynamic,
    /// Asset expected to require its own page.
    Oversize,
}

/// Decoded pixel layout accepted by the atlas planner.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum ImageFormat {
    Rgba8Unorm,
}

impl ImageFormat {
    pub(crate) const fn bytes_per_pixel(self) -> u64 {
        match self {
            Self::Rgba8Unorm => 4,
        }
    }
}

/// Decoder output needed by packing. Pixel bytes are intentionally absent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DecodedAssetMetadata {
    pub(crate) key: AssetKey,
    pub(crate) content_hash: ContentHash,
    pub(crate) width: NonZeroU32,
    pub(crate) height: NonZeroU32,
    pub(crate) format: ImageFormat,
    pub(crate) class: AssetClass,
}

impl DecodedAssetMetadata {
    pub(crate) fn new(
        key: AssetKey,
        content_hash: ContentHash,
        width: u32,
        height: u32,
        format: ImageFormat,
        class: AssetClass,
    ) -> Result<Self, AtlasError> {
        Ok(Self {
            key,
            content_hash,
            width: NonZeroU32::new(width).ok_or(AtlasError::ZeroDimension)?,
            height: NonZeroU32::new(height).ok_or(AtlasError::ZeroDimension)?,
            format,
            class,
        })
    }
}

/// Index plus generation prevents stale handles from resolving after a future
/// implementation introduces slot reuse or repacking.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct RegionHandle {
    pub(crate) index: u32,
    pub(crate) generation: NonZeroU32,
}

/// Pixel coordinates within an atlas page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PixelRect {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl PixelRect {
    pub(crate) const fn right(self) -> u32 {
        self.x + self.width
    }

    pub(crate) const fn bottom(self) -> u32 {
        self.y + self.height
    }

    pub(crate) const fn intersects(self, other: Self) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }
}

/// Normalized texture coordinates for the unpadded resident image.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct UvRect {
    pub(crate) min: [f32; 2],
    pub(crate) max: [f32; 2],
}

/// Immutable metadata consumed by encoded image draw commands.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResidentRegion {
    pub(crate) handle: RegionHandle,
    pub(crate) page: u32,
    pub(crate) pixels: PixelRect,
    pub(crate) uv: UvRect,
    pub(crate) source_width: NonZeroU32,
    pub(crate) source_height: NonZeroU32,
    pub(crate) format: ImageFormat,
    pub(crate) content_hash: ContentHash,
}

/// Atlas page allocation known before GPU texture creation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AtlasPage {
    pub(crate) index: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pinned: bool,
    pub(crate) oversize: bool,
}

impl AtlasPage {
    pub(crate) fn byte_len(&self, format: ImageFormat) -> Result<u64, AtlasError> {
        u64::from(self.width)
            .checked_mul(u64::from(self.height))
            .and_then(|pixels| pixels.checked_mul(format.bytes_per_pixel()))
            .ok_or(AtlasError::ArithmeticOverflow)
    }
}

/// Hard bounds for the persistent atlas set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AtlasConfig {
    pub(crate) page_width: NonZeroU32,
    pub(crate) page_height: NonZeroU32,
    pub(crate) padding: u32,
    pub(crate) max_pages: NonZeroU32,
    pub(crate) max_bytes: u64,
}

impl AtlasConfig {
    pub(crate) fn new(
        page_width: u32,
        page_height: u32,
        padding: u32,
        max_pages: u32,
        max_bytes: u64,
    ) -> Result<Self, AtlasError> {
        if max_bytes == 0 {
            return Err(AtlasError::ZeroBudget);
        }
        Ok(Self {
            page_width: NonZeroU32::new(page_width).ok_or(AtlasError::ZeroDimension)?,
            page_height: NonZeroU32::new(page_height).ok_or(AtlasError::ZeroDimension)?,
            padding,
            max_pages: NonZeroU32::new(max_pages).ok_or(AtlasError::ZeroBudget)?,
            max_bytes,
        })
    }
}

/// Observable resident resource and deduplication counts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct AtlasMetrics {
    pub(crate) startup_builds: u64,
    pub(crate) pages: u32,
    pub(crate) pinned_pages: u32,
    pub(crate) oversize_pages: u32,
    pub(crate) allocated_bytes: u64,
    pub(crate) regions: u32,
    pub(crate) asset_keys: u32,
    pub(crate) deduplicated_assets: u64,
    pub(crate) occupied_pixels: u64,
    pub(crate) usable_pixels: u64,
    pub(crate) configured_max_pages: u32,
    pub(crate) configured_max_bytes: u64,
    pub(crate) remaining_pages: u32,
    pub(crate) remaining_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AtlasInsertKind {
    Hit,
    Inserted,
    Deduplicated,
    Versioned { deduplicated: bool },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AtlasInsertOutcome {
    pub(crate) handle: RegionHandle,
    pub(crate) previous: Option<RegionHandle>,
    pub(crate) kind: AtlasInsertKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AtlasError {
    EmptyAssetKey,
    ZeroDimension,
    ZeroBudget,
    ArithmeticOverflow,
    DuplicateKey {
        key: AssetKey,
    },
    ContentHashCollision {
        hash: ContentHash,
    },
    RuntimeAssetMustBeDynamic {
        key: AssetKey,
    },
    PageCapacityExceeded {
        max_pages: u32,
        requested_pages: u32,
    },
    MemoryBudgetExceeded {
        max_bytes: u64,
        requested_bytes: u64,
    },
    RegionIndexOverflow,
}

impl fmt::Display for AtlasError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyAssetKey => formatter.write_str("asset key must not be empty"),
            Self::ZeroDimension => formatter.write_str("atlas dimensions must be non-zero"),
            Self::ZeroBudget => formatter.write_str("atlas budgets must be non-zero"),
            Self::ArithmeticOverflow => formatter.write_str("atlas size arithmetic overflowed"),
            Self::DuplicateKey { key } => {
                write!(formatter, "asset key {key} identifies different content")
            }
            Self::ContentHashCollision { hash } => {
                write!(formatter, "content hash collision for {hash:?}")
            }
            Self::RuntimeAssetMustBeDynamic { key } => {
                write!(formatter, "runtime asset {key} must use the dynamic class")
            }
            Self::PageCapacityExceeded {
                max_pages,
                requested_pages,
            } => write!(
                formatter,
                "atlas page capacity {max_pages} exceeded by request for {requested_pages} pages"
            ),
            Self::MemoryBudgetExceeded {
                max_bytes,
                requested_bytes,
            } => write!(
                formatter,
                "atlas memory budget {max_bytes} bytes exceeded by request for {requested_bytes} bytes"
            ),
            Self::RegionIndexOverflow => formatter.write_str("atlas region index overflowed"),
        }
    }
}

impl std::error::Error for AtlasError {}
