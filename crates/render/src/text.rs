//! Rasterises `timeline::TitleSpec` into an RGBA buffer the compositor can
//! treat as an ordinary source frame.
//!
//! **Straight alpha, not premultiplied.** Every output pixel carries the
//! title's colour in RGB and the glyph's coverage in A, including where
//! coverage is zero. That's what `composite.wgsl`'s `fs_clip` expects (it
//! linearises RGB and multiplies alpha separately), and it sidesteps the
//! classic anti-aliased-text bug: premultiplying in an encoded space and
//! linearising afterwards darkens every edge pixel, which reads as a grey
//! fringe around white text on a dark background.
//!
//! **Shaped with `rustybuzz`, rasterised with `ab_glyph`.** Each line is run
//! through `rustybuzz::shape`, which applies the font's own GSUB/GPOS rules —
//! ligature substitution, Devanagari consonant-cluster and vowel-sign
//! reordering, Arabic contextual joining — before any glyph is drawn.
//! `rustybuzz::Face` only *shapes* (it decides which glyphs, in what order, at
//! what offsets); it doesn't rasterise, so the resulting glyph ids and pen
//! positions are handed to `ab_glyph::Font::outline_glyph` exactly as the
//! naive per-character path used to. The walk-forward-and-add-advances loop
//! below needs no direction branch for right-to-left text: HarfBuzz-family
//! shapers reverse the glyph order internally for RTL so a simple forward
//! walk already draws it correctly — that reversal is part of what shaping
//! does, not something a caller does afterward. Verified against real
//! Devanagari reordering in `tests/text_shaping.rs`, using the Indic font
//! Windows itself ships (`Nirmala.ttc`) rather than a synthetic case.
//!
//! **Font families resolve against the font's own `name` table**, read via
//! `ttf-parser` — "Segoe UI", not `segoeui.ttf`. The fast path is still
//! filename-stem matching (correct for the large majority of fonts, whose
//! family name and file stem agree once spaces are stripped) plus a small
//! alias table for the well-known cases where they don't ("Segoe UI" ->
//! `segoeui`). Real name-table resolution is the fallback beneath that,
//! **lazy and incremental**, not built at startup: measured on this machine,
//! parsing every installed font's name table eagerly costs ~3 seconds for
//! ~350 fonts (`FontLibrary::system` used to claim this was cheap — it
//! wasn't measured, and it wasn't cheap). A lookup that misses the fast path
//! scans unindexed files one at a time, caching every family name it
//! discovers along the way (not just the one that matched), so the *n*th
//! unusual lookup is cheaper than the first and the whole directory is never
//! scanned twice. `.ttc` collections aren't indexed by this fallback (only
//! `.ttf`/`.otf`) — a smaller, separate gap from real shaping's own, noted
//! rather than silently left unindexed.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ab_glyph::{Font, FontVec, Glyph, PxScale, ScaleFont};
use timeline::{TextAlign, TitleSpec};

/// A rasterised title. `width`/`height` match the sequence it was drawn for,
/// so it uploads and composites exactly like a decoded frame.
pub struct RasterizedText {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// A loaded, parsed font ready to draw with.
///
/// Holds the font twice, deliberately: `ab_glyph::FontVec` owns a parsed copy
/// for outline rasterisation, and `bytes` is kept alongside because
/// `rustybuzz::Face<'a>` borrows its data rather than owning it — it can't be
/// stored on this struct without a lifetime that outlives every call site, so
/// a fresh `rustybuzz::Face` is built from `bytes` on each shape. Building it
/// is cheap (it parses tables lazily) relative to the shaping and
/// rasterisation work it enables, and titles are cached by
/// `compositor::TitleCache` above this, so it isn't a per-frame cost.
pub struct FontFace {
    ab_font: FontVec,
    bytes: Arc<Vec<u8>>,
}

impl FontFace {
    pub fn from_bytes(bytes: Vec<u8>) -> Option<FontFace> {
        let bytes = Arc::new(bytes);
        let ab_font = FontVec::try_from_vec((*bytes).clone()).ok()?;
        Some(FontFace { ab_font, bytes })
    }

    pub fn from_path(path: &Path) -> Option<FontFace> {
        FontFace::from_bytes(std::fs::read(path).ok()?)
    }

