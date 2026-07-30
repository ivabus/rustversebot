//! Validated, backend-neutral paint descriptions.

use super::primitives::Point;
use std::error::Error;
use std::fmt;

/// Straight (not premultiplied) RGBA with normalized components.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    red: f32,
    green: f32,
    blue: f32,
    alpha: f32,
}

impl Color {
    pub fn new(red: f32, green: f32, blue: f32, alpha: f32) -> Result<Self, PaintError> {
        validate_color_component("color.red", red)?;
        validate_color_component("color.green", green)?;
        validate_color_component("color.blue", blue)?;
        validate_color_component("color.alpha", alpha)?;
        Ok(Self {
            red,
            green,
            blue,
            alpha,
        })
    }

    pub fn red(self) -> f32 {
        self.red
    }

    pub fn green(self) -> f32 {
        self.green
    }

    pub fn blue(self) -> f32 {
        self.blue
    }

    pub fn alpha(self) -> f32 {
        self.alpha
    }

    pub fn components(self) -> [f32; 4] {
        [self.red, self.green, self.blue, self.alpha]
    }
}

/// The coordinate system used to evaluate a gradient or pattern.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PaintSpace {
    /// Coordinates are normalized to the painted object's bounds.
    ///
    /// `(0, 0)` is the top-left and `(1, 1)` is the bottom-right. A radial
    /// gradient is evaluated before this mapping, so a circle becomes an
    /// ellipse when the object bounds are not square, matching SVG behavior.
    ObjectBoundingBox,
    /// Coordinates are logical scene coordinates and do not change with the
    /// bounds of the painted object.
    #[default]
    UserSpace,
}

/// One color sample in a gradient.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GradientStop {
    offset: f32,
    color: Color,
}

impl GradientStop {
    pub fn new(offset: f32, color: Color) -> Result<Self, PaintError> {
        require_finite("gradient_stop.offset", offset)?;
        if !(0.0..=1.0).contains(&offset) {
            return Err(PaintError::GradientOffsetOutOfRange { offset });
        }
        Ok(Self { offset, color })
    }

    pub fn offset(self) -> f32 {
        self.offset
    }

    pub fn color(self) -> Color {
        self.color
    }
}

/// A linear gradient evaluated along the non-degenerate line from `start` to
/// `end`.
///
/// Duplicate stop offsets are retained in authored order. They describe an SVG
/// hard edge: the last stop at an offset supplies the color at and immediately
/// after that offset.
#[derive(Clone, Debug, PartialEq)]
pub struct LinearGradient {
    start: Point,
    end: Point,
    stops: Vec<GradientStop>,
    space: PaintSpace,
}

impl LinearGradient {
    pub fn new(
        start: Point,
        end: Point,
        stops: Vec<GradientStop>,
        space: PaintSpace,
    ) -> Result<Self, PaintError> {
        if start == end {
            return Err(PaintError::DegenerateLinearGradient);
        }
        validate_stops(&stops)?;
        Ok(Self {
            start,
            end,
            stops,
            space,
        })
    }

    pub fn start(&self) -> Point {
        self.start
    }

    pub fn end(&self) -> Point {
        self.end
    }

    pub fn stops(&self) -> &[GradientStop] {
        &self.stops
    }

    pub fn space(&self) -> PaintSpace {
        self.space
    }
}

/// A circular radial gradient.
#[derive(Clone, Debug, PartialEq)]
pub struct RadialGradient {
    center: Point,
    radius: f32,
    stops: Vec<GradientStop>,
    space: PaintSpace,
}

impl RadialGradient {
    pub fn new(
        center: Point,
        radius: f32,
        stops: Vec<GradientStop>,
        space: PaintSpace,
    ) -> Result<Self, PaintError> {
        require_positive("radial_gradient.radius", radius)?;
        validate_stops(&stops)?;
        Ok(Self {
            center,
            radius,
            stops,
            space,
        })
    }

    pub fn center(&self) -> Point {
        self.center
    }

    pub fn radius(&self) -> f32 {
        self.radius
    }

    pub fn stops(&self) -> &[GradientStop] {
        &self.stops
    }

    pub fn space(&self) -> PaintSpace {
        self.space
    }
}

/// A backend-neutral 2D affine transform.
///
/// It maps `(x, y)` to `(m11*x + m21*y + tx, m12*x + m22*y + ty)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AffineTransform {
    coefficients: [f32; 6],
}

impl AffineTransform {
    pub const IDENTITY: Self = Self {
        coefficients: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
    };

