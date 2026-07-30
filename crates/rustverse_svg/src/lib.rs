//! Card view preparation and rendering.
//!
//! The SVG implementation is retained as a private reference backend while
//! the backend-neutral scene and renderer contracts are introduced.

pub mod cards;
mod model;
mod reference_svg;
pub mod renderer;
pub mod scene;

pub use model::*;
pub use reference_svg::{
    MJ_ENVIRONMENT, USVG_OPTIONS, da, deadly_info, deadly_info_with_begin_time, preload_da_images,
    preload_deadly_info_images, preload_shiyu_images, preload_shiyu_info_images,
    render_from_serialize, render_template_source, shiyu, shiyu_info, top_da, top_shiyu,
    try_render_from_serialize, try_render_from_serialize_with_scale,
};
pub use renderer::{RenderScale, ZOOM_FACTOR};
