//! Backend-neutral renderer configuration.

/// Default output scale used by the compatibility renderer.
///
/// Layout is expressed in logical units. Each renderer receives a
/// [`RenderScale`] and produces physical dimensions from it.
pub const ZOOM_FACTOR: f32 = 5.0;

/// A validated logical-to-physical render scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderScale(f32);

impl RenderScale {
    pub const ONE: Self = Self(1.0);
    pub const DEFAULT: Self = Self(ZOOM_FACTOR);

    pub fn new(zoom_factor: f32) -> anyhow::Result<Self> {
        anyhow::ensure!(
            zoom_factor.is_finite() && zoom_factor > 0.0,
            "render zoom factor must be finite and greater than zero"
        );
        Ok(Self(zoom_factor))
    }

    pub const fn factor(self) -> f32 {
        self.0
    }
}

impl Default for RenderScale {
    fn default() -> Self {
        Self::DEFAULT
    }
}
