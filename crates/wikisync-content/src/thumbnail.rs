//! Bounded validation for passive thumbnail formats.

use std::fmt;
use std::io::Cursor;

use image::codecs::jpeg::JpegDecoder;
use image::codecs::png::PngDecoder;
use image::{ImageDecoder, Limits};

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// A passive thumbnail format accepted by [`validate_thumbnail`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ThumbnailFormat {
    /// Joint Photographic Experts Group image.
    Jpeg,
    /// Portable Network Graphics image, excluding APNG animation.
    Png,
}

impl ThumbnailFormat {
    /// Returns the canonical MIME type for this format.
    #[must_use]
    pub const fn mime_type(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
        }
    }
}

impl fmt::Display for ThumbnailFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.mime_type())
    }
}

/// Resource ceilings applied before and during thumbnail decoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThumbnailLimits {
    /// Maximum number of bytes in the encoded file.
    pub max_encoded_bytes: u64,
    /// Maximum decoded width in pixels.
    pub max_width: u32,
    /// Maximum decoded height in pixels.
    pub max_height: u32,
    /// Maximum product of decoded width and height.
    pub max_pixels: u64,
    /// Maximum size of the decoder's output pixel buffer.
    pub max_decoded_bytes: u64,
}

impl Default for ThumbnailLimits {
    fn default() -> Self {
        Self {
            max_encoded_bytes: 16 * 1024 * 1024,
            max_width: 8_192,
            max_height: 8_192,
            max_pixels: 40_000_000,
            max_decoded_bytes: 160 * 1024 * 1024,
        }
    }
}

/// Trusted metadata obtained only after a complete, bounded decode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValidatedThumbnail {
    /// Decoded passive image format.
    pub format: ThumbnailFormat,
    /// Decoded width in pixels.
    pub width: u32,
    /// Decoded height in pixels.
    pub height: u32,
}

/// A stable validation failure that never contains input bytes or decoder text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThumbnailError {
    /// At least one configured resource ceiling is zero.
    InvalidLimits,
    /// The encoded file exceeds the configured byte ceiling.
    EncodedBytesExceeded,
    /// The declared MIME type is not one of the passive formats supported here.
    UnsupportedMimeType,
    /// The declared MIME type does not agree with the file signature.
    MimeSignatureMismatch,
    /// The file is structurally malformed, truncated, or contains trailing data.
    MalformedImage,
    /// An animated PNG was supplied where only a passive image is allowed.
    AnimationNotAllowed,
    /// A decoded width or height exceeds its configured ceiling.
    DimensionsExceeded,
    /// The decoded pixel count exceeds its configured ceiling.
    PixelCountExceeded,
    /// The decoder output exceeds its configured byte ceiling.
    DecodedBytesExceeded,
    /// Memory for an otherwise bounded decode buffer could not be reserved.
    DecodeBufferUnavailable,
    /// The complete image could not be decoded successfully.
    DecodeFailed,
}

impl fmt::Display for ThumbnailError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidLimits => "thumbnail limits must all be greater than zero",
            Self::EncodedBytesExceeded => "thumbnail encoded-byte limit exceeded",
            Self::UnsupportedMimeType => "thumbnail MIME type is not supported",
            Self::MimeSignatureMismatch => "thumbnail MIME type and file signature disagree",
            Self::MalformedImage => "thumbnail file is malformed or incomplete",
            Self::AnimationNotAllowed => "animated thumbnails are not allowed",
            Self::DimensionsExceeded => "thumbnail dimension limit exceeded",
            Self::PixelCountExceeded => "thumbnail pixel-count limit exceeded",
            Self::DecodedBytesExceeded => "thumbnail decoded-byte limit exceeded",
            Self::DecodeBufferUnavailable => "thumbnail decode buffer is unavailable",
            Self::DecodeFailed => "thumbnail decode failed",
        })
    }
}

impl std::error::Error for ThumbnailError {}

