use std::{
    ffi::OsString,
    fs::File,
    io::{BufReader, BufWriter},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};
use image::{DynamicImage, ImageDecoder, ImageReader, Limits, metadata::Orientation};
use md5::{Digest, Md5};
use url::Url;

use crate::fsops::local::create_private_dir_all;

use super::media;

const THUMBNAIL_EDGE: u32 = 128;
/// The largest cached PNG accepted from another application. The spec's
/// `normal` directory holds 128×128 thumbnails and `large` 256×256; anything
/// bigger under `normal` is not a thumbnail, and it would be decoded in full
/// by the browser's `img` element with no limits of its own.
const MAX_CACHED_EDGE: u32 = 256;
const MAX_SOURCE_PIXELS: u64 = 25_000_000;
const MAX_SOURCE_DIMENSION: u32 = 25_000;
const MAX_SOURCE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DECODE_BYTES: u64 = 128 * 1024 * 1024;

/// What a thumbnail can be made for: any image, and a video when ffmpeg is
/// on `PATH` to take a frame from it. A video thumbnail another application
/// already made is used either way; see `load_or_create`.
pub fn supports(path: &Path) -> bool {
    is_image(path) || is_video(path)
}

fn is_image(path: &Path) -> bool {
    mime_guess::from_path(path).first().is_some_and(|mime| mime.type_() == "image")
}

pub fn is_video(path: &Path) -> bool {
    mime_guess::from_path(path).first().is_some_and(|mime| mime.type_() == "video")
}

/// The freedesktop thumbnail for `path`, made if there is none. `cancelled`
/// is the listing's flag: once it is set the file is no longer on screen,
/// and the decode — or the ffmpeg child — stops where it is.
///
/// `limit_bytes` is the user's `thumbnail_limit_mb`: an image past it is
/// left as an icon rather than read in full. `MAX_SOURCE_BYTES` still caps
/// it, so raising the setting cannot talk Marcel into a decode it will not
/// survive.
pub fn load_or_create(
    path: &Path,
    limit_bytes: u64,
    cancelled: &Arc<AtomicBool>,
) -> Result<PathBuf> {
    let cache_home = thumbnail_cache_home()?;
    load_or_create_in(path, limit_bytes, &cache_home, cancelled)
}

/// The thumbnail some application already cached for `path`, if it is still
/// current. Nothing is made, and nothing is read from `path`: the size and
/// modification time come from the listing already on screen, so this costs
/// one local `open` of the cache PNG and not a single byte over a network.
///
/// This is what a share gets when [`crate::config::SpeedTradeoff`] says not
/// to spend a download on making one. A folder Nautilus or an earlier local
/// visit has already thumbnailed still looks like itself; the rest fall back
/// to icons rather than to a progress bar.
pub fn load_cached(
    path: &Path,
    size: Option<u64>,
    modified: Option<SystemTime>,
) -> Result<PathBuf> {
    let cache_home = thumbnail_cache_home()?;
    load_cached_in(path, size, modified, &cache_home)
}

fn load_cached_in(
    path: &Path,
    size: Option<u64>,
    modified: Option<SystemTime>,
    cache_home: &Path,
) -> Result<PathBuf> {
    // Without both of these there is nothing to validate a cache entry
    // against, and serving an unvalidated one would show the thumbnail of a
    // file that has since been replaced.
    let (Some(size), Some(modified)) = (size, modified) else {
        bail!("listing has no size or modification time to validate a thumbnail against");
    };
    let uri = Url::from_file_path(path)
        .map_err(|_| anyhow::anyhow!("could not build file URI"))?
        .to_string();
    let mtime = modified.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs().to_string();
    let cache_path = thumbnail_cache_path(cache_home, &uri);
    if cached_thumbnail_is_current(&cache_path, &uri, &mtime, &size.to_string()) {
        return Ok(cache_path);
    }
    bail!("no current cached thumbnail")
}

