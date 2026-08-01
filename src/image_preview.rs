use std::{
    fs::{self, File},
    io::{self, BufReader, BufWriter},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::UNIX_EPOCH,
};

use anyhow::{Context as _, Result, bail};
use image::{
    AnimationDecoder, DynamicImage, Frame, ImageDecoder, ImageFormat, ImageReader, Limits,
    codecs::{
        gif::{GifDecoder, GifEncoder, Repeat},
        webp::WebPDecoder,
    },
    metadata::Orientation,
};
use md5::{Digest, Md5};

const PREVIEW_EDGE: u32 = 2_048;
const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SOURCE_DIMENSION: u32 = 25_000;
const MAX_SOURCE_PIXELS: u64 = 40_000_000;
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ANIMATION_FRAMES: usize = 240;
const MAX_ANIMATION_OUTPUT_PIXELS: u64 = 64_000_000;
const MAX_CACHE_FILES: usize = 128;

pub fn prepare(path: &Path, cancelled: &AtomicBool) -> Result<PathBuf> {
    prepare_in(path, &preview_cache_dir(), cancelled)
}

fn prepare_in(path: &Path, cache_dir: &Path, cancelled: &AtomicBool) -> Result<PathBuf> {
    check_cancelled(cancelled)?;
    let metadata = path
        .metadata()
        .with_context(|| format!("could not inspect {}", path.display()))?;
    if metadata.len() > MAX_SOURCE_BYTES {
        bail!("image exceeds the 64 MiB preview source limit");
    }

    let format = ImageReader::open(path)?.with_guessed_format()?.format();
    let format = format.context("image format could not be identified")?;
    let extension =
        if matches!(format, ImageFormat::Gif | ImageFormat::WebP) && is_animated(path, format)? {
            "gif"
        } else {
            "png"
        };
    fs::create_dir_all(cache_dir)?;
    let destination = cache_dir.join(format!("{}.{}", cache_key(path, &metadata), extension));
    if destination
        .metadata()
        .is_ok_and(|metadata| metadata.len() > 0)
    {
        return Ok(destination);
    }

    let temporary = tempfile::Builder::new()
        .prefix("preview-")
        .suffix(&format!(".{extension}"))
        .tempfile_in(cache_dir)?;
    if extension == "gif" {
        encode_bounded_animation(path, format, temporary.as_file(), cancelled)?;
    } else {
        encode_bounded_still(path, temporary.as_file(), cancelled)?;
    }
    check_cancelled(cancelled)?;
    match temporary.persist_noclobber(&destination) {
        Ok(_) => {}
        Err(error) if destination.is_file() => drop(error.file),
        Err(error) => return Err(error.error.into()),
    }
    prune_cache(cache_dir);
    Ok(destination)
}

fn encode_bounded_still(path: &Path, output: &File, cancelled: &AtomicBool) -> Result<()> {
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
    let mut image = image.thumbnail(PREVIEW_EDGE, PREVIEW_EDGE);
    if orientation != Orientation::NoTransforms {
        image.apply_orientation(orientation);
    }
    image.write_to(&mut BufWriter::new(output.try_clone()?), ImageFormat::Png)?;
    Ok(())
}

fn encode_bounded_animation(
    path: &Path,
    format: ImageFormat,
    output: &File,
    cancelled: &AtomicBool,
) -> Result<()> {
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

    let mut encoder = GifEncoder::new_with_speed(BufWriter::new(output.try_clone()?), 20);
    encoder.set_repeat(Repeat::Infinite)?;
    let mut frame_count = 0usize;
    let mut output_pixels = 0u64;
    for frame in frames {
        check_cancelled(cancelled)?;
        frame_count += 1;
        if frame_count > MAX_ANIMATION_FRAMES {
            bail!("animation exceeds the {MAX_ANIMATION_FRAMES}-frame preview limit");
        }
        let frame = frame?;
        let delay = frame.delay();
        let image = DynamicImage::ImageRgba8(frame.into_buffer())
            .thumbnail(PREVIEW_EDGE, PREVIEW_EDGE)
            .to_rgba8();
        output_pixels = output_pixels
            .checked_add(u64::from(image.width()) * u64::from(image.height()))
            .context("animation preview size overflowed")?;
        if output_pixels > MAX_ANIMATION_OUTPUT_PIXELS {
            bail!("animation exceeds the decoded preview memory limit");
        }
        encoder.encode_frame(Frame::from_parts(image, 0, 0, delay))?;
    }
    if frame_count == 0 {
        bail!("animation contains no frames");
    }
    Ok(())
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

fn cache_key(path: &Path, metadata: &fs::Metadata) -> String {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let mut hash = Md5::new();
    hash.update(b"marcel-image-preview-v1");
    hash.update(path.as_os_str().as_encoded_bytes());
    hash.update(metadata.len().to_le_bytes());
    hash.update(modified.to_le_bytes());
    hash.update(PREVIEW_EDGE.to_le_bytes());
    format!("{:x}", hash.finalize())
}

fn preview_cache_dir() -> PathBuf {
    absolute_env_path("XDG_CACHE_HOME")
        .or_else(|| absolute_env_path("HOME").map(|home| home.join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("marcel")
        .join("image-preview-v1")
}

fn absolute_env_path(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os(name)?);
    path.is_absolute().then_some(path)
}

fn prune_cache(cache_dir: &Path) {
    let Ok(entries) = fs::read_dir(cache_dir) else {
        return;
    };
    let mut files = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            metadata
                .is_file()
                .then(|| (metadata.modified().unwrap_or(UNIX_EPOCH), entry.path()))
        })
        .collect::<Vec<_>>();
    if files.len() <= MAX_CACHE_FILES {
        return;
    }
    files.sort_unstable_by_key(|(modified, _)| *modified);
    let remove_count = files.len() - MAX_CACHE_FILES;
    for (_, path) in files.into_iter().take(remove_count) {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Delay, RgbaImage};
    use std::sync::atomic::AtomicBool;

    #[test]
    fn rejects_oversized_source_files_before_decode() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("huge.png");
        File::create(&source)
            .unwrap()
            .set_len(MAX_SOURCE_BYTES + 1)
            .unwrap();

        assert!(prepare_in(&source, &root.path().join("cache"), &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn bounds_large_still_output_dimensions() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("wide.png");
        RgbaImage::new(PREVIEW_EDGE + 512, 2).save(&source).unwrap();

        let output =
            prepare_in(&source, &root.path().join("cache"), &AtomicBool::new(false)).unwrap();
        let dimensions = image::image_dimensions(output).unwrap();

        assert!(dimensions.0 <= PREVIEW_EDGE);
        assert!(dimensions.1 <= PREVIEW_EDGE);
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

        assert!(prepare_in(&source, &root.path().join("cache"), &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn rejects_zero_and_oversized_dimensions() {
        assert!(validate_dimensions((0, 10)).is_err());
        assert!(validate_dimensions((10, 0)).is_err());
        assert!(validate_dimensions((MAX_SOURCE_DIMENSION + 1, 1)).is_err());
        assert!(validate_dimensions((10_000, 10_000)).is_err());
    }
}
