#![allow(dead_code)]
//! snapcompact-cli: standalone rasterization tool for the snapcompact frame format.
//!
//! Ports the core pixel-plotting logic from `oh-my-pi` `crates/pi-natives/src/snapcompact.rs`.
//! Renders text onto a bitmap grid using BDF/TTF pixel fonts, then encodes the result
//! as a palette-indexed or RGB PNG with automatic palette narrowing.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::f32::consts::PI;

use clap::Parser;
use fontdue::{Font as TtfFace, FontSettings, Metrics};
use png::{BitDepth, ColorType, Compression, Encoder, FilterType};

// ============================================================================
// Constants
// ============================================================================

/// Upper bound on frame edge pixels (hard stop against absurd allocations).
const MAX_FRAME_SIZE: u32 = 16384;

/// Indexed palette: 0=white bg, 1-6=sentence hues, 7=black ink, 8=highlight band, 9=dim gray.
const PALETTE: [[u8; 3]; 10] = [
    [255, 255, 255],
    [109, 2, 2],     // red
    [109, 53, 2],    // amber
    [24, 109, 2],    // green
    [2, 109, 109],   // teal
    [2, 32, 109],    // blue
    [75, 2, 109],    // violet
    [0, 0, 0],       // bw ink
    [255, 247, 194], // repeat highlight band
    [128, 128, 128], // dim ink
];
const INK_COLORS: usize = 6;
const INK_BLACK: u8 = 7;
const BG_REPEAT: u8 = 8;
const INK_DIM: u8 = 9;
const DIM_ON: u32 = 0x0e;
const DIM_OFF: u32 = 0x0f;
const FULL_BLOCK: u32 = 0x2588;

/// Character cells between two doc columns.
const GUTTER: usize = 3;

// ============================================================================
// Glyph & Font types
// ============================================================================

struct Glyph {
    w: u8,
    h: i32,
    xoff: i32,
    yoff: i32,
    /// One bitmask per bitmap row, MSB-leftmost.
    rows: Vec<u8>,
}

struct Font {
    glyphs: HashMap<u32, Glyph>,
    ascent: i32,
    cell_w: usize,
    cell_h: usize,
}

struct TtfFont {
    face: TtfFace,
    supported: HashSet<char>,
    px: f32,
    ascent: f32,
    cell_w: usize,
    cell_h: usize,
}

struct RasterizedGlyph {
    metrics: Metrics,
    bitmap: Vec<u8>,
}

// ============================================================================
// Font parsing
// ============================================================================

/// Parse an X.org BDF font. When `FONT_ASCENT` is absent, derives ascent from
/// `FONTBOUNDINGBOX w h xoff yoff`: `ascent = h - max(0, -yoff)`.
fn parse_bdf(text: &str, cell_w: usize, cell_h: usize) -> Font {
    let mut glyphs = HashMap::new();
    let mut ascent: i32 = 0;
    let mut ascent_explicit = false;
    let mut enc = -1i64;
    let mut bbx = [0i32; 4];
    // FONTBOUNDINGBOX fallback state
    let mut font_bbox = [0i32; 4];
    let mut has_font_bbox = false;
    let mut lines = text.lines();

    while let Some(line) = lines.next() {
        if let Some(rest) = line.strip_prefix("FONT_ASCENT ") {
            ascent = rest.trim().parse().unwrap_or(0);
            ascent_explicit = true;
        } else if let Some(rest) = line.strip_prefix("FONTBOUNDINGBOX ") {
            let mut parts = rest.split_ascii_whitespace();
            for slot in &mut font_bbox {
                *slot = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
            }
            has_font_bbox = true;
        } else if let Some(rest) = line.strip_prefix("ENCODING ") {
            enc = rest.trim().parse().unwrap_or(-1);
        } else if let Some(rest) = line.strip_prefix("BBX ") {
            let mut parts = rest.split_ascii_whitespace();
            for slot in &mut bbx {
                *slot = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
            }
        } else if line.starts_with("BITMAP") {
            let mut rows = Vec::new();
            for row in lines.by_ref() {
                if row.starts_with("ENDCHAR") {
                    break;
                }
                rows.push(u8::from_str_radix(row.trim(), 16).unwrap_or(0));
            }
            if enc >= 0 {
                glyphs.insert(
                    enc as u32,
                    Glyph {
                        w: bbx[0].clamp(0, 8) as u8,
                        h: bbx[1],
                        xoff: bbx[2],
                        yoff: bbx[3],
                        rows,
                    },
                );
            }
        }
    }

    if !ascent_explicit && has_font_bbox {
        // ascent = height - max(0, -yoff)
        let yoff = font_bbox[3];
        ascent = font_bbox[1] - (-yoff).max(0);
    }

    Font {
        glyphs,
        ascent,
        cell_w,
        cell_h,
    }
}

/// Parse a unifont-style `.hex` font (`CODEPOINT:16-hex-digit bitmap`, one
/// byte per row of an 8x8 glyph). Baseline sits at row 7 (`ascent` 7 with a
/// one-pixel descender row), matching the eval renderer.
fn parse_hex(text: &str) -> Font {
    let mut glyphs = HashMap::new();
    for line in text.lines() {
        let Some((cp, bits)) = line.split_once(':') else {
            continue;
        };
        let Ok(enc) = u32::from_str_radix(cp.trim(), 16) else {
            continue;
        };
        let bits = bits.trim();
        if bits.len() != 16 {
            continue;
        }
        let rows: Vec<u8> = (
            0..8
        )
        .map(|i| u8::from_str_radix(&bits[i * 2..i * 2 + 2], 16).unwrap_or(0))
        .collect();
        glyphs.insert(
            enc,
            Glyph {
                w: 8,
                h: 8,
                xoff: 0,
                yoff: -1,
                rows,
            },
        );
    }
    Font {
        glyphs,
        ascent: 7,
        cell_w: 8,
        cell_h: 8,
    }
}

