//! Video scopes: histogram, waveform, vectorscope — read from an already
//! composited, delivery-encoded RGBA frame (the same bytes `render_to_rgba`
//! hands back), on the CPU.
//!
//! CPU rather than a GPU pass, deliberately: a scope is read once per shown
//! frame during scrubbing/monitoring, not composited every layer of every
//! track, and it needs `Vec<u32>` histograms/grids the UI reads back and
//! draws with egui immediately — round-tripping that through a GPU readback
//! would add a frame of latency for no benefit at this scale.
//!
//! All three trust the Rec.709 luma/chroma matrix (`Kr = 0.2126`,
//! `Kb = 0.0722`) that the rest of `render` already uses — `color.rs` is the
//! tested reference for the forward gamut conversions; this module derives
//! its own constants directly from the same ITU-R BT.709 coefficients rather
//! than importing `color`'s matrix machinery, since a scope only ever needs
//! the three scalars, not a full 3x3 conversion pipeline.
//!
//! A scope that is merely plausible is worse than none — its entire purpose
//! is to be trusted over a possibly-miscalibrated monitor — so every number
//! here is checked in `tests/scopes.rs` against a value computed by hand from
//! the standard, not against whatever this code happens to produce.

const KR: f32 = 0.2126;
const KB: f32 = 0.0722;

/// Rec.709 luma of a linear-ish 0..1 RGB triple. Takes encoded (not
/// linear-light) values, matching every other scope input in this module —
/// a scope reads the delivered, gamma-encoded frame, the same bytes that
/// would go to a monitor, not the compositor's internal linear working
/// space.
pub fn luma_rec709(r: f32, g: f32, b: f32) -> f32 {
    KR * r + (1.0 - KR - KB) * g + KB * b
}

fn cb_cr(r: f32, g: f32, b: f32) -> (f32, f32) {
    let y = luma_rec709(r, g, b);
    let cb = (b - y) / (2.0 * (1.0 - KB));
    let cr = (r - y) / (2.0 * (1.0 - KR));
    (cb, cr)
}

/// Per-channel pixel counts at each of the 256 possible 8-bit levels.
pub struct Histogram {
    pub red: [u32; 256],
    pub green: [u32; 256],
    pub blue: [u32; 256],
    pub luma: [u32; 256],
}

impl Histogram {
    pub fn from_rgba(rgba: &[u8]) -> Self {
        let mut h = Histogram { red: [0; 256], green: [0; 256], blue: [0; 256], luma: [0; 256] };
        for px in rgba.chunks_exact(4) {
            let (r, g, b) = (px[0], px[1], px[2]);
            h.red[r as usize] += 1;
            h.green[g as usize] += 1;
            h.blue[b as usize] += 1;
            let y = luma_rec709(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
            let bin = (y * 255.0).round().clamp(0.0, 255.0) as usize;
            h.luma[bin] += 1;
        }
        h
    }
}

/// A luma waveform: one column per source pixel column, `height` rows tall,
/// each cell counting how many pixels in that column landed at that
/// brightness. Row 0 is the top of the graticule (100% white); row
/// `height - 1` is the bottom (0%, black) — the broadcast-monitor convention,
/// and the opposite of "just draw increasing values downward" if that isn't
/// deliberately corrected for.
pub struct Waveform {
    pub width: u32,
    pub height: u32,
    pub cells: Vec<u32>,
}

impl Waveform {
    pub fn from_rgba(rgba: &[u8], width: u32, height: u32, rows: u32) -> Self {
        debug_assert_eq!(
            rgba.len(),
            (width * height * 4) as usize,
            "rgba buffer doesn't match the given width/height — the x-per-pixel \
             mapping below assumes `width` is the buffer's real row stride"
        );
        let mut cells = vec![0u32; (width * rows) as usize];
        for (i, px) in rgba.chunks_exact(4).enumerate() {
            let x = i as u32 % width;
            let (r, g, b) = (px[0] as f32 / 255.0, px[1] as f32 / 255.0, px[2] as f32 / 255.0);
            let y = luma_rec709(r, g, b);
            // Bright at row 0, dark at row `rows - 1` — hence `1.0 - y`.
            let row = ((1.0 - y) * (rows - 1) as f32).round().clamp(0.0, (rows - 1) as f32) as u32;
            cells[(row * width + x) as usize] += 1;
        }
        Waveform { width, height: rows, cells }
    }
}

/// A Cb/Cr scatter plot: `size` x `size` cells, the centre cell (index
/// `size / 2` on both axes) representing zero chroma — every neutral grey
/// from black to white lands there, since a vectorscope deliberately discards
/// luma and plots colour alone.
pub struct Vectorscope {
    pub size: u32,
    pub cells: Vec<u32>,
}

impl Vectorscope {
    pub fn from_rgba(rgba: &[u8], size: u32) -> Self {
        let mut cells = vec![0u32; (size * size) as usize];
        let max_idx = size as i32 - 1;
        for px in rgba.chunks_exact(4) {
            let (r, g, b) = (px[0] as f32 / 255.0, px[1] as f32 / 255.0, px[2] as f32 / 255.0);
            let (cb, cr) = cb_cr(r, g, b);
            // Chroma is signed and roughly -0.5..0.5; recentred to 0..size so
            // zero chroma always lands at the middle cell regardless of size.
            let cb_idx = ((0.5 + cb) * size as f32).round().clamp(0.0, max_idx as f32) as u32;
            let cr_idx = ((0.5 + cr) * size as f32).round().clamp(0.0, max_idx as f32) as u32;
            cells[(cr_idx * size + cb_idx) as usize] += 1;
        }
        Vectorscope { size, cells }
    }

    /// The `(cb, cr)` index of the one populated cell, for tests that push a
    /// single solid colour through the scope. `None` if zero or more than one
    /// cell has anything in it — a real scope of course lights up many cells
    /// at once, but that makes "which cell is *the* answer" ambiguous, which
    /// is exactly why this is `tests/`-only in spirit even though it's a
    /// small enough, generally useful enough query to leave `pub`.
    pub fn only_populated_cell(&self) -> Option<(u32, u32)> {
        let mut found = None;
        for (i, &count) in self.cells.iter().enumerate() {
            if count == 0 {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some((i as u32 % self.size, i as u32 / self.size));
        }
        found
    }
}
