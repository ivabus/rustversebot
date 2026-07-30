use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashMap},
    num::NonZeroU32,
};

use super::types::{
    AssetClass, AssetKey, AtlasConfig, AtlasError, AtlasInsertKind, AtlasInsertOutcome,
    AtlasMetrics, AtlasPage, ContentHash, DecodedAssetMetadata, ImageFormat, PixelRect,
    RegionHandle, ResidentRegion, UvRect,
};

const INITIAL_GENERATION: NonZeroU32 = NonZeroU32::MIN;

#[derive(Clone, Debug)]
struct PageState {
    page: AtlasPage,
    cursor_x: u32,
    cursor_y: u32,
    row_height: u32,
}

impl PageState {
    fn standard(index: u32, config: AtlasConfig, pinned: bool) -> Self {
        Self {
            page: AtlasPage {
                index,
                width: config.page_width.get(),
                height: config.page_height.get(),
                pinned,
                oversize: false,
            },
            cursor_x: 0,
            cursor_y: 0,
            row_height: 0,
        }
    }

    fn oversize(index: u32, width: u32, height: u32, pinned: bool) -> Self {
        Self {
            page: AtlasPage {
                index,
                width,
                height,
                pinned,
                oversize: true,
            },
            cursor_x: width,
            cursor_y: height,
            row_height: height,
        }
    }

    fn place(&mut self, width: u32, height: u32, padding: u32) -> Option<PixelRect> {
        let padded_width = width.checked_add(padding.checked_mul(2)?)?;
        let padded_height = height.checked_add(padding.checked_mul(2)?)?;
        if padded_width > self.page.width || padded_height > self.page.height {
            return None;
        }

        let mut x = self.cursor_x;
        let mut y = self.cursor_y;
        let mut row_height = self.row_height;
        if x.checked_add(padded_width)? > self.page.width {
            x = 0;
            y = y.checked_add(row_height)?;
            row_height = 0;
        }
        if y.checked_add(padded_height)? > self.page.height {
            return None;
        }

        self.cursor_x = x + padded_width;
        self.cursor_y = y;
        self.row_height = row_height.max(padded_height);
        Some(PixelRect {
            x: x + padding,
            y: y + padding,
            width,
            height,
        })
    }
}

/// Deterministic CPU-side model of one persistent image atlas set.
///
/// Startup pages are immutable and pinned. Runtime insertion only uses
/// append-only dynamic pages, so every returned handle remains valid for the
/// model's complete lifetime.
#[derive(Clone, Debug)]
pub(crate) struct AtlasSetModel {
    config: AtlasConfig,
    pages: Vec<PageState>,
    regions: Vec<ResidentRegion>,
    key_handles: BTreeMap<AssetKey, RegionHandle>,
    hash_handles: HashMap<ContentHash, RegionHandle>,
    metrics: AtlasMetrics,
}

impl AtlasSetModel {
    pub(crate) fn build_startup(
        config: AtlasConfig,
        assets: impl IntoIterator<Item = DecodedAssetMetadata>,
    ) -> Result<Self, AtlasError> {
        let mut assets: Vec<_> = assets.into_iter().collect();
        assets.sort_by(|left, right| {
            (
                left.class,
                Reverse(left.height),
                Reverse(left.width),
                &left.key,
            )
                .cmp(&(
                    right.class,
                    Reverse(right.height),
                    Reverse(right.width),
                    &right.key,
                ))
        });

        let mut model = Self {
            config,
            pages: Vec::new(),
            regions: Vec::new(),
            key_handles: BTreeMap::new(),
            hash_handles: HashMap::new(),
            metrics: AtlasMetrics {
                startup_builds: 1,
                configured_max_pages: config.max_pages.get(),
                configured_max_bytes: config.max_bytes,
                remaining_pages: config.max_pages.get(),
                remaining_bytes: config.max_bytes,
                ..AtlasMetrics::default()
            },
        };
        for asset in assets {
            model.insert(asset, true)?;
        }
        Ok(model)
    }