fn parse_ttf(data: &[u8], px: f32, cell_w: usize, cell_h: usize) -> TtfFont {
    let face = TtfFace::from_bytes(data, FontSettings::default()).expect("bundled font must parse");
    let supported = face.chars().keys().copied().collect();
    let ascent = face
        .horizontal_line_metrics(px)
        .map_or(px * 0.8, |m| m.ascent);
    TtfFont {
        face,
        supported,
        px,
        ascent,
        cell_w,
        cell_h,
    }
}

// ============================================================================
// Font resolution (global lazy singletons)
// ============================================================================

fn font_5x8() -> Font {
    parse_bdf(include_str!("../fonts/5x8.bdf"), 5, 8)
}

fn font_8x8() -> Font {
    parse_hex(include_str!("../fonts/unscii-8.hex"))
}

fn font_8x13() -> Font {
    parse_bdf(include_str!("../fonts/8x13.bdf"), 8, 13)
}

fn font_6x12() -> Font {
    parse_bdf(include_str!("../fonts/6x12.bdf"), 6, 12)
}

fn font_silver() -> TtfFont {
    parse_ttf(
        include_bytes!("../fonts/Silver.ttf"),
        16.0,
        16,
        16,
    )
}

fn get_font_6x12() -> Result<Font, String> {
    Ok(font_6x12())
}

fn get_font_silver() -> Result<TtfFont, String> {
    Ok(font_silver())
}

enum RenderFont {
    Bitmap(Font),
    Ttf(TtfFont),
}

impl RenderFont {
    fn cell_w(&self) -> usize {
        match self {
            Self::Bitmap(f) => f.cell_w,
            Self::Ttf(f) => f.cell_w,
        }
    }
    fn cell_h(&self) -> usize {
        match self {
            Self::Bitmap(f) => f.cell_h,
            Self::Ttf(f) => f.cell_h,
        }
    }
    fn supports(&self, code: u32) -> bool {
        if matches!(code, DIM_ON | DIM_OFF | FULL_BLOCK | 0x0a) {
            return true;
        }
        match self {
            Self::Bitmap(font) => font.glyphs.contains_key(&code),
            Self::Ttf(font) => char::from_u32(code).is_some_and(|ch| font.supported.contains(&ch)),
        }
    }
}

fn resolve_font(name: &str) -> Result<RenderFont, String> {
    match name {
        "5x8" => Ok(RenderFont::Bitmap(font_5x8())),
        "8x8" => Ok(RenderFont::Bitmap(font_8x8())),
        "8x13" => Ok(RenderFont::Bitmap(font_8x13())),
        "6x12" => get_font_6x12().map(RenderFont::Bitmap),
        "silver" => get_font_silver().map(RenderFont::Ttf),
        _ => Err(format!(
            "Unknown font {name:?}: expected \"5x8\", \"8x8\", \"8x13\", \"6x12\", or \"silver\""
        )),
    }
}

// ============================================================================
// Grid & cell placement
// ============================================================================

struct Grid {
    cols: usize,
    rows: usize,
    repeat: usize,
    cell_w: usize,
    cell_h: usize,
}

const fn is_wide(cp: u32) -> bool {
    matches!(cp,
        0x1100..=0x115F
        | 0x2E80..=0x2EFF
        | 0x2F00..=0x2FDF
        | 0x3000..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x2FFFD
        | 0x30000..=0x3FFFD
    )
}

const fn cell_units(code: u32, wide_cells: bool) -> usize {
    match code {
        DIM_ON | DIM_OFF => 0,
        _ if wide_cells && is_wide(code) => 2,
        _ => 1,
    }
}

const fn place_cell(
    cursor: usize,
    cols: usize,
    code: u32,
    wide_cells: bool,
) -> Option<(usize, usize, usize)> {
    let units = cell_units(code, wide_cells);
    if units == 0 {
        return None;
    }
    let mut cell = cursor;
    if units == 2 && cols >= 2 && cell % cols == cols - 1 {
        cell += 1;
    }
    Some((cell, units, cell + units))
}

/// Count grid rows the text actually occupies.
fn used_rows(text: &str, grid: &Grid, doc: bool, wide_cells: bool) -> usize {
    let rows = if doc {
        text.split('\n').count()
    } else {
        let mut cursor = 0usize;
        for ch in text.chars() {
            if let Some((_, _, next)) = place_cell(cursor, grid.cols, ch as u32, wide_cells) {
                cursor = next;
            }
        }
        cursor.div_ceil(grid.cols)
    };
    rows.clamp(1, grid.rows)
}

// ============================================================================
// Pixel operations
// ============================================================================

fn fill_repeat_bands(pixels: &mut [u8], width: usize, height: usize, grid: &Grid) {
    if grid.repeat <= 1 {
        return;
    }
    for row in 0..grid.rows {
        for copy in 1..grid.repeat {
            let band_top = (row * grid.repeat + copy) * grid.cell_h;
            for y in band_top..(band_top + grid.cell_h).min(height) {
                pixels[y * width..y * width + width].fill(BG_REPEAT);
            }
        }
    }
}

