//! The colours a chart draws its series, segments and stages in: a tone's own colour, or for
//! one left neutral the next colour of a categorical palette.

use std::collections::HashMap;

use crucible_view::{Style, Tone};
use rune::color::{srgb_decode, srgb_encode};
use rune::{Color, oklch};
use rune_kit::Kit;

/// What a chart's item is drawn in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Hue {
    Tone(Tone),
    /// The palette's colour at this index, wrapping past its end.
    Palette(usize),
}

/// The hue of a chart's only item: its tone, or the palette's first colour when neutral.
impl From<Tone> for Hue {
    fn from(tone: Tone) -> Hue {
        match tone {
            Tone::Neutral => Hue::Palette(0),
            tone => Hue::Tone(tone),
        }
    }
}

/// Hands out hues by name: a neutral name takes the palette's next colour and a toned one
/// keeps its tone. A name seen again keeps its first hue.
#[derive(Default)]
pub(crate) struct Hues<'a> {
    seen: HashMap<&'a str, Hue>,
    next: usize,
}

impl<'a> Hues<'a> {
    pub fn pick(&mut self, name: &'a str, tone: Tone) -> Hue {
        if let Some(hue) = self.seen.get(name) {
            return *hue;
        }
        let hue = match tone {
            Tone::Neutral => {
                self.next += 1;
                Hue::Palette(self.next - 1)
            }
            tone => Hue::Tone(tone),
        };
        self.seen.insert(name, hue);
        hue
    }
}

/// Hues around the wheel, ordered so neighbours in the list stand apart.
const HUES: [(f32, f32); 10] = [
    (255.0, 1.0),
    (55.0, 1.0),
    (175.0, 1.0),
    (330.0, 1.0),
    (100.0, 1.0),
    (290.0, 1.0),
    (220.0, 1.0),
    (15.0, 1.0),
    (140.0, 1.0),
    (70.0, 0.55),
];

/// The categorical palette for a dark theme or a light one: one lightness and chroma, so no
/// colour stands out, with the last a quieter brown.
pub(crate) fn palette(dark: bool) -> [Color; 10] {
    let (lightness, chroma) = if dark { (0.74, 0.14) } else { (0.57, 0.15) };
    HUES.map(|(hue, scale)| {
        let lightness = if scale < 1.0 {
            lightness - 0.1
        } else {
            lightness
        };
        oklch(lightness, chroma * scale, hue)
    })
}

/// The colour a tone's text is written in.
pub(crate) fn ink(tone: Tone, k: &Kit) -> Color {
    match tone {
        Tone::Neutral => k.c.text_base,
        Tone::Muted => k.c.text_muted,
        Tone::Good => k.c.success.fg,
        Tone::Warn => k.c.warning.fg,
        Tone::Bad => k.c.danger.fg,
        Tone::Info => k.c.info.fg,
    }
}

/// The colour a tone's mark is drawn in.
pub(crate) fn dot(tone: Tone, k: &Kit) -> Color {
    match tone {
        Tone::Neutral | Tone::Muted => k.c.icon_faint,
        Tone::Good => k.c.success.icon,
        Tone::Warn => k.c.warning.icon,
        Tone::Bad => k.c.danger.icon,
        Tone::Info => k.c.info.icon,
    }
}

pub(crate) fn colour(hue: Hue, k: &Kit) -> Color {
    match hue {
        Hue::Tone(tone) => dot(tone, k),
        Hue::Palette(i) => palette(k.dark)[i % HUES.len()],
    }
}

/// `hue` as a canvas style: a tone by name, a palette colour as its value.
pub(crate) fn style(hue: Hue, k: &Kit) -> Style {
    match hue {
        Hue::Tone(tone) => Style::Tone(tone),
        Hue::Palette(_) => Style::Rgba(rgba(colour(hue, k))),
    }
}

/// `color` as sRGB bytes with straight alpha.
fn rgba(color: Color) -> [u8; 4] {
    let byte = |c: f32| (c * 255.0).round() as u8;
    [
        byte(srgb_encode(color.r)),
        byte(srgb_encode(color.g)),
        byte(srgb_encode(color.b)),
        byte(color.a),
    ]
}

/// sRGB bytes with straight alpha as a colour.
pub(crate) fn from_rgba([r, g, b, a]: [u8; 4]) -> Color {
    let unit = |c: u8| f32::from(c) / 255.0;
    Color {
        r: srgb_decode(unit(r)),
        g: srgb_decode(unit(g)),
        b: srgb_decode(unit(b)),
        a: unit(a),
    }
}

