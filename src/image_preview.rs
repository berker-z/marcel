use std::{
    fs::File,
    io::{self, BufReader},
    path::Path,
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::{Context as _, Result, bail};
use gpui::RenderImage;
use image::{
    AnimationDecoder, DynamicImage, Frame, ImageDecoder, ImageFormat, ImageReader, Limits,
    codecs::{gif::GifDecoder, webp::WebPDecoder},
    metadata::Orientation,
};

const PREVIEW_EDGE: u32 = 2_048;
const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SOURCE_DIMENSION: u32 = 25_000;
const MAX_SOURCE_PIXELS: u64 = 40_000_000;
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ANIMATION_FRAMES: usize = 240;
const MAX_ANIMATION_OUTPUT_PIXELS: u64 = 64_000_000;

pub fn prepare(path: &Path, cancelled: &AtomicBool) -> Result<Arc<RenderImage>> {
    check_cancelled(cancelled)?;
    let metadata = path
        .metadata()
        .with_context(|| format!("could not inspect {}", path.display()))?;
    if metadata.len() > MAX_SOURCE_BYTES {
        bail!("image exceeds the 64 MiB preview source limit");
    }

    let format = ImageReader::open(path)?.with_guessed_format()?.format();
    let format = format.context("image format could not be identified")?;
    let frames =
        if matches!(format, ImageFormat::Gif | ImageFormat::WebP) && is_animated(path, format)? {
            decode_bounded_animation(path, format, cancelled)?
        } else {
            vec![decode_bounded_still(path, cancelled)?]
        };
    check_cancelled(cancelled)?;
    Ok(Arc::new(RenderImage::new(frames)))
}

fn decode_bounded_still(path: &Path, cancelled: &AtomicBool) -> Result<Frame> {
    let mut limits = Limits::no_limits();
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);
    let mut reader = ImageReader::open(path)?;
    reader.limits(limits);
    let mut decoder = reader.with_guessed_format()?.into_decoder()?;
    validate_dimensions(decoder.dimensions())?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    check_cancelled(cancelled)?;
    let image = DynamicImage::from_decoder(decoder)?;
    check_cancelled(cancelled)?;
    let mut image = bound_preview_dimensions(image);
    if orientation != Orientation::NoTransforms {
        image.apply_orientation(orientation);
    }
    let mut image = image.to_rgba8();
    rgba_to_bgra(&mut image);
    Ok(Frame::new(image))
}

fn decode_bounded_animation(
    path: &Path,
    format: ImageFormat,
    cancelled: &AtomicBool,
) -> Result<Vec<Frame>> {
    let reader = BufReader::new(File::open(path)?);
    let frames = match format {
        ImageFormat::Gif => {
            let mut decoder = GifDecoder::new(reader)?;
            let dimensions = decoder.dimensions();
            validate_dimensions(dimensions)?;
            decoder.set_limits(decode_limits())?;
            decoder.into_frames()
        }
        ImageFormat::WebP => {
            let decoder = WebPDecoder::new(reader)?;
            validate_dimensions(decoder.dimensions())?;
            decoder.into_frames()
        }
        _ => bail!("unsupported animated image format"),
    };

    let mut output = Vec::new();
    let mut output_pixels = 0u64;
    for frame in frames {
        check_cancelled(cancelled)?;
        if output.len() >= MAX_ANIMATION_FRAMES {
            bail!("animation exceeds the {MAX_ANIMATION_FRAMES}-frame preview limit");
        }
        let frame = frame?;
        let delay = frame.delay();
        let mut image =
            bound_preview_dimensions(DynamicImage::ImageRgba8(frame.into_buffer())).to_rgba8();
        output_pixels = output_pixels
            .checked_add(u64::from(image.width()) * u64::from(image.height()))
            .context("animation preview size overflowed")?;
        if output_pixels > MAX_ANIMATION_OUTPUT_PIXELS {
            bail!("animation exceeds the decoded preview memory limit");
        }
        rgba_to_bgra(&mut image);
        output.push(Frame::from_parts(image, 0, 0, delay));
    }
    if output.is_empty() {
        bail!("animation contains no frames");
    }
    Ok(output)
}