    /// The raw font file bytes. Exposed for tests to confirm two lookups
    /// resolved to the *same* font, without exposing anything about how
    /// `FontLibrary` got there.
    pub fn raw_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// The family name a font declares for itself, read from its OpenType `name`
/// table — "Segoe UI", not `segoeui.ttf`. Prefers the typographic family name
/// (id 16) over the legacy one (id 1) when both exist, since the legacy name
/// is truncated for some styles ("Segoe UI Semibold" often only appears under
/// id 16, with id 1 just saying "Segoe UI"). Only Unicode-encoded entries are
/// considered — Mac Roman and other legacy 8-bit encodings are rare enough on
/// a font a user would actually pick that treating them as unresolvable is a
/// fine trade for not carrying a second decoder.
fn family_name_from_table(data: &[u8], face_index: u32) -> Option<String> {
    let face = ttf_parser::Face::parse(data, face_index).ok()?;
    let names = face.names();
    let mut legacy = None;
    for name in names {
        if !name.is_unicode() {
            continue;
        }
        match name.name_id {
            ttf_parser::name_id::TYPOGRAPHIC_FAMILY => return name.to_string(),
            ttf_parser::name_id::FAMILY if legacy.is_none() => legacy = name.to_string(),
            _ => {}
        }
    }
    legacy
}

/// Encodes a 0..1 linear-ish colour component into a byte. The title's colour
/// is authored *already* Rec.709-encoded (see `TitleSpec::color`), so this is
/// a plain scale-and-round, not a transfer function — applying one here would
/// double-encode against the compositor's own linearisation.
fn to_byte(v: f64) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Draws `spec` into a `width` x `height` straight-alpha RGBA buffer.
pub fn rasterize(spec: &TitleSpec, font: &FontFace, width: u32, height: u32) -> RasterizedText {
    let rgb = [to_byte(spec.color[0]), to_byte(spec.color[1]), to_byte(spec.color[2])];
    // The colour goes down everywhere up front, including fully transparent
    // pixels — that's what makes this straight alpha rather than premultiplied.
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for _ in 0..(width * height) {
        rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 0]);
    }
    let mut out = RasterizedText { rgba, width, height };

    let scale = PxScale::from(spec.size_px as f32);
    let scaled = font.ab_font.as_scaled(scale);
    let line_height = scaled.height() + scaled.line_gap();
    // The clip's own opacity is applied later by the compositor; this is the
    // title colour's own alpha, which multiplies glyph coverage.
    let alpha = spec.color[3].clamp(0.0, 1.0);

    let lines: Vec<&str> = spec.text.split('\n').collect();
    // Vertically centre the whole block on the anchor, so adding a line grows
    // the block symmetrically instead of pushing the existing text off its
    // mark. `ascent` shifts from block-top to the first line's baseline.
    let block_height = line_height * lines.len() as f32;
    let anchor_y = (spec.position.1 * height as f64) as f32;
    let mut baseline_y = anchor_y - block_height / 2.0 + scaled.ascent();

    for line in lines {
        let glyphs = shape_line(font, scale, line);
        let line_width = glyphs.last().map(|(_, advance_end)| *advance_end).unwrap_or(0.0);
        let anchor_x = (spec.position.0 * width as f64) as f32;
        let origin_x = anchor_x + align_offset(spec.align, line_width);

        for (glyph, _) in glyphs {
            let positioned = Glyph {
                id: glyph.id,
                scale,
                position: ab_glyph::point(origin_x + glyph.position.x, baseline_y + glyph.position.y),
            };
            let Some(outlined) = font.ab_font.outline_glyph(positioned) else {
                // No outline: whitespace, or a character this font has no
                // glyph for. Its advance has already been counted, so the
                // rest of the line still lands in the right place.
                continue;
            };
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, coverage| {
                let px = bounds.min.x as i64 + gx as i64;
                let py = bounds.min.y as i64 + gy as i64;
                // Silently dropping out-of-frame glyph pixels is what makes an
                // over-long or off-screen title a clip rather than a buffer
                // overrun. Checked per pixel because a single glyph can
                // straddle an edge.
                if px < 0 || py < 0 || px >= width as i64 || py >= height as i64 {
                    return;
                }
                let a = (coverage.clamp(0.0, 1.0) as f64 * alpha * 255.0).round() as u8;
                let i = ((py as u32 * width + px as u32) * 4 + 3) as usize;
                // Max, not overwrite: adjacent glyphs whose antialiased edges
                // overlap (a tight kern pair, an accent over a letter) would
                // otherwise have the second one punch a lighter hole through
                // the first.
                out.rgba[i] = out.rgba[i].max(a);
            });
        }
        baseline_y += line_height;
    }

    out
}