fn blit_glyph(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    glyph: &Glyph,
    left: i32,
    top: i32,
    ink: u8,
) {
    for (r, &bits) in glyph.rows.iter().enumerate() {
        if bits == 0 {
            continue;
        }
        let y = top + r as i32;
        if y < 0 || y >= height as i32 {
            continue;
        }
        let row_base = y as usize * width;
        for b in 0..glyph.w {
            if bits & (0x80u8 >> b) != 0 {
                let x = left + i32::from(b);
                if x >= 0 && (x as usize) < width {
                    pixels[row_base + x as usize] = ink;
                }
            }
        }
    }
}

fn fill_cell(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    grid: &Grid,
    x_origin: usize,
    row: usize,
    ink: u8,
) {
    let x0 = x_origin.min(width);
    let x1 = (x_origin + grid.cell_w).min(width);
    if x0 >= x1 {
        return;
    }
    for copy in 0..grid.repeat {
        let top = (row * grid.repeat + copy) * grid.cell_h;
        for y in top..(top + grid.cell_h).min(height) {
            pixels[y * width + x0..y * width + x1].fill(ink);
        }
    }
}

fn fill_repeat_bands_rgb(pixels: &mut [u8], width: usize, height: usize, grid: &Grid) {
    if grid.repeat <= 1 {
        return;
    }
    let band = PALETTE[BG_REPEAT as usize];
    for row in 0..grid.rows {
        for copy in 1..grid.repeat {
            let band_top = (row * grid.repeat + copy) * grid.cell_h;
            for y in band_top..(band_top + grid.cell_h).min(height) {
                for px in pixels[y * width * 3..(y + 1) * width * 3]
                    .as_chunks_mut::<3>()
                    .0
                {
                    px.copy_from_slice(&band);
                }
            }
        }
    }
}

fn fill_cell_rgb(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    grid: &Grid,
    x_origin: usize,
    row: usize,
    ink: u8,
) {
    let x0 = x_origin.min(width);
    let x1 = (x_origin + grid.cell_w).min(width);
    if x0 >= x1 {
        return;
    }
    let color = PALETTE[ink as usize];
    for copy in 0..grid.repeat {
        let top = (row * grid.repeat + copy) * grid.cell_h;
        for y in top..(top + grid.cell_h).min(height) {
            let row = &mut pixels[y * width * 3..(y + 1) * width * 3];
            for x in x0..x1 {
                row[x * 3..x * 3 + 3].copy_from_slice(&color);
            }
        }
    }
}

// ============================================================================
// TTF helpers
// ============================================================================

fn ttf_pixel_size(font: &TtfFont, grid: &Grid) -> f32 {
    let sx = grid.cell_w as f32 / font.cell_w as f32;
    let sy = grid.cell_h as f32 / font.cell_h as f32;
    font.px * sx.min(sy)
}

fn ttf_wide_pixel_size(font: &TtfFont, grid: &Grid) -> f32 {
    let sx = (2 * grid.cell_w) as f32 / font.cell_w as f32;
    let sy = grid.cell_h as f32 / font.cell_h as f32;
    font.px * sx.min(sy)
}

fn ttf_ascent(font: &TtfFont, px: f32) -> f32 {
    font.face
        .horizontal_line_metrics(px)
        .map_or(font.ascent * px / font.px, |m| m.ascent)
}

fn cached_ttf_glyph<'a>(
    cache: &'a mut HashMap<char, RasterizedGlyph>,
    font: &TtfFont,
    ch: char,
    px: f32,
) -> Option<&'a RasterizedGlyph> {
    if !font.supported.contains(&ch) {
        return None;
    }
    Some(cache.entry(ch).or_insert_with(|| {
        let (metrics, bitmap) = font.face.rasterize(ch, px);
        RasterizedGlyph { metrics, bitmap }
    }))
}

fn blit_ttf_glyph(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    glyph: &RasterizedGlyph,
    left: i32,
    top: i32,
    ink: u8,
) {
    if glyph.metrics.width == 0 || glyph.metrics.height == 0 {
        return;
    }
    let color = PALETTE[ink as usize];
    for y in 0..glyph.metrics.height {
        let dst_y = top + y as i32;
        if dst_y < 0 || dst_y >= height as i32 {
            continue;
        }
        for x in 0..glyph.metrics.width {
            let alpha = u16::from(glyph.bitmap[y * glyph.metrics.width + x]);
            if alpha == 0 {
                continue;
            }
            let dst_x = left + x as i32;
            if dst_x < 0 || dst_x >= width as i32 {
                continue;
            }
            let offset = (dst_y as usize * width + dst_x as usize) * 3;
            let inv = 255 - alpha;
            for c in 0..3 {
                let bg = u16::from(pixels[offset + c]);
                let fg = u16::from(color[c]);
                pixels[offset + c] = ((bg * inv + fg * alpha + 127) / 255) as u8;
            }
        }
    }
}

