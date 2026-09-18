//! Playing one audio file: a thread that decodes ahead of the sound card,
//! the sound card's callback that drains it, and the numbers the pane
//! reads while it happens.
//!
//! The player is created with the preview and costs nothing until Play:
//! the thread, the file, and the output stream all start then. Decoded
//! audio reaches the card as chunks over a bounded channel, so the callback
//! never takes a lock and the decoder never runs far ahead. Position is
//! what the card has consumed, not what was decoded, so the playhead does
//! not run ahead of what is heard. When paused the thread blocks on its
//! commands and wakes for nothing.

use std::{
    fmt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use realfft::{RealFftPlanner, num_complex::Complex};

use super::audio::AudioSource;

/// How many bars the spectrum has.
pub const SPECTRUM_BARS: usize = 48;
const FFT_SIZE: usize = 1024;
/// How many decoded chunks may wait for the card. A chunk is one packet,
/// about 25 ms of MP3, so this is roughly half a second of lead.
const QUEUE_CHUNKS: usize = 20;
/// How often the spectrum and position are refreshed while playing.
const TICK: Duration = Duration::from_millis(30);
const LOWEST_BAR_HZ: f32 = 40.0;
const HIGHEST_BAR_HZ: f32 = 16_000.0;

enum Command {
    Play,
    Pause,
    Seek(Duration),
    Stop,
}

/// What the pane reads. Everything here is written by the player thread or
/// the card's callback and only ever read on the foreground.
pub struct Shared {
    /// Whether the user wants sound. Cleared at the end of the file.
    playing: AtomicBool,
    /// The file has been played to the end.
    finished: AtomicBool,
    /// Where playback stands, in milliseconds.
    position_ms: AtomicU64,
    /// Why playback stopped, if it stopped on its own.
    error: Mutex<Option<String>>,
    spectrum: Mutex<[f32; SPECTRUM_BARS]>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            playing: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            position_ms: AtomicU64::new(0),
            error: Mutex::new(None),
            spectrum: Mutex::new([0.0; SPECTRUM_BARS]),
        }
    }
}

impl Shared {
    pub fn playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    pub fn finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }

    pub fn position(&self) -> Duration {
        Duration::from_millis(self.position_ms.load(Ordering::Relaxed))
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|error| error.clone())
    }

    pub fn spectrum(&self) -> [f32; SPECTRUM_BARS] {
        self.spectrum.lock().map(|bars| *bars).unwrap_or([0.0; SPECTRUM_BARS])
    }
}

pub struct Player {
    path: PathBuf,
    /// The thread's command channel, once the thread exists.
    commands: Mutex<Option<Sender<Command>>>,
    shared: Arc<Shared>,
}

impl fmt::Debug for Player {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Player").field("path", &self.path).finish_non_exhaustive()
    }
}

impl Player {
    pub fn new(path: &Path) -> Self {
        Self { path: path.to_path_buf(), commands: Mutex::new(None), shared: Arc::default() }
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// The file this player is for.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn play(&self) {
        self.send(Command::Play);
    }

    pub fn pause(&self) {
        self.send(Command::Pause);
    }

    pub fn toggle(&self) {
        if self.shared.playing() { self.pause() } else { self.play() }
    }

    pub fn seek(&self, position: Duration) {
        self.send(Command::Seek(position));
    }

