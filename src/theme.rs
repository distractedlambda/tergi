//! Colors. Everything is 24-bit, since ghostty and most modern terminals
//! support it.

use ratatui::style::Color;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeName {
    #[default]
    Dark,
    Light,
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub bg: Color,
    pub panel: Color,
    pub bar: Color,
    pub fg: Color,
    pub dim: Color,
    pub faint: Color,
    pub border: Color,
    pub accent: Color,
    pub sel: Color,
    pub sel_unfocused: Color,
    pub hover: Color,
    pub add_bg: Color,
    pub add_emph: Color,
    pub add_fg: Color,
    pub del_bg: Color,
    pub del_emph: Color,
    pub del_fg: Color,
    pub hunk_bg: Color,
    pub file_bg: Color,
    pub line_sel: Color,
    pub button: Color,
    pub button_hover: Color,
    pub warn: Color,
    pub error: Color,
    pub ok: Color,
    pub badge_head: Color,
    pub badge_local: Color,
    pub badge_remote: Color,
    pub badge_tag: Color,
    pub badge_fg: Color,
    pub lanes: [Color; 8],
    pub dark: bool,
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

impl Theme {
    pub fn new(name: ThemeName) -> Self {
        match name {
            ThemeName::Dark => Self::dark(),
            ThemeName::Light => Self::light(),
        }
    }

    pub fn dark() -> Self {
        Theme {
            bg: rgb(0x22252b),
            panel: rgb(0x1c1f24),
            bar: rgb(0x2a2e35),
            fg: rgb(0xd5d8de),
            dim: rgb(0x8b919c),
            faint: rgb(0x5a606b),
            border: rgb(0x383d46),
            accent: rgb(0x5fa8e8),
            sel: rgb(0x2f4566),
            sel_unfocused: rgb(0x343a45),
            hover: rgb(0x2b3038),
            add_bg: rgb(0x1f3326),
            add_emph: rgb(0x2f5a3a),
            add_fg: rgb(0x8fd19e),
            del_bg: rgb(0x3a2226),
            del_emph: rgb(0x6e3038),
            del_fg: rgb(0xf09090),
            hunk_bg: rgb(0x262b33),
            file_bg: rgb(0x2c313a),
            line_sel: rgb(0x3a5078),
            button: rgb(0x363c47),
            button_hover: rgb(0x4a5568),
            warn: rgb(0xe5c07b),
            error: rgb(0xe06c75),
            ok: rgb(0x98c379),
            badge_head: rgb(0x3f7fd0),
            badge_local: rgb(0x3b6e4a),
            badge_remote: rgb(0x5b4a80),
            badge_tag: rgb(0x80652a),
            badge_fg: rgb(0xf0f2f5),
            lanes: [
                rgb(0x5fa8e8),
                rgb(0xe5a15f),
                rgb(0x98c379),
                rgb(0xc678dd),
                rgb(0xe06c75),
                rgb(0x56b6c2),
                rgb(0xd1c45f),
                rgb(0xa0a8b8),
            ],
            dark: true,
        }
    }

    pub fn light() -> Self {
        Theme {
            bg: rgb(0xfafafa),
            panel: rgb(0xf0f1f3),
            bar: rgb(0xe4e6ea),
            fg: rgb(0x2b2f36),
            dim: rgb(0x6a717d),
            faint: rgb(0xa0a6b0),
            border: rgb(0xd0d4da),
            accent: rgb(0x2a6fc0),
            sel: rgb(0xc9dcf5),
            sel_unfocused: rgb(0xdde2ea),
            hover: rgb(0xe8ebef),
            add_bg: rgb(0xe3f5e6),
            add_emph: rgb(0xb5e3bd),
            add_fg: rgb(0x2a7a3b),
            del_bg: rgb(0xfbe6e8),
            del_emph: rgb(0xf2b8be),
            del_fg: rgb(0xb3343f),
            hunk_bg: rgb(0xeceff3),
            file_bg: rgb(0xe2e6ec),
            line_sel: rgb(0xb4cdf0),
            button: rgb(0xdde1e7),
            button_hover: rgb(0xc5ccd6),
            warn: rgb(0x9a6a00),
            error: rgb(0xc0303c),
            ok: rgb(0x3a7d2a),
            badge_head: rgb(0x2a6fc0),
            badge_local: rgb(0x2f7a45),
            badge_remote: rgb(0x6a4fa0),
            badge_tag: rgb(0x9a7420),
            badge_fg: rgb(0xffffff),
            lanes: [
                rgb(0x2a6fc0),
                rgb(0xc06a1a),
                rgb(0x3a8a2a),
                rgb(0x9a40b8),
                rgb(0xc0303c),
                rgb(0x1a8a96),
                rgb(0x8a7a10),
                rgb(0x6a717d),
            ],
            dark: false,
        }
    }

    pub fn lane(&self, i: u8) -> Color {
        self.lanes[usize::from(i) % self.lanes.len()]
    }
}
