//! Backend-neutral image handles, fitting, and sampling geometry.
//!
//! Scene images refer to already-resident resources by handle. Paths, URLs,
//! and encoded image bytes deliberately do not cross this boundary.

use super::Rect;
use std::error::Error;
use std::fmt;
use std::num::NonZeroU32;

/// An opaque reference to an image owned by the renderer's persistent atlas.
///
/// The generation prevents a handle from silently referring to a different
/// image if an atlas slot is ever recycled.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ImageHandle {
    slot: u32,
    generation: NonZeroU32,
}

impl ImageHandle {
    /// Creates a handle for an atlas-owned slot.
    ///
    /// This stays crate-private so public scene construction cannot invent
    /// handles that were not issued by the renderer.
    pub(crate) fn new(slot: u32, generation: NonZeroU32) -> Self {
        Self { slot, generation }
    }

    pub(crate) fn slot(self) -> u32 {
        self.slot
    }

    pub(crate) fn generation(self) -> NonZeroU32 {
        self.generation
    }
}

/// The decoded dimensions of an image before fitting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageDimensions {
    width: u32,
    height: u32,
}

impl ImageDimensions {
    pub fn new(width: u32, height: u32) -> Result<Self, ImageGeometryError> {
        if width == 0 {
            return Err(ImageGeometryError::ZeroDimension {
                field: "image.width",
            });
        }
        if height == 0 {
            return Err(ImageGeometryError::ZeroDimension {
                field: "image.height",
            });
        }
        Ok(Self { width, height })
    }

    pub fn width(self) -> u32 {
        self.width
    }

    pub fn height(self) -> u32 {
        self.height
    }
}

/// A normalized source-image rectangle.
///
/// Coordinates use a top-left origin and are constrained to `[0, 1]`. The
/// atlas resolves this source-relative rectangle to page UVs at draw time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageUv {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl ImageUv {
    pub const FULL: Self = Self {
        x: 0.0,
        y: 0.0,
        width: 1.0,
        height: 1.0,
    };

    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Result<Self, ImageGeometryError> {
        for (field, value) in [
            ("image_uv.x", x),
            ("image_uv.y", y),
            ("image_uv.width", width),
            ("image_uv.height", height),
        ] {
            if !value.is_finite() {
                return Err(ImageGeometryError::NonFinite { field });
            }
        }
        if width <= 0.0 {
            return Err(ImageGeometryError::NotPositive {
                field: "image_uv.width",
            });
        }
        if height <= 0.0 {
            return Err(ImageGeometryError::NotPositive {
                field: "image_uv.height",
            });
        }
        if x < 0.0 || y < 0.0 || x > 1.0 || y > 1.0 || width > 1.0 - x || height > 1.0 - y {
            return Err(ImageGeometryError::UvOutOfBounds);
        }
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

/// How an image is mapped into its logical destination rectangle.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum ImageFit {
    /// Preserve aspect ratio and center the entire image inside the destination.
    Contain,
    /// Preserve aspect ratio, fill the destination, and crop equally on
    /// opposing sides.
    #[default]
    Cover,
    /// Stretch the complete image to the destination.
    Fill,
    /// Stretch an explicit normalized source region to the destination.
    ExplicitUv(ImageUv),
}

/// Ordinary scene images are sampled linearly and clamped at their edges.
///
/// Repeated sampling belongs to the separate pattern paint contract.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ImageAddressMode {
    #[default]
    ClampToEdge,
}

/// Validated geometry consumed by an image draw command.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImagePlacement {
    destination: Rect,
    uv: ImageUv,
    address_mode: ImageAddressMode,
}

impl ImagePlacement {
    pub fn destination(self) -> Rect {
        self.destination
    }

    pub fn uv(self) -> ImageUv {
        self.uv
    }

    pub fn address_mode(self) -> ImageAddressMode {
        self.address_mode
    }
}

