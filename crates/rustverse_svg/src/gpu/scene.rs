//! Conversion from the public backend-neutral scene into GPU primitive batches.

use std::ops::Deref;

use crate::scene::{
    AffineTransform, Color, ImageNode, Paint, PaintSpace, PatternDescriptor, Rect, Scene,
    SceneNode, Shape, ShapeNode,
};

use super::patterns::{
    DiagonalPattern, DotPattern, LogicalToPattern, PatternTextureHandle, RepeatedTexturePattern,
};
use super::primitives::{
    GradientStop, Primitive, PrimitiveColor, PrimitivePaint, PrimitiveRect, PrimitiveShape,
    PrimitiveStyle,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DrawItem {
    Primitives { start: u32, end: u32 },
    Image(u32),
}

pub(crate) struct PreparedScene {
    pub(crate) primitives: Vec<Primitive>,
    pub(crate) images: Vec<ImageNode>,
    pub(crate) order: Vec<DrawItem>,
}

impl Deref for PreparedScene {
    type Target = [Primitive];

    fn deref(&self) -> &Self::Target {
        &self.primitives
    }
}

pub(crate) enum DrawableRef<'a> {
    Shape(&'a ShapeNode),
    Image(&'a ImageNode),
}

pub(crate) trait DrawableNode {
    fn drawable(&self) -> DrawableRef<'_>;
}

impl DrawableNode for ShapeNode {
    fn drawable(&self) -> DrawableRef<'_> {
        DrawableRef::Shape(self)
    }
}

impl DrawableNode for SceneNode {
    fn drawable(&self) -> DrawableRef<'_> {
        match self {
            Self::Shape(node) => DrawableRef::Shape(node),
            Self::Image(node) => DrawableRef::Image(node),
        }
    }
}

pub(crate) fn prepare_scene<Node>(scene: &Scene<Node>) -> anyhow::Result<PreparedScene>
where
    Node: DrawableNode,
{
    let mut primitives = Vec::with_capacity(scene.nodes.len().saturating_mul(2));
    for node in &scene.nodes {
        let DrawableRef::Shape(node) = node.drawable() else {
            continue;
        };
        let bounds = node.shape().bounds();
        let (primitive_bounds, primitive_shape) = geometry(node.shape());
        primitives.push(Primitive {
            bounds: primitive_bounds,
            shape: primitive_shape,
            style: PrimitiveStyle::Fill,
            paint: paint(node.fill().paint(), bounds)?,
        });
        if let Some(stroke) = node.stroke() {
            primitives.push(Primitive {
                bounds: primitive_bounds,
                shape: primitive_shape,
                style: PrimitiveStyle::Stroke {
                    width: stroke.width(),
                },
                paint: paint(stroke.paint(), bounds)?,
            });
        }
    }

    let mut prepared = PreparedScene {
        primitives,
        images: Vec::new(),
        order: Vec::with_capacity(scene.nodes.len()),
    };
    let mut primitive_index = 0_u32;
    for node in &scene.nodes {
        match node.drawable() {
            DrawableRef::Shape(node) => {
                let count = 1 + u32::from(node.stroke().is_some());
                let end = primitive_index + count;
                match prepared.order.last_mut() {
                    Some(DrawItem::Primitives {
                        end: previous_end, ..
                    }) if *previous_end == primitive_index => *previous_end = end,
                    _ => prepared.order.push(DrawItem::Primitives {
                        start: primitive_index,
                        end,
                    }),
                }
                primitive_index = end;
            }
            DrawableRef::Image(node) => {
                let index = u32::try_from(prepared.images.len())
                    .map_err(|_| anyhow::anyhow!("image draw count exceeds u32::MAX"))?;
                prepared.images.push(node.clone());
                prepared.order.push(DrawItem::Image(index));
            }
        }
    }
    debug_assert_eq!(primitive_index as usize, prepared.primitives.len());
    Ok(prepared)
}

fn geometry(shape: Shape) -> (PrimitiveRect, PrimitiveShape) {
    let bounds = shape.bounds();
    let primitive_bounds = PrimitiveRect {
        x: bounds.x(),
        y: bounds.y(),
        width: bounds.width(),
        height: bounds.height(),
    };
    let primitive_shape = match shape {
        Shape::Rect(_) => PrimitiveShape::Rect,
        Shape::RoundedRect(rounded) => {
            let radii = rounded.radii();
            PrimitiveShape::RoundedRect {
                radii: [
                    radii.top_left(),
                    radii.top_right(),
                    radii.bottom_right(),
                    radii.bottom_left(),
                ],
            }
        }
        Shape::Circle(_) => PrimitiveShape::Circle,
    };
    (primitive_bounds, primitive_shape)
}

