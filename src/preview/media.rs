//! What a video is, and one frame of it, through ffmpeg.
//!
//! ffmpeg is not bundled: it is found on `PATH`, and the preview says so
//! when it is missing. A poster frame costs a second once and is cached by
//! the source's identity, like a rendered PDF page. The same frame, scaled
//! down, is what the grid shows as a thumbnail.

use std::{
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use crate::fsops::local::create_private_dir_all;

use super::tool::{self, cache_dir, find_on_path, prune_cache, read_bounded, tool_failure};

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const FRAME_TIMEOUT: Duration = Duration::from_secs(20);
/// The poster's long edge. Enough for the pane; the thumbnail is made from it.
const POSTER_EDGE: u32 = 1280;
const ABSENT: &str = "Video preview needs ffmpeg (`ffprobe` and `ffmpeg` on PATH)";

/// `ffmpeg` or `ffprobe` under the shared child-process rules, with the
/// input pinned to local protocols.
///
/// ffmpeg picks a demuxer by content, so a file named `clip.mp4` may really
/// be a playlist naming `http:` segments. ffmpeg's `file` protocol already
/// limits what an input it opened may open in turn to this same list, so
/// the option is belt and braces rather than the closing of a live hole,
/// stated on every invocation so the intent outlives ffmpeg's defaults. It
/// applies to the input that follows it, so it has to precede `-i`, and it
/// leaves an output such as `pipe:1` alone.
pub(super) fn ffmpeg_command(program: &str) -> Command {
    let mut command = tool::command(program);
    command.args(["-protocol_whitelist", "file,crypto,data"]);
    command
}

/// What `ffprobe` says about a file: the streams, and the tags a container
/// carries at the top level.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaInfo {
    pub duration: Option<Duration>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// The video stream's codec name as ffmpeg calls it: "h264", "vp9".
    pub codec: Option<String>,
    pub audio_codec: Option<String>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
}

#[derive(Clone, Debug)]
pub struct VideoPreview {
    pub info: MediaInfo,
    /// A JPEG of one frame from early in the video, or why there is none.
    pub poster: Result<PathBuf, String>,
}

/// Whether the tools are there. Checked once per preview rather than
/// cached, so installing ffmpeg takes effect without a restart.
pub fn available() -> bool {
    find_on_path("ffprobe").is_some() && find_on_path("ffmpeg").is_some()
}

pub fn inspect_video(source: &Path, cancelled: &Arc<AtomicBool>) -> io::Result<VideoPreview> {
    let info = probe(source, cancelled)?;
    let poster = poster_frame(source, &info, cancelled).map_err(|error| error.to_string());
    Ok(VideoPreview { info, poster })
}

/// The stream facts alone, for Properties.
pub fn probe(source: &Path, cancelled: &Arc<AtomicBool>) -> io::Result<MediaInfo> {
    tool::check_cancelled(cancelled, "Video preview")?;
    let stdout = tempfile::tempfile()?;
    let stderr = tempfile::tempfile()?;
    let stdout_writer = stdout.try_clone()?;
    let stderr_writer = stderr.try_clone()?;

    let mut command = ffmpeg_command("ffprobe");
    command
        .args(["-v", "error", "-show_entries"])
        .arg(
            "format=duration:format_tags=title,artist,album:stream=codec_type,codec_name,width,height,sample_rate,channels:stream_tags=title,artist,album",
        )
        // One `key=value` per line, streams in order, no quoting to undo.
        .args(["-of", "flat=s=_:h=0"])
        // `-i` takes the path as its value, so a name beginning with `-`
        // cannot be read as an option.
        .arg("-i")
        .arg(source)
        .stdout(Stdio::from(stdout_writer))
        .stderr(Stdio::from(stderr_writer));
    let status = tool::run_child(&mut command, cancelled, PROBE_TIMEOUT, "Video preview", ABSENT)?;
    if !status.success() {
        return Err(tool_failure("ffprobe", status, read_bounded(stderr)?));
    }
    Ok(parse_probe(&String::from_utf8_lossy(&read_bounded(stdout)?)))
}

