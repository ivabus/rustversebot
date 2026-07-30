use std::fmt;
use std::io::Cursor;
use std::path::{Path, PathBuf};

pub const PIXEL_PARITY_ZOOM_FACTOR: f32 = 1.0;
pub const GPU_COPY_BYTES_PER_ROW_ALIGNMENT: usize = 256;
pub const RENDER_FAILURE_DIRECTORY: &str = "render-failures";
pub const SCENE_DISPLAY_LIST_FILENAME: &str = "scene-display-list.txt";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RgbaImage {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl RgbaImage {
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Result<Self, CompareError> {
        let expected_len = rgba_len(width, height)?;
        if pixels.len() != expected_len {
            return Err(CompareError::InvalidImageBuffer {
                width,
                height,
                expected_len,
                actual_len: pixels.len(),
            });
        }
        Ok(Self {
            width,
            height,
            pixels,
        })
    }

    pub fn from_png(bytes: &[u8]) -> Result<Self, CompareError> {
        let mut decoder = png::Decoder::new(Cursor::new(bytes));
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder
            .read_info()
            .map_err(|error| CompareError::Png(error.to_string()))?;
        let output_len = reader
            .output_buffer_size()
            .ok_or(CompareError::ImageDimensionsOverflow)?;
        let mut decoded = vec![0; output_len];
        let output = reader
            .next_frame(&mut decoded)
            .map_err(|error| CompareError::Png(error.to_string()))?;
        let decoded = &decoded[..output.buffer_size()];

        let pixels = match output.color_type {
            png::ColorType::Rgba => decoded.to_vec(),
            png::ColorType::Rgb => decoded
                .chunks_exact(3)
                .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
                .collect(),
            png::ColorType::GrayscaleAlpha => decoded
                .chunks_exact(2)
                .flat_map(|pixel| [pixel[0], pixel[0], pixel[0], pixel[1]])
                .collect(),
            png::ColorType::Grayscale => decoded
                .iter()
                .flat_map(|value| [*value, *value, *value, 255])
                .collect(),
            png::ColorType::Indexed => {
                return Err(CompareError::Png(
                    "PNG palette was not expanded by the decoder".to_owned(),
                ));
            }
        };

        Self::new(output.width, output.height, pixels)
    }

    pub fn from_gpu_readback(
        width: u32,
        height: u32,
        padded_bytes_per_row: usize,
        format: ReadbackFormat,
        readback: &[u8],
    ) -> Result<Self, CompareError> {
        let unpadded_bytes_per_row = usize::try_from(width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or(CompareError::ImageDimensionsOverflow)?;
        if padded_bytes_per_row < unpadded_bytes_per_row
            || !padded_bytes_per_row.is_multiple_of(GPU_COPY_BYTES_PER_ROW_ALIGNMENT)
        {
            return Err(CompareError::InvalidReadbackStride {
                unpadded_bytes_per_row,
                padded_bytes_per_row,
            });
        }
        let required_len = padded_bytes_per_row
            .checked_mul(
                usize::try_from(height).map_err(|_| CompareError::ImageDimensionsOverflow)?,
            )
            .ok_or(CompareError::ImageDimensionsOverflow)?;
        if readback.len() < required_len {
            return Err(CompareError::ReadbackTooShort {
                required_len,
                actual_len: readback.len(),
            });
        }

        let mut pixels = Vec::with_capacity(rgba_len(width, height)?);
        for row in readback[..required_len].chunks_exact(padded_bytes_per_row) {
            pixels.extend_from_slice(&row[..unpadded_bytes_per_row]);
        }
        if format == ReadbackFormat::Bgra8 {
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }
        Self::new(width, height, pixels)
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    pub fn encode_png(&self) -> Result<Vec<u8>, CompareError> {
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, self.width, self.height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder
                .write_header()
                .map_err(|error| CompareError::Png(error.to_string()))?;
            writer
                .write_image_data(&self.pixels)
                .map_err(|error| CompareError::Png(error.to_string()))?;
        }
        Ok(encoded)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadbackFormat {
    Rgba8,
    Bgra8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComparisonPolicy {
    /// A channel is considered changed only when its absolute delta is larger
    /// than this value. Zero means byte-for-byte comparison.
    pub max_channel_delta: u8,
    /// The maximum number of pixels that may contain a changed channel.
    pub max_differing_pixels: u64,
}

impl ComparisonPolicy {
    pub const EXACT: Self = Self {
        max_channel_delta: 0,
        max_differing_pixels: 0,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffBounds {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiffReport {
    pub width: u32,
    pub height: u32,
    pub different_pixels: u64,
    pub different_channels: [u64; 4],
    pub max_channel_delta: [u8; 4],
    pub total_absolute_delta: u64,
    pub mean_absolute_delta: f64,
    pub root_mean_square_delta: f64,
    pub bounds: Option<DiffBounds>,
    pub policy: ComparisonPolicy,
}

impl DiffReport {
    pub fn matches(&self) -> bool {
        self.different_pixels <= self.policy.max_differing_pixels
    }
}

impl fmt::Display for DiffReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}x{}: {} differing pixels, channel counts RGBA={:?}, max delta RGBA={:?}, \
             mean delta {:.6}, RMS delta {:.6}, bounds {:?}, policy {:?}",
            self.width,
            self.height,
            self.different_pixels,
            self.different_channels,
            self.max_channel_delta,
            self.mean_absolute_delta,
            self.root_mean_square_delta,
            self.bounds,
            self.policy
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum CompareError {
    InvalidZoomFactor {
        actual: String,
    },
    ImageDimensionsOverflow,
    InvalidImageBuffer {
        width: u32,
        height: u32,
        expected_len: usize,
        actual_len: usize,
    },
    DimensionMismatch {
        expected: (u32, u32),
        actual: (u32, u32),
    },
    InvalidReadbackStride {
        unpadded_bytes_per_row: usize,
        padded_bytes_per_row: usize,
    },
    ReadbackTooShort {
        required_len: usize,
        actual_len: usize,
    },
    InvalidCaseName(String),
    Png(String),
    Io(String),
}

impl fmt::Display for CompareError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidZoomFactor { actual } => write!(
                formatter,
                "pixel-parity comparisons require zoom factor 1.0, got {actual}"
            ),
            Self::ImageDimensionsOverflow => write!(formatter, "image dimensions overflow"),
            Self::InvalidImageBuffer {
                width,
                height,
                expected_len,
                actual_len,
            } => write!(
                formatter,
                "{width}x{height} RGBA image needs {expected_len} bytes, got {actual_len}"
            ),
            Self::DimensionMismatch { expected, actual } => {
                write!(
                    formatter,
                    "image dimensions differ: expected {expected:?}, got {actual:?}"
                )
            }
            Self::InvalidReadbackStride {
                unpadded_bytes_per_row,
                padded_bytes_per_row,
            } => write!(
                formatter,
                "GPU readback stride {padded_bytes_per_row} is invalid for an unpadded \
                 {unpadded_bytes_per_row}-byte row"
            ),
            Self::ReadbackTooShort {
                required_len,
                actual_len,
            } => write!(
                formatter,
                "GPU readback needs at least {required_len} bytes, got {actual_len}"
            ),
            Self::InvalidCaseName(name) => write!(formatter, "invalid render case name {name:?}"),
            Self::Png(error) => write!(formatter, "PNG error: {error}"),
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
        }
    }
}

impl std::error::Error for CompareError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderBackendMetadata<'a> {
    pub adapter: &'a str,
    pub backend: &'a str,
}

#[derive(Debug)]
pub enum RenderComparisonError {
    ReferenceRender(String),
    CandidateRender(String),
    Compare(CompareError),
    Mismatch {
        report: DiffReport,
        bundle_dir: PathBuf,
    },
}

impl fmt::Display for RenderComparisonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReferenceRender(error) => write!(formatter, "reference render failed: {error}"),
            Self::CandidateRender(error) => write!(formatter, "candidate render failed: {error}"),
            Self::Compare(error) => error.fmt(formatter),
            Self::Mismatch { report, bundle_dir } => write!(
                formatter,
                "render mismatch: {report}; failure bundle: {}",
                bundle_dir.display()
            ),
        }
    }
}

