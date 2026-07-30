//! Scale-1 color characterization for the temporary resvg reference renderer.
//!
//! These exact bytes select raw `Rgba8Unorm`/sRGB-like byte interpolation as
//! the engine-wide Phase 3 policy. GPU paint, image, and composition pipelines
//! must match the matrix before this reference dependency is removed.

use std::sync::Arc;

use resvg::usvg::{ImageHrefResolver, ImageKind};
use resvg::{tiny_skia, usvg};
use rustverse_svg::scene::{
    AffineTransform, Color, DiagonalPattern, DotPattern, PaintSpace, PatternDescriptor,
    PatternPaint, RepeatedTexturePattern, TexturePatternHandle,
};

const WIDTH: u32 = 5;
const HEIGHT: u32 = 1;
const TEXTURE_RGBA: [u8; 4] = [17, 34, 51, 255];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CharacterizationSample {
    name: &'static str,
    expected_resvg_rgba: [u8; 4],
    policy: ComparisonPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ComparisonPolicy {
    max_channel_delta: u8,
    max_differing_pixels: u32,
}

const EXACT: ComparisonPolicy = ComparisonPolicy {
    max_channel_delta: 0,
    max_differing_pixels: 0,
};

const REFERENCE: [CharacterizationSample; 5] = [
    CharacterizationSample {
        name: "black-white midpoint",
        expected_resvg_rgba: [128, 128, 128, 255],
        policy: EXACT,
    },
    CharacterizationSample {
        name: "colored gradient midpoint",
        expected_resvg_rgba: [128, 0, 128, 255],
        policy: EXACT,
    },
    CharacterizationSample {
        name: "translucent source-over",
        expected_resvg_rgba: [128, 0, 127, 255],
        policy: EXACT,
    },
    CharacterizationSample {
        name: "multiply",
        expected_resvg_rgba: [48, 64, 48, 255],
        policy: EXACT,
    },
    CharacterizationSample {
        name: "PNG texture sample",
        expected_resvg_rgba: TEXTURE_RGBA,
        policy: EXACT,
    },
];

#[test]
fn resvg_scale_one_reference_matrix_is_byte_exact() {
    let actual = render_reference_matrix();
    assert_eq!(actual.len(), (WIDTH * HEIGHT * 4) as usize);

    for (sample, rgba) in REFERENCE.iter().zip(actual.chunks_exact(4)) {
        assert_eq!(sample.policy, EXACT, "{} policy changed", sample.name);
        assert_eq!(
            rgba, sample.expected_resvg_rgba,
            "{} reference bytes changed; update the canonical GPU color policy intentionally",
            sample.name
        );
    }
}

#[test]
fn phase_three_pattern_contract_is_svg_independent() {
    let white = Color::new(1.0, 1.0, 1.0, 1.0).unwrap();
    let dots = DotPattern::new(5.0, 5.0, 0.5, white, None).unwrap();
    let diagonal = DiagonalPattern::new(4.0, 2.0, white, None).unwrap();
    let repeated =
        RepeatedTexturePattern::new(TexturePatternHandle::new(1).unwrap(), 512.0, 200.0).unwrap();
    let rotation = AffineTransform::new(
        std::f32::consts::FRAC_1_SQRT_2,
        -std::f32::consts::FRAC_1_SQRT_2,
        std::f32::consts::FRAC_1_SQRT_2,
        std::f32::consts::FRAC_1_SQRT_2,
        -512.0,
        -300.0,
    )
    .unwrap();
    let fixtures = [
        PatternPaint::new(
            PatternDescriptor::Dots(dots),
            AffineTransform::IDENTITY,
            PaintSpace::UserSpace,
        ),
        PatternPaint::new(
            PatternDescriptor::Diagonal(diagonal),
            AffineTransform::IDENTITY,
            PaintSpace::UserSpace,
        ),
        PatternPaint::new(
            PatternDescriptor::RepeatedTexture(repeated),
            rotation,
            PaintSpace::UserSpace,
        ),
    ];

    assert!(matches!(
        fixtures[0].descriptor(),
        PatternDescriptor::Dots(_)
    ));
    assert!(matches!(
        fixtures[1].descriptor(),
        PatternDescriptor::Diagonal(_)
    ));
    assert!(matches!(
        fixtures[2].descriptor(),
        PatternDescriptor::RepeatedTexture(_)
    ));
}

fn render_reference_matrix() -> Vec<u8> {
    let texture_png = encode_fixture_png();
    let mut options = usvg::Options::default();
    options.image_href_resolver = ImageHrefResolver {
        resolve_data: options.image_href_resolver.resolve_data,
        resolve_string: Box::new(move |href, _| {
            (href == "fixture.png").then(|| ImageKind::PNG(Arc::new(texture_png.clone())))
        }),
    };

    let tree = usvg::Tree::from_data(reference_svg().as_bytes(), &options)
        .expect("characterization SVG must parse");
    assert_eq!(tree.size().width(), WIDTH as f32);
    assert_eq!(tree.size().height(), HEIGHT as f32);

    let mut pixmap = tiny_skia::Pixmap::new(WIDTH, HEIGHT).unwrap();
    resvg::render(
        &tree,
        tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    pixmap.data().to_vec()
}

fn reference_svg() -> &'static str {
    r##"<svg xmlns="http://www.w3.org/2000/svg" width="5" height="1">
      <defs>
        <linearGradient id="gray" x1="0" y1="0" x2="1" y2="0">
          <stop offset="0" stop-color="#000000"/>
          <stop offset="1" stop-color="#ffffff"/>
        </linearGradient>
        <linearGradient id="colored" x1="0" y1="0" x2="1" y2="0">
          <stop offset="0" stop-color="#ff0000"/>
          <stop offset="1" stop-color="#0000ff"/>
        </linearGradient>
      </defs>
      <rect x="0" y="0" width="1" height="1" fill="url(#gray)"/>
      <rect x="1" y="0" width="1" height="1" fill="url(#colored)"/>
      <rect x="2" y="0" width="1" height="1" fill="#0000ff"/>
      <rect x="2" y="0" width="1" height="1" fill="#ff0000" fill-opacity="0.5"/>
      <g style="isolation:isolate">
        <rect x="3" y="0" width="1" height="1" fill="#c08040"/>
        <rect x="3" y="0" width="1" height="1" fill="#4080c0" style="mix-blend-mode:multiply"/>
      </g>
      <image x="4" y="0" width="1" height="1" href="fixture.png"
             image-rendering="optimizeSpeed"/>
    </svg>"##
}

fn encode_fixture_png() -> Vec<u8> {
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&TEXTURE_RGBA).unwrap();
    }
    encoded
}
