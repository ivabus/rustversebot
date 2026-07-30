use rustverse_svg::{
    RenderScale,
    gpu::{
        GpuInitError, GpuRenderError, GpuRendererOptions, GpuRendererService,
        MAX_CONFIGURED_TARGET_BYTES, PhysicalSize, physical_size, start_renderer_service,
    },
    renderer_service::{RenderRequest, SolidColor},
    scene::{Color, Fill, LogicalSize, Paint, Rect, Scene, Shape, ShapeNode},
};

async fn service_or_documented_skip(options: GpuRendererOptions) -> Option<GpuRendererService> {
    match start_renderer_service(options, 2).await {
        Ok(service) => Some(service),
        Err(GpuRenderError::Initialize(GpuInitError::AdapterUnavailable(error))) => {
            if std::env::var_os("RUSTVERSE_REQUIRE_GPU").is_some_and(|value| value == "1") {
                panic!(
                    "RUSTVERSE_REQUIRE_GPU=1 but no surface-free wgpu adapter is available: \
                     {error}"
                );
            }
            eprintln!("SKIP: no surface-free wgpu adapter is available: {error}");
            None
        }
        Err(error) => panic!("GPU renderer service initialization failed: {error}"),
    }
}

#[tokio::test]
async fn initializes_surface_free_adapter() {
    let Some(_service) = service_or_documented_skip(GpuRendererOptions::default()).await else {
        return;
    };
}

#[tokio::test]
async fn clear_pass_reads_back_rgba_and_encodes_png() {
    let Some(service) = service_or_documented_skip(GpuRendererOptions::default()).await else {
        return;
    };
    let png = service
        .render(RenderRequest::clear(
            LogicalSize {
                width: 3.0,
                height: 2.0,
            },
            RenderScale::ONE,
            SolidColor::rgba(255, 0, 0, 255),
        ))
        .await
        .unwrap();

    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    let decoder = png::Decoder::new(std::io::Cursor::new(&png));
    let mut reader = decoder.read_info().unwrap();
    let mut decoded = vec![0; reader.output_buffer_size().unwrap()];
    let output = reader.next_frame(&mut decoded).unwrap();
    assert_eq!((output.width, output.height), (3, 2));
    assert_eq!(&decoded[..output.buffer_size()], [255, 0, 0, 255].repeat(6));
}

#[tokio::test]
async fn production_service_renders_a_backend_neutral_shape_scene() {
    let Some(service) = service_or_documented_skip(GpuRendererOptions::default()).await else {
        return;
    };
    let logical_size = LogicalSize {
        width: 8.0,
        height: 8.0,
    };
    let mut scene = Scene::new(logical_size);
    scene.nodes.push(ShapeNode::new(
        Shape::Rect(Rect::new(2.0, 2.0, 4.0, 4.0).unwrap()),
        Fill::new(Paint::Solid(Color::new(1.0, 0.0, 0.0, 1.0).unwrap())),
    ));

    let png = service
        .render(RenderRequest::scene(
            scene,
            RenderScale::ONE,
            SolidColor::rgba(0, 0, 0, 255),
        ))
        .await
        .unwrap();
    let mut reader = png::Decoder::new(std::io::Cursor::new(png))
        .read_info()
        .unwrap();
    let mut decoded = vec![0; reader.output_buffer_size().unwrap()];
    let output = reader.next_frame(&mut decoded).unwrap();
    let pixel = |x: usize, y: usize| {
        let offset = (y * output.width as usize + x) * 4;
        &decoded[offset..offset + 4]
    };
    assert_eq!(pixel(0, 0), [0, 0, 0, 255]);
    assert_eq!(pixel(3, 3), [255, 0, 0, 255]);
}