fn paint(value: &Paint, bounds: Rect) -> anyhow::Result<PrimitivePaint> {
    match value {
        Paint::Solid(color) => Ok(PrimitivePaint::Solid(gpu_color(*color))),
        Paint::LinearGradient(gradient) => {
            let (start, end) = match gradient.space() {
                PaintSpace::UserSpace => (
                    [gradient.start().x(), gradient.start().y()],
                    [gradient.end().x(), gradient.end().y()],
                ),
                PaintSpace::ObjectBoundingBox => (
                    object_point(bounds, gradient.start().x(), gradient.start().y()),
                    object_point(bounds, gradient.end().x(), gradient.end().y()),
                ),
            };
            Ok(PrimitivePaint::LinearGradient {
                start,
                end,
                stops: stops(gradient.stops()),
            })
        }
        Paint::RadialGradient(gradient) => {
            let (center, radii) = match gradient.space() {
                PaintSpace::UserSpace => (
                    [gradient.center().x(), gradient.center().y()],
                    [gradient.radius(), gradient.radius()],
                ),
                PaintSpace::ObjectBoundingBox => (
                    object_point(bounds, gradient.center().x(), gradient.center().y()),
                    [
                        gradient.radius() * bounds.width(),
                        gradient.radius() * bounds.height(),
                    ],
                ),
            };
            Ok(PrimitivePaint::RadialGradient {
                center,
                radii,
                stops: stops(gradient.stops()),
            })
        }
        Paint::Pattern(pattern) => match pattern.descriptor() {
            PatternDescriptor::Dots(descriptor) => {
                require_untransformed_user_pattern(pattern.space(), pattern.transform(), "dot")?;
                Ok(PrimitivePaint::Dots {
                    pattern: DotPattern {
                        tile_size: descriptor.tile_size(),
                        radius: descriptor.radius(),
                    },
                    foreground: gpu_color(descriptor.dot_color()),
                    background: gpu_color(descriptor.background().unwrap_or_else(transparent)),
                })
            }
            PatternDescriptor::Diagonal(descriptor) => {
                require_untransformed_user_pattern(
                    pattern.space(),
                    pattern.transform(),
                    "diagonal",
                )?;
                Ok(PrimitivePaint::Diagonal {
                    pattern: DiagonalPattern {
                        tile_size: descriptor.tile_size(),
                        line_width: descriptor.line_width(),
                    },
                    foreground: gpu_color(descriptor.line_color()),
                    background: gpu_color(descriptor.background().unwrap_or_else(transparent)),
                })
            }
            PatternDescriptor::RepeatedTexture(descriptor) => {
                let id = u32::try_from(descriptor.texture().id())
                    .map_err(|_| anyhow::anyhow!("texture pattern handle exceeds u32::MAX"))?;
                Ok(PrimitivePaint::RepeatedTexture {
                    pattern: RepeatedTexturePattern {
                        texture: PatternTextureHandle(id),
                        tile_size: descriptor.tile_size(),
                        logical_to_pattern: logical_to_pattern(
                            pattern.transform(),
                            pattern.space(),
                            bounds,
                        ),
                    },
                    tint: PrimitiveColor([1.0; 4]),
                })
            }
        },
    }
}

fn transparent() -> Color {
    Color::new(0.0, 0.0, 0.0, 0.0).expect("transparent color is valid")
}

fn gpu_color(color: Color) -> PrimitiveColor {
    PrimitiveColor(color.components())
}

fn stops(stops: &[crate::scene::GradientStop]) -> Vec<GradientStop> {
    stops
        .iter()
        .map(|stop| GradientStop {
            offset: stop.offset(),
            color: gpu_color(stop.color()),
        })
        .collect()
}

fn object_point(bounds: Rect, x: f32, y: f32) -> [f32; 2] {
    [
        bounds.x() + x * bounds.width(),
        bounds.y() + y * bounds.height(),
    ]
}

fn require_untransformed_user_pattern(
    space: PaintSpace,
    transform: AffineTransform,
    name: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        space == PaintSpace::UserSpace && transform == AffineTransform::IDENTITY,
        "transformed or object-bounding-box {name} patterns require the GPU primitive shader's procedural pattern transform support"
    );
    Ok(())
}

