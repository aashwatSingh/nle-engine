//! Real-time audio playback of an *edited sequence* — the thing that lets you
//! hear your cut while you work.
//!
//! `AudioEngine` (this module's sibling) plays one file start to finish and
//! has no notion of cuts. This plays the timeline: a mixer thread pulls blocks
//! from `audio::mix_range` — per-clip gain and pan, track mute/solo, master
//! gain, clips summed across tracks — and pushes them into a lock-free ring
//! buffer that the CPAL callback drains. The real-time callback still only
//! pops and does atomic bookkeeping: no allocation, no locks, no blocking
//! (spec 4.6's hard rule).
//!
//! It doubles as the **master clock** (spec 4.4: audio is master). The
//! playhead the UI draws comes from samples the hardware actually consumed,
//! not a UI-thread timer, so picture follows sound rather than the two
//! drifting apart. That holds even for a sequence with no audio clips: the mix
//! is then silence, but the clock still advances correctly, so the same code
//! path drives the transport either way.
//!
//! The seek handshake (epoch bump, callback flushes, mixer waits for the ack)
//! is lifted from `AudioEngine` because the same race applies: mixing runs far
//! faster than real time, so without it a post-seek flush can discard the
//! fresh audio it was meant to make room for. See that module's doc comment.
//!
//! **Snapshot semantics.** The project is captured by `Arc` when playback
//! starts and does not change until the next play/seek. Editing during
//! playback therefore won't be heard until you restart it. Live re-mixing on
//! edit is what Premiere does, and it's a real follow-up — but it needs the
//! mixer thread to safely swap project versions mid-block and to invalidate
//! only the affected range, which is a bigger design than "hear your edit"
//! needs to get right first.

use audio::{MixOptions, SampleSource};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::HeapRb;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use timeline::{Project, SequenceId, TIMEBASE};

/// Frames mixed per block. ~43 ms at 48 kHz: big enough that the per-block
/// overhead (evaluating keyframes, locating active clips) is negligible, small
/// enough that a seek doesn't have to discard much work.
const MIX_BLOCK_FRAMES: u64 = 2048;

enum Command {
    Play,
    Pause,
    Seek(i64),
}

/// Levels for one mixed block: the master bus plus every audible track.
#[derive(Clone, Default, Debug)]
pub struct MeterSnapshot {
    pub master: audio::PeakRms,
    pub tracks: Vec<(timeline::TrackId, audio::PeakRms)>,
}

/// Meter readings tagged with the output frame position they describe.
///
/// The mixer deliberately runs *ahead* of real time — that's what the ring
/// buffer is for — so simply publishing the newest reading would show levels
/// for audio the user won't hear for another few hundred milliseconds. Meters
/// that lead the sound by that much read as broken. Tagging each block with the
/// position it covers lets `meters()` return the levels of what the hardware is
/// actually playing *now*, matched against the same consumed-frames counter the
/// playhead uses.
///
/// A `Mutex` is fine here where it would not be in the CPAL callback: both
/// writer (mixer thread) and reader (UI thread) are ordinary threads, the
/// critical section is a small push and pop, and the audio callback never
/// touches it.
struct MeterHistory {
    entries: Mutex<VecDeque<(u64, MeterSnapshot)>>,
}

/// Blocks of meter history kept. At 2048 frames per block and 48kHz that's
/// ~2.7s — comfortably longer than any sane output buffer, so the reading the
/// UI wants is always still present.
const METER_HISTORY_BLOCKS: usize = 64;

impl MeterHistory {
    fn new() -> Self {
        MeterHistory { entries: Mutex::new(VecDeque::new()) }
    }

    fn push(&self, position: u64, snapshot: MeterSnapshot) {
        let mut q = self.entries.lock().unwrap();
        q.push_back((position, snapshot));
        while q.len() > METER_HISTORY_BLOCKS {
            q.pop_front();
        }
    }

    fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }

    /// Levels for the block containing `consumed_frames`: the latest reading at
    /// or before it. `None` before anything has been mixed.
    fn at(&self, consumed_frames: u64) -> Option<MeterSnapshot> {
        let q = self.entries.lock().unwrap();
        q.iter()
            .rev()
            .find(|(pos, _)| *pos <= consumed_frames)
            .or_else(|| q.front())
            .map(|(_, s)| s.clone())
    }
}

struct ClockState {
    consumed_frames: AtomicU64,
    base_ticks: AtomicI64,
    seek_epoch: AtomicU64,
    flush_acked_epoch: AtomicU64,
    underruns: AtomicU64,
    playing: AtomicBool,
    /// Mixer reached the end of the sequence. Distinct from an underrun:
    /// silence past the end is correct, not the mixer falling behind, and
    /// counting it as an underrun would make the stat useless.
    ended: AtomicBool,
}

/// `Send + Sync` read-only view of the clock, for the UI thread and the video
/// decode-ahead path. Separate from the engine because the engine owns a
/// `cpal::Stream`, which CPAL deliberately makes `!Send`/`!Sync`.
#[derive(Clone)]
pub struct SequenceClock {
    clock: Arc<ClockState>,
    sample_rate: u32,
}

impl SequenceClock {
    /// Position in the sequence, derived from frames the hardware consumed.
    pub fn current_tick(&self) -> i64 {
        let frames = self.clock.consumed_frames.load(Ordering::Relaxed);
        let base = self.clock.base_ticks.load(Ordering::Relaxed);
        base + (frames as i128 * TIMEBASE as i128 / self.sample_rate as i128) as i64
    }

    pub fn has_ended(&self) -> bool {
        self.clock.ended.load(Ordering::Relaxed)
    }
}

pub struct SequenceAudioEngine {
    _stream: cpal::Stream,
    command_tx: mpsc::Sender<Command>,
    clock: Arc<ClockState>,
    sample_rate: u32,
    meters: Arc<MeterHistory>,
}

impl SequenceAudioEngine {
    /// Opens the default output device and starts mixing `sequence` from
    /// `start_ticks`. Playing immediately; call `pause` to hold.
    pub fn start(
        project: Arc<Project>,
        sequence: SequenceId,
        asset_paths: HashMap<media::MediaAssetId, PathBuf>,
        start_ticks: i64,
        master_gain_db: f64,
    ) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("no output device")?;
        let config = device.default_output_config().map_err(|e| e.to_string())?;
        let sample_rate = config.sample_rate().0;
        let device_channels = config.channels() as usize;

        let rb = HeapRb::<f32>::new(sample_rate as usize * device_channels * 2);
        let (mut producer, mut consumer) = rb.split();

        let clock = Arc::new(ClockState {
            consumed_frames: AtomicU64::new(0),
            base_ticks: AtomicI64::new(start_ticks),
            seek_epoch: AtomicU64::new(0),
            flush_acked_epoch: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            playing: AtomicBool::new(true),
            ended: AtomicBool::new(false),
        });

        let (command_tx, command_rx) = mpsc::channel::<Command>();
        let mix_clock = clock.clone();
        let meters = Arc::new(MeterHistory::new());
        let mix_meters = meters.clone();