    pub fn new(
        m11: f32,
        m12: f32,
        m21: f32,
        m22: f32,
        tx: f32,
        ty: f32,
    ) -> Result<Self, PaintError> {
        let coefficients = [m11, m12, m21, m22, tx, ty];
        if !coefficients.iter().all(|value| value.is_finite()) {
            return Err(PaintError::NonFinite {
                field: "pattern.transform",
            });
        }

        // Pattern sampling maps paint-space positions back into pattern-local
        // coordinates, so a singular transform has no backend-neutral meaning.
        let determinant = m11 * m22 - m12 * m21;
        if !determinant.is_finite() || determinant == 0.0 {
            return Err(PaintError::NonInvertibleTransform);
        }

        Ok(Self { coefficients })
    }

    pub fn coefficients(self) -> [f32; 6] {
        self.coefficients
    }
}

impl Default for AffineTransform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// A procedural repeated dot tile.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DotPattern {
    tile_width: f32,
    tile_height: f32,
    radius: f32,
    dot_color: Color,
    background: Option<Color>,
}

impl DotPattern {
    pub fn new(
        tile_width: f32,
        tile_height: f32,
        radius: f32,
        dot_color: Color,
        background: Option<Color>,
    ) -> Result<Self, PaintError> {
        require_positive("dot_pattern.tile_width", tile_width)?;
        require_positive("dot_pattern.tile_height", tile_height)?;
        require_positive("dot_pattern.radius", radius)?;
        let maximum = tile_width.min(tile_height) / 2.0;
        if radius > maximum {
            return Err(PaintError::PatternRadiusExceedsTile { radius, maximum });
        }
        Ok(Self {
            tile_width,
            tile_height,
            radius,
            dot_color,
            background,
        })
    }

    pub fn tile_size(self) -> [f32; 2] {
        [self.tile_width, self.tile_height]
    }

    pub fn radius(self) -> f32 {
        self.radius
    }

    pub fn dot_color(self) -> Color {
        self.dot_color
    }

    pub fn background(self) -> Option<Color> {
        self.background
    }
}

/// A procedural diagonal-line tile.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiagonalPattern {
    tile_size: f32,
    line_width: f32,
    line_color: Color,
    background: Option<Color>,
}

impl DiagonalPattern {
    pub fn new(
        tile_size: f32,
        line_width: f32,
        line_color: Color,
        background: Option<Color>,
    ) -> Result<Self, PaintError> {
        require_positive("diagonal_pattern.tile_size", tile_size)?;
        require_positive("diagonal_pattern.line_width", line_width)?;
        if line_width > tile_size {
            return Err(PaintError::PatternLineExceedsTile {
                line_width,
                tile_size,
            });
        }
        Ok(Self {
            tile_size,
            line_width,
            line_color,
            background,
        })
    }

    pub fn tile_size(self) -> f32 {
        self.tile_size
    }

    pub fn line_width(self) -> f32 {
        self.line_width
    }

    pub fn line_color(self) -> Color {
        self.line_color
    }

    pub fn background(self) -> Option<Color> {
        self.background
    }
}

/// An opaque handle for a decoded texture owned by the renderer resource layer.
///
/// It deliberately carries no path, encoded bytes, or backend object.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TexturePatternHandle(u64);

impl TexturePatternHandle {
    pub fn new(id: u64) -> Result<Self, PaintError> {
        if id == 0 {
            Err(PaintError::InvalidTextureHandle)
        } else {
            Ok(Self(id))
        }
    }

    pub fn id(self) -> u64 {
        self.0
    }
}

/// A repeated texture tile, evaluated through the containing pattern transform.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RepeatedTexturePattern {
    texture: TexturePatternHandle,
    tile_width: f32,
    tile_height: f32,
}

impl RepeatedTexturePattern {
    pub fn new(
        texture: TexturePatternHandle,
        tile_width: f32,
        tile_height: f32,
    ) -> Result<Self, PaintError> {
        require_positive("texture_pattern.tile_width", tile_width)?;
        require_positive("texture_pattern.tile_height", tile_height)?;
        Ok(Self {
            texture,
            tile_width,
            tile_height,
        })
    }

    pub fn texture(self) -> TexturePatternHandle {
        self.texture
    }

    pub fn tile_size(self) -> [f32; 2] {
        [self.tile_width, self.tile_height]
    }
}

/// The tile source used by a pattern paint.
#[derive(Clone, Debug, PartialEq)]
pub enum PatternDescriptor {
    Dots(DotPattern),
    Diagonal(DiagonalPattern),
    RepeatedTexture(RepeatedTexturePattern),
}

