//! Inline terminal image emission for `plan show --render`: PNG bytes onto the tty via the
//! iTerm2 (OSC 1337) or kitty graphics protocol, detected from the environment. Terminals
//! speaking neither get a PNG file instead — the caller handles that fallback.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageProtocol {
    Iterm,
    Kitty,
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
pub fn emit(proto: ImageProtocol, png: &[u8]) -> String {
    let b64 = B64.encode(png);
    match proto {
        ImageProtocol::Iterm => {
            format!(
                "\x1b]1337;File=inline=1;size={};preserveAspectRatio=1:{}\x07\n",
                png.len(),
                b64
            )
        }
        ImageProtocol::Kitty => {
            // Chunked APC: f=100 (PNG), a=T (transmit + display), m=1 on every chunk but
            // the last. 4096 is the protocol's max chunk payload.
            let mut out = String::new();
            let chunks: Vec<&[u8]> = b64.as_bytes().chunks(4096).collect();
            let last = chunks.len().saturating_sub(1);
            for (i, chunk) in chunks.iter().enumerate() {
                let payload = std::str::from_utf8(chunk).unwrap_or_default();
                if i == 0 {
                    let m = if last == 0 { 0 } else { 1 };
                    out.push_str(&format!("\x1b_Gf=100,a=T,m={m};{payload}\x1b\\"));
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
        let out = emit(ImageProtocol::Iterm, b"png-bytes");
        assert!(out.starts_with("\x1b]1337;File=inline=1;size=9;"));
        assert!(out.contains(&B64.encode(b"png-bytes")));
        assert!(out.ends_with("\x07\n"));
    }

    #[test]
    fn kitty_payload_chunks_and_terminates() {
        // > 4096 b64 chars forces multiple chunks: first m=1, last m=0.
        let big = vec![0u8; 6000];
        let out = emit(ImageProtocol::Kitty, &big);
        assert!(out.starts_with("\x1b_Gf=100,a=T,m=1;"));
        assert!(out.contains("\x1b_Gm=0;"));
        let small = emit(ImageProtocol::Kitty, b"x");
        assert!(
            small.starts_with("\x1b_Gf=100,a=T,m=0;"),
            "single chunk ends immediately"
        );
    }
}
