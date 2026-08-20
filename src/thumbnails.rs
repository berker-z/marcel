use std::{
    fs::File,
    io::{BufReader, BufWriter},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use anyhow::{Context as _, Result, bail};
use image::{DynamicImage, ImageDecoder, ImageReader, Limits, metadata::Orientation};
use md5::{Digest, Md5};
use url::Url;

use crate::local_fs::create_private_dir_all;

const THUMBNAIL_EDGE: u32 = 128;
const MAX_SOURCE_PIXELS: u64 = 25_000_000;
const MAX_SOURCE_DIMENSION: u32 = 25_000;
const MAX_SOURCE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DECODE_BYTES: u64 = 128 * 1024 * 1024;

pub fn supports(path: &Path) -> bool {
    mime_guess::from_path(path)
        .first()
        .is_some_and(|mime| mime.type_() == "image")
}

pub fn load_or_create(path: &Path) -> Result<PathBuf> {
    let cache_home = thumbnail_cache_home()?;
    load_or_create_in(path, &cache_home)
}

fn load_or_create_in(path: &Path, cache_home: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("could not resolve {}", path.display()))?;
    let metadata = canonical.metadata()?;
    if !metadata.is_file() {
        // Thumbnail candidates are chosen by file name alone, and opening a
        // FIFO named like an image would park a worker thread forever.
        bail!("not a regular file");
    }
    if metadata.len() > MAX_SOURCE_BYTES {
        bail!("image exceeds thumbnail source-size limit");
    }

    let uri = Url::from_file_path(&canonical)
        .map_err(|_| anyhow::anyhow!("could not build file URI"))?
        .to_string();
    let mtime = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string();
    let size = metadata.len().to_string();
    let cache_path = thumbnail_cache_path(cache_home, &uri);

    if cached_thumbnail_is_current(&cache_path, &uri, &mtime, &size) {
        return Ok(cache_path);
    }

    let mut limits = Limits::no_limits();
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);

    let mut reader = ImageReader::new(BufReader::new(crate::local_fs::open_regular_file(
        &canonical,
    )?));
    reader.limits(limits);
    let mut decoder = reader.with_guessed_format()?.into_decoder()?;
    let dimensions = decoder.dimensions();
    if u64::from(dimensions.0) * u64::from(dimensions.1) > MAX_SOURCE_PIXELS {
        bail!("image exceeds thumbnail pixel limit");
    }

    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let image = DynamicImage::from_decoder(decoder)?;

    // Adapted from Yazi's image pre-cache path: resize the expensive full
    // image before applying orientation to the much smaller result. Marcel's
    // square thumbnails do not need Yazi's orientation-aware target swap.
    // Source (MIT):
    // https://github.com/sxyazi/yazi/blob/e58022b9aafc8dabf586e2cc29b79a230071716f/yazi-adapter/src/image.rs
    let mut thumbnail = image.thumbnail(THUMBNAIL_EDGE, THUMBNAIL_EDGE);
    if orientation != Orientation::NoTransforms {
        thumbnail.apply_orientation(orientation);
    }
    let thumbnail = thumbnail.to_rgba8();

    let cache_dir = cache_path
        .parent()
        .context("thumbnail cache path has no parent")?;
    create_private_dir_all(cache_dir)?;
    let mut temporary = tempfile::NamedTempFile::new_in(cache_dir)?;
    {
        let writer = BufWriter::new(temporary.as_file_mut());
        let mut encoder = png::Encoder::new(writer, thumbnail.width(), thumbnail.height());
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        encoder.add_text_chunk("Thumb::URI".to_string(), uri)?;
        encoder.add_text_chunk("Thumb::MTime".to_string(), mtime)?;
        encoder.add_text_chunk("Thumb::Size".to_string(), size)?;
        encoder.add_text_chunk("Software".to_string(), "Marcel".to_string())?;
        let mut png = encoder.write_header()?;
        png.write_image_data(thumbnail.as_raw())?;
    }
    temporary
        .persist(&cache_path)
        .map_err(|error| error.error)?;
    Ok(cache_path)
}

fn thumbnail_cache_home() -> Result<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .context("neither XDG_CACHE_HOME nor HOME is available")
}

fn thumbnail_cache_path(cache_home: &Path, uri: &str) -> PathBuf {
    let digest = Md5::digest(uri.as_bytes());
    cache_home
        .join("thumbnails")
        .join("normal")
        .join(format!("{digest:x}.png"))
}

fn cached_thumbnail_is_current(path: &Path, uri: &str, mtime: &str, size: &str) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let Ok(reader) = png::Decoder::new(BufReader::new(file)).read_info() else {
        return false;
    };
    let info = reader.info();
    png_text_value(info, "Thumb::URI").as_deref() == Some(uri)
        && png_text_value(info, "Thumb::MTime").as_deref() == Some(mtime)
        && png_text_value(info, "Thumb::Size").is_none_or(|cached| cached == size)
}

fn png_text_value(info: &png::Info<'_>, key: &str) -> Option<String> {
    info.uncompressed_latin1_text
        .iter()
        .find(|chunk| chunk.keyword == key)
        .map(|chunk| chunk.text.clone())
        .or_else(|| {
            info.compressed_latin1_text
                .iter()
                .find(|chunk| chunk.keyword == key)
                .and_then(|chunk| chunk.get_text().ok())
        })
        .or_else(|| {
            info.utf8_text
                .iter()
                .find(|chunk| chunk.keyword == key)
                .and_then(|chunk| chunk.get_text().ok())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_image_mime_types_by_path() {
        assert!(supports(Path::new("photo.jpeg")));
        assert!(supports(Path::new("animation.gif")));
        assert!(!supports(Path::new("notes.md")));
    }

    #[test]
    fn creates_and_reuses_a_freedesktop_thumbnail() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.png");
        let cache = temp.path().join("cache");
        DynamicImage::new_rgba8(320, 180).save(&source).unwrap();

        let first = load_or_create_in(&source, &cache).unwrap();
        let second = load_or_create_in(&source, &cache).unwrap();
        let uri = Url::from_file_path(source.canonicalize().unwrap())
            .unwrap()
            .to_string();

        assert_eq!(first, second);
        assert!(first.is_file());
        assert!(cached_thumbnail_is_current(
            &first,
            &uri,
            &source
                .metadata()
                .unwrap()
                .modified()
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .to_string(),
            &source.metadata().unwrap().len().to_string(),
        ));
    }
}
