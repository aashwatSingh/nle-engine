//! Video-side playback: decodes ahead and paces itself against the audio
//! clock (spec 4.4: audio is master, video is scheduled against it). This
//! is the "Scheduler" role from docs/architecture.md's play-press-to-pixels
//! walkthrough, scoped down to a single clip for M2 — no render graph, no
//! GPU-resident ring buffer of composited frames, just "what's the right
//! frame to show right now."
//!
//! `latest_frame()` clones the decoded RGBA buffer out of a mutex on every
//! call — fine for a demo at modest resolution, but exactly the
//! clone-per-frame cost the real compositor (M4) avoids by keeping frames
//! as GPU texture handles end to end.

use media_ffmpeg::VideoDecoderStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use timeline::TIMEBASE;

#[derive(Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<Vec<u8>>,
    pub pts_ticks: i64,
}

enum Command {
    Play,
    Pause,
    Seek(i64),
}

/// How far behind the clock a decoded frame can be before we give up on
/// showing it and decode the next one instead (spec 4.4's "when behind,
/// drop composited frames — never drop audio, never stall").
const DROP_THRESHOLD_TICKS: i64 = TIMEBASE / 10; // 100ms

pub struct VideoPlayback {
    latest_frame: Arc<Mutex<Option<VideoFrame>>>,
    dropped_frames: Arc<AtomicU64>,
    command_tx: mpsc::Sender<Command>,
}

impl VideoPlayback {
    /// `clock` is called from the decode-ahead thread to read the current
    /// authoritative tick (in practice, `AudioEngine::current_tick`).
    pub fn start(
        path: PathBuf,
        clock: impl Fn() -> i64 + Send + 'static,
    ) -> Result<Self, String> {
        let latest_frame = Arc::new(Mutex::new(None));
        let dropped_frames = Arc::new(AtomicU64::new(0));
        let (command_tx, command_rx) = mpsc::channel::<Command>();

        let thread_frame = latest_frame.clone();
        let thread_dropped = dropped_frames.clone();
        let playing = Arc::new(AtomicBool::new(true));
        let seek_target = Arc::new(AtomicI64::new(-1));
        let thread_playing = playing.clone();
        let thread_seek_target = seek_target.clone();

        std::thread::spawn(move || {
            let mut stream = match VideoDecoderStream::open(&path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("video decode thread failed to open {path:?}: {e:?}");
                    return;
                }
            };

            'outer: loop {
                while let Ok(cmd) = command_rx.try_recv() {
                    match cmd {
                        Command::Play => thread_playing.store(true, Ordering::SeqCst),
                        Command::Pause => thread_playing.store(false, Ordering::SeqCst),
                        Command::Seek(ticks) => thread_seek_target.store(ticks, Ordering::SeqCst),
                    }
                }

                let pending_seek = thread_seek_target.swap(-1, Ordering::SeqCst);
                if pending_seek >= 0 {
                    let _ = stream.seek(pending_seek);
                }

                if !thread_playing.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }

                let frame = match stream.next_frame() {
                    Ok(Some(f)) => f,
                    Ok(None) => {
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                    Err(e) => {
                        eprintln!("video decode error: {e:?}");
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                };

                // Pace against the clock: wait until we're close to this
                // frame's presentation time, but re-check commands/seek
                // while waiting so a seek during a long wait isn't stuck.
                loop {
                    while let Ok(cmd) = command_rx.try_recv() {
                        match cmd {
                            Command::Play => thread_playing.store(true, Ordering::SeqCst),
                            Command::Pause => thread_playing.store(false, Ordering::SeqCst),
                            Command::Seek(ticks) => {
                                let _ = stream.seek(ticks);
                                continue 'outer;
                            }
                        }
                    }
                    let now = clock();
                    if frame.pts_ticks <= now + TIMEBASE / 60 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }

                let now = clock();
                if now - frame.pts_ticks > DROP_THRESHOLD_TICKS {
                    thread_dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }

                *thread_frame.lock().unwrap() = Some(VideoFrame {
                    width: frame.width,
                    height: frame.height,
                    rgba: Arc::new(frame.rgba),
                    pts_ticks: frame.pts_ticks,
                });
            }
        });

        Ok(VideoPlayback { latest_frame, dropped_frames, command_tx })
    }

    pub fn latest_frame(&self) -> Option<VideoFrame> {
        self.latest_frame.lock().unwrap().clone()
    }

    pub fn dropped_frame_count(&self) -> u64 {
        self.dropped_frames.load(Ordering::Relaxed)
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
}