fn blit_ttf_glyph_indexed(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    glyph: &RasterizedGlyph,
    left: i32,
    top: i32,
    ink: u8,
) {
    if glyph.metrics.width == 0 || glyph.metrics.height == 0 {
        return;
    }
    for y in 0..glyph.metrics.height {
        let dst_y = top + y as i32;
        if dst_y < 0 || dst_y >= height as i32 {
            continue;
        }
        let row_base = dst_y as usize * width;
        for x in 0..glyph.metrics.width {
            let coverage = glyph.bitmap[y * glyph.metrics.width + x];
            let cell = if coverage >= 170 {
                ink
            } else if ink == INK_BLACK && coverage >= 56 {
                INK_DIM
            } else if coverage >= 110 {
                ink
            } else {
                continue;
            };
            let dst_x = left + x as i32;
            if dst_x >= 0 && dst_x < width as i32 {
                pixels[row_base + dst_x as usize] = cell;
            }
        }
    }
}

fn ttf_glyph_origin(x_origin: usize, cell_w: usize, metrics: &Metrics) -> i32 {
    let advance = metrics.advance_width.ceil() as i32;
    let pad = (cell_w as i32 - advance).max(0) / 2;
    x_origin as i32 + pad + metrics.xmin
}

fn ttf_glyph_top(cell_top: usize, ascent: f32, metrics: &Metrics) -> i32 {
    (cell_top as f32 + ascent - metrics.height as f32 - metrics.ymin as f32).round() as i32
}

// ============================================================================
// Core rasterization
// ============================================================================

fn render_bitmap(
    text: &str,
    width: usize,
    height: usize,
    font: &Font,
    grid: &Grid,
    black_ink: bool,
) -> Vec<u8> {
    let mut pixels = vec![0u8; width * height];
    let capacity = grid.cols * grid.rows;
    if capacity == 0 {
        return pixels;
    }
    fill_repeat_bands(&mut pixels, width, height, grid);
    let codes: Vec<u32> = text.chars().map(|ch| ch as u32).collect();
    let narrow_px = if let Some(silver) = get_font_silver().ok() {
        ttf_pixel_size(&silver, grid)
    } else {
        0.0
    };
    let wide_px = if narrow_px > 0.0 {
        let sx = (2 * grid.cell_w) as f32 / 16.0;
        let sy = grid.cell_h as f32 / 16.0;
        16.0 * sx.min(sy)
    } else {
        0.0
    };
    let mut fallback_cache: HashMap<char, RasterizedGlyph> = HashMap::new();
    let mut sentence = 0usize;
    let mut dim = false;
    let mut cursor = 0usize;

    for i in 0..codes.len() {
        if cursor >= capacity {
            break;
        }
        let code = codes[i];
        match code {
            DIM_ON => {
                dim = true;
                continue;
            }
            DIM_OFF => {
                dim = false;
                continue;
            }
            _ => {}
        }
        let ink = if dim {
            INK_DIM
        } else if black_ink {
            INK_BLACK
        } else {
            (1 + sentence % INK_COLORS) as u8
        };
        if matches!(code, 0x2e | 0x21 | 0x3f)
            && matches!(codes.get(i + 1), Some(&(0x20 | FULL_BLOCK)))
        {
            sentence += 1;
        }
        let Some((at, units, next)) = place_cell(cursor, grid.cols, code, true) else {
            continue;
        };
        cursor = next;
        if at >= capacity {
            break;
        }
        let row = at / grid.cols;
        let col = at - row * grid.cols;
        if code == FULL_BLOCK {
            fill_cell(
                &mut pixels,
                width,
                height,
                grid,
                col * grid.cell_w,
                row,
                INK_BLACK,
            );
            continue;
        }
        if let Some(glyph) = font.glyphs.get(&code) {
            if glyph.rows.is_empty() {
                continue;
            }
            let left = (col * grid.cell_w) as i32 + glyph.xoff;
            for copy in 0..grid.repeat {
                let cell_top = ((row * grid.repeat + copy) * grid.cell_h) as i32;
                let top = cell_top + font.ascent - glyph.h - glyph.yoff;
                blit_glyph(&mut pixels, width, height, glyph, left, top, ink);
            }
        } else if wide_px > 0.0 {
            if let Ok(silver) = get_font_silver() {
                if let Some(ch) = char::from_u32(code) {
                    let px = if units == 2 { wide_px } else { narrow_px };
                    if let Some(glyph) = cached_ttf_glyph(&mut fallback_cache, &silver, ch, px) {
                        let span = units * grid.cell_w;
                        let left = ttf_glyph_origin(col * grid.cell_w, span, &glyph.metrics);
                        for copy in 0..grid.repeat {
                            let cell_top = (row * grid.repeat + copy) * grid.cell_h;
                            let top = ttf_glyph_top(cell_top, font.ascent as f32, &glyph.metrics);
                            blit_ttf_glyph_indexed(
                                &mut pixels,
                                width,
                                height,
                                glyph,
                                left,
                                top,
                                ink,
                            );
                        }
                    }
                }
            }
        }
    }
    pixels
}

