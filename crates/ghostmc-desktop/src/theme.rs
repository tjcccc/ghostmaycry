//! Colors, following the system appearance (light or dark). Kept plain:
//! this is a tool, not a game.

use gpui::{Rgba, Window, WindowAppearance};

#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Rgba,
    pub panel: Rgba,
    pub raised: Rgba,
    pub hover: Rgba,
    pub selected: Rgba,
    pub border: Rgba,
    pub text: Rgba,
    pub muted: Rgba,
    pub accent: Rgba,
    pub warn: Rgba,
    pub bad: Rgba,
    pub claude: Rgba,
    pub codex: Rgba,
}

const LIGHT: Palette = Palette {
    bg: rgb_const(0xffffff),
    panel: rgb_const(0xf6f7f9),
    raised: rgb_const(0xffffff),
    hover: rgb_const(0xf0f3f8),
    selected: rgb_const(0xe7f0fc),
    border: rgb_const(0xdfe3ea),
    text: rgb_const(0x1f2329),
    muted: rgb_const(0x7b8290),
    accent: rgb_const(0x2f6fdf),
    warn: rgb_const(0xc98a12),
    bad: rgb_const(0xd03b45),
    claude: rgb_const(0xd97757),
    codex: rgb_const(0x5b6cf0),
};

const DARK: Palette = Palette {
    bg: rgb_const(0x1b1d22),
    panel: rgb_const(0x22252b),
    raised: rgb_const(0x2a2e36),
    hover: rgb_const(0x2b3039),
    selected: rgb_const(0x253248),
    border: rgb_const(0x343944),
    text: rgb_const(0xe3e6ec),
    muted: rgb_const(0x8d94a3),
    accent: rgb_const(0x7aa2f7),
    warn: rgb_const(0xe8b45c),
    bad: rgb_const(0xf07178),
    claude: rgb_const(0xe08a6b),
    codex: rgb_const(0x8c98ff),
};

const fn rgb_const(hex: u32) -> Rgba {
    Rgba {
        r: ((hex >> 16) & 0xff) as f32 / 255.,
        g: ((hex >> 8) & 0xff) as f32 / 255.,
        b: (hex & 0xff) as f32 / 255.,
        a: 1.,
    }
}

fn is_dark(window: &Window) -> bool {
    matches!(
        window.appearance(),
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    )
}

pub fn palette(window: &Window) -> Palette {
    if is_dark(window) { DARK } else { LIGHT }
}

/// Colors for content drawn over Liquid Glass: no fills of its own, and
/// translucent tints for hover, selection, buttons and borders.
pub fn on_glass(window: &Window) -> Palette {
    let dark = is_dark(window);
    let base = palette(window);
    let tint = |hex: u32, a: f32| Rgba {
        a,
        ..rgb_const(hex)
    };
    let (ink, wash) = if dark {
        (0xffffff, 0xffffff)
    } else {
        (0x000000, 0xffffff)
    };
    Palette {
        bg: tint(0, 0.),
        panel: tint(0, 0.),
        raised: tint(wash, if dark { 0.10 } else { 0.55 }),
        hover: tint(ink, if dark { 0.08 } else { 0.05 }),
        selected: Rgba {
            a: if dark { 0.30 } else { 0.16 },
            ..base.accent
        },
        border: tint(ink, if dark { 0.12 } else { 0.08 }),
        ..base
    }
}

pub fn mono() -> &'static str {
    if cfg!(target_os = "macos") {
        "Menlo"
    } else {
        "DejaVu Sans Mono"
    }
}

pub fn ui_font() -> &'static str {
    if cfg!(target_os = "macos") {
        ".SystemUIFont"
    } else {
        "Inter"
    }
}