/// Validates and fully decodes one JPEG or passive PNG thumbnail.
///
/// Structural dimensions are checked before constructing a decoder or allocating
/// an output buffer. A successful result means the entire file was structurally
/// consumed and its complete first (and only) image decoded within `limits`.
/// The decoded pixels are intentionally discarded.
pub fn validate_thumbnail(
    encoded: &[u8],
    declared_mime: &str,
    limits: &ThumbnailLimits,
) -> Result<ValidatedThumbnail, ThumbnailError> {
    validate_limits(limits)?;
    if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > limits.max_encoded_bytes {
        return Err(ThumbnailError::EncodedBytesExceeded);
    }

    let format = match declared_mime {
        "image/jpeg" => ThumbnailFormat::Jpeg,
        "image/png" => ThumbnailFormat::Png,
        _ => return Err(ThumbnailError::UnsupportedMimeType),
    };
    if !signature_matches(encoded, format) {
        return Err(ThumbnailError::MimeSignatureMismatch);
    }

    let (width, height) = match format {
        ThumbnailFormat::Jpeg => inspect_jpeg(encoded)?,
        ThumbnailFormat::Png => inspect_png(encoded)?,
    };
    validate_dimensions(width, height, limits)?;

    match format {
        ThumbnailFormat::Jpeg => decode_jpeg(encoded, width, height, limits)?,
        ThumbnailFormat::Png => decode_png(encoded, width, height, limits)?,
    }

    Ok(ValidatedThumbnail {
        format,
        width,
        height,
    })
}

fn validate_limits(limits: &ThumbnailLimits) -> Result<(), ThumbnailError> {
    if limits.max_encoded_bytes == 0
        || limits.max_width == 0
        || limits.max_height == 0
        || limits.max_pixels == 0
        || limits.max_decoded_bytes == 0
    {
        return Err(ThumbnailError::InvalidLimits);
    }
    Ok(())
}

fn signature_matches(encoded: &[u8], format: ThumbnailFormat) -> bool {
    match format {
        ThumbnailFormat::Jpeg => encoded.starts_with(&[0xff, 0xd8]),
        ThumbnailFormat::Png => encoded.starts_with(PNG_SIGNATURE),
    }
}

