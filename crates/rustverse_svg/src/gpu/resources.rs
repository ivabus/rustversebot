//! Renderer-owned GPU resources that live across individual render requests.
//!
//! The concrete GPU objects will be added to these types as the GPU renderer is
//! implemented. Keeping their construction behind [`PersistentResources`]
//! makes the ownership boundary explicit in the meantime: a renderer creates
//! one aggregate at startup and reuses the same resource sets thereafter.

/// Persistent image textures and their atlas metadata.
///
/// This type intentionally implements neither [`Clone`] nor [`Default`].
pub(crate) struct ImageAtlasSet;

impl ImageAtlasSet {
    fn new(counts: &mut PersistentResourceCounts) -> Self {
        counts.image_atlas_sets += 1;
        Self
    }
}

/// Persistent text resources owned by the renderer.
///
/// The real `glyphon::Cache`, `glyphon::Viewport`, and `glyphon::TextAtlas`
/// belong directly in this type once the backend is wired up. This is
/// deliberately an ownership shell rather than a rasterizer abstraction: the
/// renderer will use glyphon as its text path without a parallel fallback.
///
/// This type intentionally implements neither [`Clone`] nor [`Default`].
pub(crate) struct GlyphonState;

impl GlyphonState {
    fn new(counts: &mut PersistentResourceCounts) -> Self {
        counts.glyphon_states += 1;
        Self
    }
}

/// Persistent registry of reusable GPU effect resources.
///
/// This type intentionally implements neither [`Clone`] nor [`Default`].
pub(crate) struct EffectRegistry;

impl EffectRegistry {
    fn new(counts: &mut PersistentResourceCounts) -> Self {
        counts.effect_registries += 1;
        Self
    }
}

/// Read-only construction counters for one persistent resource aggregate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PersistentResourceCounts {
    pub(crate) image_atlas_sets: usize,
    pub(crate) glyphon_states: usize,
    pub(crate) effect_registries: usize,
}

/// The complete set of long-lived resources owned by one renderer.
///
/// Construct this once during renderer startup, then borrow its members for
/// every render request. Private member constructors prevent callers from
/// accidentally creating a second resource set independently.
///
/// This type intentionally implements neither [`Clone`] nor [`Default`].
pub(crate) struct PersistentResources {
    _image_atlases: ImageAtlasSet,
    _glyphon: GlyphonState,
    _effects: EffectRegistry,
    construction_counts: PersistentResourceCounts,
}

impl PersistentResources {
    pub(crate) fn new() -> Self {
        let mut construction_counts = PersistentResourceCounts::default();
        Self {
            _image_atlases: ImageAtlasSet::new(&mut construction_counts),
            _glyphon: GlyphonState::new(&mut construction_counts),
            _effects: EffectRegistry::new(&mut construction_counts),
            construction_counts,
        }
    }

    pub(crate) fn construction_counts(&self) -> PersistentResourceCounts {
        self.construction_counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_constructs_each_resource_set_once() {
        let resources = PersistentResources::new();

        assert_eq!(
            resources.construction_counts(),
            PersistentResourceCounts {
                image_atlas_sets: 1,
                glyphon_states: 1,
                effect_registries: 1,
            }
        );
    }

    #[test]
    fn repeated_access_reuses_the_same_resource_instances() {
        let resources = PersistentResources::new();

        assert!(std::ptr::eq(
            &resources._image_atlases,
            &resources._image_atlases
        ));
        assert!(std::ptr::eq(&resources._glyphon, &resources._glyphon));
        assert!(std::ptr::eq(&resources._effects, &resources._effects));
        assert_eq!(
            resources.construction_counts(),
            resources.construction_counts()
        );
    }
}
