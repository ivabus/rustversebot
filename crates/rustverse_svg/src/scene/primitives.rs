//! Validated, backend-neutral geometry and shape styling.

use super::paint::Paint;
use std::error::Error;
use std::fmt;

/// A point in logical scene coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    x: f32,
    y: f32,
}

impl Point {
    pub fn new(x: f32, y: f32) -> Result<Self, GeometryError> {
        require_finite("point.x", x)?;
        require_finite("point.y", y)?;
        Ok(Self { x, y })
    }

    pub fn x(self) -> f32 {
        self.x
    }

    pub fn y(self) -> f32 {
        self.y
    }
}

/// An axis-aligned rectangle in logical scene coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Result<Self, GeometryError> {
        require_finite("rect.x", x)?;
        require_finite("rect.y", y)?;
        require_positive("rect.width", width)?;
        require_positive("rect.height", height)?;
        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }

    pub fn x(self) -> f32 {
        self.x
    }

    pub fn y(self) -> f32 {
        self.y
    }

    pub fn width(self) -> f32 {
        self.width
    }

    pub fn height(self) -> f32 {
        self.height
    }
}

/// A circle in logical scene coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Circle {
    center: Point,
    radius: f32,
}

impl Circle {
    pub fn new(center: Point, radius: f32) -> Result<Self, GeometryError> {
        require_positive("circle.radius", radius)?;
        Ok(Self { center, radius })
    }

    pub fn center(self) -> Point {
        self.center
    }

    pub fn radius(self) -> f32 {
        self.radius
    }
}

/// Independent circular radii for the four corners of a rounded rectangle.
///
/// A zero radius is intentional and represents a square corner.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CornerRadii {
    top_left: f32,
    top_right: f32,
    bottom_right: f32,
    bottom_left: f32,
}

impl CornerRadii {
    pub fn new(
        top_left: f32,
        top_right: f32,
        bottom_right: f32,
        bottom_left: f32,
    ) -> Result<Self, GeometryError> {
        require_non_negative("corner_radii.top_left", top_left)?;
        require_non_negative("corner_radii.top_right", top_right)?;
        require_non_negative("corner_radii.bottom_right", bottom_right)?;
        require_non_negative("corner_radii.bottom_left", bottom_left)?;
        Ok(Self {
            top_left,
            top_right,
            bottom_right,
            bottom_left,
        })
    }

    pub fn uniform(radius: f32) -> Result<Self, GeometryError> {
        Self::new(radius, radius, radius, radius)
    }

    pub fn top_left(self) -> f32 {
        self.top_left
    }

    pub fn top_right(self) -> f32 {
        self.top_right
    }

    pub fn bottom_right(self) -> f32 {
        self.bottom_right
    }

    pub fn bottom_left(self) -> f32 {
        self.bottom_left
    }

    fn iter(self) -> impl Iterator<Item = f32> {
        [
            self.top_left,
            self.top_right,
            self.bottom_right,
            self.bottom_left,
        ]
        .into_iter()
    }
}

/// An axis-aligned rectangle with circular corner radii.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RoundedRect {
    rect: Rect,
    radii: CornerRadii,
}

/// Geometry supported by the backend-neutral primitive scene.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shape {
    Rect(Rect),
    RoundedRect(RoundedRect),
    Circle(Circle),
}

impl Shape {
    /// Returns the axis-aligned logical bounds used by object-bounding-box
    /// paints.
    pub fn bounds(self) -> Rect {
        match self {
            Self::Rect(rect) => rect,
            Self::RoundedRect(rounded) => rounded.rect(),
            Self::Circle(circle) => {
                let diameter = circle.radius() * 2.0;
                // The source circle is already validated, so these values are
                // finite and strictly positive.
                Rect {
                    x: circle.center().x() - circle.radius(),
                    y: circle.center().y() - circle.radius(),
                    width: diameter,
                    height: diameter,
                }
            }
        }
    }
}

impl RoundedRect {
    pub fn new(rect: Rect, radii: CornerRadii) -> Result<Self, GeometryError> {
        let maximum = rect.width.min(rect.height) / 2.0;
        if radii.iter().any(|radius| radius > maximum) {
            return Err(GeometryError::CornerRadiusExceedsBounds { maximum });
        }
        Ok(Self { rect, radii })
    }

    pub fn rect(self) -> Rect {
        self.rect
    }

    pub fn radii(self) -> CornerRadii {
        self.radii
    }
}

/// A shape fill. `Paint` values are validated when constructed.
#[derive(Clone, Debug, PartialEq)]
pub struct Fill {
    paint: Paint,
}

