//! Reading audio: the tags, the cover, the samples.
//!
//! Symphonia decodes in-process, so nothing here shells out and nothing is
//! bundled. `AudioSource` is one open file yielding interleaved `f32`
//! samples on demand; the preview reads it once through for the waveform,
//! and the player reads it again, at listening speed, on its own thread.

use std::{
    io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use symphonia::core::{
    codecs::{CodecParameters, audio::AudioDecoder, audio::AudioDecoderOptions},
    errors::Error as SymphoniaError,
    formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType, probe::Hint},
    io::{MediaSourceStream, MediaSourceStreamOptions},
    meta::{MetadataOptions, StandardTag, StandardVisualKey},
    units::Time,
};

use crate::fsops::local::{create_private_dir_all, open_regular_file};

use super::{media, tool};

/// How many peaks the waveform overview holds. Enough to draw across a
/// pane; few enough that a long file's pass stays cheap to keep.
pub const WAVEFORM_BUCKETS: usize = 240;
/// A waveform pass decodes the whole file once, then keeps the peaks in the
/// cache, so it is only attempted below
/// this length; a longer file gets a flat scrubber and still plays.
const MAX_WAVEFORM_SECONDS: f64 = 3.0 * 60.0 * 60.0;

/// What the tags and the stream say about a file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioInfo {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<Duration>,
    pub sample_rate: u32,
    pub channels: usize,
    /// The codec's short name as symphonia gives it: "mp3", "flac".
    pub codec: String,
    /// The front cover's encoded bytes (JPEG or PNG, as embedded).
    pub cover: Option<Vec<u8>>,
}

/// Whether a path names something symphonia will try to decode.
pub fn supports(path: &Path) -> bool {
    mime_guess::from_path(path).first().is_some_and(|mime| mime.type_() == "audio")
}

pub struct AudioSource {
    backend: Backend,
    pub info: AudioInfo,
}

/// Who turns the file into samples.
enum Backend {
    /// In-process, for everything symphonia knows.
    Symphonia { format: Box<dyn FormatReader>, decoder: Box<dyn AudioDecoder>, track_id: u32 },
    /// A child `ffmpeg` writing raw `f32` to a pipe, for what symphonia does
    /// not know (Opus, above all: every voice note is Opus in Ogg). Seeking
    /// is a fresh child started at the position.
    Ffmpeg { path: std::path::PathBuf, child: Option<FfmpegChild>, rate: u32, channels: usize },
}

struct FfmpegChild {
    process: std::process::Child,
    stdout: std::process::ChildStdout,
}

impl Drop for FfmpegChild {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// How many frames one ffmpeg read asks for.
const FFMPEG_CHUNK_FRAMES: usize = 4_096;

impl AudioSource {
    /// Open `path` with symphonia, or with ffmpeg when symphonia does not
    /// know the codec and ffmpeg is on `PATH`.
    pub fn open(path: &Path) -> io::Result<Self> {
        match Self::open_symphonia(path) {
            Ok(source) => Ok(source),
            Err(error) if error.kind() == io::ErrorKind::Unsupported && media::available() => {
                Self::open_ffmpeg(path).map_err(|ffmpeg_error| {
                    io::Error::new(io::ErrorKind::Unsupported, format!("{error}; {ffmpeg_error}"))
                })
            }
            Err(error) => Err(error),
        }
    }

