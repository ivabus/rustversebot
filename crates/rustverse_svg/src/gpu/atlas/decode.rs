//! Strict, GPU-independent decoding into canonical RGBA8 pixels.

use std::fmt;
use std::io::{BufReader, Cursor};
use std::num::NonZeroU64;

use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

use super::types::ContentHash;

/// Conservative defaults for images admitted to the persistent atlas.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DecodeLimits {
    pub(crate) max_encoded_bytes: usize,
    pub(crate) max_width: u32,
    pub(crate) max_height: u32,
    pub(crate) max_pixels: u64,
    pub(crate) max_decoded_bytes: usize,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_encoded_bytes: 32 * 1024 * 1024,
            max_width: 8_192,
            max_height: 8_192,
            max_pixels: 32 * 1024 * 1024,
            max_decoded_bytes: 128 * 1024 * 1024,
        }
    }
}

/// Encoded format selected from a validated file signature.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EncodedImageFormat {
    Png,
    Jpeg,
    WebP,
    Gif,
}

/// A decoded image ready for packing and upload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DecodedImage {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba8: Vec<u8>,
    pub(crate) content_hash: ContentHash,
}

impl DecodedImage {
    pub(crate) fn decoded_bytes(&self) -> usize {
        self.rgba8.len()
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum DecodeImageError {
    Empty,
    EncodedTooLarge {
        actual: usize,
        limit: usize,
    },
    UnsupportedFormat,
    InvalidImage {
        format: EncodedImageFormat,
        reason: String,
    },
    DimensionsExceeded {
        width: u32,
        height: u32,
        max_width: u32,
        max_height: u32,
    },
    PixelCountExceeded {
        actual: u64,
        limit: u64,
    },
    DecodedTooLarge {
        actual: u64,
        limit: usize,
    },
    MissingFirstFrame(EncodedImageFormat),
    InconsistentPixels {
        format: EncodedImageFormat,
        expected: usize,
        actual: usize,
    },
}

impl fmt::Display for DecodeImageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("encoded image is empty"),
            Self::EncodedTooLarge { actual, limit } => {
                write!(
                    formatter,
                    "encoded image is {actual} bytes; limit is {limit}"
                )
            }
            Self::UnsupportedFormat => formatter.write_str("unsupported encoded image format"),
            Self::InvalidImage { format, reason } => {
                write!(formatter, "invalid {format:?} image: {reason}")
            }
            Self::DimensionsExceeded {
                width,
                height,
                max_width,
                max_height,
            } => write!(
                formatter,
                "image dimensions {width}x{height} exceed {max_width}x{max_height}"
            ),
            Self::PixelCountExceeded { actual, limit } => {
                write!(formatter, "image has {actual} pixels; limit is {limit}")
            }
            Self::DecodedTooLarge { actual, limit } => {
                write!(
                    formatter,
                    "decoded image is {actual} bytes; limit is {limit}"
                )
            }
            Self::MissingFirstFrame(format) => write!(formatter, "{format:?} has no first frame"),
            Self::InconsistentPixels {
                format,
                expected,
                actual,
            } => write!(
                formatter,
                "{format:?} decoder produced {actual} bytes; expected {expected}"
            ),
        }
    }
}

impl std::error::Error for DecodeImageError {}

pub(crate) fn detect_format(encoded: &[u8]) -> Result<EncodedImageFormat, DecodeImageError> {
    if encoded.is_empty() {
        return Err(DecodeImageError::Empty);
    }
    if encoded.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok(EncodedImageFormat::Png);
    }
    if encoded.starts_with(b"\xff\xd8\xff") {
        return Ok(EncodedImageFormat::Jpeg);
    }
    if encoded.starts_with(b"GIF87a") || encoded.starts_with(b"GIF89a") {
        return Ok(EncodedImageFormat::Gif);
    }
    if encoded.len() >= 12 && &encoded[..4] == b"RIFF" && &encoded[8..12] == b"WEBP" {
        return Ok(EncodedImageFormat::WebP);
    }
    Err(DecodeImageError::UnsupportedFormat)
}