/// Shapes one line via `rustybuzz` and lays the result out at pen positions
/// relative to the line's own origin, each paired with the pen x *after*
/// it — so the last pair's second element is the line's full advance width.
///
/// Falls back to naive one-glyph-per-character layout (no substitution, pair
/// kerning only) when `rustybuzz` can't parse the font at all — `FontFace`
/// already validated it parses for `ab_glyph`, so this is a belt-and-braces
/// path for a font one library accepts and the other doesn't, not the common
/// case. A title that renders in the wrong shaping mode is still far better
/// than one that renders nothing.
fn shape_line(font: &FontFace, scale: PxScale, line: &str) -> Vec<(Glyph, f32)> {
    let Some(hb_face) = rustybuzz::Face::from_slice(&font.bytes, 0) else {
        return layout_line_naive(&font.ab_font, scale, line);
    };
    let units_per_em = hb_face.units_per_em() as f32;
    if units_per_em <= 0.0 {
        return layout_line_naive(&font.ab_font, scale, line);
    }
    // Both PxScale components are equal — set together by `PxScale::from`
    // above — so either axis gives the same font-units-to-pixels factor.
    let px_per_unit = scale.x / units_per_em;

    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(line);
    buffer.guess_segment_properties();
    let shaped = rustybuzz::shape(&hb_face, &[], buffer);

    let mut out = Vec::with_capacity(shaped.len());
    let mut pen_x = 0.0f32;
    for (info, pos) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
        // OpenType glyph indices are always 16-bit by format, so this never
        // truncates a real value for a well-formed font.
        let id = ab_glyph::GlyphId(info.glyph_id as u16);
        let gx = pen_x + pos.x_offset as f32 * px_per_unit;
        // Font units are y-up; the raster below is y-down (baseline_y grows
        // downward per line), so the offset flips sign crossing that boundary.
        let gy = -(pos.y_offset as f32) * px_per_unit;
        pen_x += pos.x_advance as f32 * px_per_unit;
        out.push((id.with_scale_and_position(scale, ab_glyph::point(gx, gy)), pen_x));
    }
    out
}

/// The pre-shaping fallback: one glyph per character, pair kerning only, no
/// substitution. See `shape_line`'s doc for when this is used.
fn layout_line_naive(font: &FontVec, scale: PxScale, line: &str) -> Vec<(Glyph, f32)> {
    let scaled = font.as_scaled(scale);
    let mut out = Vec::new();
    let mut pen = 0.0f32;
    let mut previous: Option<ab_glyph::GlyphId> = None;
    for ch in line.chars() {
        let id = font.glyph_id(ch);
        if let Some(prev) = previous {
            pen += scaled.kern(prev, id);
        }
        let glyph = id.with_scale_and_position(scale, ab_glyph::point(pen, 0.0));
        pen += scaled.h_advance(id);
        out.push((glyph, pen));
        previous = Some(id);
    }
    out
}

/// Number of glyphs `text` shapes into at `size_px`, or `None` if `rustybuzz`
/// fell back to the naive per-character path (see `shape_line`'s doc) — that
/// fallback is a real, if rare, outcome and a test asking about *shaped*
/// glyph count should say so rather than silently reporting the naive count
/// as if it were the same thing.
///
/// Calls `shape_line` itself rather than shaping independently, so this is
/// provably testing what `rasterize` actually draws: a bug that made
/// `shape_line` silently fall back would change this function's answer too,
/// not just some parallel implementation of the same idea. Real substitution
/// (a ligature, a Devanagari conjunct) merges codepoints into fewer glyphs,
/// which the naive path can never do — so this single number is enough to
/// prove shaping actually ran, and mutation-tested to confirm it (see
/// `docs/decisions-log.md`).
pub fn shaped_glyph_count(font: &FontFace, text: &str, size_px: f32) -> Option<usize> {
    if rustybuzz::Face::from_slice(&font.bytes, 0).is_none() {
        return None;
    }
    Some(shape_line(font, PxScale::from(size_px), text).len())
}

/// How far left of the anchor a line of `line_width` starts, for each
/// alignment.
fn align_offset(align: TextAlign, line_width: f32) -> f32 {
    match align {
        TextAlign::Left => 0.0,
        TextAlign::Center => -line_width / 2.0,
        TextAlign::Right => -line_width,
    }
}