    fn open_symphonia(path: &Path) -> io::Result<Self> {
        let file = open_regular_file(path)?;
        let stream = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
        let mut hint = Hint::new();
        if let Some(extension) = path.extension().and_then(|extension| extension.to_str()) {
            hint.with_extension(extension);
        }
        let mut format = symphonia::default::get_probe()
            .probe(&hint, stream, FormatOptions::default(), MetadataOptions::default())
            .map_err(unsupported)?;
        let track = format
            .first_track_known_codec(TrackType::Audio)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "No playable audio track"))?;
        let Some(CodecParameters::Audio(params)) = &track.codec_params else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "No playable audio track"));
        };
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(unsupported)?;
        let track_id = track.id;
        let time_base = track.time_base;
        let duration = track
            .duration
            .and_then(|duration| time_base?.calc_duration(duration))
            .map(|time| Duration::from_secs_f64(time.as_secs_f64().max(0.0)));
        let mut info = AudioInfo {
            duration,
            sample_rate: params.sample_rate.unwrap_or(0),
            channels: params.channels.as_ref().map(|channels| channels.count()).unwrap_or(0),
            codec: decoder.codec_info().short_name.to_string(),
            ..AudioInfo::default()
        };
        read_tags(&mut format, &mut info);
        Ok(Self { backend: Backend::Symphonia { format, decoder, track_id }, info })
    }

    fn open_ffmpeg(path: &Path) -> io::Result<Self> {
        let probed = media::probe(path, &Arc::new(AtomicBool::new(false)))?;
        let codec = probed
            .audio_codec
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "No playable audio track"))?;
        let rate = probed.sample_rate.filter(|rate| *rate > 0).unwrap_or(48_000);
        let channels = probed.channels.map(|channels| channels.clamp(1, 2) as usize).unwrap_or(2);
        let info = AudioInfo {
            title: probed.title,
            artist: probed.artist,
            album: probed.album,
            duration: probed.duration,
            sample_rate: rate,
            channels,
            codec,
            cover: None,
        };
        Ok(Self {
            backend: Backend::Ffmpeg { path: path.to_path_buf(), child: None, rate, channels },
            info,
        })
    }

    /// Decode the next packet into `out` as interleaved `f32` at the
    /// stream's own rate and channel count, which the first chunk fixes in
    /// `info` when the container did not say. `Ok(false)` at the end.
    pub fn next_chunk(&mut self, out: &mut Vec<f32>) -> io::Result<bool> {
        match &mut self.backend {
            Backend::Symphonia { format, decoder, track_id } => {
                next_symphonia_chunk(format, decoder, *track_id, &mut self.info, out)
            }
            Backend::Ffmpeg { path, child, rate, channels } => {
                if child.is_none() {
                    *child = Some(spawn_ffmpeg(path, Duration::ZERO, *rate, *channels)?);
                }
                let Some(running) = child.as_mut() else { return Ok(false) };
                let mut bytes = vec![0_u8; FFMPEG_CHUNK_FRAMES * *channels * 4];
                let read = read_full(&mut running.stdout, &mut bytes)?;
                if read == 0 {
                    *child = None;
                    return Ok(false);
                }
                out.clear();
                out.extend(bytes[..read - read % 4].chunks_exact(4).map(|sample| {
                    f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]])
                }));
                Ok(true)
            }
        }
    }

    /// Jump to `position`; the next chunk starts near there.
    pub fn seek(&mut self, position: Duration) -> io::Result<()> {
        match &mut self.backend {
            Backend::Symphonia { format, decoder, track_id } => {
                let time = Time::try_new(position.as_secs() as i64, position.subsec_nanos())
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "Bad seek position")
                    })?;
                format
                    .seek(SeekMode::Coarse, SeekTo::Time { time, track_id: Some(*track_id) })
                    .map_err(|error| io::Error::other(error.to_string()))?;
                decoder.reset();
                Ok(())
            }
            Backend::Ffmpeg { path, child, rate, channels } => {
                *child = Some(spawn_ffmpeg(path, position, *rate, *channels)?);
                Ok(())
            }
        }
    }
}

fn next_symphonia_chunk(
    format: &mut Box<dyn FormatReader>,
    decoder: &mut Box<dyn AudioDecoder>,
    track_id: u32,
    info: &mut AudioInfo,
    out: &mut Vec<f32>,
) -> io::Result<bool> {
    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => return Ok(false),
            Err(SymphoniaError::IoError(error)) if error.kind() == io::ErrorKind::UnexpectedEof => {
                return Ok(false);
            }
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(error) => return Err(io::Error::other(error.to_string())),
        };
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buffer) => {
                let spec = buffer.spec();
                if info.sample_rate == 0 {
                    info.sample_rate = spec.rate();
                }
                if info.channels == 0 {
                    info.channels = spec.channels().count();
                }
                buffer.copy_to_vec_interleaved(out);
                return Ok(true);
            }
            // A damaged packet is skipped, as every player skips it.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(error) => return Err(io::Error::other(error.to_string())),
        }
    }
}

