//! Backend-neutral scene contract.
//!
//! Phase 1 establishes logical sizing and ordered layers. Concrete primitives
//! and effects are added as card families migrate away from SVG.

pub mod image;
pub mod paint;
pub mod primitives;

pub use image::{
    ImageAddressMode, ImageDimensions, ImageFit, ImageGeometryError, ImageHandle, ImageNode,
    ImagePlacement, ImageUv, place_image,
};
pub use paint::{
    AffineTransform, Color, DiagonalPattern, DotPattern, GradientStop, LinearGradient, Paint,
    PaintError, PaintSpace, PatternDescriptor, PatternPaint, RadialGradient,
    RepeatedTexturePattern, TexturePatternHandle,
};
pub use primitives::{
    Circle, CornerRadii, Fill, GeometryError, Point, Rect, RoundedRect, Shape, ShapeNode, Stroke,
};

/// A backend-neutral drawable kept in painter's order.
#[derive(Clone, Debug, PartialEq)]
pub enum SceneNode {
    Shape(ShapeNode),
    Image(ImageNode),
}

impl From<ShapeNode> for SceneNode {
    fn from(node: ShapeNode) -> Self {
        Self::Shape(node)
    }
}

impl From<ImageNode> for SceneNode {
    fn from(node: ImageNode) -> Self {
        Self::Image(node)
    }
}

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