impl std::error::Error for RenderComparisonError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Compare(error) => Some(error),
            _ => None,
        }
    }
}

impl From<CompareError> for RenderComparisonError {
    fn from(error: CompareError) -> Self {
        Self::Compare(error)
    }
}

/// Render one fixture through the reference and candidate paths and compare
/// their normalized RGBA output.
///
/// The closures receive the parity scale rather than choosing it themselves.
/// This keeps reference comparisons pinned to exactly 1.0.
pub fn compare_render_fixture<F: ?Sized, Reference, Candidate, ReferenceError, CandidateError>(
    fixture: &F,
    case_name: &str,
    policy: ComparisonPolicy,
    metadata: RenderBackendMetadata<'_>,
    scene_display_list: Option<&str>,
    reference: Reference,
    candidate: Candidate,
) -> Result<DiffReport, RenderComparisonError>
where
    Reference: FnOnce(&F, f32) -> Result<RgbaImage, ReferenceError>,
    Candidate: FnOnce(&F, f32) -> Result<RgbaImage, CandidateError>,
    ReferenceError: fmt::Display,
    CandidateError: fmt::Display,
{
    compare_render_fixture_in(
        &standard_render_failure_root(),
        fixture,
        case_name,
        policy,
        metadata,
        scene_display_list,
        reference,
        candidate,
    )
}

