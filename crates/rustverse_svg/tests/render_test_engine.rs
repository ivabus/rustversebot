mod support;

use support::render_compare::{
    ComparisonPolicy, DiffBounds, ReadbackFormat, RenderBackendMetadata, RenderComparisonError,
    RgbaImage, SCENE_DISPLAY_LIST_FILENAME, aligned_gpu_bytes_per_row, compare_at_zoom,
    compare_render_fixture, write_diff_bundle,
};

fn image(width: u32, height: u32, pixels: &[[u8; 4]]) -> RgbaImage {
    RgbaImage::new(
        width,
        height,
        pixels
            .iter()
            .flat_map(|pixel| pixel.iter().copied())
            .collect(),
    )
    .unwrap()
}

#[test]
fn exact_images_have_an_empty_diff() {
    let expected = image(2, 1, &[[1, 2, 3, 255], [4, 5, 6, 255]]);
    let report = compare_at_zoom(&expected, &expected, 1.0, ComparisonPolicy::EXACT).unwrap();

    assert!(report.matches());
    assert_eq!(report.different_pixels, 0);
    assert_eq!(report.different_channels, [0; 4]);
    assert_eq!(report.max_channel_delta, [0; 4]);
    assert_eq!(report.bounds, None);
}

#[test]
fn diff_metrics_include_channels_magnitude_and_bounding_box() {
    let expected = image(
        3,
        2,
        &[
            [0, 0, 0, 255],
            [10, 20, 30, 255],
            [0, 0, 0, 255],
            [0, 0, 0, 255],
            [0, 0, 0, 255],
            [50, 60, 70, 255],
        ],
    );
    let actual = image(
        3,
        2,
        &[
            [0, 0, 0, 255],
            [13, 18, 30, 254],
            [0, 0, 0, 255],
            [0, 0, 0, 255],
            [0, 0, 0, 255],
            [50, 60, 80, 255],
        ],
    );
    let report = compare_at_zoom(&expected, &actual, 1.0, ComparisonPolicy::EXACT).unwrap();

    assert!(!report.matches());
    assert_eq!(report.different_pixels, 2);
    assert_eq!(report.different_channels, [1, 1, 1, 1]);
    assert_eq!(report.max_channel_delta, [3, 2, 10, 1]);
    assert_eq!(report.total_absolute_delta, 16);
    assert_eq!(
        report.bounds,
        Some(DiffBounds {
            left: 1,
            top: 0,
            right: 2,
            bottom: 1,
        })
    );
}

#[test]
fn channel_tolerance_is_applied_before_the_pixel_budget() {
    let expected = image(2, 1, &[[10, 10, 10, 255], [20, 20, 20, 255]]);
    let actual = image(2, 1, &[[11, 12, 10, 255], [24, 20, 20, 255]]);
    let policy = ComparisonPolicy {
        max_channel_delta: 2,
        max_differing_pixels: 1,
    };
    let report = compare_at_zoom(&expected, &actual, 1.0, policy).unwrap();

    assert!(report.matches());
    assert_eq!(report.different_pixels, 1);
    assert_eq!(report.different_channels, [1, 0, 0, 0]);
    assert_eq!(report.max_channel_delta, [4, 2, 0, 0]);
}

#[test]
fn parity_comparison_rejects_every_zoom_other_than_exactly_one() {
    let image = image(1, 1, &[[0, 0, 0, 255]]);

    for zoom_factor in [0.5, 1.000_001, 2.0, 5.0, 0.0, -1.0, f32::INFINITY] {
        let error =
            compare_at_zoom(&image, &image, zoom_factor, ComparisonPolicy::EXACT).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("comparisons require zoom factor 1.0")
        );
    }
}

#[test]
fn gpu_readback_removes_row_padding_and_converts_bgra() {
    let width = 2;
    let height = 2;
    let stride = aligned_gpu_bytes_per_row(width).unwrap();
    assert_eq!(stride, 256);
    let mut readback = vec![0xCD; stride * height as usize];
    readback[0..8].copy_from_slice(&[30, 20, 10, 255, 60, 50, 40, 128]);
    readback[stride..stride + 8].copy_from_slice(&[90, 80, 70, 64, 120, 110, 100, 0]);

    let decoded =
        RgbaImage::from_gpu_readback(width, height, stride, ReadbackFormat::Bgra8, &readback)
            .unwrap();

    assert_eq!(
        decoded.pixels(),
        &[
            10, 20, 30, 255, 40, 50, 60, 128, 70, 80, 90, 64, 100, 110, 120, 0
        ]
    );

    let rgba_decoded =
        RgbaImage::from_gpu_readback(width, height, stride, ReadbackFormat::Rgba8, &readback)
            .unwrap();
    assert_eq!(&rgba_decoded.pixels()[..4], &[30, 20, 10, 255]);
}

