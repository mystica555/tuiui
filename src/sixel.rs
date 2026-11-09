//! Minimal Sixel decoder for graphics sent by hosted PTY apps.
//!
//! Sixel is a bitmap format carried in a DCS (`ESC P … q … ESC \`). The embedded
//! terminal emulator doesn't render DCS payloads, so the PTY reader captures
//! sixel DCS bodies and we decode them to a PNG here. The result joins the same
//! image-placement pipeline as Kitty graphics.

use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use std::collections::HashMap;

/// Hard cap on decoded pixels; a hostile PTY can emit arbitrary sixel data.
const MAX_PIXELS: usize = 4_000_000;
const MAX_DIM: u32 = 8192;
const MAX_REPEAT: u32 = 100_000;

/// Decode a sixel DCS body (everything between `ESC P` and `ESC \`) to PNG.
pub fn decode(body: &[u8]) -> Option<Vec<u8>> {
    // Ignore DCS parameters before the sixel introducer: `q` (or `P1;P2;P3 q`).
    let q = body.iter().position(|&b| b == b'q')?;
    if !body[..q].iter().all(|&b| b.is_ascii_digit() || b == b';') {
        return None;
    }
    let data = &body[q + 1..];

    let mut palette = [[0u8, 0, 0, 255]; 256];
    let mut painted: HashMap<(u32, u32), u8> = HashMap::new();
    let (mut declared_w, mut declared_h) = (0u32, 0u32);
    let mut current = 0u8;
    let mut x = 0u32;
    let mut band = 0u32;
    let mut i = 0usize;

    while i < data.len() {
        match data[i] {
            b'"' => {
                i += 1;
                let params = params(data, &mut i);
                if params.len() >= 4 {
                    declared_w = params[2].min(MAX_DIM);
                    declared_h = params[3].min(MAX_DIM);
                }
            }
            b'#' => {
                i += 1;
                let params = params(data, &mut i);
                let Some(&index) = params.first() else { continue };
                let slot = (index & 0xff) as usize;
                match params.get(1).copied() {
                    Some(2) if params.len() >= 5 => {
                        palette[slot] = [pct(params[2]), pct(params[3]), pct(params[4]), 255];
                    }
                    Some(1) if params.len() >= 5 => {
                        let [r, g, b] = hls(params[2], params[3], params[4]);
                        palette[slot] = [r, g, b, 255];
                    }
                    _ => current = slot as u8,
                }
            }
            b'!' => {
                i += 1;
                let start = i;
                while i < data.len() && data[i].is_ascii_digit() {
                    i += 1;
                }
                let count = std::str::from_utf8(&data[start..i])
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1);
                if i >= data.len() {
                    break;
                }
                let ch = data[i];
                i += 1;
                for _ in 0..count.min(MAX_REPEAT) {
                    draw(ch, x, band, current, &mut painted, declared_w, declared_h)?;
                    x += 1;
                }
            }
            b'$' => {
                x = 0;
                i += 1;
            }
            b'-' => {
                band += 1;
                x = 0;
                i += 1;
            }
            b'?'..=b'~' => {
                draw(data[i], x, band, current, &mut painted, declared_w, declared_h)?;
                x += 1;
                i += 1;
            }
            _ => i += 1, // CR/LF/whitespace and unsupported controls
        }
    }

    let max_x = painted.keys().map(|&(x, _)| x + 1).max().unwrap_or(0);
    let max_y = painted.keys().map(|&(_, y)| y + 1).max().unwrap_or(0);
    let width = declared_w.max(max_x);
    let height = declared_h.max(max_y);
    if width == 0 || height == 0 {
        return None;
    }
    if width > MAX_DIM
        || height > MAX_DIM
        || width as usize * height as usize > MAX_PIXELS
    {
        return None;
    }

    let img = RgbaImage::from_fn(width, height, |x, y| {
        painted
            .get(&(x, y))
            .map(|&idx| Rgba(palette[idx as usize]))
            .unwrap_or(Rgba([0, 0, 0, 0]))
    });
    let mut out = std::io::Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(img)
        .write_to(&mut out, ImageFormat::Png)
        .ok()?;
    Some(out.into_inner())
}

/// Parse a `;`-separated list of unsigned decimal parameters.
fn params(data: &[u8], i: &mut usize) -> Vec<u32> {
    let mut out = Vec::new();
    loop {
        let start = *i;
        while *i < data.len() && data[*i].is_ascii_digit() {
            *i += 1;
        }
        if *i == start {
            break;
        }
        let value = std::str::from_utf8(&data[start..*i])
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        out.push(value);
        if data.get(*i) == Some(&b';') {
            *i += 1;
        } else {
            break;
        }
    }
    out
}

/// Set the six bits carried by one sixel data byte.
fn draw(
    ch: u8,
    x: u32,
    band: u32,
    color: u8,
    painted: &mut HashMap<(u32, u32), u8>,
    declared_w: u32,
    declared_h: u32,
) -> Option<()> {
    if !(b'?'..=b'~').contains(&ch) {
        return Some(());
    }
    let bits = ch - b'?';
    for bit in 0..6 {
        if bits & (1 << bit) == 0 {
            continue;
        }
        let y = band * 6 + bit;
        if x >= MAX_DIM || y >= MAX_DIM {
            continue;
        }
        if (declared_w > 0 && x >= declared_w) || (declared_h > 0 && y >= declared_h) {
            continue;
        }
        if painted.len() >= MAX_PIXELS {
            return None;
        }
        painted.insert((x, y), color);
    }
    Some(())
}

/// Sixel colour components are percentages (0–100).
fn pct(v: u32) -> u8 {
    ((v.min(100) * 255 + 50) / 100) as u8
}

/// Convert a sixel HLS definition (hue 0–360, lightness/saturation 0–100) to RGB.
fn hls(h: u32, l: u32, s: u32) -> [u8; 3] {
    let h = (h % 360) as f32;
    let l = l.min(100) as f32 / 100.0;
    let s = s.min(100) as f32 / 100.0;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h as u32) / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [
        ((r + m) * 255.0).round() as u8,
        ((g + m) * 255.0).round() as u8,
        ((b + m) * 255.0).round() as u8,
    ]
}