/// Read ffprobe's flat output. Every line is `stream_N_key=value`, `format_key=value`,
/// or a `…_tags_name=value` for either; strings are quoted, numbers are not.
/// Tags are taken from the container first and any stream after, since an
/// Ogg keeps its titles on the stream.
fn parse_probe(output: &str) -> MediaInfo {
    let mut info = MediaInfo::default();
    // The first stream of each type is the one described.
    let mut video_stream: Option<&str> = None;
    let mut audio_stream: Option<&str> = None;
    let lines = output
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim(), value.trim().trim_matches('"')))
        .collect::<Vec<_>>();
    for (key, value) in &lines {
        if let Some(stream) =
            key.strip_suffix("_codec_type").and_then(|k| k.strip_prefix("stream_"))
        {
            match *value {
                "video" if video_stream.is_none() => video_stream = Some(stream),
                "audio" if audio_stream.is_none() => audio_stream = Some(stream),
                _ => {}
            }
        }
    }
    for (key, value) in &lines {
        let tag = key.strip_prefix("format_tags_").or_else(|| {
            key.strip_prefix("stream_")
                .and_then(|rest| rest.split_once("_tags_"))
                .map(|(_, tag)| tag)
        });
        if let Some(tag) = tag {
            let slot = match tag.to_ascii_lowercase().as_str() {
                "title" => &mut info.title,
                "artist" => &mut info.artist,
                "album" => &mut info.album,
                _ => continue,
            };
            if slot.is_none() || key.starts_with("format_") {
                *slot = Some((*value).to_string());
            }
            continue;
        }
        if *key == "format_duration" {
            info.duration = value
                .parse::<f64>()
                .ok()
                .filter(|s| s.is_finite() && *s >= 0.0)
                .map(Duration::from_secs_f64);
            continue;
        }
        let Some(rest) = key.strip_prefix("stream_") else { continue };
        let Some((stream, field)) = rest.split_once('_') else { continue };
        if Some(stream) == video_stream {
            match field {
                "codec_name" => info.codec = Some((*value).to_string()),
                "width" => info.width = value.parse().ok(),
                "height" => info.height = value.parse().ok(),
                _ => {}
            }
        } else if Some(stream) == audio_stream {
            match field {
                "codec_name" => info.audio_codec = Some((*value).to_string()),
                "sample_rate" => info.sample_rate = value.parse().ok(),
                "channels" => info.channels = value.parse().ok(),
                _ => {}
            }
        }
    }
    info
}

/// One frame from a little way in, as a JPEG in the cache.
///
/// Ten percent in, capped at a minute, skips title cards and black leaders
/// without waiting through a long file; a clip too short for that gets its
/// first frame.
pub fn poster_frame(
    source: &Path,
    info: &MediaInfo,
    cancelled: &Arc<AtomicBool>,
) -> io::Result<PathBuf> {
    tool::check_cancelled(cancelled, "Video preview")?;
    let cache = cache_dir("video-v1");
    create_private_dir_all(&cache)?;
    let identity =
        tool::file_identity(source, format!("marcel-video-v1-{POSTER_EDGE}").as_bytes())?;
    let rendered = cache.join(format!("{identity}.jpg"));
    if rendered.metadata().is_ok_and(|metadata| metadata.len() > 0) {
        return Ok(rendered);
    }

    let offset = info
        .duration
        .map(|duration| (duration.as_secs_f64() * 0.1).min(60.0))
        .filter(|seconds| *seconds >= 1.0)
        .unwrap_or(0.0);
    let output_dir = tempfile::Builder::new().prefix("frame-").tempdir_in(&cache)?;
    let output = output_dir.path().join("frame.jpg");
    for seek in [offset, 0.0] {
        tool::check_cancelled(cancelled, "Video preview")?;
        let stderr = tempfile::tempfile()?;
        let stderr_writer = stderr.try_clone()?;
        let mut command = ffmpeg_command("ffmpeg");
        command
            .args(["-v", "error", "-nostdin", "-y"])
            .arg("-ss")
            .arg(format!("{seek:.3}"))
            .arg("-i")
            .arg(source)
            .args(["-frames:v", "1", "-an", "-sn", "-vf"])
            .arg(format!("scale='min({POSTER_EDGE},iw)':-2"))
            .args(["-q:v", "3"])
            .arg(&output)
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_writer));
        let status =
            tool::run_child(&mut command, cancelled, FRAME_TIMEOUT, "Video preview", ABSENT)?;
        if status.success() && output.metadata().is_ok_and(|metadata| metadata.len() > 0) {
            fs::rename(&output, &rendered)?;
            prune_cache(&cache);
            return Ok(rendered);
        }
        // A seek past the end is not an error worth reporting: the offset is
        // a guess (a tenth of the duration, or a flat three seconds for a
        // thumbnail), so a clip shorter than that gets its first frame
        // instead. Older ffmpeg exited 0 with an empty file here; from 8 on,
        // the encoder never sees a frame, refuses to open, and exits 234, so
        // the status alone cannot tell "too short" from "broken".
        if seek == 0.0 {
            if !status.success() {
                return Err(tool_failure("ffmpeg", status, read_bounded(stderr)?));
            }
            break;
        }
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "ffmpeg produced no frame from this video"))
}