#[cfg(test)]
mod tests {
    use crucible_view::{Style, Tone};
    use rune::Color;
    use rune_kit::Kit;

    use crate::ui::report::hue::{Hue, Hues, colour, from_rgba, palette, rgba, style};

    #[test]
    fn a_neutral_name_takes_the_next_colour_and_keeps_it() {
        let names: Vec<String> = (3..30).map(|i| i.to_string()).collect();
        let mut hues = Hues::default();
        assert_eq!(hues.pick("a", Tone::Neutral), Hue::Palette(0));
        assert_eq!(hues.pick("bad", Tone::Bad), Hue::Tone(Tone::Bad));
        assert_eq!(hues.pick("b", Tone::Neutral), Hue::Palette(1));
        assert_eq!(hues.pick("a", Tone::Neutral), Hue::Palette(0));
        assert_eq!(
            hues.pick("a", Tone::Good),
            Hue::Palette(0),
            "a name keeps its first hue"
        );
        assert_eq!(hues.pick("muted", Tone::Muted), Hue::Tone(Tone::Muted));
        assert_eq!(hues.pick("", Tone::Neutral), Hue::Palette(2));
        for (i, name) in (3..).zip(&names) {
            assert_eq!(hues.pick(name, Tone::Neutral), Hue::Palette(i));
        }
    }

    #[test]
    fn the_palette_s_colours_stand_apart_and_off_the_background_in_both_themes() {
        for dark in [true, false] {
            let kit = Kit::neutral(dark);
            let colours = palette(dark);
            let lab = colours.map(|c| c.to_oklab());
            for i in 0..lab.len() {
                for j in i + 1..lab.len() {
                    let apart = (0..3)
                        .map(|n| (lab[i][n] - lab[j][n]).powi(2))
                        .sum::<f32>()
                        .sqrt();
                    assert!(apart > 0.07, "{dark} {i} {j} {apart}");
                }
            }
            let ground = kit.c.layer_01.luminance();
            for (i, c) in colours.iter().enumerate() {
                let (a, b) = (c.luminance(), ground);
                let contrast = (a.max(b) + 0.05) / (a.min(b) + 0.05);
                assert!(contrast > 2.0, "{dark} {i} {contrast}");
            }
            assert_eq!(colour(Hue::Palette(10), &kit), colours[0], "wraps");
            assert_eq!(colour(Hue::Palette(3), &kit), colours[3]);
        }
        assert_ne!(palette(true), palette(false));
    }

    #[test]
    fn a_palette_colour_reaches_a_canvas_as_its_value() {
        let kit = Kit::neutral(true);
        assert_eq!(style(Hue::Tone(Tone::Bad), &kit), Style::Tone(Tone::Bad));
        let Style::Rgba([r, g, b, a]) = style(Hue::Palette(0), &kit) else {
            panic!("a palette colour is a value");
        };
        assert_eq!(a, 255);
        assert!(b > r && b > g, "the first colour is blue: {r} {g} {b}");
    }

    #[test]
    fn a_colour_s_bytes_match_its_hex_both_ways() {
        let kit = Kit::neutral(false);
        for color in palette(true).into_iter().chain(palette(false)).chain([
            kit.c.text_base,
            Color::WHITE,
            Color::TRANSPARENT,
        ]) {
            assert_eq!(
                Ok(Style::Rgba(rgba(color))),
                color.hex().parse(),
                "{color:?}"
            );
        }
        for bytes in [
            [0, 0, 0, 0],
            [1, 2, 3, 4],
            [0x3b, 0x82, 0xf6, 0x80],
            [255; 4],
        ] {
            assert_eq!(
                Some(from_rgba(bytes)),
                rune::color::hex(&Style::Rgba(bytes).to_string())
            );
            assert_eq!(rgba(from_rgba(bytes)), bytes);
        }
    }

    #[test]
    fn a_lone_item_takes_its_tone_or_the_first_colour() {
        for tone in [
            Tone::Neutral,
            Tone::Muted,
            Tone::Good,
            Tone::Warn,
            Tone::Bad,
            Tone::Info,
        ] {
            assert_eq!(Hue::from(tone), Hues::default().pick("", tone));
        }
    }
}
