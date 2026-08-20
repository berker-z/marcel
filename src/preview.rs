use std::{
    io::{self, Read, Seek as _, SeekFrom},
    path::{Path, PathBuf},
    sync::Arc,
};

use gpui::RenderImage;

use crate::fs::{FileEntry, format_size};
use crate::image_preview;
use crate::local_fs::open_regular_file;
use crate::pdf_preview::inspect_pdf;

const MAX_TEXT_BYTES: u64 = 256 * 1024;
const MAX_RICH_TEXT_BYTES: usize = 32 * 1024;
const MAX_PREVIEW_LINE_CHARS: usize = 2 * 1024;

#[derive(Clone, Debug)]
pub enum Preview {
    Directory {
        path: PathBuf,
    },
    Image {
        image: Arc<RenderImage>,
        mime: String,
    },
    Pdf {
        source: PathBuf,
        pages: usize,
    },
    Text {
        contents: String,
        lines: Arc<[String]>,
        language: String,
        markdown: bool,
        render_rich: bool,
        truncated: bool,
        clipped_lines: bool,
    },
    Metadata {
        summary: String,
    },
}

#[derive(Clone, Debug)]
pub enum PreviewState {
    Empty,
    Loading { name: String },
    Ready(Preview),
    Error(String),
}

pub fn load_preview(
    entry: &FileEntry,
    cancelled: &Arc<std::sync::atomic::AtomicBool>,
) -> io::Result<Preview> {
    if entry.navigable {
        return Ok(Preview::Directory {
            path: entry.path.clone(),
        });
    }

    // Everything below previews regular files only, and `open(2)` on a FIFO
    // with no writer blocks forever — a blocked open cannot be interrupted by
    // the cancellation flag, so the worker thread would be lost for the life
    // of the process. Refuse every other object on the opened descriptor.
    let mut file = match open_regular_file(&entry.path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
            return Ok(Preview::Metadata {
                summary: format!(
                    "{}\n{}\nNo preview is available for this kind of file",
                    entry.display_kind(),
                    format_size(entry.size)
                ),
            });
        }
        Err(error) => return Err(error),
    };
    let mut head = [0_u8; 8192];
    let head_read = read_up_to(&mut file, &mut head)?;
    let inferred = infer::get(&head[..head_read]).map(|kind| kind.mime_type().to_string());

    if inferred
        .as_deref()
        .is_some_and(|mime| mime.starts_with("image/"))
        || is_image_extension(&entry.path)
    {
        let image = image_preview::prepare(&entry.path, cancelled).map_err(io::Error::other)?;
        return Ok(Preview::Image {
            image,
            mime: inferred.unwrap_or_else(|| "image".to_string()),
        });
    }

    if inferred.as_deref() == Some("application/pdf") || has_extension(&entry.path, &["pdf"]) {
        let document = inspect_pdf(&entry.path, cancelled)?;
        return Ok(Preview::Pdf {
            source: document.source,
            pages: document.pages,
        });
    }

    let markdown = has_extension(&entry.path, &["md", "markdown", "mdown", "mkd"]);
    let language = language_for_path(&entry.path);
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_TEXT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() as u64 > MAX_TEXT_BYTES;
    bytes.truncate(MAX_TEXT_BYTES as usize);

    if is_probably_text(&bytes) {
        let contents = String::from_utf8_lossy(&bytes).into_owned();
        let (lines, clipped_lines) = split_preview_lines(&contents);
        return Ok(Preview::Text {
            render_rich: should_render_rich(contents.len()),
            contents,
            lines,
            language: language.to_string(),
            markdown,
            truncated,
            clipped_lines,
        });
    }

    Ok(Preview::Metadata {
        summary: format!(
            "{}\n{}\n{}",
            entry.display_kind(),
            format_size(entry.size),
            inferred.unwrap_or_else(|| "Unknown format".to_string())
        ),
    })
}

/// Read as much of `buffer` as the file can fill, tolerating short reads.
fn read_up_to(file: &mut std::fs::File, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

fn should_render_rich(byte_len: usize) -> bool {
    byte_len <= MAX_RICH_TEXT_BYTES
}

fn split_preview_lines(contents: &str) -> (Arc<[String]>, bool) {
    let mut clipped_any = false;
    let mut lines = contents
        .split('\n')
        .map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let mut chars = line.chars();
            let mut visible = chars
                .by_ref()
                .take(MAX_PREVIEW_LINE_CHARS)
                .collect::<String>();
            if chars.next().is_some() {
                clipped_any = true;
                visible.push_str(" … [line truncated]");
            }
            visible
        })
        .collect::<Vec<_>>();

    if lines.is_empty() {
        lines.push(String::new());
    }

    (lines.into(), clipped_any)
}

fn is_probably_text(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    if bytes.contains(&0) {
        return false;
    }

    let suspicious = bytes
        .iter()
        .filter(|byte| **byte < 0x09 || (**byte > 0x0d && **byte < 0x20))
        .count();
    suspicious * 100 / bytes.len() < 5
}

fn is_image_extension(path: &Path) -> bool {
    has_extension(
        path,
        &[
            "avif", "bmp", "gif", "ico", "jpeg", "jpg", "png", "tif", "tiff", "webp",
        ],
    )
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extensions
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

fn language_for_path(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("rs") => "rust",
        Some("nix") => "nix",
        Some("js" | "mjs" | "cjs") => "javascript",
        Some("ts" | "tsx") => "typescript",
        Some("py") => "python",
        Some("sh" | "bash" | "fish") => "shell",
        Some("toml") => "toml",
        Some("json") => "json",
        Some("yaml" | "yml") => "yaml",
        Some("html") => "html",
        Some("css") => "css",
        _ => "text",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_binary_content() {
        assert!(is_probably_text(b"hello\nworld"));
        assert!(!is_probably_text(b"hello\0world"));
    }

    #[test]
    fn detects_image_extensions_case_insensitively() {
        assert!(is_image_extension(Path::new("photo.JPEG")));
        assert!(is_image_extension(Path::new("animation.gif")));
        assert!(!is_image_extension(Path::new("notes.md")));
    }

    #[test]
    fn maps_common_languages() {
        assert_eq!(language_for_path(Path::new("main.rs")), "rust");
        assert_eq!(language_for_path(Path::new("flake.nix")), "nix");
        assert_eq!(language_for_path(Path::new("unknown.data")), "text");
    }

    #[test]
    fn clips_pathological_lines_before_layout() {
        let source = "a".repeat(MAX_PREVIEW_LINE_CHARS + 1);
        let (lines, clipped) = split_preview_lines(&source);

        assert!(clipped);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].ends_with(" … [line truncated]"));
    }

    #[test]
    fn detects_pdf_extension_case_insensitively() {
        assert!(has_extension(Path::new("report.PDF"), &["pdf"]));
    }

    #[test]
    fn large_text_avoids_synchronous_rich_rendering() {
        assert!(should_render_rich(MAX_RICH_TEXT_BYTES));
        assert!(!should_render_rich(MAX_RICH_TEXT_BYTES + 1));
    }
}