fn render_ttf_rgb(
    text: &str,
    width: usize,
    height: usize,
    font: &TtfFont,
    grid: &Grid,
    black_ink: bool,
) -> Vec<u8> {
    let mut pixels = vec![255u8; width * height * 3];
    let capacity = grid.cols * grid.rows;
    if capacity == 0 {
        return pixels;
    }
    fill_repeat_bands_rgb(&mut pixels, width, height, grid);
    let px = ttf_pixel_size(font, grid);
    let ascent = ttf_ascent(font, px);
    let codes: Vec<char> = text.chars().collect();
    let mut cache: HashMap<char, RasterizedGlyph> = HashMap::new();
    let mut sentence = 0usize;
    let mut dim = false;
    let mut cell = 0usize;

    for i in 0..codes.len() {
        if cell >= capacity {
            break;
        }
        let ch = codes[i];
        let code = ch as u32;
        match code {
            DIM_ON => {
                dim = true;
                continue;
            }
            DIM_OFF => {
                dim = false;
                continue;
            }
            _ => {}
        }
        let ink = if dim {
            INK_DIM
        } else if black_ink {
            INK_BLACK
        } else {
            (1 + sentence % INK_COLORS) as u8
        };
        if matches!(code, 0x2e | 0x21 | 0x3f)
            && matches!(codes.get(i + 1).map(|n| *n as u32), Some(0x20 | FULL_BLOCK))
        {
            sentence += 1;
        }
        let row = cell / grid.cols;
        let col = cell - row * grid.cols;
        cell += 1;
        if code == FULL_BLOCK {
            fill_cell_rgb(
                &mut pixels,
                width,
                height,
                grid,
                col * grid.cell_w,
                row,
                INK_BLACK,
            );
            continue;
        }
        let Some(glyph) = cached_ttf_glyph(&mut cache, font, ch, px) else {
            continue;
        };
        let left = ttf_glyph_origin(col * grid.cell_w, grid.cell_w, &glyph.metrics);
        for copy in 0..grid.repeat {
            let cell_top = (row * grid.repeat + copy) * grid.cell_h;
            let top = ttf_glyph_top(cell_top, ascent, &glyph.metrics);
            blit_ttf_glyph(&mut pixels, width, height, glyph, left, top, ink);
        }
    }
    pixels
}

fn render_doc_bitmap(
    text: &str,
    width: usize,
    height: usize,
    font: &Font,
    grid: &Grid,
    black_ink: bool,
) -> Vec<u8> {
    let mut pixels = vec![0u8; width * height];
    let col_w = grid.cols.saturating_sub(GUTTER) / 2;
    if col_w == 0 || grid.rows == 0 {
        return pixels;
    }
    fill_repeat_bands(&mut pixels, width, height, grid);
    let codes: Vec<u32> = text.chars().map(|ch| ch as u32).collect();
    let mut fallback_cache: HashMap<char, RasterizedGlyph> = HashMap::new();
    let mut sentence = 0usize;
    let mut dim = false;
    let mut line = 0usize;
    let mut col = 0usize;
    let narrow_px = if let Ok(silver) = get_font_silver() {
        ttf_pixel_size(&silver, grid)
    } else {
        0.0
    };
    let wide_px = if narrow_px > 0.0 {
        let sx = (2 * grid.cell_w) as f32 / 16.0;
        let sy = grid.cell_h as f32 / 16.0;
        16.0 * sx.min(sy)
    } else {
        0.0
    };

    for i in 0..codes.len() {
        let code = codes[i];
        match code {
            DIM_ON => {
                dim = true;
                continue;
            }
            DIM_OFF => {
                dim = false;
                continue;
            }
            0x0a => {
                line += 1;
                col = 0;
                if line >= grid.rows * 2 {
                    break;
                }
                continue;
            }
            _ => {}
        }
        let ink = if dim {
            INK_DIM
        } else if black_ink {
            INK_BLACK
        } else {
            (1 + sentence % INK_COLORS) as u8
        };
        if matches!(code, 0x2e | 0x21 | 0x3f)
            && matches!(codes.get(i + 1), Some(&(0x20 | 0x0a | FULL_BLOCK)))
        {
            sentence += 1;
        }
        let units = cell_units(code, true);
        let mut cell = col;
        if units == 2 && col_w >= 2 && cell == col_w - 1 {
            cell += 1;
        }
        col = cell + units;
        if cell + units > col_w {
            continue;
        }
        let column = line / grid.rows;
        let row = line - column * grid.rows;
        let x_origin = column * (col_w + GUTTER) * grid.cell_w;
        if code == FULL_BLOCK {
            fill_cell(
                &mut pixels,
                width,
                height,
                grid,
                x_origin + cell * grid.cell_w,
                row,
                INK_BLACK,
            );
            continue;
        }
        if let Some(glyph) = font.glyphs.get(&code) {
            if glyph.rows.is_empty() {
                continue;
            }
            let left = (x_origin + cell * grid.cell_w) as i32 + glyph.xoff;
            for copy in 0..grid.repeat {
                let cell_top = ((row * grid.repeat + copy) * grid.cell_h) as i32;
                let top = cell_top + font.ascent - glyph.h - glyph.yoff;
                blit_glyph(&mut pixels, width, height, glyph, left, top, ink);
            }
        } else if wide_px > 0.0 {
            if let Ok(silver) = get_font_silver() {
                if let Some(ch) = char::from_u32(code) {
                    let px = if units == 2 { wide_px } else { narrow_px };
                    if let Some(glyph) = cached_ttf_glyph(&mut fallback_cache, &silver, ch, px) {
                        let span = units * grid.cell_w;
                        let left =
                            ttf_glyph_origin(x_origin + cell * grid.cell_w, span, &glyph.metrics);
                        for copy in 0..grid.repeat {
                            let cell_top = (row * grid.repeat + copy) * grid.cell_h;
                            let top = ttf_glyph_top(cell_top, font.ascent as f32, &glyph.metrics);
                            blit_ttf_glyph_indexed(
                                &mut pixels,
                                width,
                                height,
                                glyph,
                                left,
                                top,
                                ink,
                            );
                        }
                    }
                }
            }
        }
    }
    pixels
}