#[test]
fn scale_matrix_produces_expected_physical_sizes() {
    let logical = LogicalSize {
        width: 12.5,
        height: 7.25,
    };
    for (factor, expected) in [
        (
            0.5,
            PhysicalSize {
                width: 6,
                height: 4,
            },
        ),
        (
            1.0,
            PhysicalSize {
                width: 13,
                height: 7,
            },
        ),
        (
            1.25,
            PhysicalSize {
                width: 16,
                height: 9,
            },
        ),
        (
            2.0,
            PhysicalSize {
                width: 25,
                height: 15,
            },
        ),
        (
            4.0,
            PhysicalSize {
                width: 50,
                height: 29,
            },
        ),
        (
            5.0,
            PhysicalSize {
                width: 63,
                height: 36,
            },
        ),
    ] {
        assert_eq!(
            physical_size(logical, RenderScale::new(factor).unwrap()).unwrap(),
            expected
        );
    }

    assert_eq!(
        physical_size(
            LogicalSize {
                width: 1.0,
                height: 1.0
            },
            RenderScale::new(0.5).unwrap()
        )
        .unwrap(),
        PhysicalSize {
            width: 1,
            height: 1
        }
    );
}

#[test]
fn renderer_budget_is_bounded_before_gpu_initialization() {
    assert!(GpuRendererOptions::new(0).is_err());
    let error = GpuRendererOptions::new(MAX_CONFIGURED_TARGET_BYTES + 1).unwrap_err();
    assert!(error.to_string().contains("configured upper bound"));
    assert_eq!(
        GpuRendererOptions::new(4096).unwrap().max_target_bytes(),
        4096
    );
}

#[tokio::test]
async fn one_context_supports_repeated_renders() {
    let Some(service) = service_or_documented_skip(GpuRendererOptions::default()).await else {
        return;
    };
    let logical = LogicalSize {
        width: 17.0,
        height: 3.0,
    };

    let first = service
        .render(RenderRequest::clear(
            logical,
            RenderScale::ONE,
            SolidColor::rgba(0, 255, 0, 255),
        ))
        .await
        .unwrap();
    let second = service
        .render(RenderRequest::clear(
            logical,
            RenderScale::ONE,
            SolidColor::rgba(0, 0, 255, 255),
        ))
        .await
        .unwrap();

    assert_ne!(first, second);
}

#[tokio::test]
async fn renderer_service_emits_deterministic_solids_at_every_required_scale() {
    let service = match start_renderer_service(GpuRendererOptions::default(), 2).await {
        Ok(service) => service,
        Err(GpuRenderError::Initialize(GpuInitError::AdapterUnavailable(error))) => {
            eprintln!("SKIP: no surface-free wgpu adapter is available: {error}");
            return;
        }
        Err(error) => panic!("GPU renderer service initialization failed: {error}"),
    };
    let logical_size = LogicalSize {
        width: 8.0,
        height: 6.0,
    };

    for (factor, expected_size) in [
        (
            0.5,
            PhysicalSize {
                width: 4,
                height: 3,
            },
        ),
        (
            1.0,
            PhysicalSize {
                width: 8,
                height: 6,
            },
        ),
        (
            1.25,
            PhysicalSize {
                width: 10,
                height: 8,
            },
        ),
        (
            2.0,
            PhysicalSize {
                width: 16,
                height: 12,
            },
        ),
        (
            5.0,
            PhysicalSize {
                width: 40,
                height: 30,
            },
        ),
    ] {
        let request = RenderRequest::clear(
            logical_size,
            RenderScale::new(factor).unwrap(),
            SolidColor::rgba(17, 34, 51, 255),
        );
        let first = service.render(request.clone()).await.unwrap();
        let second = service.render(request).await.unwrap();
        assert_eq!(first, second, "warm render changed at scale {factor}");

        let mut reader = png::Decoder::new(std::io::Cursor::new(first))
            .read_info()
            .unwrap();
        let mut decoded = vec![0; reader.output_buffer_size().unwrap()];
        let output = reader.next_frame(&mut decoded).unwrap();
        assert_eq!(
            (output.width, output.height),
            (expected_size.width, expected_size.height)
        );
        assert_eq!(
            &decoded[..output.buffer_size()],
            [17, 34, 51, 255].repeat((expected_size.width * expected_size.height) as usize)
        );
    }
}