/// `ffmpeg` decoding `path` from `start` into raw little-endian `f32` on
/// its stdout, at `rate` and `channels`. stderr is dropped: a failure shows
/// as an early end of the stream, which the player reports as finishing.
fn spawn_ffmpeg(
    path: &Path,
    start: Duration,
    rate: u32,
    channels: usize,
) -> io::Result<FfmpegChild> {
    let mut process = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin"])
        .arg("-ss")
        .arg(format!("{:.3}", start.as_secs_f64()))
        .arg("-i")
        .arg(path)
        .args(["-vn", "-sn", "-f", "f32le", "-acodec", "pcm_f32le"])
        .arg("-ac")
        .arg(channels.to_string())
        .arg("-ar")
        .arg(rate.to_string())
        .arg("pipe:1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let stdout = process.stdout.take().ok_or_else(|| io::Error::other("ffmpeg has no stdout"))?;
    Ok(FfmpegChild { process, stdout })
}

/// Fill `buffer` from the pipe, stopping short only at its end.
fn read_full(reader: &mut impl io::Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

fn unsupported(error: SymphoniaError) -> io::Error {
    match error {
        SymphoniaError::Unsupported(what) => {
            io::Error::new(io::ErrorKind::Unsupported, format!("Unsupported audio: {what}"))
        }
        SymphoniaError::IoError(error) => error,
        other => io::Error::other(other.to_string()),
    }
}

/// Title, artist, album, and the front cover, from the newest metadata the
/// container carries.
fn read_tags(format: &mut Box<dyn FormatReader>, info: &mut AudioInfo) {
    let mut metadata = format.metadata();
    let Some(revision) = metadata.skip_to_latest() else {
        return;
    };
    for tag in &revision.media.tags {
        match &tag.std {
            Some(StandardTag::TrackTitle(value)) => info.title = Some(value.to_string()),
            Some(StandardTag::Artist(value)) if info.artist.is_none() => {
                info.artist = Some(value.to_string());
            }
            Some(StandardTag::AlbumArtist(value)) if info.artist.is_none() => {
                info.artist = Some(value.to_string());
            }
            Some(StandardTag::Album(value)) => info.album = Some(value.to_string()),
            _ => {}
        }
    }
    let visuals = &revision.media.visuals;
    let cover = visuals
        .iter()
        .find(|visual| visual.usage == Some(StandardVisualKey::FrontCover))
        .or_else(|| visuals.first());
    info.cover = cover.map(|visual| visual.data.to_vec());
}

/// Peak amplitude per bucket across the whole file, each in `0..=1`, or an
/// empty vector when the file is too long to read through or was cancelled.
///
/// The pass is one full decode, so its result is kept under the cache by
/// the file's identity: a track is measured the first time it is selected
/// and read back from 1 KB afterwards.
pub fn waveform(path: &Path, source: &mut AudioSource, cancelled: &AtomicBool) -> Vec<f32> {
    waveform_in(&tool::cache_dir("waveform-v1"), path, source, cancelled)
}

fn waveform_in(
    cache: &Path,
    path: &Path,
    source: &mut AudioSource,
    cancelled: &AtomicBool,
) -> Vec<f32> {
    let cached_at = tool::file_identity(path, b"marcel-waveform-v1")
        .ok()
        .map(|identity| cache.join(format!("{identity}.peaks")));
    if let Some(peaks) = cached_at.as_deref().and_then(read_cached_waveform) {
        return peaks;
    }
    let peaks = measure_waveform(source, cancelled);
    if !peaks.is_empty()
        && let Some(cache_path) = cached_at
        && create_private_dir_all(cache).is_ok()
    {
        let bytes = peaks.iter().flat_map(|peak| peak.to_le_bytes()).collect::<Vec<_>>();
        if let Ok(mut temporary) = tempfile::NamedTempFile::new_in(cache)
            && std::io::Write::write_all(&mut temporary, &bytes).is_ok()
            && temporary.persist(&cache_path).is_ok()
        {
            tool::prune_cache(cache);
        }
    }
    peaks
}

fn read_cached_waveform(path: &Path) -> Option<Vec<f32>> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() != WAVEFORM_BUCKETS * 4 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|peak| f32::from_le_bytes([peak[0], peak[1], peak[2], peak[3]]).clamp(0.0, 1.0))
            .collect(),
    )
}