/// Converts the authored pattern-local-to-paint transform to the inverse
/// logical-to-pattern transform consumed by the GPU shader.
fn logical_to_pattern(
    transform: AffineTransform,
    space: PaintSpace,
    bounds: Rect,
) -> LogicalToPattern {
    let [m11, m12, m21, m22, tx, ty] = transform.coefficients();
    let determinant = m11 * m22 - m12 * m21;
    let inverse = [
        [
            m22 / determinant,
            -m21 / determinant,
            (m21 * ty - m22 * tx) / determinant,
        ],
        [
            -m12 / determinant,
            m11 / determinant,
            (m12 * tx - m11 * ty) / determinant,
        ],
    ];
    match space {
        PaintSpace::UserSpace => LogicalToPattern { rows: inverse },
        PaintSpace::ObjectBoundingBox => {
            let sx = 1.0 / bounds.width();
            let sy = 1.0 / bounds.height();
            let ox = -bounds.x() * sx;
            let oy = -bounds.y() * sy;
            LogicalToPattern {
                rows: [
                    [
                        inverse[0][0] * sx,
                        inverse[0][1] * sy,
                        inverse[0][0] * ox + inverse[0][1] * oy + inverse[0][2],
                    ],
                    [
                        inverse[1][0] * sx,
                        inverse[1][1] * sy,
                        inverse[1][0] * ox + inverse[1][1] * oy + inverse[1][2],
                    ],
                ],
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{Fill, GradientStop as SceneStop, LinearGradient, Paint, Point};

    fn color(value: f32) -> Color {
        Color::new(value, value, value, 1.0).unwrap()
    }

    fn stops() -> Vec<SceneStop> {
        vec![
            SceneStop::new(0.0, color(0.0)).unwrap(),
            SceneStop::new(1.0, color(1.0)).unwrap(),
        ]
    }

    #[test]
    fn object_bounding_box_linear_gradient_is_mapped_to_logical_bounds() {
        let bounds = Rect::new(10.0, 20.0, 40.0, 60.0).unwrap();
        let gradient = LinearGradient::new(
            Point::new(0.0, 0.25).unwrap(),
            Point::new(1.0, 0.75).unwrap(),
            stops(),
            PaintSpace::ObjectBoundingBox,
        )
        .unwrap();
        let scene = Scene {
            logical_size: crate::scene::LogicalSize {
                width: 100.0,
                height: 100.0,
            },
            nodes: vec![ShapeNode::new(
                Shape::Rect(bounds),
                Fill::new(Paint::LinearGradient(gradient)),
            )],
        };
        let primitives = prepare_scene(&scene).unwrap();
        assert!(matches!(
            &primitives[0].paint,
            PrimitivePaint::LinearGradient {
                start,
                end,
                ..
            } if *start == [10.0, 35.0] && *end == [50.0, 65.0]
        ));
    }

    #[test]
    fn repeated_texture_transform_is_inverted_and_composed_with_object_bounds() {
        let bounds = Rect::new(10.0, 20.0, 40.0, 80.0).unwrap();
        let transform = AffineTransform::new(2.0, 0.0, 0.0, 4.0, 0.0, 0.0).unwrap();
        let rows = logical_to_pattern(transform, PaintSpace::ObjectBoundingBox, bounds).rows;
        assert_eq!(rows, [[0.0125, 0.0, -0.125], [0.0, 0.003125, -0.0625]]);
    }

    #[test]
    fn non_square_object_bounding_box_radial_gradient_becomes_an_ellipse() {
        let gradient = crate::scene::RadialGradient::new(
            Point::new(0.5, 0.5).unwrap(),
            0.5,
            stops(),
            PaintSpace::ObjectBoundingBox,
        )
        .unwrap();
        let scene = Scene {
            logical_size: crate::scene::LogicalSize {
                width: 100.0,
                height: 100.0,
            },
            nodes: vec![ShapeNode::new(
                Shape::Rect(Rect::new(10.0, 20.0, 40.0, 60.0).unwrap()),
                Fill::new(Paint::RadialGradient(gradient)),
            )],
        };
        let primitives = prepare_scene(&scene).unwrap();
        assert!(matches!(
            &primitives[0].paint,
            PrimitivePaint::RadialGradient {
                center,
                radii,
                ..
            } if *center == [30.0, 50.0] && *radii == [20.0, 30.0]
        ));
    }
}