        std::thread::spawn(move || {
            let mut source = audio_source::DecodedSampleSource::new(asset_paths);
            let options = MixOptions { sample_rate, master_gain_db };
            let total_frames = audio::total_frames(&project, sequence, sample_rate);
            // Absolute audio frame index into the sequence that the mixer has
            // produced up to. Tracked in samples rather than re-derived from
            // ticks each block so rounding can't accumulate into drift.
            let mut position = frames_at(start_ticks, sample_rate);
            let mut pending: Vec<f32> = Vec::new();
            let mut pending_offset = 0usize;

            loop {
                let mut disconnected = false;
                loop {
                    match command_rx.try_recv() {
                        Ok(Command::Play) => mix_clock.playing.store(true, Ordering::SeqCst),
                        Ok(Command::Pause) => mix_clock.playing.store(false, Ordering::SeqCst),
                        Ok(Command::Seek(ticks)) => {
                            pending.clear();
                            pending_offset = 0;
                            position = frames_at(ticks, sample_rate);
                            mix_clock.base_ticks.store(ticks, Ordering::SeqCst);
                            mix_clock.consumed_frames.store(0, Ordering::SeqCst);
                            mix_clock.ended.store(false, Ordering::SeqCst);
                            // Positions in the history refer to the old
                            // timeline base and would match the wrong blocks.
                            mix_meters.clear();

                            // Wait for the callback to actually flush before
                            // pushing post-seek audio — see the module doc.
                            let target =
                                mix_clock.seek_epoch.fetch_add(1, Ordering::SeqCst) + 1;
                            let deadline = std::time::Instant::now()
                                + std::time::Duration::from_millis(200);
                            while mix_clock.flush_acked_epoch.load(Ordering::SeqCst) < target
                                && std::time::Instant::now() < deadline
                            {
                                std::thread::sleep(std::time::Duration::from_millis(1));
                            }
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        // The engine was dropped; the stream is going away
                        // too, so stop rather than mixing into a dead ring.
                        Err(mpsc::TryRecvError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }
                if disconnected {
                    return;
                }

                if !mix_clock.playing.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }

                if pending_offset >= pending.len() {
                    if position >= total_frames {
                        mix_clock.ended.store(true, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        continue;
                    }
                    let want = MIX_BLOCK_FRAMES.min(total_frames - position);
                    let start_tick = ticks_at(position, sample_rate);
                    let end_tick = ticks_at(position + want, sample_rate);
                    let block_start_position = position;
                    let (stereo, mix_stats) = audio::mix_range(
                        &project,
                        sequence,
                        start_tick,
                        end_tick - start_tick,
                        &options,
                        &mut source as &mut dyn SampleSource,
                    );
                    let mixed_frames = (stereo.len() / audio::CHANNELS) as u64;
                    // Guard against a zero-length mix (a duration that rounds
                    // to no frames) turning into a spin that never advances.
                    if mixed_frames == 0 {
                        position += want.max(1);
                        continue;
                    }
                    // Positions are relative to the current seek base, exactly
                    // like `consumed_frames`, so the two are directly
                    // comparable in `meters()`.
                    mix_meters.push(
                        block_start_position - frames_at(
                            mix_clock.base_ticks.load(Ordering::SeqCst),
                            sample_rate,
                        ),
                        MeterSnapshot {
                            master: mix_stats.master_meter,
                            tracks: mix_stats.track_meters.clone(),
                        },
                    );
                    pending = to_device_channels(&stereo, device_channels);
                    pending_offset = 0;
                    position += mixed_frames;
                }

                let pushed = producer.push_slice(&pending[pending_offset..]);
                pending_offset += pushed;
                if pushed == 0 {
                    // Ring full: real time has caught up with us, which is the
                    // healthy steady state. Sleep briefly rather than spin.
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        });

        let cb_clock = clock.clone();
        let mut local_epoch = 0u64;
        let stream = device
            .build_output_stream(
                &config.into(),
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    if !cb_clock.playing.load(Ordering::Relaxed) {
                        data.fill(0.0);
                        return;
                    }
                    let epoch_now = cb_clock.seek_epoch.load(Ordering::SeqCst);
                    if epoch_now != local_epoch {
                        while consumer.try_pop().is_some() {}
                        local_epoch = epoch_now;
                        cb_clock.flush_acked_epoch.store(epoch_now, Ordering::SeqCst);
                    }
                    let popped = consumer.pop_slice(data);
                    if popped < data.len() {
                        data[popped..].fill(0.0);
                        if !cb_clock.ended.load(Ordering::Relaxed) {
                            cb_clock.underruns.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    cb_clock
                        .consumed_frames
                        .fetch_add((popped / device_channels) as u64, Ordering::Relaxed);
                },
                move |err| eprintln!("sequence audio stream error: {err}"),
                None,
            )
            .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;

        Ok(SequenceAudioEngine { _stream: stream, command_tx, clock, sample_rate, meters })
    }

    pub fn clock(&self) -> SequenceClock {
        SequenceClock { clock: self.clock.clone(), sample_rate: self.sample_rate }
    }

    pub fn current_tick(&self) -> i64 {
        self.clock().current_tick()
    }

    pub fn play(&self) {
        let _ = self.command_tx.send(Command::Play);
    }

    pub fn pause(&self) {
        let _ = self.command_tx.send(Command::Pause);
    }

    pub fn seek(&self, ticks: i64) {
        let _ = self.command_tx.send(Command::Seek(ticks));
    }

    pub fn is_playing(&self) -> bool {
        self.clock.playing.load(Ordering::Relaxed)
    }

    pub fn has_ended(&self) -> bool {
        self.clock.ended.load(Ordering::Relaxed)
    }

    /// Levels of the audio the hardware is playing right now — matched to the
    /// consumed-frames clock, not the mixer's read-ahead position. `None` until
    /// the first block has been mixed.
    pub fn meters(&self) -> Option<MeterSnapshot> {
        self.meters.at(self.clock.consumed_frames.load(Ordering::Relaxed))
    }

    pub fn underrun_count(&self) -> u64 {
        self.clock.underruns.load(Ordering::Relaxed)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

fn frames_at(ticks: i64, sample_rate: u32) -> u64 {
    (ticks.max(0) as i128 * sample_rate as i128 / TIMEBASE as i128) as u64
}

fn ticks_at(frames: u64, sample_rate: u32) -> i64 {
    (frames as i128 * TIMEBASE as i128 / sample_rate as i128) as i64
}

/// Maps the mixer's interleaved stereo onto the device's channel count.
///
/// Done here on the mixer thread, never in the callback — the callback must
/// not allocate. A mono device gets the average of L/R (rather than just the
/// left channel, which would silently drop anything panned right); a device
/// with more than two channels gets L/R in the first two and silence
/// elsewhere, which is the honest default without a surround panner.
fn to_device_channels(stereo: &[f32], device_channels: usize) -> Vec<f32> {
    match device_channels {
        2 => stereo.to_vec(),
        1 => stereo.chunks_exact(2).map(|f| (f[0] + f[1]) * 0.5).collect(),
        n => {
            let frames = stereo.len() / audio::CHANNELS;
            let mut out = vec![0.0; frames * n];
            for f in 0..frames {
                out[f * n] = stereo[f * 2];
                if n > 1 {
                    out[f * n + 1] = stereo[f * 2 + 1];
                }
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_and_frame_conversions_round_trip_at_block_boundaries() {
        let rate = 48_000;
        for frames in [0u64, 1, 2048, 48_000, 123_456] {
            let ticks = ticks_at(frames, rate);
            assert_eq!(frames_at(ticks, rate), frames, "round trip failed at {frames} frames");
        }
    }

    #[test]
    fn stereo_passes_through_unchanged_on_a_stereo_device() {
        let stereo = vec![0.1, 0.2, 0.3, 0.4];
        assert_eq!(to_device_channels(&stereo, 2), stereo);
    }

    #[test]
    fn a_mono_device_averages_both_channels_rather_than_dropping_one() {
        // Taking only the left channel would silently mute a hard-right pan.
        let stereo = vec![0.0, 1.0, 1.0, 0.0];
        assert_eq!(to_device_channels(&stereo, 1), vec![0.5, 0.5]);
    }

    #[test]
    fn a_surround_device_gets_l_r_in_the_first_two_channels() {
        let stereo = vec![0.5, -0.5];
        let out = to_device_channels(&stereo, 6);
        assert_eq!(out.len(), 6);
        assert_eq!(&out[..2], &[0.5, -0.5]);
        assert!(out[2..].iter().all(|s| *s == 0.0), "extra channels should be silent");
    }
}