    /// Hand the thread a command, starting the thread on the first one.
    fn send(&self, command: Command) {
        let Ok(mut commands) = self.commands.lock() else { return };
        if commands.is_none() {
            let (sender, receiver) = mpsc::channel();
            let spawned = thread::Builder::new()
                .name("marcel-audio".to_string())
                .spawn({
                    let path = self.path.clone();
                    let shared = self.shared.clone();
                    move || run(&path, receiver, shared)
                })
                .is_ok();
            if !spawned {
                fail(&self.shared, "Could not start the audio thread".to_string());
                return;
            }
            *commands = Some(sender);
        }
        if let Some(sender) = commands.as_ref() {
            let _ = sender.send(command);
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // The thread notices and exits on its own; the foreground does not
        // wait for the card to close.
        if let Ok(commands) = self.commands.lock()
            && let Some(sender) = commands.as_ref()
        {
            let _ = sender.send(Command::Stop);
        }
    }
}

/// A run of samples for the card, stamped with the seek generation it
/// belongs to so a seek can leave stale chunks in the channel and have the
/// callback drop them.
struct Chunk {
    generation: u32,
    samples: Vec<f32>,
}

/// What the card's callback shares with the decode loop.
struct Line {
    /// Bumped on every seek; chunks from before it are discarded unplayed.
    generation: AtomicU32,
    /// Frames the card has consumed since the last seek.
    played_frames: AtomicU64,
    /// The last `FFT_SIZE` mono samples the card played, for the spectrum.
    /// The callback tries the lock and skips a buffer if it cannot; the
    /// bars miss a frame, the sound does not.
    recent: Mutex<std::collections::VecDeque<f32>>,
    /// The decode loop reached the end; silence after the chunks is fine.
    draining: AtomicBool,
    /// The card ran dry after draining: the file is over.
    exhausted: AtomicBool,
}

struct Output {
    stream: cpal::Stream,
    rate: u32,
    channels: usize,
    chunks: SyncSender<Chunk>,
    line: Arc<Line>,
}

fn open_output() -> Result<Output, String> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("No audio output device")?;
    let configs = device.supported_output_configs().map_err(|error| error.to_string())?;
    // Float output at a common rate, on as many channels as the device
    // wants; stereo content is spread over the first two and the rest stay
    // silent.
    let range = configs
        .filter(|range| range.sample_format() == cpal::SampleFormat::F32)
        .min_by_key(|range| range.channels())
        .ok_or("The audio device has no float output")?;
    let rate = 48_000_u32.clamp(range.min_sample_rate(), range.max_sample_rate());
    let config = range.with_sample_rate(rate).config();
    let channels = usize::from(config.channels);
    let line = Arc::new(Line {
        generation: AtomicU32::new(0),
        played_frames: AtomicU64::new(0),
        recent: Mutex::new(std::collections::VecDeque::with_capacity(FFT_SIZE)),
        draining: AtomicBool::new(false),
        exhausted: AtomicBool::new(false),
    });
    let (chunks, incoming) = mpsc::sync_channel(QUEUE_CHUNKS);
    let stream = device
        .build_output_stream(
            config,
            {
                let line = line.clone();
                let mut tap = Tap { incoming, current: None };
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    feed(&line, &mut tap, data, channels)
                }
            },
            |_error| {},
            None,
        )
        .map_err(|error| error.to_string())?;
    stream.play().map_err(|error| error.to_string())?;
    Ok(Output { stream, rate, channels, chunks, line })
}

/// The callback's end of the channel, and the chunk it is part way through.
struct Tap {
    incoming: Receiver<Chunk>,
    current: Option<(Chunk, usize)>,
}

/// The card's callback. It never blocks and never allocates: chunks arrive
/// ready to copy, and a chunk from before the last seek is thrown away.
fn feed(line: &Line, tap: &mut Tap, data: &mut [f32], channels: usize) {
    let generation = line.generation.load(Ordering::Acquire);
    let mut written = 0;
    while written < data.len() {
        if tap.current.as_ref().is_some_and(|(chunk, offset)| {
            chunk.generation != generation || *offset >= chunk.samples.len()
        }) {
            tap.current = None;
        }
        if tap.current.is_none() {
            match tap.incoming.try_recv() {
                Ok(chunk) => tap.current = Some((chunk, 0)),
                Err(_) => break,
            }
            continue;
        }
        let Some((chunk, offset)) = tap.current.as_mut() else { break };
        let take = (chunk.samples.len() - *offset).min(data.len() - written);
        data[written..written + take].copy_from_slice(&chunk.samples[*offset..*offset + take]);
        *offset += take;
        written += take;
    }
    if written < data.len() {
        data[written..].fill(0.0);
        if line.draining.load(Ordering::Acquire) {
            line.exhausted.store(true, Ordering::Release);
        }
    }
    let frames = written / channels.max(1);
    line.played_frames.fetch_add(frames as u64, Ordering::Relaxed);
    if let Ok(mut recent) = line.recent.try_lock() {
        for frame in data[..written].chunks(channels.max(1)) {
            let mono = frame.iter().take(2).sum::<f32>() / frame.len().min(2) as f32;
            if recent.len() == FFT_SIZE {
                recent.pop_front();
            }
            recent.push_back(mono);
        }
    }
}