/// A procedural or texture-backed repeated pattern.
#[derive(Clone, Debug, PartialEq)]
pub struct PatternPaint {
    descriptor: PatternDescriptor,
    transform: AffineTransform,
    space: PaintSpace,
}

impl PatternPaint {
    /// Creates a pattern whose transform maps pattern-local coordinates into
    /// the selected paint space. Renderers invert it when sampling the pattern.
    pub fn new(
        descriptor: PatternDescriptor,
        transform: AffineTransform,
        space: PaintSpace,
    ) -> Self {
        Self {
            descriptor,
            transform,
            space,
        }
    }

    pub fn descriptor(&self) -> &PatternDescriptor {
        &self.descriptor
    }

    pub fn transform(&self) -> AffineTransform {
        self.transform
    }

    pub fn space(&self) -> PaintSpace {
        self.space
    }
}

/// A shape paint independent of any concrete rendering backend.
#[derive(Clone, Debug, PartialEq)]
pub enum Paint {
    Solid(Color),
    LinearGradient(LinearGradient),
    RadialGradient(RadialGradient),
    Pattern(PatternPaint),
}

/// Why a paint description could not be constructed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PaintError {
    NonFinite { field: &'static str },
    ColorComponentOutOfRange { field: &'static str, value: f32 },
    TooFewGradientStops { actual: usize },
    GradientOffsetOutOfRange { offset: f32 },
    GradientStopsDecreasing { index: usize },
    DegenerateLinearGradient,
    NotPositive { field: &'static str },
    PatternRadiusExceedsTile { radius: f32, maximum: f32 },
    PatternLineExceedsTile { line_width: f32, tile_size: f32 },
    NonInvertibleTransform,
    InvalidTextureHandle,
}

impl fmt::Display for PaintError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFinite { field } => write!(formatter, "{field} must be finite"),
            Self::ColorComponentOutOfRange { field, value } => {
                write!(formatter, "{field} must be in [0, 1], got {value}")
            }
            Self::TooFewGradientStops { actual } => {
                write!(
                    formatter,
                    "gradient requires at least two stops, got {actual}"
                )
            }
            Self::GradientOffsetOutOfRange { offset } => {
                write!(
                    formatter,
                    "gradient stop offset must be in [0, 1], got {offset}"
                )
            }
            Self::GradientStopsDecreasing { index } => {
                write!(formatter, "gradient stop at index {index} is out of order")
            }
            Self::DegenerateLinearGradient => {
                write!(formatter, "linear gradient start and end must differ")
            }
            Self::NotPositive { field } => write!(formatter, "{field} must be positive"),
            Self::PatternRadiusExceedsTile { radius, maximum } => {
                write!(
                    formatter,
                    "pattern radius {radius} exceeds maximum {maximum}"
                )
            }
            Self::PatternLineExceedsTile {
                line_width,
                tile_size,
            } => write!(
                formatter,
                "pattern line width {line_width} exceeds tile size {tile_size}"
            ),
            Self::NonInvertibleTransform => {
                write!(formatter, "pattern transform must be invertible")
            }
            Self::InvalidTextureHandle => {
                write!(formatter, "texture pattern handle must be nonzero")
            }
        }
    }
}

impl Error for PaintError {}

fn validate_color_component(field: &'static str, value: f32) -> Result<(), PaintError> {
    require_finite(field, value)?;
    if (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(PaintError::ColorComponentOutOfRange { field, value })
    }
}

fn validate_stops(stops: &[GradientStop]) -> Result<(), PaintError> {
    if stops.len() < 2 {
        return Err(PaintError::TooFewGradientStops {
            actual: stops.len(),
        });
    }
    for (index, pair) in stops.windows(2).enumerate() {
        if pair[0].offset > pair[1].offset {
            return Err(PaintError::GradientStopsDecreasing { index: index + 1 });
        }
    }
    Ok(())
}

fn require_finite(field: &'static str, value: f32) -> Result<(), PaintError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(PaintError::NonFinite { field })
    }
}