/// Families the user is likely to name whose file stem differs from the family
/// name. Small and Windows-leaning on purpose: this is a fallback for the
/// common cases, not a substitute for real name-table parsing.
const FAMILY_ALIASES: &[(&str, &str)] = &[
    ("segoe ui", "segoeui"),
    ("times new roman", "times"),
    ("courier new", "cour"),
    ("comic sans ms", "comic"),
    ("trebuchet ms", "trebuc"),
];

/// Families tried, in order, when the requested one can't be found. A title
/// rendering in the wrong font is far better than a title not rendering.
const FALLBACK_FAMILIES: &[&str] = &["arial", "segoeui", "calibri", "verdana", "tahoma", "dejavusans"];

/// One discovered `(display-case family name, file)` pair, plus how far the
/// incremental scan that found it has gotten.
#[derive(Default)]
struct FamilyScan {
    /// Lowercased family name -> (display-case name, path). Filled in as
    /// `scan_until_found` walks `FontLibrary::all_paths`; never re-parses a
    /// file once it's been scanned once, match or not.
    found: HashMap<String, (String, PathBuf)>,
    /// Index into `all_paths` of the next file this scan hasn't looked at yet.
    next_unscanned: usize,
}

/// An index of installed fonts, with parsed faces cached on first use.
///
/// `RefCell` rather than a lock: a `Compositor` owns one and is used from a
/// single thread at a time (each of the preview, export, and playback paths
/// builds its own). That keeps it `Send` — movable to the export thread — but
/// deliberately not `Sync`.
pub struct FontLibrary {
    by_stem: HashMap<String, PathBuf>,
    /// Every indexed font's path, in the stable order the scan walks them —
    /// same list `by_stem`'s values came from, kept separately so the
    /// incremental family-name scan has something to advance an index into.
    all_paths: Vec<PathBuf>,
    scan: RefCell<FamilyScan>,
    cache: RefCell<HashMap<String, Option<Arc<FontFace>>>>,
}

impl FontLibrary {
    /// Indexes the platform's font directories. Only file *names* are read
    /// here — real family names are resolved lazily, see the module doc on
    /// why an eager version of that was measured too slow to do at startup.
    pub fn system() -> FontLibrary {
        let mut by_stem = HashMap::new();
        let mut all_paths = Vec::new();
        for dir in Self::font_dirs() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                let is_font = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| matches!(e.to_ascii_lowercase().as_str(), "ttf" | "otf"))
                    .unwrap_or(false);
                if !is_font {
                    continue;
                }
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    // First wins, so a user font directory listed before the
                    // system one takes precedence over the system copy.
                    by_stem.entry(stem.to_ascii_lowercase()).or_insert_with(|| path.clone());
                }
                all_paths.push(path);
            }
        }
        FontLibrary {
            by_stem,
            all_paths,
            scan: RefCell::new(FamilyScan::default()),
            cache: RefCell::new(HashMap::new()),
        }
    }

    fn font_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            dirs.push(PathBuf::from(local).join(r"Microsoft\Windows\Fonts"));
        }
        if let Ok(windir) = std::env::var("WINDIR") {
            dirs.push(PathBuf::from(windir).join("Fonts"));
        }
        // Harmless on Windows (they simply don't exist) and correct elsewhere.
        dirs.push(PathBuf::from("/usr/share/fonts"));
        dirs.push(PathBuf::from("/Library/Fonts"));
        dirs.push(PathBuf::from("/System/Library/Fonts"));
        dirs
    }

    /// Scans `all_paths` forward from wherever the last scan left off, until
    /// `wanted` (already lowercased) turns up in `scan.found` or every file
    /// has been looked at. Every family discovered along the way is kept, not
    /// just a match for `wanted` — an unrelated earlier lookup's leftover scan
    /// progress is exactly what makes a later, different lookup cheaper.
    fn scan_until_found(&self, wanted: &str) -> Option<PathBuf> {
        loop {
            if let Some((_, path)) = self.scan.borrow().found.get(wanted) {
                return Some(path.clone());
            }
            let idx = self.scan.borrow().next_unscanned;
            let Some(path) = self.all_paths.get(idx) else { return None };
            let discovered = std::fs::read(path).ok().and_then(|data| family_name_from_table(&data, 0));
            let mut scan = self.scan.borrow_mut();
            scan.next_unscanned = idx + 1;
            if let Some(name) = discovered {
                scan.found.entry(name.to_ascii_lowercase()).or_insert((name, path.clone()));
            }
        }
    }

    /// The face for `family`, falling back through `FALLBACK_FAMILIES` when it
    /// isn't installed. `None` only when no usable font was found at all,
    /// which the compositor reports as a missing source rather than drawing
    /// an empty frame.
    pub fn face(&self, family: &str) -> Option<Arc<FontFace>> {
        let requested = family.trim().to_ascii_lowercase();
        if let Some(hit) = self.cache.borrow().get(&requested) {
            return hit.clone();
        }

        let stem_guess = requested.replace(' ', "");
        let alias_stem = FAMILY_ALIASES.iter().find(|(name, _)| *name == requested).map(|(_, s)| *s);
        let path = self
            .by_stem
            .get(&stem_guess)
            .or_else(|| alias_stem.and_then(|s| self.by_stem.get(s)))
            .cloned()
            .or_else(|| self.scan_until_found(&requested))
            .or_else(|| FALLBACK_FAMILIES.iter().find_map(|s| self.by_stem.get(*s)).cloned());

        let loaded = path.and_then(|p| FontFace::from_path(&p)).map(Arc::new);
        self.cache.borrow_mut().insert(requested, loaded.clone());
        loaded
    }

    /// Real, display-case family names available to offer in a UI, sorted.
    /// Forces the incremental scan to completion — the one place this pays
    /// the full cost the module doc measured, and only when a caller actually
    /// wants the whole list rather than one lookup.
    pub fn available_families(&self) -> Vec<String> {
        while self.all_paths.get(self.scan.borrow().next_unscanned).is_some() {
            let idx = self.scan.borrow().next_unscanned;
            let path = &self.all_paths[idx];
            let discovered = std::fs::read(path).ok().and_then(|data| family_name_from_table(&data, 0));
            let mut scan = self.scan.borrow_mut();
            scan.next_unscanned = idx + 1;
            if let Some(name) = discovered {
                scan.found.entry(name.to_ascii_lowercase()).or_insert((name, path.clone()));
            }
        }
        let mut names: Vec<String> = self.scan.borrow().found.values().map(|(name, _)| name.clone()).collect();
        names.sort();
        names.dedup();
        names
    }
}

