//! Card-specific scene-building boundary.
//!
//! Card builders consume the shared prepared views from `model`; they do not
//! define parallel copies of those models. Implementations arrive one card
//! family at a time during the GPU migration.

use crate::scene::Scene;

/// Converts one prepared card view into backend-neutral scene nodes.
pub trait SceneBuilder<View: ?Sized> {
    type Node;

    fn build(&self, view: &View) -> anyhow::Result<Scene<Self::Node>>;
}