/// The frame for a thumbnail: no probe first, since a tile has no use for
/// the duration and the probe is a second process per file. Three seconds
/// in, or the first frame of anything shorter.
pub fn thumbnail_frame(source: &Path, cancelled: &Arc<AtomicBool>) -> io::Result<PathBuf> {
    let info = MediaInfo { duration: Some(Duration::from_secs(30)), ..MediaInfo::default() };
    poster_frame(source, &info, cancelled)
}

/// "mono", "stereo", or "6 channels".
pub fn describe_channels(channels: usize) -> String {
    match channels {
        1 => "mono".to_string(),
        2 => "stereo".to_string(),
        n => format!("{n} channels"),
    }
}

/// "1:23:45" or "4:56", as a player shows it.
pub fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A thumbnail seeks three seconds in without asking how long the clip
    /// is. A clip shorter than that has to yield its first frame rather than
    /// the encoder-init failure ffmpeg 8 and later exit with on a seek past
    /// the end.
    #[test]
    fn a_clip_shorter_than_the_guessed_offset_still_gets_a_frame() {
        if !available() {
            eprintln!("skipping: ffmpeg is not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("short.mp4");
        let encoded = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc=size=64x48:rate=10:duration=1")
            .arg(&clip)
            .status()
            .unwrap();
        assert!(encoded.success());

        let frame = thumbnail_frame(&clip, &Arc::new(AtomicBool::new(false))).unwrap();
        assert!(frame.metadata().unwrap().len() > 0);
    }

    #[test]
    fn reads_the_first_video_and_audio_streams_from_flat_probe_output() {
        let output = "\
stream_0_codec_name=\"aac\"
stream_0_codec_type=\"audio\"
stream_1_codec_name=\"h264\"
stream_1_codec_type=\"video\"
stream_1_width=1920
stream_1_height=1080
stream_2_codec_name=\"png\"
stream_2_codec_type=\"video\"
stream_2_width=300
stream_2_height=300
format_duration=\"125.500000\"
stream_1_tags_title=\"Stream title\"
format_tags_title=\"Clip\"
stream_0_sample_rate=\"48000\"
stream_0_channels=2
";
        assert_eq!(
            parse_probe(output),
            MediaInfo {
                duration: Some(Duration::from_millis(125_500)),
                width: Some(1920),
                height: Some(1080),
                codec: Some("h264".to_string()),
                audio_codec: Some("aac".to_string()),
                sample_rate: Some(48_000),
                channels: Some(2),
                title: Some("Clip".to_string()),
                ..MediaInfo::default()
            }
        );
    }

    #[test]
    fn tolerates_missing_and_nonsense_fields() {
        assert_eq!(parse_probe(""), MediaInfo::default());
        assert_eq!(parse_probe("format_duration=\"N/A\"\n").duration, None);
        assert_eq!(parse_probe("format_duration=\"-1\"\n").duration, None);
    }

    /// The whitelist leads every command line, ahead of `-i`, and the
    /// shared rules come with it.
    #[test]
    fn every_ffmpeg_invocation_is_pinned_to_local_protocols() {
        for program in ["ffmpeg", "ffprobe"] {
            let mut command = ffmpeg_command(program);
            command.args(["-i", "clip.mp4"]);
            let args = command.get_args().map(|arg| arg.to_string_lossy().into_owned());
            assert_eq!(
                args.collect::<Vec<_>>(),
                ["-protocol_whitelist", "file,crypto,data", "-i", "clip.mp4"]
            );
            assert!(
                command.get_envs().any(|(key, value)| key == "LD_LIBRARY_PATH" && value.is_none())
            );
        }
    }

    #[test]
    fn durations_read_like_a_player() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0:00");
        assert_eq!(format_duration(Duration::from_secs(296)), "4:56");
        assert_eq!(format_duration(Duration::from_secs(5025)), "1:23:45");
    }
}