    pub(crate) fn insert_runtime(
        &mut self,
        asset: DecodedAssetMetadata,
    ) -> Result<AtlasInsertOutcome, AtlasError> {
        if asset.class != AssetClass::Dynamic && asset.class != AssetClass::Oversize {
            return Err(AtlasError::RuntimeAssetMustBeDynamic {
                key: asset.key.clone(),
            });
        }
        let previous = self.key_handles.get(&asset.key).copied();
        if let Some(existing) = previous {
            let region = self
                .region(existing)
                .expect("internally stored atlas handle must resolve");
            if region.content_hash == asset.content_hash
                && region.source_width == asset.width
                && region.source_height == asset.height
                && region.format == asset.format
            {
                self.metrics.deduplicated_assets += 1;
                return Ok(AtlasInsertOutcome {
                    handle: existing,
                    previous: None,
                    kind: AtlasInsertKind::Hit,
                });
            }
        }

        let old_region_count = self.regions.len();
        let handle = self.insert_without_key_check(asset, false, previous.is_none())?;
        let deduplicated = self.regions.len() == old_region_count;
        let kind = match previous {
            Some(_) => AtlasInsertKind::Versioned { deduplicated },
            None if deduplicated => AtlasInsertKind::Deduplicated,
            None => AtlasInsertKind::Inserted,
        };
        Ok(AtlasInsertOutcome {
            handle,
            previous,
            kind,
        })
    }

    pub(crate) fn handle(&self, key: &AssetKey) -> Option<RegionHandle> {
        self.key_handles.get(key).copied()
    }

    pub(crate) const fn config(&self) -> AtlasConfig {
        self.config
    }

    pub(crate) fn region(&self, handle: RegionHandle) -> Option<&ResidentRegion> {
        let region = self.regions.get(handle.index as usize)?;
        (region.handle.generation == handle.generation).then_some(region)
    }

    pub(crate) fn pages(&self) -> impl ExactSizeIterator<Item = &AtlasPage> {
        self.pages.iter().map(|state| &state.page)
    }

    pub(crate) fn regions(&self) -> impl ExactSizeIterator<Item = &ResidentRegion> {
        self.regions.iter()
    }

    pub(crate) const fn metrics(&self) -> AtlasMetrics {
        self.metrics
    }

    fn insert(
        &mut self,
        asset: DecodedAssetMetadata,
        startup: bool,
    ) -> Result<RegionHandle, AtlasError> {
        if let Some(existing) = self.key_handles.get(&asset.key).copied() {
            let region = self
                .region(existing)
                .expect("internally stored atlas handle must resolve");
            if region.content_hash != asset.content_hash
                || region.source_width != asset.width
                || region.source_height != asset.height
                || region.format != asset.format
            {
                return Err(AtlasError::DuplicateKey { key: asset.key });
            }
            self.metrics.deduplicated_assets += 1;
            return Ok(existing);
        }

        self.insert_without_key_check(asset, startup, true)
    }

