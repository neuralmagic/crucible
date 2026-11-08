//! The app's kits: OpenCode's tokens, type, radii and elevation, recoloured per theme.

use std::sync::OnceLock;

use rune::Color;
use rune_kit::{Kit, PaletteSeed};

use crate::settings::Theme;

/// Background, ink and accent a theme derives its palette from, dark then light.
type Seed = (&'static str, &'static str, &'static str);

fn seeds(theme: Theme) -> Option<[Seed; 2]> {
    match theme {
        Theme::Graphite => None,
        Theme::Midnight => Some([
            ("#0b0e17", "#e6e9f2", "#6e8bff"),
            ("#f6f7fb", "#141824", "#3d5bdb"),
        ]),
        Theme::Ember => Some([
            ("#120d0b", "#f2ebe6", "#ff7a3d"),
            ("#fbf7f4", "#1d1512", "#e2551b"),
        ]),
        Theme::Moss => Some([
            ("#0b110e", "#e4eee8", "#3ecf8e"),
            ("#f5f9f6", "#13201a", "#12a26a"),
        ]),
    }
}

fn hex(code: &str) -> Color {
    rune::color::hex(code).unwrap_or(Color::BLACK)
}

pub(crate) fn kit(theme: Theme, dark: bool) -> &'static Kit {
    static KITS: [[OnceLock<Kit>; 2]; 4] = [const { [const { OnceLock::new() }; 2] }; 4];
    let slot = Theme::ALL.iter().position(|t| *t == theme).unwrap_or(0);
    KITS[slot][dark as usize].get_or_init(|| {
        let base = rune_theme_opencode::kit(dark);
        let Some(seeds) = seeds(theme) else {
            return base.clone();
        };
        let (bg, fg, accent) = seeds[if dark { 0 } else { 1 }];
        let mut kit = base.clone();
        kit.c = Kit::from_palette(PaletteSeed {
            bg: hex(bg),
            fg: hex(fg),
            accent: hex(accent),
        })
        .c;
        kit
    })
}