pub(crate) fn decode_image(
    encoded: &[u8],
    limits: DecodeLimits,
) -> Result<DecodedImage, DecodeImageError> {
    if encoded.len() > limits.max_encoded_bytes {
        return Err(DecodeImageError::EncodedTooLarge {
            actual: encoded.len(),
            limit: limits.max_encoded_bytes,
        });
    }

    let format = detect_format(encoded)?;
    let (width, height, rgba8) = match format {
        EncodedImageFormat::Png => decode_png(encoded, limits)?,
        EncodedImageFormat::Jpeg => decode_jpeg(encoded, limits)?,
        EncodedImageFormat::WebP => decode_webp(encoded, limits)?,
        EncodedImageFormat::Gif => decode_gif(encoded, limits)?,
    };
    validate_dimensions(format, width, height, limits)?;
    let expected = rgba_len(width, height, limits)?;
    if rgba8.len() != expected {
        return Err(DecodeImageError::InconsistentPixels {
            format,
            expected,
            actual: rgba8.len(),
        });
    }
    let content_hash = stable_content_hash(width, height, &rgba8);
    Ok(DecodedImage {
        width,
        height,
        rgba8,
        content_hash,
    })
}

fn validate_dimensions(
    _format: EncodedImageFormat,
    width: u32,
    height: u32,
    limits: DecodeLimits,
) -> Result<(), DecodeImageError> {
    if width == 0 || height == 0 || width > limits.max_width || height > limits.max_height {
        return Err(DecodeImageError::DimensionsExceeded {
            width,
            height,
            max_width: limits.max_width,
            max_height: limits.max_height,
        });
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > limits.max_pixels {
        return Err(DecodeImageError::PixelCountExceeded {
            actual: pixels,
            limit: limits.max_pixels,
        });
    }
    rgba_len(width, height, limits).map(|_| ())
}

fn rgba_len(width: u32, height: u32, limits: DecodeLimits) -> Result<usize, DecodeImageError> {
    let decoded = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .unwrap_or(u64::MAX);
    if decoded > limits.max_decoded_bytes as u64 || decoded > usize::MAX as u64 {
        return Err(DecodeImageError::DecodedTooLarge {
            actual: decoded,
            limit: limits.max_decoded_bytes,
        });
    }
    Ok(decoded as usize)
}

fn invalid(format: EncodedImageFormat, error: impl fmt::Display) -> DecodeImageError {
    DecodeImageError::InvalidImage {
        format,
        reason: error.to_string(),
    }
}

fn decode_png(
    encoded: &[u8],
    limits: DecodeLimits,
) -> Result<(u32, u32, Vec<u8>), DecodeImageError> {
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(encoded),
        png::Limits {
            bytes: limits.max_decoded_bytes,
        },
    );
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    let mut reader = decoder
        .read_info()
        .map_err(|error| invalid(EncodedImageFormat::Png, error))?;
    let (width, height) = (reader.info().width, reader.info().height);
    validate_dimensions(EncodedImageFormat::Png, width, height, limits)?;
    let buffer_size = reader
        .output_buffer_size()
        .ok_or(DecodeImageError::DecodedTooLarge {
            actual: u64::MAX,
            limit: limits.max_decoded_bytes,
        })?;
    if buffer_size > limits.max_decoded_bytes {
        return Err(DecodeImageError::DecodedTooLarge {
            actual: buffer_size as u64,
            limit: limits.max_decoded_bytes,
        });
    }
    let mut pixels = vec![0; buffer_size];
    let output = reader
        .next_frame(&mut pixels)
        .map_err(|error| invalid(EncodedImageFormat::Png, error))?;
    pixels.truncate(output.buffer_size());
    let frame_rgba = expand_to_rgba(pixels, output.color_type, EncodedImageFormat::Png)?;
    let (frame_x, frame_y) = reader
        .info()
        .frame_control
        .map(|control| (control.x_offset, control.y_offset))
        .unwrap_or((0, 0));
    let rgba = composite_frame_on_transparent_canvas(
        width,
        height,
        output.width,
        output.height,
        frame_x,
        frame_y,
        &frame_rgba,
        limits,
    )?;
    Ok((width, height, rgba))
}

