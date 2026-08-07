//! Real-time audio playback, per spec 4.4: audio is the master clock, and
//! the actual hardware callback must never allocate, lock, or block (spec
//! 4.6). The decode-ahead thread does all the real work (decoding,
//! resampling, pushing into the ring buffer) off the real-time path; the
//! CPAL callback only pops from a lock-free ring buffer and does atomic
//! bookkeeping.
//!
//! Known, documented imprecision (see `seek`): a seek can leave up to one
//! ring-buffer-fill's worth of stale or slightly-early audio audible before
//! the callback catches up to the new seek epoch. Acceptable for this M2
//! proof; not a v1.0 guarantee.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use media_ffmpeg::AudioDecoderStream;
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::HeapRb;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use timeline::TIMEBASE;

enum TransportCommand {
    Play,
    Pause,
    Seek(i64),
}

struct ClockState {
    consumed_frames: AtomicU64,
    base_ticks: AtomicI64,
    seek_epoch: AtomicU64,
    underruns: AtomicU64,
    playing: AtomicBool,
    /// Set once the decode-ahead thread hits real end-of-stream. Distinct
    /// from an underrun: silence after the source is legitimately exhausted
    /// is expected behavior, not the decoder falling behind, so it must not
    /// spam the underrun counter (caught by actually running this to the
    /// end of a clip — see docs/decisions-log.md).
    ended: AtomicBool,
}

/// A cheap, genuinely `Send + Sync` handle to the playback clock — separate
/// from `AudioEngine` because `AudioEngine` owns a `cpal::Stream`, and CPAL
/// deliberately makes `Stream` `!Send`/`!Sync` on Windows (it wraps COM
/// objects, which aren't safely shareable across threads without care).
/// The video decode-ahead thread needs to read the clock but must never
/// touch the stream itself, so it gets this instead of the whole engine.
#[derive(Clone)]
pub struct AudioClock {
    clock: Arc<ClockState>,
    sample_rate: u32,
}

impl AudioClock {
    pub fn current_tick(&self) -> i64 {
        let frames = self.clock.consumed_frames.load(Ordering::Relaxed);
        let base = self.clock.base_ticks.load(Ordering::Relaxed);
        base + (frames as i128 * TIMEBASE as i128 / self.sample_rate as i128) as i64
    }
}

pub struct AudioEngine {
    _stream: cpal::Stream,
    command_tx: mpsc::Sender<TransportCommand>,
    clock: Arc<ClockState>,
    sample_rate: u32,
}

impl AudioEngine {
    pub fn start(path: PathBuf, start_ticks: i64) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("no output device")?;
        let config = device.default_output_config().map_err(|e| e.to_string())?;
        let sample_rate = config.sample_rate().0;
        let channels = config.channels() as usize;

        let mut decoder = AudioDecoderStream::open(&path, sample_rate, config.channels())
            .map_err(|e| format!("{e:?}"))?;
        let _ = decoder.seek(start_ticks);

        // ~2 seconds of headroom between the decode-ahead thread and the
        // real-time callback.
        let rb = HeapRb::<f32>::new(sample_rate as usize * channels * 2);
        let (mut producer, mut consumer) = rb.split();

        let clock = Arc::new(ClockState {
            consumed_frames: AtomicU64::new(0),
            base_ticks: AtomicI64::new(start_ticks),
            seek_epoch: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            playing: AtomicBool::new(true),
            ended: AtomicBool::new(false),
        });

        let (command_tx, command_rx) = mpsc::channel::<TransportCommand>();

        let decode_clock = clock.clone();
        std::thread::spawn(move || {
            let mut pending: Vec<f32> = Vec::new();
            let mut pending_offset = 0usize;
            loop {
                while let Ok(cmd) = command_rx.try_recv() {
                    match cmd {
                        TransportCommand::Play => decode_clock.playing.store(true, Ordering::SeqCst),
                        TransportCommand::Pause => decode_clock.playing.store(false, Ordering::SeqCst),
                        TransportCommand::Seek(ticks) => {
                            let _ = decoder.seek(ticks);
                            pending.clear();
                            pending_offset = 0;
                            decode_clock.base_ticks.store(ticks, Ordering::SeqCst);
                            decode_clock.consumed_frames.store(0, Ordering::SeqCst);
                            decode_clock.seek_epoch.fetch_add(1, Ordering::SeqCst);
                            decode_clock.ended.store(false, Ordering::SeqCst);
                        }
                    }
                }
                if !decode_clock.playing.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                if pending_offset >= pending.len() {
                    match decoder.next_samples() {
                        Ok(Some(samples)) => {
                            pending = samples;
                            pending_offset = 0;
                        }
                        Ok(None) => {
                            decode_clock.ended.store(true, Ordering::SeqCst);
                            std::thread::sleep(std::time::Duration::from_millis(20));
                            continue;
                        }
                        Err(_) => {
                            std::thread::sleep(std::time::Duration::from_millis(20));
                            continue;
                        }
                    }
                }
                let pushed = producer.push_slice(&pending[pending_offset..]);
                pending_offset += pushed;
                if pushed == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        });

        let callback_clock = clock.clone();
        let mut local_epoch = 0u64;
        let stream = device
            .build_output_stream(
                &config.into(),
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    // Paused: output silence without touching the consumer
                    // or the clock, so whatever's already buffered (ahead of
                    // the current position) is still there — untouched and
                    // ready — the instant playback resumes.
                    if !callback_clock.playing.load(Ordering::Relaxed) {
                        data.fill(0.0);
                        return;
                    }
                    let epoch_now = callback_clock.seek_epoch.load(Ordering::SeqCst);
                    if epoch_now != local_epoch {
                        while consumer.try_pop().is_some() {}
                        local_epoch = epoch_now;
                    }
                    let popped = consumer.pop_slice(data);
                    if popped < data.len() {
                        for s in &mut data[popped..] {
                            *s = 0.0;
                        }
                        if !callback_clock.ended.load(Ordering::Relaxed) {
                            callback_clock.underruns.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    callback_clock
                        .consumed_frames
                        .fetch_add((popped / channels) as u64, Ordering::Relaxed);
                },
                move |err| eprintln!("audio stream error: {err}"),
                None,
            )
            .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;

        Ok(AudioEngine { _stream: stream, command_tx, clock, sample_rate })
    }

    /// The authoritative playhead: derived from samples actually consumed
    /// by the real hardware callback, not a UI-thread timer.
    pub fn current_tick(&self) -> i64 {
        self.clock().current_tick()
    }

    /// A `Send + Sync` handle for other threads (e.g. video decode-ahead)
    /// that only need to read the clock, not control the audio stream.
    pub fn clock(&self) -> AudioClock {
        AudioClock { clock: self.clock.clone(), sample_rate: self.sample_rate }
    }

    pub fn play(&self) {
        let _ = self.command_tx.send(TransportCommand::Play);
    }

    pub fn pause(&self) {
        let _ = self.command_tx.send(TransportCommand::Pause);
    }

    pub fn seek(&self, ticks: i64) {
        let _ = self.command_tx.send(TransportCommand::Seek(ticks));
    }

    pub fn is_playing(&self) -> bool {
        self.clock.playing.load(Ordering::Relaxed)
    }

    pub fn underrun_count(&self) -> u64 {
        self.clock.underruns.load(Ordering::Relaxed)
    }

    pub fn has_ended(&self) -> bool {
        self.clock.ended.load(Ordering::Relaxed)
    }
}
