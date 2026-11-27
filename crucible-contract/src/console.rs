//! Console text as programs write it to a terminal: SGR escapes become styled runs over the plain
//! text, and every other escape is dropped.
//!
//! ```text
//! "\e[32;1mok\e[0;m done"  ──Pen::write──▶  text "ok done"
//!                                            runs [0..2 fg=Palette(2) bold]
//! ```

pub mod gitlab;

/// A terminal color: the 256-color palette (0–7 normal, 8–15 bright, 16–231 the 6×6×6 cube,
/// 232–255 grays) or 24-bit RGB.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Color {
    Palette(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    /// The sRGB bytes of palette entries 16–255 and of RGB; `None` for 0–15, whose look is the
    /// renderer's theme.
    pub fn fixed_rgb(self) -> Option<(u8, u8, u8)> {
        match self {
            Color::Rgb(r, g, b) => Some((r, g, b)),
            Color::Palette(n @ 16..=231) => {
                let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                let n = n - 16;
                Some((level(n / 36), level(n / 6 % 6), level(n % 6)))
            }
            Color::Palette(n @ 232..=255) => {
                let v = 8 + (n - 232) * 10;
                Some((v, v, v))
            }
            Color::Palette(_) => None,
        }
    }
}

/// What SGR has set. The default is the terminal's plain text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
}

impl Style {
    fn is_plain(&self) -> bool {
        *self == Style::default()
    }