/// Resolves image fitting into destination geometry and source-relative UVs.
///
/// `Cover` cropping is centered exactly: any excess is split equally between
/// the two opposing source edges. Fractional logical destinations remain
/// fractional and are not rounded at this layer.
pub fn place_image(
    destination: Rect,
    source: ImageDimensions,
    fit: ImageFit,
) -> Result<ImagePlacement, ImageGeometryError> {
    let source_width = f64::from(source.width);
    let source_height = f64::from(source.height);

    let (destination, uv) = match fit {
        ImageFit::Contain => {
            let destination_width = f64::from(destination.width());
            let destination_height = f64::from(destination.height());
            let scale = (destination_width / source_width).min(destination_height / source_height);
            let width = source_width * scale;
            let height = source_height * scale;
            let x = f64::from(destination.x()) + (destination_width - width) * 0.5;
            let y = f64::from(destination.y()) + (destination_height - height) * 0.5;
            (
                Rect::new(
                    finite_f32(x)?,
                    finite_f32(y)?,
                    positive_f32(width)?,
                    positive_f32(height)?,
                )
                .map_err(|_| ImageGeometryError::UnrepresentablePlacement)?,
                ImageUv::FULL,
            )
        }
        ImageFit::Cover => {
            let destination_aspect =
                f64::from(destination.width()) / f64::from(destination.height());
            let source_aspect = source_width / source_height;
            let uv = if source_aspect > destination_aspect {
                let width = destination_aspect / source_aspect;
                ImageUv::new(
                    finite_f32((1.0 - width) * 0.5)?,
                    0.0,
                    positive_f32(width)?,
                    1.0,
                )
            } else {
                let height = source_aspect / destination_aspect;
                ImageUv::new(
                    0.0,
                    finite_f32((1.0 - height) * 0.5)?,
                    1.0,
                    positive_f32(height)?,
                )
            }
            .map_err(|_| ImageGeometryError::UnrepresentablePlacement)?;
            (destination, uv)
        }
        ImageFit::Fill => (destination, ImageUv::FULL),
        ImageFit::ExplicitUv(uv) => (destination, uv),
    };

    Ok(ImagePlacement {
        destination,
        uv,
        address_mode: ImageAddressMode::ClampToEdge,
    })
}

fn finite_f32(value: f64) -> Result<f32, ImageGeometryError> {
    let value = value as f32;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(ImageGeometryError::UnrepresentablePlacement)
    }
}

fn positive_f32(value: f64) -> Result<f32, ImageGeometryError> {
    let value = finite_f32(value)?;
    if value > 0.0 {
        Ok(value)
    } else {
        Err(ImageGeometryError::UnrepresentablePlacement)
    }
}

/// One persistent-atlas image in painter's order.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageNode {
    handle: ImageHandle,
    source: ImageDimensions,
    destination: Rect,
    fit: ImageFit,
}

impl ImageNode {
    pub fn new(
        handle: ImageHandle,
        source: ImageDimensions,
        destination: Rect,
        fit: ImageFit,
    ) -> Self {
        Self {
            handle,
            source,
            destination,
            fit,
        }
    }

    pub fn handle(&self) -> ImageHandle {
        self.handle
    }

    pub fn source(&self) -> ImageDimensions {
        self.source
    }

    pub fn destination(&self) -> Rect {
        self.destination
    }

    pub fn fit(&self) -> ImageFit {
        self.fit
    }

    pub fn placement(&self) -> Result<ImagePlacement, ImageGeometryError> {
        place_image(self.destination, self.source, self.fit)
    }
}

/// Why image source geometry or UVs could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageGeometryError {
    ZeroDimension { field: &'static str },
    NonFinite { field: &'static str },
    NotPositive { field: &'static str },
    UvOutOfBounds,
    UnrepresentablePlacement,
}

impl fmt::Display for ImageGeometryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroDimension { field } => write!(formatter, "{field} must not be zero"),
            Self::NonFinite { field } => write!(formatter, "{field} must be finite"),
            Self::NotPositive { field } => write!(formatter, "{field} must be positive"),
            Self::UvOutOfBounds => write!(formatter, "image UV rectangle must fit within [0, 1]"),
            Self::UnrepresentablePlacement => {
                formatter.write_str("fitted image geometry cannot be represented")
            }
        }
    }
}