#[test]
fn png_round_trip_preserves_normalized_rgba_pixels() {
    let expected = image(
        2,
        2,
        &[
            [255, 0, 0, 255],
            [0, 255, 0, 192],
            [0, 0, 255, 128],
            [255, 255, 255, 0],
        ],
    );

    let png = expected.encode_png().unwrap();
    let decoded = RgbaImage::from_png(&png).unwrap();

    assert_eq!(decoded.width(), 2);
    assert_eq!(decoded.height(), 2);
    assert_eq!(decoded, expected);
}

#[test]
fn mismatch_bundle_contains_expected_actual_diff_and_report() {
    let expected = image(1, 1, &[[10, 20, 30, 255]]);
    let actual = image(1, 1, &[[11, 20, 30, 255]]);
    let report = compare_at_zoom(&expected, &actual, 1.0, ComparisonPolicy::EXACT).unwrap();
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let output_root = std::env::temp_dir().join(format!(
        "rustverse-svg-render-test-{}-{unique}",
        std::process::id()
    ));

    let case_dir = write_diff_bundle(
        &output_root,
        "one-pixel-mismatch",
        &expected,
        &actual,
        &report,
    )
    .unwrap();

    for filename in ["expected.png", "actual.png", "diff.png", "report.txt"] {
        assert!(case_dir.join(filename).is_file(), "{filename} is missing");
    }
    let report_text = std::fs::read_to_string(case_dir.join("report.txt")).unwrap();
    assert!(report_text.contains("1 differing pixels"));

    std::fs::remove_dir_all(output_root).unwrap();
}

#[test]
fn fixture_runner_passes_exactly_scale_one_to_both_renderers() {
    let fixture = [10, 20, 30, 255];
    let report = compare_render_fixture(
        &fixture,
        "runner-exact-match",
        ComparisonPolicy::EXACT,
        RenderBackendMetadata {
            adapter: "self-test adapter",
            backend: "self-test backend",
        },
        None,
        |fixture, scale| {
            assert_eq!(scale.to_bits(), 1.0_f32.to_bits());
            Ok::<_, &'static str>(image(1, 1, &[*fixture]))
        },
        |fixture, scale| {
            assert_eq!(scale.to_bits(), 1.0_f32.to_bits());
            Ok::<_, &'static str>(image(1, 1, &[*fixture]))
        },
    )
    .unwrap();

    assert!(report.matches());
}

#[test]
fn fixture_runner_writes_standard_mismatch_bundle_with_diagnostics() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let case_name = format!("runner-mismatch-{}-{unique}", std::process::id());
    let error = compare_render_fixture(
        &[10, 20, 30, 255],
        &case_name,
        ComparisonPolicy::EXACT,
        RenderBackendMetadata {
            adapter: "llvmpipe test adapter",
            backend: "Vulkan",
        },
        Some("DrawImage atlas=0 region=3\n"),
        |fixture, _scale| Ok::<_, &'static str>(image(1, 1, &[*fixture])),
        |_fixture, _scale| Ok::<_, &'static str>(image(1, 1, &[[11, 20, 30, 255]])),
    )
    .unwrap_err();

    let RenderComparisonError::Mismatch { report, bundle_dir } = error else {
        panic!("expected a mismatch error");
    };
    assert_eq!(report.different_pixels, 1);
    for filename in ["expected.png", "actual.png", "diff.png", "report.txt"] {
        assert!(bundle_dir.join(filename).is_file(), "{filename} is missing");
    }
    let report_text = std::fs::read_to_string(bundle_dir.join("report.txt")).unwrap();
    assert!(report_text.contains("scale: 1.0"));
    assert!(report_text.contains("adapter: \"llvmpipe test adapter\""));
    assert!(report_text.contains("backend: \"Vulkan\""));
    assert_eq!(
        std::fs::read_to_string(bundle_dir.join(SCENE_DISPLAY_LIST_FILENAME)).unwrap(),
        "DrawImage atlas=0 region=3\n"
    );

    std::fs::remove_dir_all(bundle_dir).unwrap();
}
