//! M1 linking spike: does `ffmpeg-next` actually link and initialize against
//! the BtbN win64-gpl-shared 8.1 build on our GNU/MinGW Rust toolchain?
//! Throwaway proof, same role as `spike_wgpu` was for M0.

fn main() {
    ffmpeg_next::init().expect("ffmpeg_next::init failed");
    println!("ffmpeg linked and initialized ok");
    println!("libavformat version: {:#x}", ffmpeg_next::format::version());
    println!("libavcodec configuration: {}", ffmpeg_next::codec::configuration());
}