impl Error for ImageGeometryError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Rect {
        Rect::new(x, y, width, height).unwrap()
    }

    fn dimensions(width: u32, height: u32) -> ImageDimensions {
        ImageDimensions::new(width, height).unwrap()
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() <= 1.0e-6,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn centered_cover_crops_landscape_portrait_and_preserves_square() {
        let destination = rect(10.0, 20.0, 100.0, 100.0);

        let landscape = place_image(destination, dimensions(200, 100), ImageFit::Cover).unwrap();
        assert_eq!(landscape.destination(), destination);
        assert_eq!(landscape.uv(), ImageUv::new(0.25, 0.0, 0.5, 1.0).unwrap());

        let portrait = place_image(destination, dimensions(100, 200), ImageFit::Cover).unwrap();
        assert_eq!(portrait.uv(), ImageUv::new(0.0, 0.25, 1.0, 0.5).unwrap());

        let square = place_image(destination, dimensions(64, 64), ImageFit::Cover).unwrap();
        assert_eq!(square.uv(), ImageUv::FULL);
    }

    #[test]
    fn contain_centers_fractional_geometry_without_rounding() {
        let placement = place_image(
            rect(0.25, 1.5, 101.5, 80.25),
            dimensions(200, 100),
            ImageFit::Contain,
        )
        .unwrap();
        let fitted = placement.destination();

        assert_close(fitted.x(), 0.25);
        assert_close(fitted.width(), 101.5);
        assert_close(fitted.height(), 50.75);
        assert_close(fitted.y(), 16.25);
        assert_eq!(placement.uv(), ImageUv::FULL);
    }

    #[test]
    fn fill_and_explicit_uv_use_the_complete_destination() {
        let destination = rect(2.5, 3.75, 90.25, 40.5);
        let fill = place_image(destination, dimensions(16, 9), ImageFit::Fill).unwrap();
        assert_eq!(fill.destination(), destination);
        assert_eq!(fill.uv(), ImageUv::FULL);

        let uv = ImageUv::new(0.125, 0.25, 0.5, 0.625).unwrap();
        let explicit =
            place_image(destination, dimensions(16, 9), ImageFit::ExplicitUv(uv)).unwrap();
        assert_eq!(explicit.destination(), destination);
        assert_eq!(explicit.uv(), uv);
        assert_eq!(explicit.address_mode(), ImageAddressMode::ClampToEdge);
    }

    #[test]
    fn source_dimensions_and_uvs_reject_invalid_values() {
        assert_eq!(
            ImageDimensions::new(0, 1),
            Err(ImageGeometryError::ZeroDimension {
                field: "image.width"
            })
        );
        assert_eq!(
            ImageDimensions::new(1, 0),
            Err(ImageGeometryError::ZeroDimension {
                field: "image.height"
            })
        );
        assert!(matches!(
            ImageUv::new(f32::NAN, 0.0, 1.0, 1.0),
            Err(ImageGeometryError::NonFinite { .. })
        ));
        assert!(matches!(
            ImageUv::new(0.0, 0.0, 0.0, 1.0),
            Err(ImageGeometryError::NotPositive { .. })
        ));
        assert_eq!(
            ImageUv::new(0.75, 0.0, 0.5, 1.0),
            Err(ImageGeometryError::UvOutOfBounds)
        );
        assert_eq!(
            ImageUv::new(-0.1, 0.0, 0.5, 1.0),
            Err(ImageGeometryError::UvOutOfBounds)
        );
    }

    #[test]
    fn extreme_finite_aspect_ratios_return_errors_without_panicking() {
        let too_wide = rect(0.0, 0.0, f32::MAX, f32::from_bits(1));
        assert_eq!(
            place_image(too_wide, dimensions(1, 1), ImageFit::Cover),
            Err(ImageGeometryError::UnrepresentablePlacement)
        );

        let too_short_to_contain = rect(0.0, 0.0, f32::MAX, f32::from_bits(1));
        assert_eq!(
            place_image(
                too_short_to_contain,
                dimensions(1, u32::MAX),
                ImageFit::Contain
            ),
            Err(ImageGeometryError::UnrepresentablePlacement)
        );

        let overflowing_center = rect(f32::MAX, 0.0, f32::MAX, 1.0);
        assert_eq!(
            place_image(
                overflowing_center,
                dimensions(1, u32::MAX),
                ImageFit::Contain
            ),
            Err(ImageGeometryError::UnrepresentablePlacement)
        );
    }

    #[test]
    fn image_nodes_are_stable_painter_order_data() {
        use crate::scene::SceneNode;

        let handle = ImageHandle::new(7, NonZeroU32::new(3).unwrap());
        let first = ImageNode::new(
            handle,
            dimensions(320, 180),
            rect(1.0, 2.0, 30.0, 40.0),
            ImageFit::Cover,
        );
        let second = first.clone();
        let later = ImageNode::new(
            ImageHandle::new(8, NonZeroU32::new(1).unwrap()),
            dimensions(320, 180),
            rect(1.0, 2.0, 30.0, 40.0),
            ImageFit::Cover,
        );

        assert_eq!(first, second);
        assert_ne!(first, later);
        assert_eq!(first.handle().slot(), 7);
        assert_eq!(first.handle().generation().get(), 3);
        assert_eq!(
            vec![
                SceneNode::from(first.clone()),
                SceneNode::from(later.clone())
            ],
            vec![SceneNode::Image(first), SceneNode::Image(later)]
        );
    }
}
