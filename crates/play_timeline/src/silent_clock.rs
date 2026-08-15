//! A wall-clock-driven playback clock, for when none of the user's selected
//! files have an audio track. Spec 4.4 makes audio the master clock because
//! audio glitches are far more perceptible than a dropped video frame — but
//! that assumes there IS audio. A real user picking their own files (a
//! silent screen recording, a muted phone clip) can easily have none, and
//! `AudioEngine::start` would fail outright on a file with no audio stream.
//! This is the fallback: same `current_tick`/`play`/`pause`/`seek` shape as
//! `playback::AudioEngine`, driven by `Instant::now()` instead of samples
//! consumed. Never used when real audio is available — see `main.rs`'s
//! `Control` enum.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use timeline::TIMEBASE;

struct State {
    base_ticks: AtomicI64,
    anchor: Mutex<Instant>,
    playing: AtomicBool,
}

pub struct SilentClock {
    state: Arc<State>,
}

#[derive(Clone)]
pub struct SilentClockHandle {
    state: Arc<State>,
}

impl SilentClock {
    pub fn new(start_ticks: i64) -> Self {
        SilentClock {
            state: Arc::new(State {
                base_ticks: AtomicI64::new(start_ticks),
                anchor: Mutex::new(Instant::now()),
                playing: AtomicBool::new(true),
            }),
        }
    }

    pub fn handle(&self) -> SilentClockHandle {
        SilentClockHandle { state: self.state.clone() }
    }

    pub fn current_tick(&self) -> i64 {
        current_tick(&self.state)
    }

    pub fn play(&self) {
        if !self.state.playing.swap(true, Ordering::SeqCst) {
            *self.state.anchor.lock().unwrap() = Instant::now();
        }
    }

    pub fn pause(&self) {
        if self.state.playing.swap(false, Ordering::SeqCst) {
            let frozen = current_tick(&self.state);
            self.state.base_ticks.store(frozen, Ordering::SeqCst);
        }
    }

    pub fn seek(&self, ticks: i64) {
        self.state.base_ticks.store(ticks, Ordering::SeqCst);
        *self.state.anchor.lock().unwrap() = Instant::now();
    }
}

impl SilentClockHandle {
    pub fn current_tick(&self) -> i64 {
        current_tick(&self.state)
    }
}

fn current_tick(state: &State) -> i64 {
    let base = state.base_ticks.load(Ordering::SeqCst);
    if !state.playing.load(Ordering::SeqCst) {
        return base;
    }
    let elapsed = state.anchor.lock().unwrap().elapsed();
    base + (elapsed.as_secs_f64() * TIMEBASE as f64) as i64
}