// ============================================================================
// Lanczos3 resampling (stretch shapes)
// ============================================================================

fn lanczos3(x: f32) -> f32 {
    let x = x.abs();
    if x < 1e-6 {
        return 1.0;
    }
    if x >= 3.0 {
        return 0.0;
    }
    let pix = PI * x;
    (pix.sin() / pix) * ((pix / 3.0).sin() / (pix / 3.0))
}

fn contributions(src_len: usize, dst_len: usize) -> Vec<(usize, Vec<f32>)> {
    let scale = src_len as f32 / dst_len as f32;
    let filt_scale = scale.max(1.0);
    let support = 3.0 * filt_scale;
    let mut out = Vec::with_capacity(dst_len);
    for i in 0..dst_len {
        let center = (i as f32 + 0.5) * scale;
        let begin = ((center - support) as isize).max(0) as usize;
        let end = ((center + support).ceil() as usize).min(src_len);
        let mut weights = Vec::with_capacity(end.saturating_sub(begin));
        let mut total = 0.0f32;
        for x in begin..end {
            let w = lanczos3((x as f32 + 0.5 - center) / filt_scale);
            weights.push(w);
            total += w;
        }
        if total != 0.0 {
            for w in &mut weights {
                *w /= total;
            }
        }
        out.push((begin, weights));
    }
    out
}

fn resize_rgb(src: &[f32], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<f32> {
    let horiz = contributions(sw, dw);
    let mut tmp = vec![0f32; dw * sh * 3];
    for y in 0..sh {
        let src_row = &src[y * sw * 3..(y + 1) * sw * 3];
        let dst_row = &mut tmp[y * dw * 3..(y + 1) * dw * 3];
        for (x, (begin, weights)) in horiz.iter().enumerate() {
            let mut acc = [0f32; 3];
            for (k, &w) in weights.iter().enumerate() {
                let s = (begin + k) * 3;
                acc[0] = src_row[s].mul_add(w, acc[0]);
                acc[1] = src_row[s + 1].mul_add(w, acc[1]);
                acc[2] = src_row[s + 2].mul_add(w, acc[2]);
            }
            dst_row[x * 3..x * 3 + 3].copy_from_slice(&acc);
        }
    }
    let vert = contributions(sh, dh);
    let mut out = vec![0f32; dw * dh * 3];
    for (y, (begin, weights)) in vert.iter().enumerate() {
        let dst_row = &mut out[y * dw * 3..(y + 1) * dw * 3];
        for (k, &w) in weights.iter().enumerate() {
            let src_row = &tmp[(begin + k) * dw * 3..(begin + k + 1) * dw * 3];
            for (d, &s) in dst_row.iter_mut().zip(src_row) {
                *d = s.mul_add(w, *d);
            }
        }
    }
    out
}

// ============================================================================
// PNG encoding
// ============================================================================

fn pack_bits(
    pixels: &[u8],
    width: usize,
    height: usize,
    bits: usize,
    remap: &[u8; PALETTE.len()],
) -> Vec<u8> {
    let per = 8 / bits;
    let row_bytes = width.div_ceil(per);
    let mut packed = vec![0u8; row_bytes * height];
    for y in 0..height {
        let src = &pixels[y * width..(y + 1) * width];
        let dst = &mut packed[y * row_bytes..(y + 1) * row_bytes];
        for (x, &px) in src.iter().enumerate() {
            dst[x / per] |= remap[px as usize] << (bits * (per - 1 - x % per));
        }
    }
    packed
}

/// Encode a palette-indexed bitmap as an indexed PNG with narrowed palette.
fn encode_indexed_png(pixels: &[u8], width: usize, height: usize) -> Result<Vec<u8>, String> {
    let mut used = [false; PALETTE.len()];
    for &px in pixels {
        used[px as usize] = true;
    }
    let mut remap = [0u8; PALETTE.len()];
    let mut palette = Vec::with_capacity(PALETTE.len() * 3);
    let mut count = 0u8;
    for (global, &is_used) in used.iter().enumerate() {
        if is_used {
            remap[global] = count;
            count += 1;
            palette.extend_from_slice(&PALETTE[global]);
        }
    }
    let (depth, bits) = match count {
        0..=2 => (BitDepth::One, 1),
        3..=4 => (BitDepth::Two, 2),
        _ => (BitDepth::Four, 4),
    };
    let mut out = Vec::new();
    let mut encoder = Encoder::new(&mut out, width as u32, height as u32);
    encoder.set_color(ColorType::Indexed);
    encoder.set_depth(depth);
    encoder.set_palette(Cow::Owned(palette));
    encoder.set_compression(Compression::Best);
    encoder.set_filter(FilterType::NoFilter);
    let mut writer = encoder
        .write_header()
        .map_err(|e| format!("Failed to write PNG header: {e}"))?;
    writer
        .write_image_data(&pack_bits(pixels, width, height, bits, &remap))
        .map_err(|e| format!("Failed to write PNG data: {e}"))?;
    writer
        .finish()
        .map_err(|e| format!("Failed to finish PNG stream: {e}"))?;
    Ok(out)
}

fn encode_rgb_png(pixels: &[u8], width: usize, height: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut encoder = Encoder::new(&mut out, width as u32, height as u32);
    encoder.set_color(ColorType::Rgb);
    encoder.set_depth(BitDepth::Eight);
    encoder.set_compression(Compression::Best);
    let mut writer = encoder
        .write_header()
        .map_err(|e| format!("Failed to write PNG header: {e}"))?;
    writer
        .write_image_data(pixels)
        .map_err(|e| format!("Failed to write PNG data: {e}"))?;
    writer
        .finish()
        .map_err(|e| format!("Failed to finish PNG stream: {e}"))?;
    Ok(out)
}

// ============================================================================
// Stopword dimming
// ============================================================================

/// High-frequency function words (verbatim from the original research `bdf.py` `_STOPWORDS`).
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "of", "to", "in", "on", "at", "as", "is", "are", "was", "were",
    "be", "been", "by", "for", "with", "that", "this", "it", "its", "from", "had", "has", "have",
    "not", "but", "he", "she", "his", "her", "they", "their", "them", "which", "also", "who",
    "whom", "when", "where", "while", "will", "would", "could", "should", "there", "then", "than",
    "into", "over", "under", "about", "after", "before", "between", "during", "each", "such",
    "these", "those", "some", "most", "more", "other", "only", "same", "so",
];

