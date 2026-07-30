//! Backend-neutral scene contract.
//!
//! Phase 1 establishes logical sizing and ordered layers. Concrete primitives
//! and effects are added as card families migrate away from SVG.

/// A scene size in logical pixels, independent of output scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogicalSize {
    pub width: f32,
    pub height: f32,
}

/// A typed scene whose nodes are kept in painter's order.
#[derive(Clone, Debug, PartialEq)]
pub struct Scene<Node> {
    pub logical_size: LogicalSize,
    pub nodes: Vec<Node>,
}

impl<Node> Scene<Node> {
    pub fn new(logical_size: LogicalSize) -> Self {
        Self {
            logical_size,
            nodes: Vec::new(),
        }
    }
}