fn compare_render_fixture_in<F: ?Sized, Reference, Candidate, ReferenceError, CandidateError>(
    failure_root: &Path,
    fixture: &F,
    case_name: &str,
    policy: ComparisonPolicy,
    metadata: RenderBackendMetadata<'_>,
    scene_display_list: Option<&str>,
    reference: Reference,
    candidate: Candidate,
) -> Result<DiffReport, RenderComparisonError>
where
    Reference: FnOnce(&F, f32) -> Result<RgbaImage, ReferenceError>,
    Candidate: FnOnce(&F, f32) -> Result<RgbaImage, CandidateError>,
    ReferenceError: fmt::Display,
    CandidateError: fmt::Display,
{
    let reference = reference(fixture, PIXEL_PARITY_ZOOM_FACTOR)
        .map_err(|error| RenderComparisonError::ReferenceRender(error.to_string()))?;
    let candidate = candidate(fixture, PIXEL_PARITY_ZOOM_FACTOR)
        .map_err(|error| RenderComparisonError::CandidateRender(error.to_string()))?;
    let report = compare_at_zoom(&reference, &candidate, PIXEL_PARITY_ZOOM_FACTOR, policy)?;
    if report.matches() {
        return Ok(report);
    }

    let bundle_dir = write_failure_bundle(
        failure_root,
        case_name,
        &reference,
        &candidate,
        &report,
        metadata,
        scene_display_list,
    )?;
    Err(RenderComparisonError::Mismatch { report, bundle_dir })
}

pub fn standard_render_failure_root() -> PathBuf {
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    target_dir.join(RENDER_FAILURE_DIRECTORY)
}

pub fn aligned_gpu_bytes_per_row(width: u32) -> Result<usize, CompareError> {
    let row = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or(CompareError::ImageDimensionsOverflow)?;
    let alignment = GPU_COPY_BYTES_PER_ROW_ALIGNMENT;
    row.checked_add(alignment - 1)
        .map(|value| value / alignment * alignment)
        .ok_or(CompareError::ImageDimensionsOverflow)
}

pub fn compare_at_zoom(
    expected: &RgbaImage,
    actual: &RgbaImage,
    zoom_factor: f32,
    policy: ComparisonPolicy,
) -> Result<DiffReport, CompareError> {
    if zoom_factor.to_bits() != PIXEL_PARITY_ZOOM_FACTOR.to_bits() {
        return Err(CompareError::InvalidZoomFactor {
            actual: zoom_factor.to_string(),
        });
    }
    if (expected.width, expected.height) != (actual.width, actual.height) {
        return Err(CompareError::DimensionMismatch {
            expected: (expected.width, expected.height),
            actual: (actual.width, actual.height),
        });
    }

    let mut different_pixels = 0_u64;
    let mut different_channels = [0_u64; 4];
    let mut max_channel_delta = [0_u8; 4];
    let mut total_absolute_delta = 0_u64;
    let mut total_squared_delta = 0_u64;
    let mut bounds = None;

    for (pixel_index, (expected_pixel, actual_pixel)) in expected
        .pixels
        .chunks_exact(4)
        .zip(actual.pixels.chunks_exact(4))
        .enumerate()
    {
        let mut pixel_differs = false;
        for channel in 0..4 {
            let delta = expected_pixel[channel].abs_diff(actual_pixel[channel]);
            max_channel_delta[channel] = max_channel_delta[channel].max(delta);
            total_absolute_delta += u64::from(delta);
            total_squared_delta += u64::from(delta) * u64::from(delta);
            if delta > policy.max_channel_delta {
                different_channels[channel] += 1;
                pixel_differs = true;
            }
        }
        if pixel_differs {
            different_pixels += 1;
            let width = usize::try_from(expected.width).unwrap();
            let x = u32::try_from(pixel_index % width).unwrap();
            let y = u32::try_from(pixel_index / width).unwrap();
            extend_bounds(&mut bounds, x, y);
        }
    }

    let sample_count = expected.pixels.len() as f64;
    Ok(DiffReport {
        width: expected.width,
        height: expected.height,
        different_pixels,
        different_channels,
        max_channel_delta,
        total_absolute_delta,
        mean_absolute_delta: total_absolute_delta as f64 / sample_count,
        root_mean_square_delta: (total_squared_delta as f64 / sample_count).sqrt(),
        bounds,
        policy,
    })
}

