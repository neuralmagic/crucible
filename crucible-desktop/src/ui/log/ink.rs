//! Terminal colors and attributes as text spans in the app's light or dark theme.

use crucible_contract::console::{self, Run};
use rune::color::srgb_decode;
use rune::{Color, Span};

const DARK: [u32; 16] = [
    0x484f58, 0xff7b72, 0x3fb950, 0xd29922, 0x58a6ff, 0xbc8cff, 0x39c5cf, 0xb1bac4, 0x6e7681,
    0xffa198, 0x56d364, 0xe3b341, 0x79c0ff, 0xd2a8ff, 0x56d4dd, 0xffffff,
];

const LIGHT: [u32; 16] = [
    0x24292f, 0xcf222e, 0x116329, 0x4d2d00, 0x0969da, 0x8250df, 0x1b7c83, 0x6e7781, 0x57606a,
    0xa40e26, 0x1a7f37, 0x633c01, 0x218bff, 0xa475f9, 0x3192aa, 0x8c959f,
];

const DIM: f32 = 0.6;

fn srgb(r: u8, g: u8, b: u8) -> Color {
    let channel = |v: u8| srgb_decode(f32::from(v) / 255.0);
    Color {
        r: channel(r),
        g: channel(g),
        b: channel(b),
        a: 1.0,
    }
}

pub(crate) fn color(color: console::Color, dark: bool) -> Color {
    let (r, g, b) = match (color, color.fixed_rgb()) {
        (_, Some(rgb)) => rgb,
        (console::Color::Palette(n), None) => {
            let hex = if dark { DARK } else { LIGHT }[usize::from(n & 15)];
            ((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
        }
        (console::Color::Rgb(r, g, b), None) => (r, g, b),
    };
    srgb(r, g, b)
}

pub(crate) fn spans(runs: &[Run], ink: Color, dark: bool) -> Vec<Span> {
    runs.iter()
        .map(|run| {
            let style = run.style;
            let fg = style.fg.map(|fg| color(fg, dark));
            let mut span = Span::new(run.start, run.end);
            let shown = fg.unwrap_or(ink);
            if style.dim {
                span = span.color(shown.alpha(DIM));
            } else if let Some(fg) = fg {
                span = span.color(fg);
            }
            if style.bold {
                span = span.bold();
            }
            if style.italic {
                span = span.italic();
            }
            if style.underline {
                span = span.underline(shown);
            }
            if let Some(bg) = style.bg {
                span = span.bg(color(bg, dark), 1.0);
            }
            span
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crucible_contract::console::{Color as Term, Pen};
    use rune::Color;

    use crate::ui::log::ink::{DARK, LIGHT, color, spans};

    #[test]
    fn the_sixteen_follow_the_theme_and_the_rest_are_fixed() {
        for n in 0..16u8 {
            assert_ne!(
                color(Term::Palette(n), true),
                color(Term::Palette(n), false)
            );
        }
        assert_ne!(DARK, LIGHT);
        let red = color(Term::Palette(196), true);
        assert_eq!(red, color(Term::Palette(196), false));
        assert_eq!(red, color(Term::Rgb(255, 0, 0), true));
        assert_eq!((red.r, red.g, red.b, red.a), (1.0, 0.0, 0.0, 1.0));
        assert_eq!(color(Term::Rgb(0, 0, 0), false), Color::BLACK);
    }

    #[test]
    fn each_run_becomes_one_span_over_the_same_bytes() {
        let ink = Color::WHITE;
        let line = Pen::default().write(
            "\u{1b}[32;1mok\u{1b}[0m \u{1b}[2mdim\u{1b}[0m \u{1b}[4;3;41mbad\u{1b}[0m \u{1b}[2;34mfaint\u{1b}[0m",
        );
        let spans = spans(&line.runs, ink, true);
        assert_eq!(spans.len(), 4);
        let green = color(Term::Palette(2), true);
        assert_eq!((spans[0].start, spans[0].end), (0, 2));
        assert_eq!(spans[0].color, Some(green));
        assert_eq!(spans[0].weight, Some(600.0));
        assert_eq!(spans[1].color, Some(ink.alpha(0.6)));
        assert_eq!(spans[1].weight, None);
        assert!(spans[2].italic);
        assert_eq!(spans[2].underline, Some(ink));
        assert_eq!(spans[2].bg, Some(color(Term::Palette(1), true)));
        assert_eq!(spans[2].color, None, "no fg keeps the line's ink");
        assert_eq!(
            spans[3].color,
            Some(color(Term::Palette(4), true).alpha(0.6))
        );
        assert_eq!(
            &line.text[spans[3].start as usize..spans[3].end as usize],
            "faint"
        );
    }
}
