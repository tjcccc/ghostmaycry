//! Colors and small styling helpers. One dark palette, kept deliberately
//! plain: this is a tool, not a game.

use gpui::{Hsla, Rgba, rgb};

pub fn bg() -> Rgba {
    rgb(0x16181d)
}
pub fn panel() -> Rgba {
    rgb(0x1d2027)
}
pub fn raised() -> Rgba {
    rgb(0x262a33)
}
pub fn hover() -> Rgba {
    rgb(0x2e3340)
}
pub fn selected() -> Rgba {
    rgb(0x2f3b52)
}
pub fn border() -> Rgba {
    rgb(0x30343e)
}
pub fn text() -> Rgba {
    rgb(0xe3e6ec)
}
pub fn muted() -> Rgba {
    rgb(0x8d94a3)
}
pub fn accent() -> Rgba {
    rgb(0x7aa2f7)
}
pub fn good() -> Rgba {
    rgb(0x8bd49c)
}
pub fn warn() -> Rgba {
    rgb(0xe8b45c)
}
pub fn bad() -> Rgba {
    rgb(0xf07178)
}

pub fn mono() -> &'static str {
    if cfg!(target_os = "macos") {
        "Menlo"
    } else {
        "DejaVu Sans Mono"
    }
}

pub fn transparent() -> Hsla {
    gpui::transparent_black()
}