pub fn write_diff_bundle(
    base_dir: &Path,
    case_name: &str,
    expected: &RgbaImage,
    actual: &RgbaImage,
    report: &DiffReport,
) -> Result<PathBuf, CompareError> {
    if case_name.is_empty()
        || !case_name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(CompareError::InvalidCaseName(case_name.to_owned()));
    }
    if (expected.width, expected.height) != (actual.width, actual.height) {
        return Err(CompareError::DimensionMismatch {
            expected: (expected.width, expected.height),
            actual: (actual.width, actual.height),
        });
    }

    let case_dir = base_dir.join(case_name);
    std::fs::create_dir_all(&case_dir).map_err(|error| CompareError::Io(error.to_string()))?;
    write_file(&case_dir.join("expected.png"), &expected.encode_png()?)?;
    write_file(&case_dir.join("actual.png"), &actual.encode_png()?)?;
    write_file(
        &case_dir.join("diff.png"),
        &difference_image(expected, actual)?.encode_png()?,
    )?;
    write_file(
        &case_dir.join("report.txt"),
        format!("{report}\n").as_bytes(),
    )?;
    Ok(case_dir)
}

pub fn write_failure_bundle(
    base_dir: &Path,
    case_name: &str,
    expected: &RgbaImage,
    actual: &RgbaImage,
    report: &DiffReport,
    metadata: RenderBackendMetadata<'_>,
    scene_display_list: Option<&str>,
) -> Result<PathBuf, CompareError> {
    let case_dir = write_diff_bundle(base_dir, case_name, expected, actual, report)?;
    let report = format!(
        "{report}\nscale: {PIXEL_PARITY_ZOOM_FACTOR:.1}\nadapter: {:?}\nbackend: {:?}\n",
        metadata.adapter, metadata.backend
    );
    write_file(&case_dir.join("report.txt"), report.as_bytes())?;

    let display_list_path = case_dir.join(SCENE_DISPLAY_LIST_FILENAME);
    if let Some(display_list) = scene_display_list {
        write_file(&display_list_path, display_list.as_bytes())?;
    } else if display_list_path.exists() {
        std::fs::remove_file(display_list_path)
            .map_err(|error| CompareError::Io(error.to_string()))?;
    }
    Ok(case_dir)
}

fn difference_image(expected: &RgbaImage, actual: &RgbaImage) -> Result<RgbaImage, CompareError> {
    let pixels = expected
        .pixels
        .chunks_exact(4)
        .zip(actual.pixels.chunks_exact(4))
        .flat_map(|(expected, actual)| {
            let max_delta = (0..4)
                .map(|channel| expected[channel].abs_diff(actual[channel]))
                .max()
                .unwrap();
            if max_delta == 0 {
                [0, 0, 0, 255]
            } else {
                [max_delta.saturating_mul(4).max(32), 0, 0, 255]
            }
        })
        .collect();
    RgbaImage::new(expected.width, expected.height, pixels)
}

fn extend_bounds(bounds: &mut Option<DiffBounds>, x: u32, y: u32) {
    match bounds {
        Some(bounds) => {
            bounds.left = bounds.left.min(x);
            bounds.top = bounds.top.min(y);
            bounds.right = bounds.right.max(x);
            bounds.bottom = bounds.bottom.max(y);
        }
        None => {
            *bounds = Some(DiffBounds {
                left: x,
                top: y,
                right: x,
                bottom: y,
            });
        }
    }
}

fn rgba_len(width: u32, height: u32) -> Result<usize, CompareError> {
    usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(CompareError::ImageDimensionsOverflow)
}

fn write_file(path: &Path, contents: &[u8]) -> Result<(), CompareError> {
    std::fs::write(path, contents).map_err(|error| CompareError::Io(error.to_string()))
}