fn validate_dimensions(
    width: u32,
    height: u32,
    limits: &ThumbnailLimits,
) -> Result<(), ThumbnailError> {
    if width == 0 || height == 0 {
        return Err(ThumbnailError::MalformedImage);
    }
    if width > limits.max_width || height > limits.max_height {
        return Err(ThumbnailError::DimensionsExceeded);
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > limits.max_pixels {
        return Err(ThumbnailError::PixelCountExceeded);
    }
    Ok(())
}

fn decoder_limits(limits: &ThumbnailLimits) -> Limits {
    let mut decoder_limits = Limits::default();
    decoder_limits.max_image_width = Some(limits.max_width);
    decoder_limits.max_image_height = Some(limits.max_height);
    decoder_limits.max_alloc = Some(limits.max_decoded_bytes);
    decoder_limits
}

fn decode_jpeg(
    encoded: &[u8],
    width: u32,
    height: u32,
    limits: &ThumbnailLimits,
) -> Result<(), ThumbnailError> {
    let mut decoder =
        JpegDecoder::new(Cursor::new(encoded)).map_err(|_| ThumbnailError::DecodeFailed)?;
    decoder
        .set_limits(decoder_limits(limits))
        .map_err(|_| ThumbnailError::DecodeFailed)?;
    decode_pixels(decoder, width, height, limits)
}

fn decode_png(
    encoded: &[u8],
    width: u32,
    height: u32,
    limits: &ThumbnailLimits,
) -> Result<(), ThumbnailError> {
    let decoder = PngDecoder::with_limits(Cursor::new(encoded), decoder_limits(limits))
        .map_err(|_| ThumbnailError::DecodeFailed)?;
    if decoder
        .is_apng()
        .map_err(|_| ThumbnailError::DecodeFailed)?
    {
        return Err(ThumbnailError::AnimationNotAllowed);
    }
    decode_pixels(decoder, width, height, limits)
}

fn decode_pixels(
    decoder: impl ImageDecoder,
    expected_width: u32,
    expected_height: u32,
    limits: &ThumbnailLimits,
) -> Result<(), ThumbnailError> {
    if decoder.dimensions() != (expected_width, expected_height) {
        return Err(ThumbnailError::MalformedImage);
    }
    let decoded_bytes = decoder.total_bytes();
    if decoded_bytes > limits.max_decoded_bytes {
        return Err(ThumbnailError::DecodedBytesExceeded);
    }
    let buffer_len =
        usize::try_from(decoded_bytes).map_err(|_| ThumbnailError::DecodedBytesExceeded)?;
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(buffer_len)
        .map_err(|_| ThumbnailError::DecodeBufferUnavailable)?;
    pixels.resize(buffer_len, 0);
    decoder
        .read_image(&mut pixels)
        .map_err(|_| ThumbnailError::DecodeFailed)
}

fn inspect_png(encoded: &[u8]) -> Result<(u32, u32), ThumbnailError> {
    let mut offset = PNG_SIGNATURE.len();
    let mut dimensions = None;
    let mut chunk_index = 0_u64;

    while offset < encoded.len() {
        let header_end = offset
            .checked_add(8)
            .ok_or(ThumbnailError::MalformedImage)?;
        let header = encoded
            .get(offset..header_end)
            .ok_or(ThumbnailError::MalformedImage)?;
        let length = usize::try_from(u32::from_be_bytes(
            header[0..4]
                .try_into()
                .map_err(|_| ThumbnailError::MalformedImage)?,
        ))
        .map_err(|_| ThumbnailError::MalformedImage)?;
        let chunk_type = &header[4..8];
        let chunk_end = header_end
            .checked_add(length)
            .and_then(|value| value.checked_add(4))
            .ok_or(ThumbnailError::MalformedImage)?;
        let data_end = header_end
            .checked_add(length)
            .ok_or(ThumbnailError::MalformedImage)?;
        let data = encoded
            .get(header_end..data_end)
            .ok_or(ThumbnailError::MalformedImage)?;
        if chunk_end > encoded.len() {
            return Err(ThumbnailError::MalformedImage);
        }

        match chunk_type {
            b"IHDR" => {
                if chunk_index != 0 || dimensions.is_some() || data.len() != 13 {
                    return Err(ThumbnailError::MalformedImage);
                }
                let width = u32::from_be_bytes(
                    data[0..4]
                        .try_into()
                        .map_err(|_| ThumbnailError::MalformedImage)?,
                );
                let height = u32::from_be_bytes(
                    data[4..8]
                        .try_into()
                        .map_err(|_| ThumbnailError::MalformedImage)?,
                );
                dimensions = Some((width, height));
            }
            b"acTL" => return Err(ThumbnailError::AnimationNotAllowed),
            b"IEND" => {
                if data.is_empty() && chunk_end == encoded.len() {
                    return dimensions.ok_or(ThumbnailError::MalformedImage);
                }
                return Err(ThumbnailError::MalformedImage);
            }
            _ => {}
        }

        offset = chunk_end;
        chunk_index += 1;
    }
    Err(ThumbnailError::MalformedImage)
}

fn inspect_jpeg(encoded: &[u8]) -> Result<(u32, u32), ThumbnailError> {
    let mut offset = 2;
    let mut dimensions = None;
    let mut saw_scan = false;

    while offset < encoded.len() {
        if encoded.get(offset) != Some(&0xff) {
            return Err(ThumbnailError::MalformedImage);
        }
        let marker_start = offset;
        while encoded.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let marker = *encoded.get(offset).ok_or(ThumbnailError::MalformedImage)?;
        offset += 1;

        match marker {
            0xd9 => {
                if saw_scan && offset == encoded.len() {
                    return dimensions.ok_or(ThumbnailError::MalformedImage);
                }
                return Err(ThumbnailError::MalformedImage);
            }
            0xd8 | 0x00 | 0x01 | 0xd0..=0xd7 => return Err(ThumbnailError::MalformedImage),
            _ => {}
        }

        let length_end = offset
            .checked_add(2)
            .ok_or(ThumbnailError::MalformedImage)?;
        let segment_length = usize::from(u16::from_be_bytes(
            encoded
                .get(offset..length_end)
                .ok_or(ThumbnailError::MalformedImage)?
                .try_into()
                .map_err(|_| ThumbnailError::MalformedImage)?,
        ));
        if segment_length < 2 {
            return Err(ThumbnailError::MalformedImage);
        }
        let segment_end = offset
            .checked_add(segment_length)
            .ok_or(ThumbnailError::MalformedImage)?;
        let payload = encoded
            .get(length_end..segment_end)
            .ok_or(ThumbnailError::MalformedImage)?;

        if is_start_of_frame(marker) {
            if payload.len() < 6 || dimensions.is_some() {
                return Err(ThumbnailError::MalformedImage);
            }
            let height = u32::from(u16::from_be_bytes([payload[1], payload[2]]));
            let width = u32::from(u16::from_be_bytes([payload[3], payload[4]]));
            dimensions = Some((width, height));
        }

        offset = segment_end;
        if marker == 0xda {
            saw_scan = true;
            offset = next_jpeg_marker(encoded, offset)?;
            if offset <= marker_start {
                return Err(ThumbnailError::MalformedImage);
            }
        }
    }
    Err(ThumbnailError::MalformedImage)
}

fn next_jpeg_marker(encoded: &[u8], mut offset: usize) -> Result<usize, ThumbnailError> {
    while offset < encoded.len() {
        if encoded[offset] != 0xff {
            offset += 1;
            continue;
        }
        let marker_start = offset;
        while encoded.get(offset) == Some(&0xff) {
            offset += 1;
        }
        match encoded.get(offset).copied() {
            Some(0x00 | 0xd0..=0xd7) => offset += 1,
            Some(_) => return Ok(marker_start),
            None => return Err(ThumbnailError::MalformedImage),
        }
    }
    Err(ThumbnailError::MalformedImage)
}

fn is_start_of_frame(marker: u8) -> bool {
    matches!(
        marker,
        0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::jpeg::JpegEncoder;
    use image::codecs::png::PngEncoder;
    use image::{ExtendedColorType, ImageEncoder};

    fn png() -> Vec<u8> {
        let mut encoded = Vec::new();
        PngEncoder::new(&mut encoded)
            .write_image(
                &[
                    255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
                ],
                2,
                2,
                ExtendedColorType::Rgba8,
            )
            .unwrap();
        encoded
    }

    fn jpeg() -> Vec<u8> {
        let mut encoded = Vec::new();
        JpegEncoder::new_with_quality(&mut encoded, 90)
            .write_image(
                &[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255],
                2,
                2,
                ExtendedColorType::Rgb8,
            )
            .unwrap();
        encoded
    }

    #[test]
    fn accepts_fully_decoded_passive_png_and_jpeg() {
        let limits = ThumbnailLimits::default();
        assert_eq!(
            validate_thumbnail(&png(), "image/png", &limits),
            Ok(ValidatedThumbnail {
                format: ThumbnailFormat::Png,
                width: 2,
                height: 2,
            })
        );
        assert_eq!(
            validate_thumbnail(&jpeg(), "image/jpeg", &limits),
            Ok(ValidatedThumbnail {
                format: ThumbnailFormat::Jpeg,
                width: 2,
                height: 2,
            })
        );
    }

    #[test]
    fn rejects_mime_signature_disagreement_and_active_mime() {
        assert_eq!(
            validate_thumbnail(&png(), "image/jpeg", &ThumbnailLimits::default()),
            Err(ThumbnailError::MimeSignatureMismatch)
        );
        assert_eq!(
            validate_thumbnail(b"<svg/>", "image/svg+xml", &ThumbnailLimits::default()),
            Err(ThumbnailError::UnsupportedMimeType)
        );
    }

    #[test]
    fn rejects_truncated_malformed_and_trailing_data() {
        let limits = ThumbnailLimits::default();
        let mut truncated_png = png();
        truncated_png.pop();
        assert_eq!(
            validate_thumbnail(&truncated_png, "image/png", &limits),
            Err(ThumbnailError::MalformedImage)
        );

        let mut truncated_jpeg = jpeg();
        truncated_jpeg.pop();
        assert_eq!(
            validate_thumbnail(&truncated_jpeg, "image/jpeg", &limits),
            Err(ThumbnailError::MalformedImage)
        );

        let mut trailing_jpeg = jpeg();
        trailing_jpeg.push(0);
        assert_eq!(
            validate_thumbnail(&trailing_jpeg, "image/jpeg", &limits),
            Err(ThumbnailError::MalformedImage)
        );
        assert_eq!(
            validate_thumbnail(PNG_SIGNATURE, "image/png", &limits),
            Err(ThumbnailError::MalformedImage)
        );
    }

    #[test]
    fn rejects_structurally_complete_but_corrupt_pixel_data() {
        let mut encoded = png();
        let idat_type = encoded
            .windows(4)
            .position(|window| window == b"IDAT")
            .expect("encoder must emit IDAT");
        encoded[idat_type + 4] ^= 0xff;
        assert_eq!(
            validate_thumbnail(&encoded, "image/png", &ThumbnailLimits::default()),
            Err(ThumbnailError::DecodeFailed)
        );
    }

    #[test]
    fn rejects_dimensions_before_decoding_bomb_header() {
        let mut bomb = png();
        bomb[16..20].copy_from_slice(&100_000_u32.to_be_bytes());
        bomb[20..24].copy_from_slice(&100_000_u32.to_be_bytes());
        let error = validate_thumbnail(&bomb, "image/png", &ThumbnailLimits::default());
        assert_eq!(error, Err(ThumbnailError::DimensionsExceeded));
    }

    #[test]
    fn applies_pixel_encoded_and_decoded_byte_limits() {
        let encoded = png();
        let pixel_limits = ThumbnailLimits {
            max_pixels: 3,
            ..ThumbnailLimits::default()
        };
        assert_eq!(
            validate_thumbnail(&encoded, "image/png", &pixel_limits),
            Err(ThumbnailError::PixelCountExceeded)
        );

        let encoded_limits = ThumbnailLimits {
            max_encoded_bytes: 1,
            ..ThumbnailLimits::default()
        };
        assert_eq!(
            validate_thumbnail(&encoded, "image/png", &encoded_limits),
            Err(ThumbnailError::EncodedBytesExceeded)
        );

        let decoded_limits = ThumbnailLimits {
            max_decoded_bytes: 15,
            ..ThumbnailLimits::default()
        };
        assert_eq!(
            validate_thumbnail(&encoded, "image/png", &decoded_limits),
            Err(ThumbnailError::DecodedBytesExceeded)
        );
    }

    #[test]
    fn rejects_apng_control_chunk_without_decoding_frames() {
        let mut encoded = png();
        let iend = encoded.len() - 12;
        let mut animation_chunk = Vec::new();
        animation_chunk.extend_from_slice(&8_u32.to_be_bytes());
        animation_chunk.extend_from_slice(b"acTL");
        animation_chunk.extend_from_slice(&1_u32.to_be_bytes());
        animation_chunk.extend_from_slice(&0_u32.to_be_bytes());
        animation_chunk.extend_from_slice(&0_u32.to_be_bytes());
        encoded.splice(iend..iend, animation_chunk);
        assert_eq!(
            validate_thumbnail(&encoded, "image/png", &ThumbnailLimits::default()),
            Err(ThumbnailError::AnimationNotAllowed)
        );
    }

    #[test]
    fn errors_are_stable_and_do_not_include_input_data() {
        let input = b"secret thumbnail bytes";
        let error = validate_thumbnail(input, "image/png", &ThumbnailLimits::default())
            .expect_err("invalid signature must fail");
        assert_eq!(error, ThumbnailError::MimeSignatureMismatch);
        assert_eq!(
            error.to_string(),
            "thumbnail MIME type and file signature disagree"
        );
        assert!(!error.to_string().contains("secret"));
    }
}