impl Fill {
    pub fn new(paint: Paint) -> Self {
        Self { paint }
    }

    pub fn paint(&self) -> &Paint {
        &self.paint
    }

    pub fn into_paint(self) -> Paint {
        self.paint
    }
}

/// A centered shape stroke with a positive width in logical pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct Stroke {
    paint: Paint,
    width: f32,
}

impl Stroke {
    pub fn new(paint: Paint, width: f32) -> Result<Self, GeometryError> {
        require_positive("stroke.width", width)?;
        Ok(Self { paint, width })
    }

    pub fn paint(&self) -> &Paint {
        &self.paint
    }

    pub fn width(&self) -> f32 {
        self.width
    }
}

/// One primitive in painter's order.
///
/// The fill is drawn first. When present, the stroke is centered on the shape
/// boundary and drawn over the fill.
#[derive(Clone, Debug, PartialEq)]
pub struct ShapeNode {
    shape: Shape,
    fill: Fill,
    stroke: Option<Stroke>,
}

impl ShapeNode {
    pub fn new(shape: Shape, fill: Fill) -> Self {
        Self {
            shape,
            fill,
            stroke: None,
        }
    }

    pub fn with_stroke(shape: Shape, fill: Fill, stroke: Stroke) -> Self {
        Self {
            shape,
            fill,
            stroke: Some(stroke),
        }
    }

    pub fn shape(&self) -> Shape {
        self.shape
    }

    pub fn fill(&self) -> &Fill {
        &self.fill
    }

    pub fn stroke(&self) -> Option<&Stroke> {
        self.stroke.as_ref()
    }
}

/// Why a geometry or stroke value could not be constructed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GeometryError {
    NonFinite { field: &'static str },
    Negative { field: &'static str },
    NotPositive { field: &'static str },
    CornerRadiusExceedsBounds { maximum: f32 },
}

impl fmt::Display for GeometryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFinite { field } => write!(formatter, "{field} must be finite"),
            Self::Negative { field } => write!(formatter, "{field} must not be negative"),
            Self::NotPositive { field } => write!(formatter, "{field} must be positive"),
            Self::CornerRadiusExceedsBounds { maximum } => {
                write!(formatter, "corner radius must not exceed {maximum}")
            }
        }
    }
}

impl Error for GeometryError {}

fn require_finite(field: &'static str, value: f32) -> Result<(), GeometryError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(GeometryError::NonFinite { field })
    }
}

fn require_non_negative(field: &'static str, value: f32) -> Result<(), GeometryError> {
    require_finite(field, value)?;
    if value >= 0.0 {
        Ok(())
    } else {
        Err(GeometryError::Negative { field })
    }
}

fn require_positive(field: &'static str, value: f32) -> Result<(), GeometryError> {
    require_finite(field, value)?;
    if value > 0.0 {
        Ok(())
    } else {
        Err(GeometryError::NotPositive { field })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::paint::Color;

    #[test]
    fn geometry_rejects_non_finite_and_empty_values() {
        assert!(matches!(
            Rect::new(f32::NAN, 0.0, 1.0, 1.0),
            Err(GeometryError::NonFinite { .. })
        ));
        assert!(matches!(
            Rect::new(0.0, 0.0, -1.0, 1.0),
            Err(GeometryError::NotPositive { .. })
        ));
        assert!(matches!(
            Rect::new(0.0, 0.0, 0.0, 1.0),
            Err(GeometryError::NotPositive { .. })
        ));
        assert!(matches!(
            Circle::new(Point::new(0.0, 0.0).unwrap(), 0.0),
            Err(GeometryError::NotPositive { .. })
        ));
    }

    #[test]
    fn rounded_rect_rejects_negative_and_oversized_radii() {
        assert!(CornerRadii::uniform(-0.5).is_err());

        let rect = Rect::new(0.0, 0.0, 20.0, 10.0).unwrap();
        let radii = CornerRadii::uniform(5.01).unwrap();
        assert!(matches!(
            RoundedRect::new(rect, radii),
            Err(GeometryError::CornerRadiusExceedsBounds { .. })
        ));
        assert!(RoundedRect::new(rect, CornerRadii::uniform(5.0).unwrap()).is_ok());
    }

    #[test]
    fn stroke_requires_positive_finite_width() {
        let paint = Paint::Solid(Color::new(1.0, 0.5, 0.0, 1.0).unwrap());
        assert!(Stroke::new(paint.clone(), -0.1).is_err());
        assert!(Stroke::new(paint.clone(), f32::INFINITY).is_err());
        assert!(Stroke::new(paint, 0.0).is_err());
    }
}