/// Wrap stopwords in DIM_ON/DIM_OFF markers for dim-capable shapes.
fn dim_stopwords(text: &str) -> String {
    let dim_marker_re = |c: char| c == '\u{000e}' || c == '\u{000f}';
    let parts: Vec<&str> = text.split(|c: char| dim_marker_re(c)).collect();
    let mut dim = false;
    let mut out = String::with_capacity(text.len() + 64);

    for (i, part) in parts.iter().enumerate() {
        if i < parts.len() - 1 {
            // Insert the dim marker that was split on
            let ch = text.chars().nth(
                // find the marker char position
                text.find(*part).unwrap_or(0) + part.len(),
            );
            if ch == Some('\u{000e}') {
                dim = true;
                out.push('\u{000e}');
            } else if ch == Some('\u{000f}') {
                dim = false;
                out.push('\u{000f}');
            }
        }
        if dim {
            out.push_str(part);
        } else {
            // Replace alphabetic runs that are stopwords with dimmed versions
            let mut last_end = 0;
            for word_match in part.match_indices(|c: char| {
                c.is_ascii_alphabetic()
                    || ('\u{00c0}'..='\u{00d6}').contains(&c)
                    || ('\u{00d8}'..='\u{00f6}').contains(&c)
                    || ('\u{00f8}'..='\u{00ff}').contains(&c)
            }) {
                let (pos, word) = word_match;
                out.push_str(&part[last_end..pos]);
                if STOPWORDS.contains(&word.to_lowercase().as_str()) {
                    out.push('\u{000e}');
                    out.push_str(word);
                    out.push('\u{000f}');
                } else {
                    out.push_str(word);
                }
                last_end = pos + word.len();
            }
            out.push_str(&part[last_end..]);
        }
    }
    out
}

// ============================================================================
// Shape definitions
// ============================================================================

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum Shape {
    /// 5x8 BDF font (legacy shape), black ink.
    #[value(name = "5x8-bw")]
    S5x8Bw,
    /// 8x8 unscii-8 hex font (Latin-1 subset), black ink.
    #[value(name = "8x8-bw")]
    S8x8Bw,
    /// 8x13 glyphs on an 11px advance (extra tracking), black ink.
    #[value(name = "11on16-bw")]
    S11on16Bw,
    /// 8x13 glyphs on a 22px pitch (extra leading), black ink.
    #[value(name = "8on22-bw")]
    S8on22Bw,
    /// 8x13 glyphs on an 8x16 cell pitch (no stretch), black ink.
    #[value(name = "8on16-bw")]
    S8on16Bw,
    /// 6x12 BDF font, black ink, stopword dimming.
    #[value(name = "6x12-dim")]
    S6x12Dim,
    /// Silver TrueType font on a 16px grid, black ink.
    #[value(name = "silver16-bw")]
    Silver16Bw,
}

struct ShapeParams {
    font_name: &'static str,
    cell_width: usize,
    cell_height: usize,
    black_ink: bool,
    stopword_dim: bool,
    is_ttf: bool,
}

impl Shape {
    fn params(&self) -> ShapeParams {
        match self {
            Shape::S5x8Bw => ShapeParams {
                font_name: "5x8",
                cell_width: 5,
                cell_height: 8,
                black_ink: true,
                stopword_dim: false,
                is_ttf: false,
            },
            Shape::S8x8Bw => ShapeParams {
                font_name: "8x8",
                cell_width: 8,
                cell_height: 8,
                black_ink: true,
                stopword_dim: false,
                is_ttf: false,
            },
            Shape::S11on16Bw => ShapeParams {
                font_name: "8x13",
                cell_width: 11,
                cell_height: 16,
                black_ink: true,
                stopword_dim: false,
                is_ttf: false,
            },
            Shape::S8on22Bw => ShapeParams {
                font_name: "8x13",
                cell_width: 8,
                cell_height: 22,
                black_ink: true,
                stopword_dim: false,
                is_ttf: false,
            },
            Shape::S8on16Bw => ShapeParams {
                font_name: "8x13",
                cell_width: 8,
                cell_height: 16,
                black_ink: true,
                stopword_dim: false,
                is_ttf: false,
            },
            Shape::S6x12Dim => ShapeParams {
                font_name: "6x12",
                cell_width: 6,
                cell_height: 12,
                black_ink: true,
                stopword_dim: true,
                is_ttf: false,
            },
            Shape::Silver16Bw => ShapeParams {
                font_name: "silver",
                cell_width: 16,
                cell_height: 16,
                black_ink: true,
                stopword_dim: false,
                is_ttf: true,
            },
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Shape::S5x8Bw => "5x8-bw",
            Shape::S8x8Bw => "8x8-bw",
            Shape::S11on16Bw => "11on16-bw",
            Shape::S8on22Bw => "8on22-bw",
            Shape::S8on16Bw => "8on16-bw",
            Shape::S6x12Dim => "6x12-dim",
            Shape::Silver16Bw => "silver16-bw",
        }
    }
}