    fn insert_without_key_check(
        &mut self,
        asset: DecodedAssetMetadata,
        pinned: bool,
        key_is_new: bool,
    ) -> Result<RegionHandle, AtlasError> {
        if let Some(existing) = self.hash_handles.get(&asset.content_hash).copied() {
            let region = self
                .region(existing)
                .expect("internally stored atlas handle must resolve");
            if region.source_width != asset.width
                || region.source_height != asset.height
                || region.format != asset.format
            {
                return Err(AtlasError::ContentHashCollision {
                    hash: asset.content_hash,
                });
            }
            self.key_handles.insert(asset.key, existing);
            self.metrics.asset_keys += u32::from(key_is_new);
            self.metrics.deduplicated_assets += 1;
            return Ok(existing);
        }

        let width = asset.width.get();
        let height = asset.height.get();
        let double_padding = self
            .config
            .padding
            .checked_mul(2)
            .ok_or(AtlasError::ArithmeticOverflow)?;
        let padded_width = width
            .checked_add(double_padding)
            .ok_or(AtlasError::ArithmeticOverflow)?;
        let padded_height = height
            .checked_add(double_padding)
            .ok_or(AtlasError::ArithmeticOverflow)?;
        let requires_oversize = asset.class == AssetClass::Oversize
            || padded_width > self.config.page_width.get()
            || padded_height > self.config.page_height.get();

        let (page_index, pixels) = if requires_oversize {
            let page_index = self.allocate_page(padded_width, padded_height, pinned, true)?;
            (
                page_index,
                PixelRect {
                    x: self.config.padding,
                    y: self.config.padding,
                    width,
                    height,
                },
            )
        } else {
            let mut placement = None;
            for state in &mut self.pages {
                if state.page.pinned == pinned && !state.page.oversize {
                    if let Some(pixels) = state.place(width, height, self.config.padding) {
                        placement = Some((state.page.index, pixels));
                        break;
                    }
                }
            }
            match placement {
                Some(placement) => placement,
                None => {
                    let page_index = self.allocate_page(
                        self.config.page_width.get(),
                        self.config.page_height.get(),
                        pinned,
                        false,
                    )?;
                    let pixels = self.pages[page_index as usize]
                        .place(width, height, self.config.padding)
                        .expect("new standard atlas page must fit a non-oversize asset");
                    (page_index, pixels)
                }
            }
        };

        let region_index =
            u32::try_from(self.regions.len()).map_err(|_| AtlasError::RegionIndexOverflow)?;
        let handle = RegionHandle {
            index: region_index,
            generation: INITIAL_GENERATION,
        };
        let page = &self.pages[page_index as usize].page;
        let region = ResidentRegion {
            handle,
            page: page_index,
            pixels,
            uv: UvRect {
                min: [
                    pixels.x as f32 / page.width as f32,
                    pixels.y as f32 / page.height as f32,
                ],
                max: [
                    pixels.right() as f32 / page.width as f32,
                    pixels.bottom() as f32 / page.height as f32,
                ],
            },
            source_width: asset.width,
            source_height: asset.height,
            format: asset.format,
            content_hash: asset.content_hash,
        };
        self.regions.push(region);
        self.key_handles.insert(asset.key, handle);
        self.hash_handles.insert(asset.content_hash, handle);
        self.metrics.regions += 1;
        self.metrics.asset_keys += u32::from(key_is_new);
        let padded_width = width
            .checked_add(double_padding)
            .ok_or(AtlasError::ArithmeticOverflow)?;
        let padded_height = height
            .checked_add(double_padding)
            .ok_or(AtlasError::ArithmeticOverflow)?;
        self.metrics.occupied_pixels = self
            .metrics
            .occupied_pixels
            .checked_add(
                u64::from(padded_width)
                    .checked_mul(u64::from(padded_height))
                    .ok_or(AtlasError::ArithmeticOverflow)?,
            )
            .ok_or(AtlasError::ArithmeticOverflow)?;
        Ok(handle)
    }