fn load_or_create_in(
    path: &Path,
    limit_bytes: u64,
    cache_home: &Path,
    cancelled: &Arc<AtomicBool>,
) -> Result<PathBuf> {
    check_cancelled(cancelled)?;
    let canonical =
        path.canonicalize().with_context(|| format!("could not resolve {}", path.display()))?;
    let metadata = canonical.metadata()?;
    if !metadata.is_file() {
        // Thumbnail candidates are chosen by file name alone, and opening a
        // FIFO named like an image would park a worker thread forever.
        bail!("not a regular file");
    }
    // The size limit guards the image decode; a video is only ever decoded
    // by ffmpeg, one frame at a time, so its length does not matter here.
    if !is_video(&canonical) && metadata.len() > limit_bytes.min(MAX_SOURCE_BYTES) {
        bail!("image exceeds thumbnail source-size limit");
    }

    let uri = Url::from_file_path(&canonical)
        .map_err(|_| anyhow::anyhow!("could not build file URI"))?
        .to_string();
    let mtime =
        metadata.modified()?.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs().to_string();
    let size = metadata.len().to_string();
    let cache_path = thumbnail_cache_path(cache_home, &uri);

    if cached_thumbnail_is_current(&cache_path, &uri, &mtime, &size) {
        return Ok(cache_path);
    }

    // A video's frame comes from ffmpeg, and only then goes through the
    // resize below like any picture. Without ffmpeg the cache lookup above
    // is all a video gets: a thumbnail Nautilus or Dolphin left behind
    // shows, and one that was never made stays an icon.
    let frame;
    let decode_source: &Path = if is_video(&canonical) {
        if !media::available() {
            bail!("video thumbnails need ffmpeg on PATH");
        }
        frame = media::thumbnail_frame(&canonical, cancelled)?;
        &frame
    } else {
        &canonical
    };
    check_cancelled(cancelled)?;

    let mut limits = Limits::no_limits();
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);

    let mut reader =
        ImageReader::new(BufReader::new(crate::fsops::local::open_regular_file(decode_source)?));
    reader.limits(limits);
    let mut decoder = reader.with_guessed_format()?.into_decoder()?;
    let dimensions = decoder.dimensions();
    if u64::from(dimensions.0) * u64::from(dimensions.1) > MAX_SOURCE_PIXELS {
        bail!("image exceeds thumbnail pixel limit");
    }

    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let image = DynamicImage::from_decoder(decoder)?;
    check_cancelled(cancelled)?;

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

    let cache_dir = cache_path.parent().context("thumbnail cache path has no parent")?;
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
    temporary.persist(&cache_path).map_err(|error| error.error)?;
    Ok(cache_path)
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        bail!("thumbnail was cancelled");
    }
    Ok(())
}

fn thumbnail_cache_home() -> Result<PathBuf> {
    cache_home_from(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME"))
}

/// `$XDG_CACHE_HOME`, or `~/.cache` without it. The spec says a relative
/// `XDG_CACHE_HOME` is invalid and to be ignored; honouring one would scatter
/// thumbnail directories under whatever the working directory happens to be.
/// The same rule `tool::cache_dir` applies.
fn cache_home_from(xdg_cache_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    xdg_cache_home
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home.map(|home| PathBuf::from(home).join(".cache")))
        .context("neither XDG_CACHE_HOME nor HOME is available")
}

fn thumbnail_cache_path(cache_home: &Path, uri: &str) -> PathBuf {
    let digest = Md5::digest(uri.as_bytes());
    cache_home.join("thumbnails").join("normal").join(format!("{digest:x}.png"))
}

