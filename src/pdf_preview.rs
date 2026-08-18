use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

use md5::{Digest, Md5};

use crate::local_fs::create_private_dir_all;

const PDF_RENDER_LIMIT: u32 = 1_800;
const PDF_JPEG_QUALITY: u8 = 85;
const PDF_PROCESS_TIMEOUT: Duration = Duration::from_secs(20);
const PDF_PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(15);
const MAX_TOOL_OUTPUT_BYTES: u64 = 64 * 1024;
const MAX_CACHE_FILES: usize = 512;

#[derive(Clone, Debug)]
pub struct PdfPage {
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct PdfDocument {
    pub source: PathBuf,
    pub pages: usize,
}

pub fn inspect_pdf(source: &Path, cancelled: &Arc<AtomicBool>) -> io::Result<PdfDocument> {
    check_cancelled(cancelled)?;
    let cache_dir = pdf_cache_dir();
    create_private_dir_all(&cache_dir)?;
    let identity = file_identity(source)?;
    let pages = page_count(source, &cache_dir, &identity, cancelled)?;
    Ok(PdfDocument {
        source: source.to_path_buf(),
        pages,
    })
}

/// Rasterize one PDF page through Poppler without loading PDF machinery into
/// Marcel's process.
///
/// The requested-page cache and `pdftoppm` bridge follow Yazi's PDF preloader:
/// https://github.com/sxyazi/yazi/blob/e58022b9aafc8dabf586e2cc29b79a230071716f/yazi-plugin/preset/plugins/pdf.lua
///
/// Marcel adds a fixed raster bound, an execution timeout, cooperative process
/// cancellation, and a file-identity cache suited to a persistent GUI preview.
pub fn render_pdf_page(
    source: &Path,
    requested_page: usize,
    cancelled: &Arc<AtomicBool>,
) -> io::Result<PdfPage> {
    check_cancelled(cancelled)?;

    let cache_dir = pdf_cache_dir();
    create_private_dir_all(&cache_dir)?;
    let identity = file_identity(source)?;
    let pages = page_count(source, &cache_dir, &identity, cancelled)?;
    let page = requested_page.clamp(1, pages);
    let rendered = cache_dir.join(format!("{identity}-page-{page}.jpg"));

    if rendered.metadata().is_ok_and(|metadata| metadata.len() > 0) {
        return Ok(PdfPage { path: rendered });
    }

    check_cancelled(cancelled)?;
    let output_dir = tempfile::Builder::new()
        .prefix("render-")
        .tempdir_in(&cache_dir)?;
    let output_prefix = output_dir.path().join("page");
    let stderr = tempfile::tempfile()?;
    let stderr_writer = stderr.try_clone()?;

    let mut command = Command::new("pdftoppm");
    command
        .arg("-f")
        .arg(page.to_string())
        .arg("-l")
        .arg(page.to_string())
        .arg("-singlefile")
        .arg("-jpeg")
        .arg("-jpegopt")
        .arg(format!("quality={PDF_JPEG_QUALITY}"))
        .arg("-scale-to")
        .arg(PDF_RENDER_LIMIT.to_string())
        // A path is data, never more options, whatever it starts with. The same
        // rule `archive_ops` already applies to its backend.
        .arg("--")
        .arg(source)
        .arg(&output_prefix)
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_writer));

    let status = run_child(&mut command, cancelled)?;
    if !status.success() {
        return Err(tool_failure("pdftoppm", status, read_bounded(stderr)?));
    }

    let temporary_render = output_prefix.with_extension("jpg");
    if !temporary_render
        .metadata()
        .is_ok_and(|metadata| metadata.len() > 0)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Poppler produced an empty PDF preview",
        ));
    }

    fs::rename(&temporary_render, &rendered)?;
    prune_cache(&cache_dir);

    Ok(PdfPage { path: rendered })
}

fn page_count(
    source: &Path,
    cache_dir: &Path,
    identity: &str,
    cancelled: &Arc<AtomicBool>,
) -> io::Result<usize> {
    match cached_page_count(cache_dir, identity) {
        Some(pages) if pages > 0 => Ok(pages),
        _ => run_pdfinfo(source, cache_dir, identity, cancelled),
    }
}

