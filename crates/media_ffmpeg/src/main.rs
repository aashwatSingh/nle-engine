//! M1 demo: probe the three mixed-codec test fixtures and print what came
//! back. Started as the M0->M1 FFmpeg linking spike (docs/decisions-log.md);
//! now doubles as a quick manual sanity check for `probe()`.

use std::path::PathBuf;

fn main() {
    media_ffmpeg::init().expect("ffmpeg init failed");

    let fixtures_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("test_fixtures");

    for name in ["test_h264.mp4", "test_vp9.webm", "test_prores.mov"] {
        let path = fixtures_dir.join(name);
        match media_ffmpeg::probe(&path) {
            Ok(asset) => {
                println!("--- {name} ---");
                println!("  container: {}", asset.container_format);
                println!(
                    "  content_hash: {}...",
                    asset.content_hash.iter().take(6).map(|b| format!("{b:02x}")).collect::<String>()
                );
                println!("  duration_ticks: {}", asset.duration_ticks);
                if let Some(v) = &asset.video {
                    println!("  video: {}x{} {:?}", v.width, v.height, v.pixel_format);
                    println!("  frame_rate: {:?}", v.frame_rate);
                    println!("  keyframes indexed: {}", v.keyframe_index.len());
                    println!("  color: {:?}", v.color);
                }
                if let Some(a) = &asset.audio {
                    println!("  audio: {} Hz, {} ch", a.sample_rate, a.channel_count);
                }
            }
            Err(e) => println!("--- {name} --- FAILED: {e:?}"),
        }
    }
}