/// Whether the PNG at `path` is a thumbnail of the source as it is now. Only
/// the header is read here; the pixels are decoded later by the browser, so
/// a cached file whose dimensions say it is not a thumbnail is refused before
/// that happens and Marcel writes its own in its place.
fn cached_thumbnail_is_current(path: &Path, uri: &str, mtime: &str, size: &str) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let Ok(reader) = png::Decoder::new(BufReader::new(file)).read_info() else {
        return false;
    };
    let info = reader.info();
    info.width <= MAX_CACHED_EDGE
        && info.height <= MAX_CACHED_EDGE
        && png_text_value(info, "Thumb::URI").as_deref() == Some(uri)
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
    use crate::testing::{Sandbox, no_cancel};

    /// The source's identity as the cache records it.
    fn identity(source: &Path) -> (String, String, String) {
        let metadata = source.metadata().unwrap();
        (
            Url::from_file_path(source.canonicalize().unwrap()).unwrap().to_string(),
            metadata
                .modified()
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .to_string(),
            metadata.len().to_string(),
        )
    }

    /// A PNG at `path` with the freedesktop chunks for `source`, as another
    /// application would leave it.
    fn write_foreign_thumbnail(path: &Path, source: &Path, edge: u32) {
        let (uri, mtime, size) = identity(source);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut encoder =
            png::Encoder::new(BufWriter::new(File::create(path).unwrap()), edge, edge);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.add_text_chunk("Thumb::URI".to_string(), uri).unwrap();
        encoder.add_text_chunk("Thumb::MTime".to_string(), mtime).unwrap();
        encoder.add_text_chunk("Thumb::Size".to_string(), size).unwrap();
        let mut png = encoder.write_header().unwrap();
        png.write_image_data(&vec![0; (edge * edge * 4) as usize]).unwrap();
    }

    #[test]
    fn recognizes_image_mime_types_by_path() {
        assert!(supports(Path::new("photo.jpeg")));
        assert!(supports(Path::new("animation.gif")));
        assert!(!supports(Path::new("notes.md")));
        assert!(supports(Path::new("clip.mp4")));
        assert!(is_video(Path::new("clip.mkv")));
        assert!(!is_video(Path::new("photo.jpeg")));
    }

    #[test]
    fn creates_and_reuses_a_freedesktop_thumbnail() {
        let sandbox = Sandbox::new();
        let source = sandbox.path("source.png");
        let cache = sandbox.path("cache");
        DynamicImage::new_rgba8(320, 180).save(&source).unwrap();

        let first = load_or_create_in(&source, MAX_SOURCE_BYTES, &cache, &no_cancel()).unwrap();
        let second = load_or_create_in(&source, MAX_SOURCE_BYTES, &cache, &no_cancel()).unwrap();
        let (uri, mtime, size) = identity(&source);

        assert_eq!(first, second);
        assert!(first.is_file());
        assert!(cached_thumbnail_is_current(&first, &uri, &mtime, &size));
    }

    /// The flag is the listing's: once it is set the tile is gone and the
    /// work, ffmpeg included, has nobody to finish for.
    #[test]
    fn a_cancelled_flag_stops_before_anything_is_decoded_or_written() {
        let sandbox = Sandbox::new();
        let source = sandbox.path("source.png");
        let cache = sandbox.path("cache");
        DynamicImage::new_rgba8(320, 180).save(&source).unwrap();

        let cancelled = Arc::new(AtomicBool::new(true));
        let error = load_or_create_in(&source, MAX_SOURCE_BYTES, &cache, &cancelled).unwrap_err();

        assert!(error.to_string().contains("cancelled"), "{error}");
        assert!(!cache.exists(), "a cancelled thumbnail leaves no cache directory behind");
    }

    /// A relative `XDG_CACHE_HOME` is invalid by the spec, and honouring it
    /// would put a thumbnail tree under the working directory.
    #[test]
    fn a_relative_xdg_cache_home_falls_back_to_the_home_cache() {
        let home = Some(OsString::from("/home/someone"));
        assert_eq!(
            cache_home_from(Some(OsString::from("relative/cache")), home.clone()).unwrap(),
            PathBuf::from("/home/someone/.cache")
        );
        assert_eq!(
            cache_home_from(Some(OsString::from("/var/cache/someone")), home.clone()).unwrap(),
            PathBuf::from("/var/cache/someone")
        );
        assert_eq!(cache_home_from(None, home).unwrap(), PathBuf::from("/home/someone/.cache"));
        assert!(cache_home_from(Some(OsString::from("relative")), None).is_err());
    }

    /// Another application's cache entry is trusted for its text chunks but
    /// not for its size: a 4096×4096 "thumbnail" would be decoded whole by
    /// the browser, so it is regenerated at Marcel's size instead.
    #[test]
    fn an_oversized_cached_thumbnail_from_another_app_is_replaced() {
        let sandbox = Sandbox::new();
        let source = sandbox.path("source.png");
        let cache = sandbox.path("cache");
        DynamicImage::new_rgba8(320, 180).save(&source).unwrap();
        let (uri, mtime, size) = identity(&source);
        let cache_path = thumbnail_cache_path(&cache, &uri);

        write_foreign_thumbnail(&cache_path, &source, MAX_CACHED_EDGE);
        assert!(cached_thumbnail_is_current(&cache_path, &uri, &mtime, &size));

        write_foreign_thumbnail(&cache_path, &source, MAX_CACHED_EDGE + 1);
        assert!(!cached_thumbnail_is_current(&cache_path, &uri, &mtime, &size));

        let replaced = load_or_create_in(&source, MAX_SOURCE_BYTES, &cache, &no_cancel()).unwrap();
        let info =
            png::Decoder::new(BufReader::new(File::open(&replaced).unwrap())).read_info().unwrap();
        assert_eq!(replaced, cache_path);
        assert!(info.info().width <= THUMBNAIL_EDGE && info.info().height <= THUMBNAIL_EDGE);
    }
}
