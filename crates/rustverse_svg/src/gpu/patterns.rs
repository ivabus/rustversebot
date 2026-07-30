//! GPU pattern-paint contract shared by scene preparation and the pattern pipeline.
//!
//! Dot and diagonal paints are analytic. Repeated images refer to an already
//! resident texture and carry only the logical-to-pattern transform required by
//! the shader; file paths and encoded image bytes do not cross this boundary.

/// Stable renderer-owned handle for a texture that is already GPU resident.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PatternTextureHandle(pub(crate) u32);

/// A two-dimensional affine transform stored as two rows.
///
/// `transform_point([x, y])` returns pattern-space logical coordinates. This
/// explicit direction avoids ambiguity when a paint transform includes both
/// translation and rotation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LogicalToPattern {
    pub(crate) rows: [[f32; 3]; 2],
}

impl LogicalToPattern {
    #[cfg(test)]
    pub(crate) const IDENTITY: Self = Self {
        rows: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
    };

    #[cfg(test)]
    pub(crate) fn transform_point(self, point: [f32; 2]) -> [f32; 2] {
        [
            self.rows[0][0] * point[0] + self.rows[0][1] * point[1] + self.rows[0][2],
            self.rows[1][0] * point[0] + self.rows[1][1] * point[1] + self.rows[1][2],
        ]
    }

    fn is_finite(self) -> bool {
        self.rows.into_iter().flatten().all(f32::is_finite)
    }
}

/// Parameters for the current 5x5 card dot pattern.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DotPattern {
    pub(crate) tile_size: [f32; 2],
    pub(crate) radius: f32,
}

impl DotPattern {
    #[cfg(test)]
    pub(crate) const CARD: Self = Self {
        tile_size: [5.0, 5.0],
        radius: 0.5,
    };
}

/// Parameters for the current 4x4 diagonal background pattern.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DiagonalPattern {
    pub(crate) tile_size: f32,
    pub(crate) line_width: f32,
}

impl DiagonalPattern {
    #[cfg(test)]
    pub(crate) const BACKGROUND: Self = Self {
        tile_size: 4.0,
        line_width: 2.0,
    };
}

/// A repeat-addressed resident texture and its paint-space transform.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RepeatedTexturePattern {
    pub(crate) texture: PatternTextureHandle,
    /// Logical size of one repetition before the paint transform is applied.
    pub(crate) tile_size: [f32; 2],
    pub(crate) logical_to_pattern: LogicalToPattern,
}

/// Pattern payload accepted by scene-to-GPU preparation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PatternPaint {
    Dots(DotPattern),
    Diagonal(DiagonalPattern),
    RepeatedTexture(RepeatedTexturePattern),
}

impl PatternPaint {
    pub(crate) fn validate(self) -> Result<(), PatternValidationError> {
        match self {
            Self::Dots(pattern) => {
                positive_pair(pattern.tile_size)?;
                positive(pattern.radius)?;
                if pattern.radius * 2.0 > pattern.tile_size[0].min(pattern.tile_size[1]) {
                    return Err(PatternValidationError::DotRadiusExceedsTile);
                }
            }
            Self::Diagonal(pattern) => {
                positive(pattern.tile_size)?;
                positive(pattern.line_width)?;
                if pattern.line_width > pattern.tile_size {
                    return Err(PatternValidationError::DiagonalLineExceedsTile);
                }
            }
            Self::RepeatedTexture(pattern) => {
                positive_pair(pattern.tile_size)?;
                if !pattern.logical_to_pattern.is_finite() {
                    return Err(PatternValidationError::NonFiniteTransform);
                }
            }
        }
        Ok(())
    }
}

fn positive(value: f32) -> Result<(), PatternValidationError> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(PatternValidationError::InvalidExtent)
    }
}

fn positive_pair(value: [f32; 2]) -> Result<(), PatternValidationError> {
    positive(value[0])?;
    positive(value[1])
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PatternValidationError {
    InvalidExtent,
    DotRadiusExceedsTile,
    DiagonalLineExceedsTile,
    NonFiniteTransform,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_procedural_patterns_are_valid() {
        PatternPaint::Dots(DotPattern::CARD).validate().unwrap();
        PatternPaint::Diagonal(DiagonalPattern::BACKGROUND)
            .validate()
            .unwrap();
    }

    #[test]
    fn repeated_texture_transform_has_an_explicit_direction() {
        let transform = LogicalToPattern {
            rows: [[0.0, 1.0, -300.0], [-1.0, 0.0, -512.0]],
        };
        assert_eq!(transform.transform_point([10.0, 20.0]), [-280.0, -522.0]);

        let paint = PatternPaint::RepeatedTexture(RepeatedTexturePattern {
            texture: PatternTextureHandle(7),
            tile_size: [512.0, 200.0],
            logical_to_pattern: transform,
        });
        paint.validate().unwrap();
    }

    #[test]
    fn invalid_pattern_parameters_do_not_reach_the_pipeline() {
        assert_eq!(
            PatternPaint::Dots(DotPattern {
                tile_size: [5.0, 5.0],
                radius: 3.0,
            })
            .validate(),
            Err(PatternValidationError::DotRadiusExceedsTile)
        );
        assert_eq!(
            PatternPaint::Diagonal(DiagonalPattern {
                tile_size: 4.0,
                line_width: 5.0,
            })
            .validate(),
            Err(PatternValidationError::DiagonalLineExceedsTile)
        );
        assert_eq!(
            PatternPaint::RepeatedTexture(RepeatedTexturePattern {
                texture: PatternTextureHandle(0),
                tile_size: [512.0, 0.0],
                logical_to_pattern: LogicalToPattern::IDENTITY,
            })
            .validate(),
            Err(PatternValidationError::InvalidExtent)
        );
    }
}
