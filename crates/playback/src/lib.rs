pub mod audio_engine;
pub use audio_engine::AudioEngine;

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
        std::thread::sleep(Duration::from_millis(50));
        let right_after_seek = engine.current_tick();
        assert!(
            (right_after_seek - one_sec).abs() < timeline::TIMEBASE / 2,
            "expected clock to land near the seek target, got {right_after_seek}"
        );

        std::thread::sleep(Duration::from_millis(500));
        assert!(engine.current_tick() > right_after_seek, "clock should keep advancing after seek");
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
