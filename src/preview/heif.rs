//! HEIC and AVIF, through libheif.
//!
//! The `image` crate has no decoder for either and is not getting one, so
//! libheif registers itself as a decoding hook: an `ImageReader` that sniffs
//! a HEIF brand hands the file to libheif, and every other path through
//! `image` works unchanged. The one thing done here directly is the
//! embedded thumbnail, which the hook has no way to ask for.
//!
//! libheif applies the container's own rotation and mirroring when it
//! decodes, and the hook reports no EXIF orientation. Nothing downstream
//! rotates a HEIF image a second time.

use std::{fs::File, io::BufReader, path::Path, sync::OnceLock};

use anyhow::{Context as _, Result};
use image::RgbaImage;
use libheif_rs::{ColorSpace, HeifContext, ImageHandle, LibHeif, RgbChroma, StreamReader};

use crate::fsops::local::open_regular_file;

/// The extensions libheif is asked about. HEVC stills from a phone are
/// `.heic`, AV1 stills `.avif`; `.heif` and `.hif` are the generic container
/// and a camera maker's spelling of it.
pub const EXTENSIONS: &[&str] = &["avif", "heic", "heif", "hif"];

/// A thumbnail stored in the file is used only when its aspect ratio is
/// within this share of the picture's. A crop or an unrotated thumbnail
/// would otherwise stand in for the photo it does not match.
const ASPECT_TOLERANCE: f64 = 0.02;

/// Nothing past this is treated as a thumbnail, whatever the file calls it.
const MAX_EMBEDDED_EDGE: u32 = 1_024;

/// Makes HEIF decodable through `image`. Cheap after the first call.
pub fn register() {
    library();
}

/// The process's libheif, initialised once and never released.
///
/// The hook initialises libheif around each decode and releases it
/// afterwards, and the last release unloads its plugins, dav1d among them.
/// The reference held here keeps that from happening once per photo.
fn library() -> &'static LibHeif {
    static LIBRARY: OnceLock<LibHeif> = OnceLock::new();
    LIBRARY.get_or_init(|| {
        let library = LibHeif::new();
        libheif_rs::integration::image::register_all_decoding_hooks();
        library
    })
}

pub fn has_extension(path: &Path) -> bool {
    super::has_extension(path, EXTENSIONS)
}

/// The smallest thumbnail stored in the file whose longer edge reaches
/// `edge`, decoded. `None` when there is none that fits.
///
/// An iPhone photo carries one of about 320×240 next to its 12 or 24 MP
/// picture. Decoding it instead of the photo is the difference between a
/// few milliseconds and most of a second per file, and a folder of camera
/// uploads is a folder of nothing else.
pub fn embedded_thumbnail(path: &Path, edge: u32) -> Result<Option<RgbaImage>> {
    let file = open_regular_file(path)?;
    let context = read_context(file)?;
    let primary = context.primary_image_handle()?;

    let mut ids = vec![0; primary.number_of_thumbnails()];
    let count = primary.thumbnail_ids(&mut ids);
    let thumbnail = ids[..count]
        .iter()
        .filter_map(|id| primary.thumbnail(*id).ok())
        .filter(|thumbnail| {
            let longer = thumbnail.width().max(thumbnail.height());
            (edge..=MAX_EMBEDDED_EDGE).contains(&longer) && same_shape(&primary, thumbnail)
        })
        .min_by_key(|thumbnail| u64::from(thumbnail.width()) * u64::from(thumbnail.height()));
    thumbnail.map(|thumbnail| decode_rgba(&thumbnail)).transpose()
}

fn read_context(file: File) -> Result<HeifContext<'static>> {
    let size = file.metadata()?.len();
    let reader = StreamReader::new(BufReader::new(file), size);
    Ok(HeifContext::read_from_reader(Box::new(reader))?)
}

/// Whether `thumbnail` shows the picture `primary` does, going by shape.
/// Both sizes are after the container's transformations.
fn same_shape(primary: &ImageHandle, thumbnail: &ImageHandle) -> bool {
    if primary.height() == 0 || thumbnail.height() == 0 {
        return false;
    }
    let primary = f64::from(primary.width()) / f64::from(primary.height());
    let thumbnail = f64::from(thumbnail.width()) / f64::from(thumbnail.height());
    (primary - thumbnail).abs() <= primary * ASPECT_TOLERANCE
}

fn decode_rgba(handle: &ImageHandle) -> Result<RgbaImage> {
    let image = library().decode(handle, ColorSpace::Rgb(RgbChroma::Rgba), None)?;
    let planes = image.planes();
    let plane = planes.interleaved.context("libheif returned planar pixels for RGBA")?;
    let row = plane.width as usize * 4;
    let mut pixels = Vec::with_capacity(row * plane.height as usize);
    for line in plane.data.chunks(plane.stride).take(plane.height as usize) {
        pixels.extend_from_slice(line.get(..row).context("libheif row is shorter than its width")?);
    }
    RgbaImage::from_raw(plane.width, plane.height, pixels)
        .context("libheif returned fewer pixels than its dimensions")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::fixture;

    #[test]
    fn uses_the_thumbnail_the_file_carries() {
        let thumbnail = embedded_thumbnail(&fixture("photo.heic"), 128).unwrap().unwrap();
        assert_eq!(thumbnail.dimensions(), (160, 120));
    }

    #[test]
    fn a_thumbnail_smaller_than_asked_for_is_not_used() {
        assert!(embedded_thumbnail(&fixture("photo.heic"), 161).unwrap().is_none());
    }

    #[test]
    fn a_file_without_a_thumbnail_has_none() {
        assert!(embedded_thumbnail(&fixture("rotated.heic"), 128).unwrap().is_none());
        assert!(embedded_thumbnail(&fixture("photo.avif"), 16).unwrap().is_none());
    }

    #[test]
    fn recognises_the_extensions_regardless_of_case() {
        assert!(has_extension(Path::new("IMG_0001.HEIC")));
        assert!(has_extension(Path::new("photo.avif")));
        assert!(!has_extension(Path::new("photo.jpg")));
    }
}