#[allow(clippy::too_many_arguments)]
fn composite_frame_on_transparent_canvas(
    canvas_width: u32,
    canvas_height: u32,
    frame_width: u32,
    frame_height: u32,
    frame_x: u32,
    frame_y: u32,
    frame_rgba: &[u8],
    limits: DecodeLimits,
) -> Result<Vec<u8>, DecodeImageError> {
    let frame_right = frame_x.checked_add(frame_width);
    let frame_bottom = frame_y.checked_add(frame_height);
    if frame_right.is_none_or(|right| right > canvas_width)
        || frame_bottom.is_none_or(|bottom| bottom > canvas_height)
    {
        return Err(invalid(
            EncodedImageFormat::Png,
            "first frame is outside the APNG canvas",
        ));
    }
    let expected_frame_len = rgba_len(frame_width, frame_height, limits)?;
    if frame_rgba.len() != expected_frame_len {
        return Err(DecodeImageError::InconsistentPixels {
            format: EncodedImageFormat::Png,
            expected: expected_frame_len,
            actual: frame_rgba.len(),
        });
    }
    if frame_width == canvas_width && frame_height == canvas_height && frame_x == 0 && frame_y == 0
    {
        return Ok(frame_rgba.to_vec());
    }

    // APNG starts with a transparent displayed canvas. SOURCE and OVER are
    // therefore equivalent for the first frame at its declared offset.
    let mut canvas = vec![0; rgba_len(canvas_width, canvas_height, limits)?];
    let canvas_stride = canvas_width as usize * 4;
    let frame_stride = frame_width as usize * 4;
    for row in 0..frame_height as usize {
        let source_start = row * frame_stride;
        let target_start = (frame_y as usize + row) * canvas_stride + frame_x as usize * 4;
        canvas[target_start..target_start + frame_stride]
            .copy_from_slice(&frame_rgba[source_start..source_start + frame_stride]);
    }
    Ok(canvas)
}