// ============================================================================
// CLI
// ============================================================================

#[derive(Parser)]
#[command(
    name = "snapcompact",
    version,
    about = "Rasterize text into snapcompact pixel-font PNG frames"
)]
struct Cli {
    /// Text to render (when omitted, read all of stdin)
    text: Option<String>,

    /// Frame shape
    #[arg(long, value_parser = clap::value_parser!(Shape), default_value = "11on16-bw")]
    shape: Shape,

    /// Frame edge in pixels (the original `size`): the bitmap is `size` wide
    /// and the grid holds `floor(size/cellWidth)` x `floor(size/cellHeight/
    /// lineRepeat)` character cells; the height hugs the rows the text uses.
    #[arg(long, default_value_t = 1568)]
    size: u32,

    /// Output PNG path (omit to pipe raw PNG bytes to stdout)
    #[arg(short, long)]
    output: Option<String>,


    /// Enable stopword dimming (gray ink for STOPWORDS)
    #[arg(long)]
    dim: bool,
}

fn main() {
    let cli = Cli::parse();

    let text = if let Some(t) = &cli.text {
        t.clone()
    } else {
        use std::io::Read;
        let mut buf = String::new();
        if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
            eprintln!("error: read stdin: {e}");
            std::process::exit(1);
        }
        buf
    };

    let shape_params = cli.shape.params();
    let use_dim = cli.dim || shape_params.stopword_dim;

    // Resolve font
    let render_font = match shape_params.font_name {
        "5x8" => RenderFont::Bitmap(font_5x8()),
        "8x8" => RenderFont::Bitmap(font_8x8()),
        "8x13" => RenderFont::Bitmap(font_8x13()),
        "6x12" => match get_font_6x12() {
            Ok(f) => RenderFont::Bitmap(f),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
        "silver" => match get_font_silver() {
            Ok(f) => RenderFont::Ttf(f),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
        _ => {
            eprintln!("error: unknown font {}", shape_params.font_name);
            std::process::exit(1);
        }
    };

    // Apply stopword dimming if requested
    let text = if use_dim { dim_stopwords(&text) } else { text };

    // Original `size` scheme: one frame edge in pixels. The bitmap is `size`
    // wide; the grid holds `floor(size/cellWidth)` x `floor(size/cellHeight/
    // lineRepeat)` character cells, and the height hugs the rows the text uses.
    let size = cli.size;
    if size == 0 || size > MAX_FRAME_SIZE {
        eprintln!("error: frame size {size} must be in 1..={MAX_FRAME_SIZE}");
        std::process::exit(1);
    }
    let cell_w = shape_params.cell_width;
    let cell_h = shape_params.cell_height;
    let cols = (size as usize) / cell_w;
    let rows = (size as usize) / cell_h;
    if cols == 0 || rows == 0 {
        eprintln!("error: frame size {size} cannot fit a {cell_w}x{cell_h} cell grid");
        std::process::exit(1);
    }

    let grid = Grid {
        cols,
        rows,
        repeat: 1,
        cell_w,
        cell_h,
    };

    // Compute used rows (hugging); canvas width stays the frame edge.
    let wide_cells = !shape_params.is_ttf;
    let used = used_rows(&text, &grid, false, wide_cells);
    let height = used * cell_h;
    let width = size as usize;

    // Render
    let (png_data, _) = match &render_font {
        RenderFont::Bitmap(font) => {
            let pixels = render_bitmap(&text, width, height, font, &grid, shape_params.black_ink);
            let png = encode_indexed_png(&pixels, width, height).unwrap_or_else(|e| {
                eprintln!("error: {e}");
                std::process::exit(1);
            });
            (png, false)
        }
        RenderFont::Ttf(font) => {
            let pixels = render_ttf_rgb(&text, width, height, font, &grid, shape_params.black_ink);
            let png = encode_rgb_png(&pixels, width, height).unwrap_or_else(|e| {
                eprintln!("error: {e}");
                std::process::exit(1);
            });
            (png, true)
        }
    };

    // Output
    match &cli.output {
        Some(path) => {
            if let Some(parent) = std::path::Path::new(path).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(path, &png_data).unwrap_or_else(|e| {
                eprintln!("error: cannot write {path}: {e}");
                std::process::exit(1);
            });
            eprintln!(
                "wrote {} ({} KB, shape {}, {}x{} cells, {} rows used)",
                path,
                png_data.len() / 1024,
                shape_params.font_name,
                cols,
                rows,
                used,
            );
        }
        None => {
            use std::io::Write;
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            lock.write_all(&png_data).unwrap_or_else(|e| {
                eprintln!("error: write stdout: {e}");
                std::process::exit(1);
            });
        }
    }
}
