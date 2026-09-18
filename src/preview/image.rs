use std::{
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

use crate::fsops::local::open_regular_file;

const PREVIEW_EDGE: u32 = 2_048;
const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SOURCE_DIMENSION: u32 = 25_000;
const MAX_SOURCE_PIXELS: u64 = 40_000_000;
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;
/// An animation is kept whole up to these, and cut there: the frames decoded
/// so far play as a loop. Every frame is retained for as long as the preview
/// shows, so the byte bound is what matters — 240 frames of a bounded
/// 2048×2048 GIF would be 4 GiB — and the frame bound keeps a tiny endless
/// GIF from being decoded for seconds before anything appears.
const MAX_ANIMATION_FRAMES: usize = 240;
const MAX_ANIMATION_BYTES: u64 = 64 * 1024 * 1024;

pub fn prepare(path: &Path, cancelled: &AtomicBool) -> Result<Arc<RenderImage>> {
    check_cancelled(cancelled)?;
    let metadata =
        path.metadata().with_context(|| format!("could not inspect {}", path.display()))?;
    if metadata.len() > MAX_SOURCE_BYTES {
        bail!("image exceeds the 64 MiB preview source limit");
    }

    // Every open below goes through `open_regular_file`: a FIFO named like an
    // image would otherwise block the preview worker forever.
    let format =
        ImageReader::new(BufReader::new(open_regular_file(path)?)).with_guessed_format()?.format();
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

/// A still image from bytes already in memory: an embedded cover.
pub fn prepare_bytes(bytes: &[u8]) -> Result<Arc<RenderImage>> {
    if bytes.len() as u64 > MAX_SOURCE_BYTES {
        bail!("image exceeds the 64 MiB preview source limit");
    }
    let mut limits = Limits::no_limits();
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);
    let mut reader = ImageReader::new(io::Cursor::new(bytes));
    reader.limits(limits);
    let decoder = reader.with_guessed_format()?.into_decoder()?;
    validate_dimensions(decoder.dimensions())?;
    let image = bound_preview_dimensions(DynamicImage::from_decoder(decoder)?);
    let mut image = image.to_rgba8();
    rgba_to_bgra(&mut image);
    Ok(Arc::new(RenderImage::new(vec![Frame::new(image)])))
}

fn decode_bounded_still(path: &Path, cancelled: &AtomicBool) -> Result<Frame> {
    let mut limits = Limits::no_limits();
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);
    let mut reader = ImageReader::new(BufReader::new(open_regular_file(path)?));
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
    let reader = BufReader::new(open_regular_file(path)?);
    let frames = match format {
        ImageFormat::Gif => {
            let mut decoder = GifDecoder::new(reader)?;
            let dimensions = decoder.dimensions();
            validate_dimensions(dimensions)?;
            decoder.set_limits(decode_limits())?;
            decoder.into_frames()
        }
        ImageFormat::WebP => {
            let mut decoder = WebPDecoder::new(reader)?;
            validate_dimensions(decoder.dimensions())?;
            decoder.set_limits(decode_limits())?;
            decoder.into_frames()
        }
        _ => bail!("unsupported animated image format"),
    };

    let mut output = Vec::new();
    let mut budget = AnimationBudget::default();
    for frame in frames {
        check_cancelled(cancelled)?;
        let frame = frame?;
        let delay = frame.delay();
        let mut image =
            bound_preview_dimensions(DynamicImage::ImageRgba8(frame.into_buffer())).to_rgba8();
        if !budget.admit(image.width(), image.height()) {
            break;
        }
        rgba_to_bgra(&mut image);
        output.push(Frame::from_parts(image, 0, 0, delay));
    }
    if output.is_empty() {
        bail!("animation contains no frames");
    }
    Ok(output)
}

/// What an animated preview may still take: frames and decoded bytes.
struct AnimationBudget {
    frames: usize,
    bytes: u64,
}