fn expand_to_rgba(
    pixels: Vec<u8>,
    color_type: png::ColorType,
    format: EncodedImageFormat,
) -> Result<Vec<u8>, DecodeImageError> {
    let mut rgba = Vec::with_capacity(pixels.len().saturating_mul(4));
    match color_type {
        png::ColorType::Rgba => return Ok(pixels),
        png::ColorType::Rgb => {
            for pixel in pixels.chunks_exact(3) {
                rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
        }
        png::ColorType::Grayscale => {
            for value in pixels {
                rgba.extend_from_slice(&[value, value, value, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for pixel in pixels.chunks_exact(2) {
                rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
            }
        }
        png::ColorType::Indexed => {
            return Err(invalid(format, "palette was not expanded"));
        }
    }
    Ok(rgba)
}

fn decode_jpeg(
    encoded: &[u8],
    limits: DecodeLimits,
) -> Result<(u32, u32, Vec<u8>), DecodeImageError> {
    let options = DecoderOptions::default()
        .set_max_width(limits.max_width as usize)
        .set_max_height(limits.max_height as usize)
        .set_strict_mode(true)
        .jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(encoded), options);
    decoder
        .decode_headers()
        .map_err(|error| invalid(EncodedImageFormat::Jpeg, error))?;
    let info = decoder
        .info()
        .ok_or_else(|| invalid(EncodedImageFormat::Jpeg, "decoder returned no image info"))?;
    let (width, height) = (u32::from(info.width), u32::from(info.height));
    validate_dimensions(EncodedImageFormat::Jpeg, width, height, limits)?;
    let rgba = decoder
        .decode()
        .map_err(|error| invalid(EncodedImageFormat::Jpeg, error))?;
    Ok((width, height, rgba))
}

fn decode_webp(
    encoded: &[u8],
    limits: DecodeLimits,
) -> Result<(u32, u32, Vec<u8>), DecodeImageError> {
    let mut decoder = image_webp::WebPDecoder::new(BufReader::new(Cursor::new(encoded)))
        .map_err(|error| invalid(EncodedImageFormat::WebP, error))?;
    decoder.set_memory_limit(limits.max_decoded_bytes);
    let (width, height) = decoder.dimensions();
    validate_dimensions(EncodedImageFormat::WebP, width, height, limits)?;
    let output_size = decoder
        .output_buffer_size()
        .ok_or(DecodeImageError::DecodedTooLarge {
            actual: u64::MAX,
            limit: limits.max_decoded_bytes,
        })?;
    if output_size > limits.max_decoded_bytes {
        return Err(DecodeImageError::DecodedTooLarge {
            actual: output_size as u64,
            limit: limits.max_decoded_bytes,
        });
    }
    let has_alpha = decoder.has_alpha();
    let mut pixels = vec![0; output_size];
    decoder
        .read_image(&mut pixels)
        .map_err(|error| invalid(EncodedImageFormat::WebP, error))?;
    let rgba = if has_alpha {
        pixels
    } else {
        let mut rgba = Vec::with_capacity(rgba_len(width, height, limits)?);
        for pixel in pixels.chunks_exact(3) {
            rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
        }
        rgba
    };
    Ok((width, height, rgba))
}

fn decode_gif(
    encoded: &[u8],
    limits: DecodeLimits,
) -> Result<(u32, u32, Vec<u8>), DecodeImageError> {
    let memory_limit = NonZeroU64::new(limits.max_decoded_bytes as u64).ok_or(
        DecodeImageError::DecodedTooLarge {
            actual: 1,
            limit: 0,
        },
    )?;
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    options.set_memory_limit(gif::MemoryLimit::Bytes(memory_limit));
    options.check_frame_consistency(true);
    let mut decoder = options
        .read_info(Cursor::new(encoded))
        .map_err(|error| invalid(EncodedImageFormat::Gif, error))?;
    let (width, height) = (u32::from(decoder.width()), u32::from(decoder.height()));
    validate_dimensions(EncodedImageFormat::Gif, width, height, limits)?;
    let canvas_len = rgba_len(width, height, limits)?;
    let frame = decoder
        .read_next_frame()
        .map_err(|error| invalid(EncodedImageFormat::Gif, error))?
        .ok_or(DecodeImageError::MissingFirstFrame(EncodedImageFormat::Gif))?;
    let frame_width = usize::from(frame.width);
    let frame_height = usize::from(frame.height);
    let left = usize::from(frame.left);
    let top = usize::from(frame.top);
    let canvas_width = width as usize;
    let mut rgba = vec![0; canvas_len];
    for row in 0..frame_height {
        let source_start = row * frame_width * 4;
        let target_start = ((top + row) * canvas_width + left) * 4;
        rgba[target_start..target_start + frame_width * 4]
            .copy_from_slice(&frame.buffer[source_start..source_start + frame_width * 4]);
    }
    Ok((width, height, rgba))
}

fn stable_content_hash(width: u32, height: u32, rgba8: &[u8]) -> ContentHash {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    context.update(&width.to_le_bytes());
    context.update(&height.to_le_bytes());
    context.update(rgba8);
    let digest = context.finish();
    let mut result = [0; 32];
    result.copy_from_slice(digest.as_ref());
    ContentHash::new(result)
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    const PIXELS: [u8; 8] = [255, 0, 0, 255, 0, 255, 0, 128];

    fn encode_png() -> Vec<u8> {
        let mut encoded = Vec::new();
        let mut encoder = png::Encoder::new(&mut encoded, 2, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&PIXELS).unwrap();
        drop(writer);
        encoded
    }

    fn encode_apng_first_frame() -> Vec<u8> {
        let mut encoded = Vec::new();
        let mut encoder = png::Encoder::new(&mut encoded, 2, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_animated(1, 0).unwrap();
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&PIXELS).unwrap();
        drop(writer);
        encoded
    }

    fn encode_webp() -> Vec<u8> {
        let mut encoded = Vec::new();
        image_webp::WebPEncoder::new(&mut encoded)
            .encode(&PIXELS, 2, 1, image_webp::ColorType::Rgba8)
            .unwrap();
        encoded
    }

    fn encode_gif() -> Vec<u8> {
        let mut encoded = Vec::new();
        {
            let mut encoder =
                gif::Encoder::new(&mut encoded, 2, 1, &[255, 0, 0, 0, 255, 0]).unwrap();
            let frame = gif::Frame {
                width: 2,
                height: 1,
                buffer: Cow::Owned(vec![0, 1]),
                ..gif::Frame::default()
            };
            encoder.write_frame(&frame).unwrap();
        }
        encoded
    }

    fn jpeg_fixture() -> Vec<u8> {
        const BASE64: &str = "\
            /9j/4AAQSkZJRgABAQAA2ADYAAD/4QCARXhpZgAATU0AKgAAAAgABAEaAAUAAAABAAAAPgEbAAUA\
            AAABAAAARgEoAAMAAAABAAIAAIdpAAQAAAABAAAATgAAAAAAAADYAAAAAQAAANgAAAABAAOgAQAD\
            AAAAAQABAACgAgAEAAAAAQAAAAGgAwAEAAAAAQAAAAEAAAAA/+0AOFBob3Rvc2hvcCAzLjAAOEJJ\
            TQQEAAAAAAAAOEJJTQQlAAAAAAAQ1B2M2Y8AsgTpgAmY7PhCfv/AABEIAAEAAQMBIgACEQEDEQH/\
            xAAfAAABBQEBAQEBAQAAAAAAAAAAAQIDBAUGBwgJCgv/xAC1EAACAQMDAgQDBQUEBAAAAX0BAgMA\
            BBEFEiExQQYTUWEHInEUMoGRoQgjQrHBFVLR8CQzYnKCCQoWFxgZGiUmJygpKjQ1Njc4OTpDREVG\
            R0hJSlNUVVZXWFlaY2RlZmdoaWpzdHV2d3h5eoOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0\
            tba3uLm6wsPExcbHyMnK0tPU1dbX2Nna4eLj5OXm5+jp6vHy8/T19vf4+fr/xAAfAQADAQEBAQEB\
            AQEBAAAAAAAAAQIDBAUGBwgJCgv/xAC1EQACAQIEBAMEBwUEBAABAncAAQIDEQQFITEGEkFRB2Fx\
            EyIygQgUQpGhscEJIzNS8BVictEKFiQ04SXxFxgZGiYnKCkqNTY3ODk6Q0RFRkdISUpTVFVWV1hZ\
            WmNkZWZnaGlqc3R1dnd4eXqCg4SFhoeIiYqSk5SVlpeYmZqio6Slpqeoqaqys7S1tre4ubrCw8TF\
            xsfIycrS09TV1tfY2dri4+Tl5ufo6ery8/T19vf4+fr/2wBDAAICAgICAgMCAgMFAwMDBQYFBQUF\
            BggGBgYGBggKCAgICAgICgoKCgoKCgoMDAwMDAwODg4ODg8PDw8PDw8PDw//2wBDAQICAgQEBAcE\
            BAcQCwkLEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBD/\
            3QAEAAH/2gAMAwEAAhEDEQA/AI6KKK/l8/TD/9k=";
        let mut decoded = Vec::with_capacity(BASE64.len() * 3 / 4);
        let mut accumulator = 0_u32;
        let mut bits = 0_u8;
        for byte in BASE64.bytes().filter(|byte| !byte.is_ascii_whitespace()) {
            if byte == b'=' {
                break;
            }
            let value = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => panic!("invalid base64 fixture"),
            };
            accumulator = (accumulator << 6) | u32::from(value);
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                decoded.push((accumulator >> bits) as u8);
                accumulator &= (1 << bits) - 1;
            }
        }
        decoded
    }

    #[test]
    fn decodes_png_to_canonical_rgba8() {
        let decoded = decode_image(&encode_png(), DecodeLimits::default()).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 1));
        assert_eq!(decoded.rgba8, PIXELS);
        assert_eq!(decoded.decoded_bytes(), 8);
    }

    #[test]
    fn apng_first_frame_decodes_as_the_displayed_canvas() {
        let decoded = decode_image(&encode_apng_first_frame(), DecodeLimits::default()).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 1));
        assert_eq!(decoded.rgba8, PIXELS);
    }

    #[test]
    fn offset_apng_subframe_composites_on_transparent_canvas() {
        let decoded = composite_frame_on_transparent_canvas(
            2,
            1,
            1,
            1,
            1,
            0,
            &[255, 0, 0, 128],
            DecodeLimits::default(),
        )
        .unwrap();
        assert_eq!(decoded, [0, 0, 0, 0, 255, 0, 0, 128]);
    }

    #[test]
    fn decodes_webp_to_canonical_rgba8() {
        let decoded = decode_image(&encode_webp(), DecodeLimits::default()).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 1));
        assert_eq!(decoded.rgba8, PIXELS);
    }

    #[test]
    fn decodes_jpeg_to_canonical_rgba8() {
        let decoded = decode_image(&jpeg_fixture(), DecodeLimits::default()).unwrap();
        assert_eq!((decoded.width, decoded.height), (1, 1));
        assert_eq!(decoded.rgba8.len(), 4);
        assert_eq!(decoded.rgba8[3], 255);
    }

    #[test]
    fn gif_uses_the_first_frame_deterministically() {
        let decoded = decode_image(&encode_gif(), DecodeLimits::default()).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 1));
        assert_eq!(decoded.rgba8, [255, 0, 0, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn content_hash_depends_on_canonical_content_not_encoding() {
        let png = decode_image(&encode_png(), DecodeLimits::default()).unwrap();
        let webp = decode_image(&encode_webp(), DecodeLimits::default()).unwrap();
        assert_eq!(png.content_hash, webp.content_hash);
        assert_eq!(
            png.content_hash.0,
            [
                0x70, 0xaf, 0x0d, 0x83, 0x31, 0xda, 0x0a, 0xd1, 0x6c, 0xfa, 0x3f, 0x85, 0xee, 0x12,
                0x8e, 0x4a, 0x91, 0x00, 0xf1, 0x46, 0x52, 0x97, 0xa9, 0x62, 0x9a, 0x63, 0x3e, 0x89,
                0x04, 0xc7, 0x4b, 0x84,
            ]
        );
    }

    #[test]
    fn rejects_unknown_truncated_and_over_limit_inputs() {
        assert_eq!(
            decode_image(b"not an image", DecodeLimits::default()).unwrap_err(),
            DecodeImageError::UnsupportedFormat
        );
        assert!(matches!(
            decode_image(b"\x89PNG\r\n\x1a\n", DecodeLimits::default()).unwrap_err(),
            DecodeImageError::InvalidImage {
                format: EncodedImageFormat::Png,
                ..
            }
        ));
        let limits = DecodeLimits {
            max_encoded_bytes: 1,
            ..DecodeLimits::default()
        };
        assert_eq!(
            decode_image(&encode_png(), limits).unwrap_err(),
            DecodeImageError::EncodedTooLarge {
                actual: encode_png().len(),
                limit: 1,
            }
        );
    }

    #[test]
    fn rejects_decoded_dimensions_before_pixel_allocation() {
        let limits = DecodeLimits {
            max_width: 1,
            ..DecodeLimits::default()
        };
        assert!(matches!(
            decode_image(&encode_png(), limits).unwrap_err(),
            DecodeImageError::DimensionsExceeded { width: 2, .. }
        ));
    }
}