fn require_positive(field: &'static str, value: f32) -> Result<(), PaintError> {
    require_finite(field, value)?;
    if value > 0.0 {
        Ok(())
    } else {
        Err(PaintError::NotPositive { field })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(x: f32, y: f32) -> Point {
        Point::new(x, y).unwrap()
    }

    fn color(red: f32, green: f32, blue: f32, alpha: f32) -> Color {
        Color::new(red, green, blue, alpha).unwrap()
    }

    fn stop(offset: f32) -> GradientStop {
        GradientStop::new(offset, color(offset, 0.5, 1.0 - offset, 1.0)).unwrap()
    }

    #[test]
    fn color_is_straight_normalized_rgba() {
        let value = color(0.25, 0.5, 0.75, 0.4);
        assert_eq!(value.components(), [0.25, 0.5, 0.75, 0.4]);
        assert!(Color::new(f32::NAN, 0.0, 0.0, 1.0).is_err());
        assert!(Color::new(1.01, 0.0, 0.0, 1.0).is_err());
        assert!(Color::new(0.0, 0.0, 0.0, -0.01).is_err());
    }

    #[test]
    fn gradients_accept_arbitrary_sorted_stops_and_duplicates() {
        let stops = vec![
            stop(0.0),
            stop(0.25),
            stop(0.25),
            stop(0.4),
            stop(0.5),
            stop(0.6),
            stop(0.75),
            stop(1.0),
        ];
        let gradient = LinearGradient::new(
            point(0.0, 0.0),
            point(1.0, 0.0),
            stops,
            PaintSpace::UserSpace,
        )
        .unwrap();
        assert_eq!(gradient.stops().len(), 8);
        assert_eq!(gradient.stops()[1].offset(), gradient.stops()[2].offset());
    }

    #[test]
    fn gradients_reject_too_few_out_of_range_and_decreasing_stops() {
        assert!(matches!(
            LinearGradient::new(
                point(0.0, 0.0),
                point(1.0, 0.0),
                Vec::new(),
                PaintSpace::UserSpace
            ),
            Err(PaintError::TooFewGradientStops { actual: 0 })
        ));
        assert!(GradientStop::new(f32::NAN, color(0.0, 0.0, 0.0, 1.0)).is_err());
        assert!(GradientStop::new(1.1, color(0.0, 0.0, 0.0, 1.0)).is_err());
        assert!(matches!(
            LinearGradient::new(
                point(0.0, 0.0),
                point(1.0, 0.0),
                vec![stop(0.5), stop(0.4)],
                PaintSpace::UserSpace
            ),
            Err(PaintError::GradientStopsDecreasing { index: 1 })
        ));
        assert!(matches!(
            LinearGradient::new(
                point(0.5, 0.5),
                point(0.5, 0.5),
                vec![stop(0.0), stop(1.0)],
                PaintSpace::ObjectBoundingBox
            ),
            Err(PaintError::DegenerateLinearGradient)
        ));
    }

    #[test]
    fn radial_gradient_rejects_invalid_radius() {
        let stops = vec![stop(0.0), stop(1.0)];
        assert!(
            RadialGradient::new(point(0.0, 0.0), 0.0, stops.clone(), PaintSpace::UserSpace)
                .is_err()
        );
        assert!(
            RadialGradient::new(point(0.0, 0.0), f32::NAN, stops, PaintSpace::UserSpace).is_err()
        );
    }

    #[test]
    fn pattern_descriptors_validate_geometry_and_transform() {
        let foreground = color(1.0, 1.0, 1.0, 1.0);
        assert!(DotPattern::new(5.0, 5.0, 1.0, foreground, None).is_ok());
        assert!(DotPattern::new(5.0, 5.0, 3.0, foreground, None).is_err());
        assert!(DiagonalPattern::new(4.0, 1.0, foreground, None).is_ok());
        assert!(DiagonalPattern::new(4.0, 5.0, foreground, None).is_err());
        assert!(
            RepeatedTexturePattern::new(TexturePatternHandle::new(7).unwrap(), 32.0, 32.0).is_ok()
        );
        assert!(matches!(
            TexturePatternHandle::new(0),
            Err(PaintError::InvalidTextureHandle)
        ));
        assert!(AffineTransform::new(1.0, 0.0, 0.0, 1.0, f32::NAN, 0.0).is_err());
        assert!(matches!(
            AffineTransform::new(1.0, 2.0, 2.0, 4.0, 0.0, 0.0),
            Err(PaintError::NonInvertibleTransform)
        ));
    }

    #[test]
    fn repeated_texture_pattern_contains_only_an_opaque_handle() {
        let texture =
            RepeatedTexturePattern::new(TexturePatternHandle::new(42).unwrap(), 128.0, 64.0)
                .unwrap();
        let pattern = PatternPaint::new(
            PatternDescriptor::RepeatedTexture(texture),
            AffineTransform::new(0.707, -0.707, 0.707, 0.707, 12.0, 4.0).unwrap(),
            PaintSpace::UserSpace,
        );
        assert_eq!(pattern.transform().coefficients()[4..], [12.0, 4.0]);
    }
}