    fn allocate_page(
        &mut self,
        width: u32,
        height: u32,
        pinned: bool,
        oversize: bool,
    ) -> Result<u32, AtlasError> {
        let requested_pages = u32::try_from(self.pages.len())
            .map_err(|_| AtlasError::ArithmeticOverflow)?
            .checked_add(1)
            .ok_or(AtlasError::ArithmeticOverflow)?;
        if requested_pages > self.config.max_pages.get() {
            return Err(AtlasError::PageCapacityExceeded {
                max_pages: self.config.max_pages.get(),
                requested_pages,
            });
        }

        let page = AtlasPage {
            index: requested_pages - 1,
            width,
            height,
            pinned,
            oversize,
        };
        let requested_bytes = self
            .metrics
            .allocated_bytes
            .checked_add(page.byte_len(ImageFormat::Rgba8Unorm)?)
            .ok_or(AtlasError::ArithmeticOverflow)?;
        if requested_bytes > self.config.max_bytes {
            return Err(AtlasError::MemoryBudgetExceeded {
                max_bytes: self.config.max_bytes,
                requested_bytes,
            });
        }

        let state = if oversize {
            PageState::oversize(page.index, width, height, pinned)
        } else {
            PageState::standard(page.index, self.config, pinned)
        };
        self.pages.push(state);
        self.metrics.pages = requested_pages;
        self.metrics.allocated_bytes = requested_bytes;
        self.metrics.usable_pixels = self
            .metrics
            .usable_pixels
            .checked_add(
                u64::from(width)
                    .checked_mul(u64::from(height))
                    .ok_or(AtlasError::ArithmeticOverflow)?,
            )
            .ok_or(AtlasError::ArithmeticOverflow)?;
        self.metrics.remaining_pages = self.config.max_pages.get() - requested_pages;
        self.metrics.remaining_bytes = self.config.max_bytes - requested_bytes;
        self.metrics.pinned_pages += u32::from(pinned);
        self.metrics.oversize_pages += u32::from(oversize);
        Ok(page.index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(value: &str) -> AssetKey {
        AssetKey::new(value).unwrap()
    }

    fn asset(
        key_value: &str,
        hash_byte: u8,
        width: u32,
        height: u32,
        class: AssetClass,
    ) -> DecodedAssetMetadata {
        DecodedAssetMetadata::new(
            key(key_value),
            ContentHash::new([hash_byte; 32]),
            width,
            height,
            ImageFormat::Rgba8Unorm,
            class,
        )
        .unwrap()
    }

    fn config() -> AtlasConfig {
        AtlasConfig::new(64, 64, 1, 8, 8 * 64 * 64 * 4).unwrap()
    }

    fn snapshot(model: &AtlasSetModel, keys: &[&str]) -> Vec<(RegionHandle, ResidentRegion)> {
        keys.iter()
            .map(|value| {
                let handle = model.handle(&key(value)).unwrap();
                (handle, model.region(handle).unwrap().clone())
            })
            .collect()
    }

    #[test]
    fn startup_packing_is_deterministic_across_manifest_permutations() {
        let ordered = vec![
            asset("logo", 1, 20, 10, AssetClass::StartupStatic),
            asset("star", 2, 8, 8, AssetClass::StartupStatic),
            asset("boss", 3, 30, 24, AssetClass::StartupStatic),
        ];
        let mut reversed = ordered.clone();
        reversed.reverse();

        let first = AtlasSetModel::build_startup(config(), ordered).unwrap();
        let second = AtlasSetModel::build_startup(config(), reversed).unwrap();

        assert_eq!(
            snapshot(&first, &["logo", "star", "boss"]),
            snapshot(&second, &["logo", "star", "boss"])
        );
        assert_eq!(
            first.pages().cloned().collect::<Vec<_>>(),
            second.pages().cloned().collect::<Vec<_>>()
        );
    }

    #[test]
    fn resident_regions_stay_in_bounds_and_do_not_overlap() {
        let model = AtlasSetModel::build_startup(
            config(),
            [
                asset("a", 1, 30, 20, AssetClass::StartupStatic),
                asset("b", 2, 24, 24, AssetClass::StartupStatic),
                asset("c", 3, 18, 31, AssetClass::StartupStatic),
            ],
        )
        .unwrap();
        let regions: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|value| model.region(model.handle(&key(value)).unwrap()).unwrap())
            .collect();

        for region in &regions {
            let page = model.pages().nth(region.page as usize).unwrap();
            assert!(region.pixels.right() <= page.width);
            assert!(region.pixels.bottom() <= page.height);
            assert!(region.uv.min[0] >= 0.0 && region.uv.min[1] >= 0.0);
            assert!(region.uv.max[0] <= 1.0 && region.uv.max[1] <= 1.0);
        }
        for (index, left) in regions.iter().enumerate() {
            for right in &regions[index + 1..] {
                if left.page == right.page {
                    assert!(!left.pixels.intersects(right.pixels));
                }
            }
        }
    }

    #[test]
    fn content_hash_deduplicates_distinct_asset_keys() {
        let model = AtlasSetModel::build_startup(
            config(),
            [
                asset("logo-a", 7, 10, 10, AssetClass::StartupStatic),
                asset("logo-b", 7, 10, 10, AssetClass::StartupStatic),
            ],
        )
        .unwrap();

        assert_eq!(model.handle(&key("logo-a")), model.handle(&key("logo-b")));
        assert_eq!(model.metrics().regions, 1);
        assert_eq!(model.metrics().asset_keys, 2);
        assert_eq!(model.metrics().deduplicated_assets, 1);
    }

    #[test]
    fn startup_rejects_conflicting_versions_of_same_key() {
        let error = AtlasSetModel::build_startup(
            config(),
            [
                asset("logo", 1, 10, 10, AssetClass::StartupStatic),
                asset("logo", 2, 10, 10, AssetClass::StartupStatic),
            ],
        )
        .unwrap_err();

        assert!(matches!(error, AtlasError::DuplicateKey { .. }));
    }

    #[test]
    fn startup_preloads_once_and_pins_its_pages() {
        let model = AtlasSetModel::build_startup(
            config(),
            [asset("startup", 1, 20, 20, AssetClass::StartupStatic)],
        )
        .unwrap();

        assert_eq!(model.metrics().startup_builds, 1);
        assert_eq!(model.metrics().pinned_pages, 1);
        assert!(model.handle(&key("startup")).is_some());
        assert!(model.pages().all(|page| page.pinned));
    }

    #[test]
    fn first_runtime_insertion_allocates_dynamic_page() {
        let mut model = AtlasSetModel::build_startup(
            config(),
            [asset("startup", 1, 10, 10, AssetClass::StartupStatic)],
        )
        .unwrap();
        let before = model.metrics();

        let handle = model
            .insert_runtime(asset("remote", 2, 12, 12, AssetClass::Dynamic))
            .unwrap()
            .handle;
        let region = model.region(handle).unwrap();
        let page = model.pages().nth(region.page as usize).unwrap();

        assert!(!page.pinned);
        assert_eq!(model.metrics().pages, before.pages + 1);
        assert_eq!(model.metrics().regions, before.regions + 1);
    }

    #[test]
    fn warm_runtime_insertion_has_zero_new_allocation() {
        let mut model = AtlasSetModel::build_startup(config(), []).unwrap();
        let asset = asset("remote", 9, 12, 12, AssetClass::Dynamic);
        let first = model.insert_runtime(asset.clone()).unwrap();
        let warm_metrics = model.metrics();
        let second = model.insert_runtime(asset).unwrap();

        assert_eq!(first.handle, second.handle);
        assert_eq!(first.kind, AtlasInsertKind::Inserted);
        assert_eq!(second.kind, AtlasInsertKind::Hit);
        assert_eq!(model.metrics().pages, warm_metrics.pages);
        assert_eq!(
            model.metrics().allocated_bytes,
            warm_metrics.allocated_bytes
        );
        assert_eq!(model.metrics().regions, warm_metrics.regions);
        assert_eq!(
            model.metrics().deduplicated_assets,
            warm_metrics.deduplicated_assets + 1
        );
    }

    #[test]
    fn handles_remain_live_until_capacity_is_exhausted() {
        let tight = AtlasConfig::new(16, 16, 0, 2, 2 * 16 * 16 * 4).unwrap();
        let mut model = AtlasSetModel::build_startup(
            tight,
            [asset("static", 1, 16, 16, AssetClass::StartupStatic)],
        )
        .unwrap();
        let static_handle = model.handle(&key("static")).unwrap();
        let dynamic_handle = model
            .insert_runtime(asset("dynamic", 2, 16, 16, AssetClass::Dynamic))
            .unwrap()
            .handle;

        assert!(model.region(static_handle).is_some());
        assert!(model.region(dynamic_handle).is_some());
        assert!(matches!(
            model.insert_runtime(asset("overflow", 3, 16, 16, AssetClass::Dynamic)),
            Err(AtlasError::PageCapacityExceeded { .. })
        ));
        assert!(model.region(static_handle).is_some());
        assert!(model.region(dynamic_handle).is_some());
        assert!(
            model
                .region(RegionHandle {
                    generation: NonZeroU32::new(2).unwrap(),
                    ..static_handle
                })
                .is_none()
        );
    }

    #[test]
    fn memory_budget_is_enforced_before_page_allocation() {
        let too_small = AtlasConfig::new(32, 32, 0, 2, 32 * 32 * 4 - 1).unwrap();
        let error = AtlasSetModel::build_startup(
            too_small,
            [asset("image", 1, 8, 8, AssetClass::StartupStatic)],
        )
        .unwrap_err();

        assert!(matches!(error, AtlasError::MemoryBudgetExceeded { .. }));
    }

    #[test]
    fn oversize_asset_uses_owned_dedicated_page() {
        let mut model = AtlasSetModel::build_startup(config(), []).unwrap();
        let handle = model
            .insert_runtime(asset("portrait", 5, 80, 100, AssetClass::Oversize))
            .unwrap()
            .handle;
        let region = model.region(handle).unwrap();
        let page = model.pages().nth(region.page as usize).unwrap();

        assert!(page.oversize);
        assert!(!page.pinned);
        assert_eq!(page.width, 82);
        assert_eq!(page.height, 102);
        assert_eq!(model.metrics().oversize_pages, 1);
        assert_eq!(region.pixels.x, 1);
        assert_eq!(region.pixels.y, 1);
        assert_eq!(region.pixels.width, 80);
        assert_eq!(region.pixels.height, 100);
    }

    #[test]
    fn versioned_key_repoints_without_invalidating_old_handle() {
        let mut model = AtlasSetModel::build_startup(config(), []).unwrap();
        let first = model
            .insert_runtime(asset("boss", 1, 12, 12, AssetClass::Dynamic))
            .unwrap();
        let versioned = model
            .insert_runtime(asset("boss", 2, 12, 12, AssetClass::Dynamic))
            .unwrap();

        assert_eq!(
            versioned.kind,
            AtlasInsertKind::Versioned {
                deduplicated: false
            }
        );
        assert_eq!(versioned.previous, Some(first.handle));
        assert_ne!(versioned.handle, first.handle);
        assert_eq!(model.handle(&key("boss")), Some(versioned.handle));
        assert_eq!(
            model.region(first.handle).unwrap().content_hash,
            ContentHash::new([1; 32])
        );
        assert_eq!(
            model.region(versioned.handle).unwrap().content_hash,
            ContentHash::new([2; 32])
        );
        assert_eq!(model.metrics().asset_keys, 1);
        assert_eq!(model.metrics().regions, 2);
    }

    #[test]
    fn versioned_key_can_repoint_to_existing_content() {
        let mut model = AtlasSetModel::build_startup(config(), []).unwrap();
        let old = model
            .insert_runtime(asset("boss", 1, 12, 12, AssetClass::Dynamic))
            .unwrap();
        let existing = model
            .insert_runtime(asset("agent", 2, 12, 12, AssetClass::Dynamic))
            .unwrap();
        let versioned = model
            .insert_runtime(asset("boss", 2, 12, 12, AssetClass::Dynamic))
            .unwrap();

        assert_eq!(
            versioned.kind,
            AtlasInsertKind::Versioned { deduplicated: true }
        );
        assert_eq!(versioned.previous, Some(old.handle));
        assert_eq!(versioned.handle, existing.handle);
        assert_eq!(model.handle(&key("boss")), Some(existing.handle));
        assert!(model.region(old.handle).is_some());
        assert_eq!(model.metrics().asset_keys, 2);
        assert_eq!(model.metrics().regions, 2);
    }

    #[test]
    fn failed_version_keeps_previous_key_mapping() {
        let tight = AtlasConfig::new(16, 16, 0, 1, 16 * 16 * 4).unwrap();
        let mut model = AtlasSetModel::build_startup(tight, []).unwrap();
        let first = model
            .insert_runtime(asset("boss", 1, 16, 16, AssetClass::Dynamic))
            .unwrap();
        let before = model.metrics();

        assert!(matches!(
            model.insert_runtime(asset("boss", 2, 16, 16, AssetClass::Dynamic)),
            Err(AtlasError::PageCapacityExceeded { .. })
        ));
        assert_eq!(model.handle(&key("boss")), Some(first.handle));
        assert!(model.region(first.handle).is_some());
        assert_eq!(model.metrics(), before);
    }

    #[test]
    fn occupancy_and_capacity_metrics_are_exact() {
        let config = config();
        let mut model = AtlasSetModel::build_startup(
            config,
            [asset("static", 1, 10, 20, AssetClass::StartupStatic)],
        )
        .unwrap();
        let startup = model.metrics();

        assert_eq!(startup.occupied_pixels, 12 * 22);
        assert_eq!(startup.usable_pixels, 64 * 64);
        assert_eq!(startup.configured_max_pages, 8);
        assert_eq!(startup.configured_max_bytes, 8 * 64 * 64 * 4);
        assert_eq!(startup.remaining_pages, 7);
        assert_eq!(startup.remaining_bytes, 7 * 64 * 64 * 4);

        model
            .insert_runtime(asset("runtime", 2, 12, 12, AssetClass::Dynamic))
            .unwrap();
        let runtime = model.metrics();
        assert_eq!(runtime.occupied_pixels, 12 * 22 + 14 * 14);
        assert_eq!(runtime.usable_pixels, 2 * 64 * 64);
        assert_eq!(runtime.remaining_pages, 6);
        assert_eq!(runtime.remaining_bytes, 6 * 64 * 64 * 4);
    }
}