fn run_pdfinfo(
    source: &Path,
    cache_dir: &Path,
    identity: &str,
    cancelled: &Arc<AtomicBool>,
) -> io::Result<usize> {
    let stdout = tempfile::tempfile()?;
    let stderr = tempfile::tempfile()?;
    let stdout_writer = stdout.try_clone()?;
    let stderr_writer = stderr.try_clone()?;

    let mut command = Command::new("pdfinfo");
    command
        .arg("--")
        .arg(source)
        .stdout(Stdio::from(stdout_writer))
        .stderr(Stdio::from(stderr_writer));

    let status = run_child(&mut command, cancelled)?;
    if !status.success() {
        return Err(tool_failure("pdfinfo", status, read_bounded(stderr)?));
    }

    let output = read_bounded(stdout)?;
    let pages = parse_page_count(&output).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Poppler did not report a PDF page count",
        )
    })?;

    fs::write(
        cache_dir.join(format!("{identity}.pages")),
        pages.to_string(),
    )?;
    Ok(pages)
}

fn run_child(command: &mut Command, cancelled: &AtomicBool) -> io::Result<ExitStatus> {
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            io::Error::new(
                io::ErrorKind::NotFound,
                "PDF preview requires Poppler (`pdfinfo` and `pdftoppm`)",
            )
        } else {
            error
        }
    })?;
    let deadline = Instant::now() + PDF_PROCESS_TIMEOUT;

    loop {
        if cancelled.load(Ordering::Acquire) {
            terminate(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "PDF preview was cancelled",
            ));
        }
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            terminate(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "PDF preview exceeded the 20 second time limit",
            ));
        }
        thread::sleep(PDF_PROCESS_POLL_INTERVAL);
    }
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn check_cancelled(cancelled: &AtomicBool) -> io::Result<()> {
    if cancelled.load(Ordering::Acquire) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "PDF preview was cancelled",
        ))
    } else {
        Ok(())
    }
}

fn parse_page_count(output: &[u8]) -> Option<usize> {
    String::from_utf8_lossy(output).lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if !name.trim().eq_ignore_ascii_case("pages") {
            return None;
        }
        value.trim().parse().ok().filter(|pages| *pages > 0)
    })
}

fn cached_page_count(cache_dir: &Path, identity: &str) -> Option<usize> {
    fs::read_to_string(cache_dir.join(format!("{identity}.pages")))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn file_identity(path: &Path) -> io::Result<String> {
    let metadata = path.metadata()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let mut hash = Md5::new();
    hash.update(b"marcel-pdf-v1");
    hash.update(path.as_os_str().as_encoded_bytes());
    hash.update(metadata.len().to_le_bytes());
    hash.update(modified.to_le_bytes());
    hash.update(PDF_RENDER_LIMIT.to_le_bytes());
    Ok(format!("{:x}", hash.finalize()))
}

fn pdf_cache_dir() -> PathBuf {
    absolute_env_path("XDG_CACHE_HOME")
        .or_else(|| absolute_env_path("HOME").map(|home| home.join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("marcel")
        .join("pdf-v1")
}

fn absolute_env_path(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os(name)?);
    path.is_absolute().then_some(path)
}

fn read_bounded(mut file: File) -> io::Result<Vec<u8>> {
    // `File::try_clone` duplicates the descriptor but shares its open-file
    // position. Poppler therefore leaves the reader positioned at EOF after
    // writing; rewind before consuming the bounded output.
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_TOOL_OUTPUT_BYTES)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn tool_failure(tool: &str, status: ExitStatus, stderr: Vec<u8>) -> io::Error {
    let detail = String::from_utf8_lossy(&stderr);
    let detail = detail.trim();
    let message = if detail.is_empty() {
        format!("`{tool}` exited with {status}")
    } else {
        format!("`{tool}` exited with {status}: {detail}")
    };
    io::Error::other(message)
}

fn prune_cache(cache_dir: &Path) {
    let Ok(entries) = fs::read_dir(cache_dir) else {
        return;
    };
    let mut files = entries
        .flatten()
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
    use std::io::Write;

    #[test]
    fn parses_poppler_page_count() {
        assert_eq!(
            parse_page_count(b"Title:          Test\nPages:          42\n"),
            Some(42)
        );
    }

    #[test]
    fn rejects_missing_or_invalid_page_count() {
        assert_eq!(parse_page_count(b"Title: Test\n"), None);
        assert_eq!(parse_page_count(b"Pages: many\n"), None);
        assert_eq!(parse_page_count(b"Pages: 0\n"), None);
    }

    #[test]
    fn cancellation_is_observable_before_work_starts() {
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            check_cancelled(&cancelled).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn bounded_tool_output_rewinds_a_cloned_descriptor() {
        let file = tempfile::tempfile().unwrap();
        let mut writer = file.try_clone().unwrap();
        writer.write_all(b"Pages: 7\n").unwrap();
        writer.flush().unwrap();

        assert_eq!(read_bounded(file).unwrap(), b"Pages: 7\n");
    }
}