/// The decode loop: blocks on commands while paused, keeps the channel
/// topped up while playing, and answers seeks.
fn run(path: &Path, commands: Receiver<Command>, shared: Arc<Shared>) {
    let mut source = match AudioSource::open(path) {
        Ok(source) => source,
        Err(error) => {
            fail(&shared, error.to_string());
            return;
        }
    };
    let duration = source.info.duration;
    let mut output: Option<Output> = None;
    let mut resampler = Resampler::default();
    let mut chunk = Vec::new();
    let mut at_end = false;
    // Where the current run of played frames started.
    let mut base = Duration::ZERO;
    let mut spectrum = Spectrum::new();
    let mut last_tick = Instant::now();

    loop {
        let playing = shared.playing.load(Ordering::Relaxed);
        let command = if playing {
            commands.try_recv().ok()
        } else if spectrum.settled() {
            // Nothing to draw and nothing to decode: sleep until asked.
            match commands.recv() {
                Ok(command) => Some(command),
                Err(_) => return,
            }
        } else {
            match commands.recv_timeout(TICK) {
                Ok(command) => Some(command),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };
        match command {
            Some(Command::Stop) => return,
            Some(Command::Play) => {
                if output.is_none() {
                    match open_output() {
                        Ok(opened) => output = Some(opened),
                        Err(error) => {
                            fail(&shared, error);
                            continue;
                        }
                    }
                }
                if shared.finished.swap(false, Ordering::Relaxed) || at_end {
                    // Play again from the top.
                    if let Some(output) = &output {
                        restart_line(&output.line);
                    }
                    let _ = source.seek(Duration::ZERO);
                    resampler = Resampler::default();
                    base = Duration::ZERO;
                    at_end = false;
                    shared.position_ms.store(0, Ordering::Relaxed);
                }
                shared.playing.store(true, Ordering::Relaxed);
                if let Some(output) = &output {
                    let _ = output.stream.play();
                }
            }
            Some(Command::Pause) => {
                shared.playing.store(false, Ordering::Relaxed);
                if let Some(output) = &output {
                    let _ = output.stream.pause();
                }
            }
            Some(Command::Seek(position)) => {
                let position = duration.map_or(position, |duration| position.min(duration));
                if source.seek(position).is_ok() {
                    if let Some(output) = &output {
                        restart_line(&output.line);
                    }
                    resampler = Resampler::default();
                    base = position;
                    at_end = false;
                    shared.finished.store(false, Ordering::Relaxed);
                    shared.position_ms.store(position.as_millis() as u64, Ordering::Relaxed);
                }
            }
            None => {}
        }

        let Some(out) = output.as_ref().filter(|_| shared.playing.load(Ordering::Relaxed)) else {
            if last_tick.elapsed() >= TICK {
                last_tick = Instant::now();
                spectrum.publish_silence(&shared);
            }
            continue;
        };

        // Decode one chunk and offer it; a full channel means the card is
        // well ahead, so wait a little rather than spin.
        if !at_end {
            match source.next_chunk(&mut chunk) {
                Ok(true) => {
                    let samples = resampler.convert(
                        &chunk,
                        source.info.channels.max(1),
                        source.info.sample_rate.max(1),
                        out.channels,
                        out.rate,
                    );
                    let mut pending =
                        Chunk { generation: out.line.generation.load(Ordering::Acquire), samples };
                    // A full channel means the card is half a second ahead;
                    // a command that arrives meanwhile waits at most one
                    // chunk's worth, since the card keeps draining.
                    loop {
                        match out.chunks.try_send(pending) {
                            Ok(()) => break,
                            Err(TrySendError::Full(back)) => {
                                pending = back;
                                tick(&mut spectrum, &mut last_tick, out, &shared, base, duration);
                                thread::sleep(Duration::from_millis(10));
                            }
                            Err(TrySendError::Disconnected(_)) => break,
                        }
                    }
                }
                Ok(false) => {
                    at_end = true;
                    out.line.draining.store(true, Ordering::Release);
                }
                Err(error) => {
                    fail(&shared, error.to_string());
                    at_end = true;
                    out.line.draining.store(true, Ordering::Release);
                }
            }
        } else {
            thread::sleep(Duration::from_millis(10));
        }

        tick(&mut spectrum, &mut last_tick, out, &shared, base, duration);

        if at_end && out.line.exhausted.load(Ordering::Acquire) {
            shared.playing.store(false, Ordering::Relaxed);
            shared.finished.store(true, Ordering::Relaxed);
            if let Some(duration) = duration {
                shared.position_ms.store(duration.as_millis() as u64, Ordering::Relaxed);
            }
        }
    }
}

/// Position and spectrum, refreshed at the tick rate.
fn tick(
    spectrum: &mut Spectrum,
    last_tick: &mut Instant,
    out: &Output,
    shared: &Shared,
    base: Duration,
    duration: Option<Duration>,
) {
    let played = Duration::from_secs_f64(
        out.line.played_frames.load(Ordering::Relaxed) as f64 / f64::from(out.rate),
    );
    let position = duration.map_or(base + played, |duration| (base + played).min(duration));
    shared.position_ms.store(position.as_millis() as u64, Ordering::Relaxed);
    if last_tick.elapsed() >= TICK {
        *last_tick = Instant::now();
        let recent =
            out.line.recent.lock().map(|recent| recent.iter().copied().collect::<Vec<_>>());
        if let Ok(recent) = recent {
            spectrum.publish(&recent, out.rate, shared);
        }
    }
}

fn fail(shared: &Shared, message: String) {
    if let Ok(mut error) = shared.error.lock() {
        *error = Some(message);
    }
    shared.playing.store(false, Ordering::Relaxed);
}

/// Forget what was queued: bump the generation so the callback drops it,
/// and start counting frames again.
fn restart_line(line: &Line) {
    line.generation.fetch_add(1, Ordering::AcqRel);
    line.played_frames.store(0, Ordering::Relaxed);
    line.draining.store(false, Ordering::Release);
    line.exhausted.store(false, Ordering::Release);
}

/// Linear resampling and channel mapping from the file's layout to the
/// card's. Linear is audibly fine for a preview and needs no window of
/// history beyond one frame, which is what makes seeking a plain reset.
#[derive(Default)]
struct Resampler {
    /// Position within the source, in source frames, carried across chunks.
    cursor: f64,
    /// The last frame of the previous chunk, as (left, right).
    tail: Option<(f32, f32)>,
}

impl Resampler {
    fn convert(
        &mut self,
        input: &[f32],
        in_channels: usize,
        in_rate: u32,
        out_channels: usize,
        out_rate: u32,
    ) -> Vec<f32> {
        let frames = input.chunks(in_channels).map(stereo).collect::<Vec<_>>();
        let mut out = Vec::with_capacity(frames.len() * out_channels * 2);
        if frames.is_empty() {
            return out;
        }
        let step = f64::from(in_rate) / f64::from(out_rate);
        // Frame -1 is the previous chunk's last frame, so the seam between
        // chunks interpolates like everywhere else.
        let at = |index: i64| -> (f32, f32) {
            if index < 0 {
                self.tail.unwrap_or(frames[0])
            } else {
                frames[(index as usize).min(frames.len() - 1)]
            }
        };
        while self.cursor < frames.len() as f64 {
            let base = self.cursor.floor();
            let fraction = (self.cursor - base) as f32;
            let index = base as i64 - 1;
            let (a, b) = (at(index), at(index + 1));
            let left = a.0 + (b.0 - a.0) * fraction;
            let right = a.1 + (b.1 - a.1) * fraction;
            write_frame(&mut out, out_channels, left, right);
            self.cursor += step;
        }
        self.cursor -= frames.len() as f64;
        self.tail = frames.last().copied();
        out
    }
}

/// The first two channels of a frame, or the one channel twice.
fn stereo(frame: &[f32]) -> (f32, f32) {
    match frame {
        [] => (0.0, 0.0),
        [mono] => (*mono, *mono),
        [left, right, ..] => (*left, *right),
    }
}

fn write_frame(out: &mut Vec<f32>, channels: usize, left: f32, right: f32) {
    match channels {
        0 => {}
        1 => out.push((left + right) * 0.5),
        _ => {
            out.push(left);
            out.push(right);
            out.extend(std::iter::repeat_n(0.0, channels - 2));
        }
    }
}

/// The spectrum bars: a Hann-windowed FFT of the last samples played,
/// grouped into log-spaced bands, in decibels scaled to `0..=1`, with
/// a fast rise and a slow fall so they read as bouncing rather than
/// flickering.
struct Spectrum {
    planner: RealFftPlanner<f32>,
    window: Vec<f32>,
    bars: [f32; SPECTRUM_BARS],
}

impl Spectrum {
    fn new() -> Self {
        let window = (0..FFT_SIZE)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / (FFT_SIZE - 1) as f32).cos())
            .collect();
        Self { planner: RealFftPlanner::new(), window, bars: [0.0; SPECTRUM_BARS] }
    }

    fn publish(&mut self, recent: &[f32], rate: u32, shared: &Shared) {
        if recent.len() < FFT_SIZE {
            self.publish_silence(shared);
            return;
        }
        let fft = self.planner.plan_fft_forward(FFT_SIZE);
        let mut input = recent[recent.len() - FFT_SIZE..]
            .iter()
            .zip(&self.window)
            .map(|(sample, weight)| sample * weight)
            .collect::<Vec<_>>();
        let mut output = vec![Complex::new(0.0_f32, 0.0); FFT_SIZE / 2 + 1];
        if fft.process(&mut input, &mut output).is_err() {
            return;
        }
        let bin_hz = rate as f32 / FFT_SIZE as f32;
        let ratio = HIGHEST_BAR_HZ / LOWEST_BAR_HZ;
        let magnitudes = output.iter().map(|value| value.norm() / FFT_SIZE as f32 * 2.0);
        let magnitudes = magnitudes.collect::<Vec<_>>();
        for (index, bar) in self.bars.iter_mut().enumerate() {
            let low = LOWEST_BAR_HZ * ratio.powf(index as f32 / SPECTRUM_BARS as f32);
            let high = LOWEST_BAR_HZ * ratio.powf((index + 1) as f32 / SPECTRUM_BARS as f32);
            let first = ((low / bin_hz) as usize).min(magnitudes.len() - 1);
            let last = ((high / bin_hz) as usize).clamp(first, magnitudes.len() - 1);
            let peak = magnitudes[first..=last].iter().copied().fold(0.0_f32, f32::max);
            let decibels = 20.0 * (peak + 1e-6).log10();
            let level = ((decibels + 60.0) / 60.0).clamp(0.0, 1.0);
            *bar = if level > *bar { level } else { *bar * 0.82 + level * 0.18 };
        }
        if let Ok(mut bars) = shared.spectrum.lock() {
            *bars = self.bars;
        }
    }

    /// Every bar is down: nothing left to animate.
    fn settled(&self) -> bool {
        self.bars.iter().all(|bar| *bar == 0.0)
    }

    fn publish_silence(&mut self, shared: &Shared) {
        let mut moved = false;
        for bar in &mut self.bars {
            if *bar > 0.001 {
                *bar *= 0.82;
                moved = true;
            } else {
                *bar = 0.0;
            }
        }
        if moved && let Ok(mut bars) = shared.spectrum.lock() {
            *bars = self.bars;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_keeps_length_proportional_and_maps_channels() {
        let mut resampler = Resampler::default();
        let mono = (0..100).map(|i| i as f32 / 100.0).collect::<Vec<_>>();
        let out = resampler.convert(&mono, 1, 24_000, 2, 48_000);
        // Twice the frames, two channels, both carrying the same signal.
        assert_eq!(out.len(), 400);
        assert!(out.chunks(2).all(|frame| frame[0] == frame[1]));
        assert!(out.windows(2).step_by(2).all(|pair| pair[0] <= pair[1] + 0.02));

        let mut resampler = Resampler::default();
        let stereo = vec![1.0, -1.0, 1.0, -1.0];
        let out = resampler.convert(&stereo, 2, 48_000, 1, 48_000);
        assert_eq!(out, vec![0.0, 0.0]);

        let mut resampler = Resampler::default();
        let out = resampler.convert(&stereo, 2, 48_000, 6, 48_000);
        assert_eq!(out.len(), 12);
        assert_eq!(&out[..6], &[1.0, -1.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn a_tone_lights_the_bars_around_its_pitch() {
        let rate = 48_000;
        let recent = (0..FFT_SIZE)
            .map(|i| (std::f32::consts::TAU * 1_000.0 * i as f32 / rate as f32).sin())
            .collect::<Vec<_>>();
        let shared = Shared::default();
        let mut spectrum = Spectrum::new();
        spectrum.publish(&recent, rate, &shared);
        let bars = shared.spectrum();
        let loudest = bars.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        let ratio = HIGHEST_BAR_HZ / LOWEST_BAR_HZ;
        let low = LOWEST_BAR_HZ * ratio.powf(loudest as f32 / SPECTRUM_BARS as f32);
        let high = LOWEST_BAR_HZ * ratio.powf((loudest + 1) as f32 / SPECTRUM_BARS as f32);
        // Bins are ~47 Hz wide at this rate and bars near 1 kHz ~130 Hz, so
        // the peak bin can belong to either of two neighbouring bars.
        assert!(low <= 1_150.0 && 870.0 <= high, "loudest bar spans {low}..{high}");
        assert!(bars[0] < bars[loudest]);

        spectrum.publish_silence(&shared);
        assert!(shared.spectrum()[loudest] < bars[loudest]);
    }

    #[test]
    fn the_callback_copies_chunks_drops_stale_ones_and_counts_frames() {
        let line = Line {
            generation: AtomicU32::new(1),
            played_frames: AtomicU64::new(0),
            recent: Mutex::new(std::collections::VecDeque::new()),
            draining: AtomicBool::new(true),
            exhausted: AtomicBool::new(false),
        };
        let (sender, incoming) = mpsc::sync_channel(4);
        sender.send(Chunk { generation: 0, samples: vec![9.0; 6] }).unwrap();
        sender.send(Chunk { generation: 1, samples: vec![0.0, 1.0, 2.0, 3.0] }).unwrap();
        sender.send(Chunk { generation: 1, samples: vec![4.0, 5.0, 6.0, 7.0] }).unwrap();
        let mut tap = Tap { incoming, current: None };

        let mut data = [9.0_f32; 6];
        feed(&line, &mut tap, &mut data, 2);
        assert_eq!(data, [0.0, 1.0, 2.0, 3.0, 4.0, 5.0], "stale chunk skipped, seam crossed");
        assert_eq!(line.played_frames.load(Ordering::Relaxed), 3);
        assert!(!line.exhausted.load(Ordering::Relaxed));

        let mut data = [9.0_f32; 6];
        feed(&line, &mut tap, &mut data, 2);
        assert_eq!(data, [6.0, 7.0, 0.0, 0.0, 0.0, 0.0], "the rest, then silence");
        assert_eq!(line.played_frames.load(Ordering::Relaxed), 4);
        assert!(line.exhausted.load(Ordering::Relaxed), "ran dry while draining");
        assert_eq!(line.recent.lock().unwrap().len(), 4);
    }
}
