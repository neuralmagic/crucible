//! Inline terminal image emission for `plan show --render`: PNG bytes onto the tty via the
//! iTerm2 (OSC 1337) or kitty graphics protocol, detected from the environment. Terminals
//! speaking neither get a PNG file instead; the caller handles that fallback.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageProtocol {
    Iterm,
    Kitty,
}

/// HiDPI fallback for terminals that omit pixel geometry.
const ASSUMED_CELL_WIDTH_PX: u32 = 16;
const ASSUMED_CELL_HEIGHT_PX: f32 = 32.0;

/// The tty's usable geometry, in physical pixels.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub cols: u16,
    pub width_px: u32,
    pub cell_height_px: f32,
}

/// Query stdout geometry, or return `None` for non-TTY output.
pub fn geometry() -> Option<Geometry> {
    let ws = rustix::termios::tcgetwinsize(std::io::stdout()).ok()?;
    if ws.ws_col == 0 {
        return None;
    }
    let width_px = if ws.ws_xpixel > 0 {
        u32::from(ws.ws_xpixel)
    } else {
        u32::from(ws.ws_col) * ASSUMED_CELL_WIDTH_PX
    };
    let cell_height_px = if ws.ws_ypixel > 0 && ws.ws_row > 0 {
        f32::from(ws.ws_ypixel) / f32::from(ws.ws_row)
    } else {
        ASSUMED_CELL_HEIGHT_PX
    };
    Some(Geometry {
        cols: ws.ws_col,
        width_px,
        cell_height_px,
    })
}

/// Best-effort protocol sniff. WezTerm speaks the iTerm protocol; ghostty and kitty speak
/// the kitty graphics protocol.
pub fn detect() -> Option<ImageProtocol> {
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    if term_program.contains("iTerm") || term_program.contains("WezTerm") {
        return Some(ImageProtocol::Iterm);
    }
    let term = std::env::var("TERM").unwrap_or_default();
    if term.contains("kitty")
        || term.contains("ghostty")
        || std::env::var("KITTY_WINDOW_ID").is_ok()
    {
        return Some(ImageProtocol::Kitty);
    }
    None
}

/// The escape-sequence payload that displays `png` inline.
///
/// `fit_cols` is `Some` only when the raster overflows the viewport. Neither protocol is
/// told a height, so a deep DAG scrolls rather than being squashed.
pub fn emit(proto: ImageProtocol, png: &[u8], fit_cols: Option<u16>) -> String {
    let b64 = B64.encode(png);
    match proto {
        ImageProtocol::Iterm => {
            // A bare `width=N` is N character cells; height stays `auto` so
            // preserveAspectRatio picks it.
            let width = match fit_cols {
                Some(cols) => format!("width={cols};"),
                None => String::new(),
            };
            format!(
                "\x1b]1337;File=inline=1;size={};{width}preserveAspectRatio=1:{}\x07\n",
                png.len(),
                b64
            )
        }
        ImageProtocol::Kitty => {
            // Chunked APC: f=100 (PNG), a=T (transmit + display), m=1 on every chunk but
            // the last. 4096 is the protocol's max chunk payload. `c` without `r` means kitty
            // computes the row count from the aspect ratio.
            let cols = match fit_cols {
                Some(cols) => format!("c={cols},"),
                None => String::new(),
            };
            let mut out = String::new();
            let chunks: Vec<&[u8]> = b64.as_bytes().chunks(4096).collect();
            let last = chunks.len().saturating_sub(1);
            for (i, chunk) in chunks.iter().enumerate() {
                let payload = std::str::from_utf8(chunk).unwrap_or_default();
                if i == 0 {
                    let m = if last == 0 { 0 } else { 1 };
                    out.push_str(&format!("\x1b_Gf=100,a=T,{cols}m={m};{payload}\x1b\\"));
                } else {
                    let m = if i == last { 0 } else { 1 };
                    out.push_str(&format!("\x1b_Gm={m};{payload}\x1b\\"));
                }
            }
            out.push('\n');
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iterm_payload_wraps_base64_png() {
        let out = emit(ImageProtocol::Iterm, b"png-bytes", Some(80));
        assert!(out.starts_with("\x1b]1337;File=inline=1;size=9;width=80;preserveAspectRatio=1:"));
        assert!(out.contains(&B64.encode(b"png-bytes")));
        assert!(out.ends_with("\x07\n"));
    }

    #[test]
    fn kitty_payload_chunks_and_terminates() {
        // > 4096 b64 chars forces multiple chunks: first m=1, last m=0.
        let big = vec![0u8; 6000];
        let out = emit(ImageProtocol::Kitty, &big, Some(80));
        assert!(out.starts_with("\x1b_Gf=100,a=T,c=80,m=1;"));
        assert!(out.contains("\x1b_Gm=0;"));
        let small = emit(ImageProtocol::Kitty, b"x", Some(80));
        assert!(
            small.starts_with("\x1b_Gf=100,a=T,c=80,m=0;"),
            "single chunk ends immediately"
        );
    }

    #[test]
    fn placement_pins_width_only_so_tall_graphs_scroll() {
        let iterm = emit(ImageProtocol::Iterm, b"x", Some(120));
        assert!(!iterm.contains("height="));
        let kitty = emit(ImageProtocol::Kitty, b"x", Some(120));
        assert!(!kitty.contains("r="), "no row count in {kitty:?}");
    }

    #[test]
    fn a_diagram_that_fits_is_emitted_at_native_size() {
        let iterm = emit(ImageProtocol::Iterm, b"x", None);
        assert!(!iterm.contains("width="), "no width in {iterm:?}");
        let kitty = emit(ImageProtocol::Kitty, b"x", None);
        assert!(kitty.starts_with("\x1b_Gf=100,a=T,m=0;"));
        assert!(!kitty.contains("c="), "no column count in {kitty:?}");
    }
}
