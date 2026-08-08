pub mod audio_engine;
pub mod video_playback;
pub use audio_engine::{AudioClock, AudioEngine};
pub use video_playback::{VideoFrame, VideoPlayback};

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("test_fixtures").join(name)
    }

    #[test]
    fn plays_without_underrun_and_clock_advances_near_real_time() {
        media_ffmpeg::init().unwrap();
        let engine = AudioEngine::start(fixture("test_h264.mp4"), 0).unwrap();

        std::thread::sleep(Duration::from_millis(500));
        let tick_a = engine.current_tick();
        std::thread::sleep(Duration::from_millis(1000));
        let tick_b = engine.current_tick();

        let elapsed_ticks = tick_b - tick_a;
        let elapsed_seconds = elapsed_ticks as f64 / timeline::TIMEBASE as f64;
        assert!(
            (0.8..1.3).contains(&elapsed_seconds),
            "expected ~1.0s of audio-clock advance for 1.0s of wall time, got {elapsed_seconds}"
        );
        assert_eq!(engine.underrun_count(), 0, "audio decode-ahead fell behind real time");
    }

    #[test]
    fn seek_resets_clock_to_target_and_keeps_playing() {
        media_ffmpeg::init().unwrap();
        let engine = AudioEngine::start(fixture("test_h264.mp4"), 0).unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let one_sec = timeline::TIMEBASE;
        engine.seek(one_sec);
        std::thread::sleep(Duration::from_millis(100));
        let right_after_seek = engine.current_tick();
        assert!(
            (right_after_seek - one_sec).abs() < timeline::TIMEBASE / 2,
            "expected clock to land near the seek target, got {right_after_seek}"
        );

        std::thread::sleep(Duration::from_millis(500));
        assert!(engine.current_tick() > right_after_seek, "clock should keep advancing after seek");
    }

    #[test]
    fn end_of_stream_freezes_clock_without_spamming_underruns() {
        // Regression test: running play_clip past the end of a real clip
        // showed underruns climbing into the hundreds because end-of-stream
        // silence was being counted the same as the decoder falling behind.
        media_ffmpeg::init().unwrap();
        // test_playback_demo.mp4 is 8s; start near its end so EOF hits soon.
        let near_end = timeline::TIMEBASE * 7;
        let engine = AudioEngine::start(fixture("test_playback_demo.mp4"), near_end).unwrap();

        std::thread::sleep(Duration::from_millis(1500));
        assert!(engine.has_ended(), "expected to reach end of an 8s clip after 1.5s from the 7s mark");

        let tick_a = engine.current_tick();
        std::thread::sleep(Duration::from_millis(300));
        let tick_b = engine.current_tick();
        assert_eq!(tick_a, tick_b, "clock should hold steady at end of stream, not drift");
        assert_eq!(engine.underrun_count(), 0, "end-of-stream silence must not count as underrun");
    }

    #[test]
    fn video_playback_publishes_frames_tracking_a_driven_clock() {
        use std::sync::atomic::{AtomicI64, Ordering};

        media_ffmpeg::init().unwrap();
        let clock_ticks = std::sync::Arc::new(AtomicI64::new(0));
        let clock_for_thread = clock_ticks.clone();
        let video = VideoPlayback::start(fixture("test_h264.mp4"), move || {
            clock_for_thread.load(Ordering::Relaxed)
        })
        .unwrap();

        // Drive the clock forward in real-ish steps, like the audio engine
        // would, and confirm the published frame's pts tracks it.
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(50));
            clock_ticks.fetch_add(timeline::TIMEBASE / 20, Ordering::Relaxed); // +50ms
        }
        std::thread::sleep(Duration::from_millis(100));

        let frame = video.latest_frame().expect("should have published at least one frame");
        assert_eq!((frame.width, frame.height), (640, 360));
        let clock_now = clock_ticks.load(Ordering::Relaxed);
        assert!(
            (clock_now - frame.pts_ticks).abs() < timeline::TIMEBASE / 2,
            "published frame's pts ({}) should track the driven clock ({})",
            frame.pts_ticks,
            clock_now
        );
    }

    #[test]
    fn pause_stops_clock_advance() {
        media_ffmpeg::init().unwrap();
        let engine = AudioEngine::start(fixture("test_h264.mp4"), 0).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        engine.pause();
        std::thread::sleep(Duration::from_millis(50)); // let the Pause command land
        let tick_a = engine.current_tick();
        std::thread::sleep(Duration::from_millis(300));
        let tick_b = engine.current_tick();
        assert_eq!(tick_a, tick_b, "clock must not advance while paused");
    }
}