    /// Applies one SGR sequence's parameters, `0;32;1` or `38:2::255:0:0`.
    fn apply(&mut self, params: &str) {
        let mut codes = params.split(';').map(|param| {
            param
                .split(':')
                .map(|sub| sub.parse::<u16>().ok())
                .collect::<Vec<_>>()
        });
        while let Some(code) = codes.next() {
            match code.first().copied().flatten().unwrap_or(0) {
                0 => *self = Style::default(),
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                4 => self.underline = true,
                21 => self.underline = true,
                22 => {
                    self.bold = false;
                    self.dim = false;
                }
                23 => self.italic = false,
                24 => self.underline = false,
                n @ 30..=37 => self.fg = Some(Color::Palette((n - 30) as u8)),
                n @ 40..=47 => self.bg = Some(Color::Palette((n - 40) as u8)),
                n @ 90..=97 => self.fg = Some(Color::Palette((n - 90 + 8) as u8)),
                n @ 100..=107 => self.bg = Some(Color::Palette((n - 100 + 8) as u8)),
                39 => self.fg = None,
                49 => self.bg = None,
                n @ (38 | 48) => {
                    let color = if code.len() > 1 {
                        extended(&code[1..])
                    } else {
                        let kind = codes.next();
                        match kind.as_deref().and_then(|k| k.first().copied().flatten()) {
                            Some(5) => codes.next().and_then(|n| extended(&[Some(5), n[0]])),
                            Some(2) => {
                                let mut rgb = vec![Some(2)];
                                for _ in 0..3 {
                                    rgb.push(codes.next().and_then(|v| v[0]));
                                }
                                extended(&rgb)
                            }
                            _ => None,
                        }
                    };
                    if let Some(color) = color {
                        if n == 38 {
                            self.fg = Some(color);
                        } else {
                            self.bg = Some(color);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// `5;n` or `2;r;g;b` after a 38 or 48; the colon form may put an empty color space id before
/// `r` (`2::r:g:b`).
fn extended(args: &[Option<u16>]) -> Option<Color> {
    let byte = |v: Option<u16>| v.and_then(|v| u8::try_from(v).ok());
    match args.first().copied().flatten()? {
        5 => Some(Color::Palette(byte(*args.get(1)?)?)),
        2 => {
            let rgb = if args.len() >= 5 {
                &args[2..5]
            } else {
                args.get(1..4)?
            };
            Some(Color::Rgb(byte(rgb[0])?, byte(rgb[1])?, byte(rgb[2])?))
        }
        _ => None,
    }
}

/// A styled byte range of [`Styled::text`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub start: usize,
    pub end: usize,
    pub style: Style,
}

/// One line of console text without its escapes, and the runs SGR styled. Runs are ordered, do
/// not overlap, and skip plain text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Styled {
    pub text: String,
    pub runs: Vec<Run>,
}

impl Styled {
    fn push(&mut self, c: char, style: Style) {
        let start = self.text.len();
        self.text.push(c);
        if style.is_plain() {
            return;
        }
        match self.runs.last_mut() {
            Some(run) if run.end == start && run.style == style => run.end = self.text.len(),
            _ => self.runs.push(Run {
                start,
                end: self.text.len(),
                style,
            }),
        }
    }

    fn clear(&mut self) {
        self.text.clear();
        self.runs.clear();
    }
}

/// Reads console lines in order, carrying SGR state from one line to the next as a terminal
/// does.
#[derive(Clone, Debug, Default)]
pub struct Pen {
    style: Style,
}

impl Pen {
    /// `raw` as styled text. A carriage return followed by more text starts the line over, so a
    /// progress bar shows its last state.
    pub fn write(&mut self, raw: &str) -> Styled {
        let mut out = Styled::default();
        let mut chars = raw.chars().peekable();
        let mut returned = false;
        while let Some(c) = chars.next() {
            match c {
                '\u{1b}' => match chars.next() {
                    Some('[') => {
                        let mut params = String::new();
                        for c in chars.by_ref() {
                            if ('\u{40}'..='\u{7e}').contains(&c) {
                                if c == 'm' {
                                    self.style.apply(&params);
                                }
                                break;
                            }
                            params.push(c);
                        }
                    }
                    Some(']') => {
                        while let Some(c) = chars.next() {
                            if c == '\u{7}' {
                                break;
                            }
                            if c == '\u{1b}' {
                                if chars.peek() == Some(&'\\') {
                                    chars.next();
                                }
                                break;
                            }
                        }
                    }
                    Some(c) if ('\u{20}'..='\u{2f}').contains(&c) => {
                        for c in chars.by_ref() {
                            if !('\u{20}'..='\u{2f}').contains(&c) {
                                break;
                            }
                        }
                    }
                    _ => {}
                },
                '\r' => returned = true,
                '\t' => {
                    if std::mem::take(&mut returned) {
                        out.clear();
                    }
                    out.push(c, self.style);
                }
                c if c.is_control() => {}
                c => {
                    if std::mem::take(&mut returned) {
                        out.clear();
                    }
                    out.push(c, self.style);
                }
            }
        }
        out
    }
}

/// `text` without its escape sequences.
pub fn plain(text: &str) -> String {
    Pen::default().write(text).text
}

#[cfg(test)]
mod tests {
    use crate::console::{Color, Pen, Run, Style, Styled, plain};

    /// A `tracing_subscriber` line with spans, as an `[mcp]` server wrote it with colors on.
    pub const COLORED: &str = "\u{1b}[2m2026-11-09T18:02:41.069974Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m \u{1b}[1mcve-triage\u{1b}[0m\u{1b}[1m{\u{1b}[0m\u{1b}[3mcommand\u{1b}[0m\u{1b}[2m=\u{1b}[0m\"discover\"\u{1b}[1m}\u{1b}[0m\u{1b}[2m:\u{1b}[0m\u{1b}[1mtriage.prepare\u{1b}[0m\u{1b}[1m{\u{1b}[0m\u{1b}[3mprofile\u{1b}[0m\u{1b}[2m=\u{1b}[0mllm-d\u{1b}[1m}\u{1b}[0m\u{1b}[2m:\u{1b}[0m \u{1b}[2mcve_triage::tickets\u{1b}[0m\u{1b}[2m:\u{1b}[0m [llm-d] Jira search returned 176 ticket(s)";

    fn styled(raw: &str) -> Styled {
        Pen::default().write(raw)
    }

    fn fg(n: u8) -> Style {
        Style {
            fg: Some(Color::Palette(n)),
            ..Style::default()
        }
    }

    fn run(start: usize, end: usize, style: Style) -> Run {
        Run { start, end, style }
    }

    #[test]
    fn a_colored_tracing_line_reads_as_its_text_with_its_styles() {
        let line = styled(COLORED);
        assert_eq!(
            line.text,
            "2026-11-09T18:02:41.069974Z  INFO cve-triage{command=\"discover\"}:triage.prepare{profile=llm-d}: cve_triage::tickets: [llm-d] Jira search returned 176 ticket(s)"
        );
        let dim = Style {
            dim: true,
            ..Style::default()
        };
        assert_eq!(line.runs[0], run(0, 27, dim));
        assert_eq!(line.runs[1], run(28, 33, fg(2)));
        let bold = Style {
            bold: true,
            ..Style::default()
        };
        assert_eq!(line.runs[2], run(34, 45, bold), "adjacent bold runs merge");
        assert_eq!(&line.text[34..45], "cve-triage{");
        let italic = line.runs.iter().find(|r| r.style.italic).expect("italic");
        assert_eq!(&line.text[italic.start..italic.end], "command");
        assert!(line.runs.windows(2).all(|w| w[0].end <= w[1].start));
    }

    #[test]
    fn the_sixteen_colors_and_their_bright_halves_map_to_palette_slots() {
        for n in 0..8u8 {
            assert_eq!(styled(&format!("\u{1b}[3{n}mx")).runs[0].style, fg(n));
            assert_eq!(styled(&format!("\u{1b}[9{n}mx")).runs[0].style, fg(n + 8));
            assert_eq!(
                styled(&format!("\u{1b}[4{n}mx")).runs[0].style.bg,
                Some(Color::Palette(n))
            );
            assert_eq!(
                styled(&format!("\u{1b}[10{n}mx")).runs[0].style.bg,
                Some(Color::Palette(n + 8))
            );
        }
    }

    #[test]
    fn extended_colors_read_in_both_separators() {
        let cases = [
            ("\u{1b}[38;5;208mx", Color::Palette(208)),
            ("\u{1b}[38:5:208mx", Color::Palette(208)),
            ("\u{1b}[38;2;255;100;0mx", Color::Rgb(255, 100, 0)),
            ("\u{1b}[38:2:255:100:0mx", Color::Rgb(255, 100, 0)),
            ("\u{1b}[38:2::255:100:0mx", Color::Rgb(255, 100, 0)),
        ];
        for (raw, color) in cases {
            assert_eq!(styled(raw).runs[0].style.fg, Some(color), "{raw:?}");
        }
        let both = styled("\u{1b}[1;38;5;9;48;2;1;2;3;4mx").runs[0].style;
        assert_eq!(
            both,
            Style {
                fg: Some(Color::Palette(9)),
                bg: Some(Color::Rgb(1, 2, 3)),
                bold: true,
                underline: true,
                ..Style::default()
            }
        );
        assert_eq!(
            styled("\u{1b}[38;5;300mx").runs,
            vec![],
            "out of range is ignored"
        );
        assert_eq!(styled("\u{1b}[38;2;1;2mx").runs, vec![], "cut short");
        assert_eq!(styled("\u{1b}[38mx").runs, vec![]);
    }

    #[test]
    fn palette_entries_past_sixteen_have_fixed_rgb() {
        assert_eq!(Color::Palette(1).fixed_rgb(), None);
        assert_eq!(Color::Palette(15).fixed_rgb(), None);
        assert_eq!(Color::Palette(16).fixed_rgb(), Some((0, 0, 0)));
        assert_eq!(Color::Palette(196).fixed_rgb(), Some((255, 0, 0)));
        assert_eq!(Color::Palette(208).fixed_rgb(), Some((255, 135, 0)));
        assert_eq!(Color::Palette(231).fixed_rgb(), Some((255, 255, 255)));
        assert_eq!(Color::Palette(232).fixed_rgb(), Some((8, 8, 8)));
        assert_eq!(Color::Palette(255).fixed_rgb(), Some((238, 238, 238)));
        assert_eq!(Color::Rgb(1, 2, 3).fixed_rgb(), Some((1, 2, 3)));
    }

    #[test]
    fn attributes_set_and_clear_one_at_a_time() {
        let line =
            styled("\u{1b}[1;2;3;4ma\u{1b}[22mb\u{1b}[23mc\u{1b}[24md\u{1b}[31me\u{1b}[39mf");
        assert_eq!(line.text, "abcdef");
        let all = Style {
            bold: true,
            dim: true,
            italic: true,
            underline: true,
            ..Style::default()
        };
        assert_eq!(line.runs[0], run(0, 1, all));
        assert_eq!(
            line.runs[1].style,
            Style {
                italic: true,
                underline: true,
                ..Style::default()
            }
        );
        assert_eq!(
            line.runs[2].style,
            Style {
                underline: true,
                ..Style::default()
            }
        );
        assert_eq!(line.runs[3], run(4, 5, fg(1)));
        assert_eq!(line.runs.len(), 4, "f is plain again");
        let reset = styled("\u{1b}[1;31ma\u{1b}[mb\u{1b}[0;mc\u{1b}[42;49md");
        assert_eq!(
            reset.runs.len(),
            1,
            "empty, 0; and set-then-unset all reset"
        );
    }

    #[test]
    fn style_carries_to_the_next_line_until_reset() {
        let mut pen = Pen::default();
        assert_eq!(pen.write("\u{1b}[33mwarn").runs, vec![run(0, 4, fg(3))]);
        assert_eq!(pen.write("still").runs, vec![run(0, 5, fg(3))]);
        assert_eq!(pen.write("\u{1b}[0mplain").runs, vec![]);
    }

    #[test]
    fn other_escapes_go_whole_and_returns_overwrite() {
        assert_eq!(plain("no escapes [2m here"), "no escapes [2m here");
        assert_eq!(plain("a\u{1b}[38;5;208mb\u{1b}[Kc"), "abc");
        assert_eq!(plain("\u{1b}[0Kcleared\u{1b}[2A\u{1b}[1G"), "cleared");
        assert_eq!(
            plain("\u{1b}]8;;https://example.com\u{7}link\u{1b}]8;;\u{1b}\\ done"),
            "link done"
        );
        assert_eq!(plain("x\u{1b}(By"), "xy");
        assert_eq!(plain("cut off \u{1b}[3"), "cut off ");
        assert_eq!(plain(" 10%\r 55%\r100% done"), "100% done");
        assert_eq!(plain("kept\r"), "kept");
        assert_eq!(plain("kept\r\u{1b}[0K"), "kept");
        assert_eq!(plain("a\tb\u{7}\u{8}c"), "a\tbc");
        let line = styled("\u{1b}[31mold\r\u{1b}[32mnew");
        assert_eq!(line.text, "new");
        assert_eq!(line.runs, vec![run(0, 3, fg(2))]);
    }

    #[test]
    fn runs_index_bytes_of_multibyte_text() {
        let line = styled("\u{1b}[35m\u{2714} pass\u{1b}[0m \u{1f527}");
        assert_eq!(line.runs, vec![run(0, 8, fg(5))]);
        assert_eq!(&line.text[0..8], "\u{2714} pass");
    }
}