impl Default for AnimationBudget {
    fn default() -> Self {
        Self { frames: MAX_ANIMATION_FRAMES, bytes: MAX_ANIMATION_BYTES }
    }
}

impl AnimationBudget {
    /// Whether a `width`×`height` RGBA frame still fits, charging it if so.
    /// A refusal ends the animation where it is; the frames before it are
    /// kept.
    fn admit(&mut self, width: u32, height: u32) -> bool {
        let bytes = u64::from(width) * u64::from(height) * 4;
        if self.frames == 0 || bytes > self.bytes {
            return false;
        }
        self.frames -= 1;
        self.bytes -= bytes;
        true
    }
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
    let reader = BufReader::new(open_regular_file(path)?);
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
    use std::fs::File;
    use std::io::BufWriter;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn rejects_oversized_source_files_before_decode() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("huge.png");
        File::create(&source).unwrap().set_len(MAX_SOURCE_BYTES + 1).unwrap();

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
        RgbaImage::from_pixel(1, 1, image::Rgba([10, 20, 30, 40])).save(&source).unwrap();

        let output = prepare(&source, &AtomicBool::new(false)).unwrap();

        assert_eq!(output.size(0).width.0, 1);
        assert_eq!(output.size(0).height.0, 1);
        assert_eq!(output.as_bytes(0), Some([30, 20, 10, 40].as_slice()));
    }

    /// A long animation is cut at the limit and plays what was decoded,
    /// rather than failing the preview outright.
    #[test]
    fn animations_over_the_frame_limit_keep_the_first_frames() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("many.gif");
        let mut encoder = GifEncoder::new(BufWriter::new(File::create(&source).unwrap()));
        for _ in 0..MAX_ANIMATION_FRAMES + 5 {
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

        let output = prepare(&source, &AtomicBool::new(false)).unwrap();
        assert_eq!(output.frame_count(), MAX_ANIMATION_FRAMES);
    }

    /// The byte budget is the binding one for anything but a tiny GIF: it
    /// admits whole frames until the next would not fit, whatever the count.
    #[test]
    fn the_animation_budget_stops_at_the_first_frame_that_does_not_fit() {
        let mut budget = AnimationBudget { frames: 10, bytes: 100 };
        // 4×2 RGBA is 32 bytes: three fit, the fourth would need 128.
        assert!(budget.admit(4, 2));
        assert!(budget.admit(4, 2));
        assert!(budget.admit(4, 2));
        assert!(!budget.admit(4, 2));
        assert_eq!(budget.bytes, 4);
        assert_eq!(budget.frames, 7);
        // Smaller frames still fit in what is left.
        assert!(budget.admit(1, 1));
        assert!(!budget.admit(1, 1));
    }

    #[test]
    fn the_animation_budget_counts_frames_independently_of_bytes() {
        let mut budget = AnimationBudget { frames: 2, bytes: u64::MAX };
        assert!(budget.admit(1, 1));
        assert!(budget.admit(1, 1));
        assert!(!budget.admit(1, 1));
    }

    /// The default budget: a bounded 2048×2048 frame is 16 MiB, so four of
    /// them fit and a fifth does not, well short of the frame count.
    #[test]
    fn the_default_budget_holds_four_full_size_frames() {
        let mut budget = AnimationBudget::default();
        for _ in 0..4 {
            assert!(budget.admit(PREVIEW_EDGE, PREVIEW_EDGE));
        }
        assert!(!budget.admit(PREVIEW_EDGE, PREVIEW_EDGE));
    }

    #[test]
    fn rejects_zero_and_oversized_dimensions() {
        assert!(validate_dimensions((0, 10)).is_err());
        assert!(validate_dimensions((10, 0)).is_err());
        assert!(validate_dimensions((MAX_SOURCE_DIMENSION + 1, 1)).is_err());
        assert!(validate_dimensions((10_000, 10_000)).is_err());
    }
}
