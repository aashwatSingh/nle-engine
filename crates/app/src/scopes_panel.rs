//! Video scopes panel: histogram, waveform, and vectorscope for the current
//! preview frame, computed by `render::scopes` and drawn as small egui
//! images.
//!
//! Closed by default and gated on its own visibility flag rather than
//! updating unconditionally: reading the preview back from the GPU
//! (`Preview::read_back_rgba`) blocks on a synchronous copy, which is a real
//! cost to pay every frame during playback for a panel most edits don't have
//! open. Real NLEs treat scopes the same way — a monitoring tool you switch
//! on to check levels, not a free background computation.

use render::scopes::{Histogram, Vectorscope, Waveform};

#[derive(Default)]
pub struct ScopesPanelState {
    pub open: bool,
    histogram_tex: Option<egui::TextureHandle>,
    waveform_tex: Option<egui::TextureHandle>,
    vectorscope_tex: Option<egui::TextureHandle>,
}

const HIST_H: usize = 100;
const WAVE_ROWS: u32 = 100;
const VECTOR_SIZE: u32 = 128;

pub fn show(
    ui: &mut egui::Ui,
    state: &mut ScopesPanelState,
    preview: &crate::preview::Preview,
    device: &render::wgpu::Device,
    queue: &render::wgpu::Queue,
) {
    ui.checkbox(&mut state.open, "Scopes");
    if !state.open {
        return;
    }
    let Some((rgba, width, height)) = preview.read_back_rgba(device, queue) else {
        ui.weak("nothing composited yet");
        return;
    };

    let hist = Histogram::from_rgba(&rgba);
    let wave = Waveform::from_rgba(&rgba, width, height, WAVE_ROWS);
    let vector = Vectorscope::from_rgba(&rgba, VECTOR_SIZE);

    ui.label("Histogram (R/G/B, log height)");
    let hist_img = histogram_image(&hist);
    update_texture(ui.ctx(), &mut state.histogram_tex, "scope-histogram", hist_img);
    if let Some(tex) = &state.histogram_tex {
        ui.image((tex.id(), egui::vec2(256.0, HIST_H as f32)));
    }

    ui.label("Waveform (luma)");
    let wave_img = waveform_image(&wave);
    update_texture(ui.ctx(), &mut state.waveform_tex, "scope-waveform", wave_img);
    if let Some(tex) = &state.waveform_tex {
        let display_w = 256.0;
        let display_h = display_w * wave.height as f32 / wave.width.max(1) as f32;
        ui.image((tex.id(), egui::vec2(display_w, display_h)));
    }

    ui.label("Vectorscope");
    let vector_img = vectorscope_image(&vector);
    update_texture(ui.ctx(), &mut state.vectorscope_tex, "scope-vectorscope", vector_img);
    if let Some(tex) = &state.vectorscope_tex {
        ui.image((tex.id(), egui::vec2(160.0, 160.0)));
    }
}

fn update_texture(ctx: &egui::Context, slot: &mut Option<egui::TextureHandle>, name: &str, image: egui::ColorImage) {
    match slot {
        Some(tex) => tex.set(image, egui::TextureOptions::NEAREST),
        None => *slot = Some(ctx.load_texture(name, image, egui::TextureOptions::NEAREST)),
    }
}

/// One column per channel level, log-scaled height so a single bright pixel
/// still shows a hairline rather than vanishing next to a large flat field —
/// the standard trade every real histogram display makes, since raw linear
/// counts make anything but the tallest peak invisible.
fn histogram_image(h: &Histogram) -> egui::ColorImage {
    let mut img = egui::ColorImage::new([256, HIST_H], egui::Color32::BLACK);
    let plot = |img: &mut egui::ColorImage, bins: &[u32; 256], colour: [u8; 3]| {
        let max = *bins.iter().max().unwrap_or(&1).max(&1) as f32;
        for x in 0..256usize {
            let frac = (bins[x] as f32 + 1.0).ln() / (max + 1.0).ln();
            let bar_h = (frac * HIST_H as f32).round() as usize;
            for y in (HIST_H - bar_h.min(HIST_H))..HIST_H {
                let i = y * 256 + x;
                let px = img.pixels[i];
                img.pixels[i] = egui::Color32::from_rgb(
                    px.r().saturating_add(colour[0]),
                    px.g().saturating_add(colour[1]),
                    px.b().saturating_add(colour[2]),
                );
            }
        }
    };
    plot(&mut img, &h.red, [180, 0, 0]);
    plot(&mut img, &h.green, [0, 180, 0]);
    plot(&mut img, &h.blue, [0, 0, 180]);
    img
}

fn waveform_image(w: &Waveform) -> egui::ColorImage {
    let max = *w.cells.iter().max().unwrap_or(&1).max(&1) as f32;
    let pixels: Vec<egui::Color32> = w
        .cells
        .iter()
        .map(|&c| {
            // Square root rather than log: a waveform's whole point is
            // relative brightness across the trace, and sqrt keeps a lightly
            // populated row visible without flattening a genuinely bright
            // clipped highlight down to the same level as everything else.
            let v = ((c as f32 / max).sqrt() * 255.0).round() as u8;
            egui::Color32::from_rgb(v, v, v)
        })
        .collect();
    egui::ColorImage { size: [w.width as usize, w.height as usize], pixels }
}

fn vectorscope_image(v: &Vectorscope) -> egui::ColorImage {
    let max = *v.cells.iter().max().unwrap_or(&1).max(&1) as f32;
    let size = v.size as usize;
    let mut pixels = vec![egui::Color32::from_rgb(20, 20, 20); size * size];
    // Centre crosshair, drawn under the trace, so a neutral frame's single
    // centre dot is still visible against it rather than being just another
    // grey pixel in the middle of a grey field.
    let centre = size / 2;
    for i in 0..size {
        pixels[centre * size + i] = egui::Color32::from_gray(50);
        pixels[i * size + centre] = egui::Color32::from_gray(50);
    }
    for (i, &c) in v.cells.iter().enumerate() {
        if c == 0 {
            continue;
        }
        let bright = ((c as f32 / max).sqrt() * 255.0).round().max(60.0) as u8;
        pixels[i] = egui::Color32::from_rgb(bright, bright, 0);
    }
    egui::ColorImage { size: [size, size], pixels }
}