fn measure_waveform(source: &mut AudioSource, cancelled: &AtomicBool) -> Vec<f32> {
    let Some(duration) = source.info.duration else {
        return Vec::new();
    };
    if duration.as_secs_f64() > MAX_WAVEFORM_SECONDS || duration.is_zero() {
        return Vec::new();
    }
    let mut chunk = Vec::new();
    let mut peaks = vec![0_f32; WAVEFORM_BUCKETS];
    let mut frames_seen: u64 = 0;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Vec::new();
        }
        match source.next_chunk(&mut chunk) {
            Ok(true) => {}
            Ok(false) | Err(_) => break,
        }
        let channels = source.info.channels.max(1);
        let rate = f64::from(source.info.sample_rate.max(1));
        let total_frames = (duration.as_secs_f64() * rate).max(1.0);
        for frame in chunk.chunks(channels) {
            let peak = frame.iter().fold(0_f32, |peak, sample| peak.max(sample.abs()));
            let bucket = ((frames_seen as f64 / total_frames) * WAVEFORM_BUCKETS as f64) as usize;
            let bucket = bucket.min(WAVEFORM_BUCKETS - 1);
            peaks[bucket] = peaks[bucket].max(peak);
            frames_seen += 1;
        }
    }
    if frames_seen == 0 {
        return Vec::new();
    }
    // Rewind so a player that reuses this source starts at the top.
    let _ = source.seek(Duration::ZERO);
    peaks.iter_mut().for_each(|peak| *peak = peak.clamp(0.0, 1.0));
    peaks
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A WAV file with one second of a 440 Hz tone at 8 kHz, so the reader
    /// has something with a duration, a rate, and real peaks.
    fn tone(path: &Path) {
        let rate = 8_000_u32;
        let samples = (0..rate)
            .map(|i| {
                let t = i as f32 / rate as f32;
                ((t * 440.0 * std::f32::consts::TAU).sin() * 0.5 * i16::MAX as f32) as i16
            })
            .collect::<Vec<_>>();
        let data_len = (samples.len() * 2) as u32;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * 2).to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn reads_a_wav_file_through_and_measures_its_waveform() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        tone(&path);

        let mut source = AudioSource::open(&path).unwrap();
        assert_eq!(source.info.sample_rate, 8_000);
        assert_eq!(source.info.channels, 1);
        assert_eq!(source.info.duration, Some(Duration::from_secs(1)));
        assert_eq!(source.info.codec, "pcm_s16le");

        let cache = dir.path().join("cache");
        let peaks = waveform_in(&cache, &path, &mut source, &AtomicBool::new(false));
        assert_eq!(peaks.len(), WAVEFORM_BUCKETS);
        assert!(peaks.iter().all(|peak| (0.45..=0.55).contains(peak)), "{peaks:?}");

        // Rewound: the first chunk after the waveform is the file's start.
        let mut chunk = Vec::new();
        assert!(source.next_chunk(&mut chunk).unwrap());
        assert!(!chunk.is_empty());
    }

    #[test]
    fn a_cancelled_waveform_pass_gives_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        tone(&path);
        let mut source = AudioSource::open(&path).unwrap();
        let cancelled = Arc::new(AtomicBool::new(true));
        assert!(waveform_in(&dir.path().join("cache"), &path, &mut source, &cancelled).is_empty());
    }

    /// The second look at a file reads the peaks back rather than decoding
    /// it again: whatever the cache file says is what comes back.
    #[test]
    fn a_measured_waveform_is_read_from_the_cache_next_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        tone(&path);
        let cache = dir.path().join("cache");
        let mut source = AudioSource::open(&path).unwrap();
        let first = waveform_in(&cache, &path, &mut source, &AtomicBool::new(false));
        assert_eq!(first.len(), WAVEFORM_BUCKETS);

        let cached = std::fs::read_dir(&cache).unwrap().flatten().next().unwrap().path();
        let planted =
            (0..WAVEFORM_BUCKETS).flat_map(|_| 0.75_f32.to_le_bytes()).collect::<Vec<_>>();
        std::fs::write(&cached, planted).unwrap();
        let second = waveform_in(&cache, &path, &mut source, &AtomicBool::new(false));
        assert!(second.iter().all(|peak| *peak == 0.75));

        // A cache entry of the wrong shape is ignored, not trusted.
        std::fs::write(&cached, b"junk").unwrap();
        let third = waveform_in(&cache, &path, &mut source, &AtomicBool::new(false));
        assert_eq!(third, first);
    }

    #[test]
    fn refuses_what_is_not_audio() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "hello").unwrap();
        assert!(AudioSource::open(&path).is_err());
        assert!(supports(Path::new("song.mp3")));
        assert!(supports(Path::new("song.flac")));
        assert!(!supports(Path::new("clip.mp4")));
    }

    /// Opus is what symphonia lacks and voice notes are made of. With ffmpeg
    /// on PATH the file opens through it and reads like any other.
    #[test]
    fn opus_decodes_through_ffmpeg_when_symphonia_cannot() {
        if !media::available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.ogg");
        let encoded = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
            .arg("sine=frequency=440:duration=2")
            .args(["-c:a", "libopus", "-metadata", "title=Voice note"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(encoded.success());

        let mut source = AudioSource::open(&path).unwrap();
        assert!(matches!(source.backend, Backend::Ffmpeg { .. }));
        assert_eq!(source.info.codec, "opus");
        assert_eq!(source.info.title.as_deref(), Some("Voice note"));
        let duration = source.info.duration.unwrap().as_secs_f64();
        assert!((1.9..=2.1).contains(&duration), "{duration}");

        let cache = dir.path().join("cache");
        let peaks = waveform_in(&cache, &path, &mut source, &AtomicBool::new(false));
        assert_eq!(peaks.len(), WAVEFORM_BUCKETS);
        // lavfi's sine is quiet (about 0.125 of full scale); a steady tone
        // fills the buckets evenly at whatever level it has.
        let loud = peaks.iter().filter(|peak| **peak > 0.05).count();
        assert!(loud > WAVEFORM_BUCKETS / 2, "{peaks:?}");

        source.seek(Duration::from_secs(1)).unwrap();
        let mut chunk = Vec::new();
        assert!(source.next_chunk(&mut chunk).unwrap());
        assert_eq!(chunk.len() % source.info.channels, 0);
    }
}
