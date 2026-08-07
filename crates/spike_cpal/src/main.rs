//! M2 risk spike: does CPAL link and actually produce audio on this
//! toolchain/machine before the real audio engine is built on top of it?
//! Plays a 2-second 440Hz sine tone and counts callback invocations to
//! sanity-check the real-time callback is actually being driven.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn main() {
    let host = cpal::default_host();
    let device = host.default_output_device().expect("no output device");
    println!("output device: {}", device.name().unwrap_or_default());

    let config = device.default_output_config().expect("no default output config");
    println!("sample format: {:?}, sample rate: {}, channels: {}", config.sample_format(), config.sample_rate().0, config.channels());

    let sample_rate = config.sample_rate().0 as f32;
    let channels = config.channels() as usize;
    let callback_count = Arc::new(AtomicU64::new(0));
    let cb_count = callback_count.clone();

    // Real-time callback: no allocation, no locks — phase is the only
    // mutable state, captured by value into the closure.
    let mut phase: f32 = 0.0;
    let phase_step = 440.0 * std::f32::consts::TAU / sample_rate;

    let stream = device
        .build_output_stream(
            &config.into(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                for frame in data.chunks_mut(channels) {
                    let sample = phase.sin() * 0.2;
                    for s in frame {
                        *s = sample;
                    }
                    phase += phase_step;
                }
                cb_count.fetch_add(1, Ordering::Relaxed);
            },
            move |err| eprintln!("stream error: {err}"),
            None,
        )
        .expect("build_output_stream failed");

    stream.play().expect("stream.play failed");
    std::thread::sleep(Duration::from_secs(2));
    drop(stream);

    let count = callback_count.load(Ordering::Relaxed);
    println!("audio callback invoked {count} times over 2s");
    assert!(count > 0, "callback never fired — CPAL stream isn't actually driving audio");
    println!("OK");
}