impl RasterizedText {
    /// RGBA of one pixel, for tests and pixel inspection.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [self.rgba[i], self.rgba[i + 1], self.rgba[i + 2], self.rgba[i + 3]]
    }

    /// How many pixels carry ink at least `min_alpha` opaque. The simplest
    /// honest measure of "did something get drawn", and stable across font
    /// versions in a way comparing exact glyph shapes would not be.
    pub fn inked_pixels(&self, min_alpha: u8) -> u32 {
        self.rgba.chunks_exact(4).filter(|px| px[3] >= min_alpha).count() as u32
    }

    /// Mean x of inked pixels, in pixels. `None` when nothing is inked.
    /// Alignment tests compare this rather than hunting for glyph edges.
    pub fn ink_centroid_x(&self, min_alpha: u8) -> Option<f64> {
        self.centroid(min_alpha, |i, w| (i % w) as f64)
    }

    /// Mean y of inked pixels, in pixels. `None` when nothing is inked.
    pub fn ink_centroid_y(&self, min_alpha: u8) -> Option<f64> {
        self.centroid(min_alpha, |i, w| (i / w) as f64)
    }

    /// Bounding box of the inked pixels as `(min_x, min_y, max_x, max_y)`,
    /// inclusive. `None` when nothing is inked. Distinguishes "the text moved"
    /// from "the text grew", which a centroid alone cannot: two lines stacked
    /// around an anchor have the same centroid as one line at that anchor.
    pub fn ink_bounds(&self, min_alpha: u8) -> Option<(u32, u32, u32, u32)> {
        let mut bounds: Option<(u32, u32, u32, u32)> = None;
        for (i, px) in self.rgba.chunks_exact(4).enumerate() {
            if px[3] < min_alpha {
                continue;
            }
            let (x, y) = (i as u32 % self.width, i as u32 / self.width);
            bounds = Some(match bounds {
                None => (x, y, x, y),
                Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
            });
        }
        bounds
    }

    fn centroid(&self, min_alpha: u8, axis: impl Fn(u32, u32) -> f64) -> Option<f64> {
        let mut sum = 0f64;
        let mut n = 0u32;
        for (i, px) in self.rgba.chunks_exact(4).enumerate() {
            if px[3] >= min_alpha {
                sum += axis(i as u32, self.width);
                n += 1;
            }
        }
        (n > 0).then(|| sum / n as f64)
    }
}