fn rgba_to_bgra(image: &mut image::RgbaImage) {
    for pixel in image.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}

fn bound_preview_dimensions(image: DynamicImage) -> DynamicImage {
    if image.width() > PREVIEW_EDGE || image.height() > PREVIEW_EDGE {
        image.thumbnail(PREVIEW_EDGE, PREVIEW_EDGE)
    } else {
        image
    }
}

fn is_animated(path: &Path, format: ImageFormat) -> Result<bool> {
    let reader = BufReader::new(File::open(path)?);
    match format {
        ImageFormat::Gif => Ok(true),
        ImageFormat::WebP => Ok(WebPDecoder::new(reader)?.has_animation()),
        _ => Ok(false),
    }
}

fn decode_limits() -> Limits {
    let mut limits = Limits::no_limits();
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);
    limits
}

fn validate_dimensions((width, height): (u32, u32)) -> Result<()> {
    if width == 0 || height == 0 {
        bail!("image has zero dimensions");
    }
    if width > MAX_SOURCE_DIMENSION || height > MAX_SOURCE_DIMENSION {
        bail!("image exceeds the preview dimension limit");
    }
    if u64::from(width) * u64::from(height) > MAX_SOURCE_PIXELS {
        bail!("image exceeds the preview pixel limit");
    }
    Ok(())
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        Err(io::Error::new(io::ErrorKind::Interrupted, "image preview was cancelled").into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Delay, RgbaImage, codecs::gif::GifEncoder};
    use std::io::BufWriter;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn rejects_oversized_source_files_before_decode() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("huge.png");
        File::create(&source)
            .unwrap()
            .set_len(MAX_SOURCE_BYTES + 1)
            .unwrap();

        assert!(prepare(&source, &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn bounds_large_still_output_dimensions() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("wide.png");
        RgbaImage::new(PREVIEW_EDGE + 512, 2).save(&source).unwrap();

        let output = prepare(&source, &AtomicBool::new(false)).unwrap();
        let dimensions = output.size(0);

        assert!(dimensions.width.0 <= PREVIEW_EDGE as i32);
        assert!(dimensions.height.0 <= PREVIEW_EDGE as i32);
    }

    #[test]
    fn prepares_gpui_bgra_pixels_without_a_second_decode() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("pixel.png");
        RgbaImage::from_pixel(1, 1, image::Rgba([10, 20, 30, 40]))
            .save(&source)
            .unwrap();

        let output = prepare(&source, &AtomicBool::new(false)).unwrap();

        assert_eq!(output.size(0).width.0, 1);
        assert_eq!(output.size(0).height.0, 1);
        assert_eq!(output.as_bytes(0), Some([30, 20, 10, 40].as_slice()));
    }

    #[test]
    fn rejects_animations_over_the_frame_limit() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("many.gif");
        let mut encoder = GifEncoder::new(BufWriter::new(File::create(&source).unwrap()));
        for _ in 0..=MAX_ANIMATION_FRAMES {
            encoder
                .encode_frame(Frame::from_parts(
                    RgbaImage::new(1, 1),
                    0,
                    0,
                    Delay::from_numer_denom_ms(10, 1),
                ))
                .unwrap();
        }
        drop(encoder);

        assert!(prepare(&source, &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn rejects_zero_and_oversized_dimensions() {
        assert!(validate_dimensions((0, 10)).is_err());
        assert!(validate_dimensions((10, 0)).is_err());
        assert!(validate_dimensions((MAX_SOURCE_DIMENSION + 1, 1)).is_err());
        assert!(validate_dimensions((10_000, 10_000)).is_err());
    }
}
