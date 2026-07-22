//! Color themes, selected by `theme` in config.

use ratatui::style::Color;

/// The palette used to render the UI.
#[derive(Clone, Copy)]
pub struct Theme {
    pub user: Color,
    pub assistant: Color,
    pub tool: Color,
    pub system: Color,
    pub dim: Color,
    pub accent: Color,
    /// Subtle border/rule color.
    pub border: Color,
    /// Subtle raised surface (user message + input backgrounds).
    pub surface: Color,
    /// The app background, filled behind everything.
    pub base: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Theme::dark()
    }
}

impl Theme {
    pub fn dark() -> Self {
        Theme {
            user: Color::Rgb(225, 230, 240),
            assistant: Color::Rgb(205, 212, 224),
            tool: Color::Rgb(180, 190, 205),
            system: Color::Rgb(120, 130, 145),
            dim: Color::Rgb(105, 114, 130),
            accent: Color::Rgb(110, 155, 245),
            border: Color::Rgb(58, 64, 78),
            surface: Color::Rgb(32, 35, 43),
            base: Color::Rgb(20, 22, 28),
        }
    }

    pub fn light() -> Self {
        Theme {
            user: Color::Rgb(30, 34, 42),
            assistant: Color::Rgb(45, 50, 60),
            tool: Color::Rgb(90, 96, 108),
            system: Color::Rgb(120, 128, 140),
            dim: Color::Rgb(130, 138, 150),
            accent: Color::Rgb(30, 100, 210),
            border: Color::Rgb(210, 214, 222),
            surface: Color::Rgb(238, 240, 244),
            base: Color::Rgb(250, 250, 252),
        }
    }

    pub fn mono() -> Self {
        Theme {
            user: Color::White,
            assistant: Color::Gray,
            tool: Color::Gray,
            system: Color::DarkGray,
            dim: Color::DarkGray,
            accent: Color::White,
            border: Color::DarkGray,
            surface: Color::Rgb(28, 28, 28),
            base: Color::Rgb(12, 12, 12),
        }
    }

    pub fn tokyonight() -> Self {
        Theme {
            user: Color::Rgb(192, 202, 245),
            assistant: Color::Rgb(169, 177, 214),
            tool: Color::Rgb(224, 175, 104),
            system: Color::Rgb(86, 95, 137),
            dim: Color::Rgb(86, 95, 137),
            accent: Color::Rgb(122, 162, 247),
            border: Color::Rgb(47, 51, 77),
            surface: Color::Rgb(36, 40, 59),
            base: Color::Rgb(26, 27, 38),
        }
    }

    pub fn catppuccin() -> Self {
        Theme {
            user: Color::Rgb(205, 214, 244),
            assistant: Color::Rgb(186, 194, 222),
            tool: Color::Rgb(250, 179, 135),
            system: Color::Rgb(108, 112, 134),
            dim: Color::Rgb(108, 112, 134),
            accent: Color::Rgb(137, 180, 250),
            border: Color::Rgb(49, 50, 68),
            surface: Color::Rgb(49, 50, 68),
            base: Color::Rgb(30, 30, 46),
        }
    }

    pub fn gruvbox() -> Self {
        Theme {
            user: Color::Rgb(235, 219, 178),
            assistant: Color::Rgb(213, 196, 161),
            tool: Color::Rgb(250, 189, 47),
            system: Color::Rgb(146, 131, 116),
            dim: Color::Rgb(146, 131, 116),
            accent: Color::Rgb(131, 165, 152),
            border: Color::Rgb(60, 56, 54),
            surface: Color::Rgb(50, 48, 47),
            base: Color::Rgb(40, 40, 40),
        }
    }

    pub fn nord() -> Self {
        Theme {
            user: Color::Rgb(236, 239, 244),
            assistant: Color::Rgb(216, 222, 233),
            tool: Color::Rgb(235, 203, 139),
            system: Color::Rgb(97, 110, 136),
            dim: Color::Rgb(97, 110, 136),
            accent: Color::Rgb(136, 192, 208),
            border: Color::Rgb(59, 66, 82),
            surface: Color::Rgb(59, 66, 82),
            base: Color::Rgb(46, 52, 64),
        }
    }

    pub fn rosepine() -> Self {
        Theme {
            user: Color::Rgb(224, 222, 244),
            assistant: Color::Rgb(206, 202, 221),
            tool: Color::Rgb(246, 193, 119),
            system: Color::Rgb(110, 106, 134),
            dim: Color::Rgb(110, 106, 134),
            accent: Color::Rgb(156, 207, 216),
            border: Color::Rgb(38, 35, 58),
            surface: Color::Rgb(31, 29, 46),
            base: Color::Rgb(25, 23, 36),
        }
    }

    /// Override individual role colors from `#rrggbb` strings (used for `[colors]` in config).
    pub fn with_overrides(mut self, c: &ColorOverrides) -> Self {
        let set = |slot: &mut Color, hex: &Option<String>| {
            if let Some(h) = hex
                && let Some(col) = parse_hex(h)
            {
                *slot = col;
            }
        };
        set(&mut self.user, &c.user);
        set(&mut self.assistant, &c.assistant);
        set(&mut self.tool, &c.tool);
        set(&mut self.system, &c.system);
        set(&mut self.dim, &c.dim);
        set(&mut self.accent, &c.accent);
        set(&mut self.border, &c.border);
        set(&mut self.surface, &c.surface);
        set(&mut self.base, &c.base);
        self
    }
}

/// Per-role hex color overrides from `[colors]` in config; any unset field keeps the theme's.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ColorOverrides {
    pub user: Option<String>,
    pub assistant: Option<String>,
    pub tool: Option<String>,
    pub system: Option<String>,
    pub dim: Option<String>,
    pub accent: Option<String>,
    pub border: Option<String>,
    pub surface: Option<String>,
    pub base: Option<String>,
}

/// All built-in theme names (order matches the `<leader>t` picker).
pub const THEME_NAMES: [&str; 8] = [
    "dark",
    "tokyonight",
    "catppuccin",
    "gruvbox",
    "nord",
    "rosepine",
    "light",
    "mono",
];

/// Resolve a theme by name (defaults to dark for unknown names).
pub fn theme_by_name(name: &str) -> Theme {
    match name.to_ascii_lowercase().as_str() {
        "light" => Theme::light(),
        "mono" => Theme::mono(),
        "tokyonight" => Theme::tokyonight(),
        "catppuccin" => Theme::catppuccin(),
        "gruvbox" => Theme::gruvbox(),
        "nord" => Theme::nord(),
        "rosepine" => Theme::rosepine(),
        _ => Theme::dark(),
    }
}

/// Parse a `#rrggbb` (or `rrggbb`) hex string into a color.
pub fn parse_hex(s: &str) -> Option<Color> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&h[0..2], 16).ok()?;
    let g = u8::from_str_radix(&h[2..4], 16).ok()?;
    let b = u8::from_str_radix(&h[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_named_themes() {
        // mono is distinct; unknown falls back to dark.
        assert_eq!(theme_by_name("mono").user, Color::White);
        assert_eq!(theme_by_name("nope").user, Theme::dark().user);
        assert_eq!(theme_by_name("light").user, Theme::light().user);
        assert_ne!(theme_by_name("light").user, theme_by_name("mono").user);
        // presets resolve.
        assert_eq!(
            theme_by_name("tokyonight").accent,
            Color::Rgb(122, 162, 247)
        );
        assert_eq!(theme_by_name("nord").accent, Color::Rgb(136, 192, 208));
    }

    #[test]
    fn hex_parsing_and_overrides() {
        assert_eq!(parse_hex("#ff8800"), Some(Color::Rgb(255, 136, 0)));
        assert_eq!(parse_hex("00ff00"), Some(Color::Rgb(0, 255, 0)));
        assert_eq!(parse_hex("bad"), None);
        let over = ColorOverrides {
            accent: Some("#010203".into()),
            ..Default::default()
        };
        let t = Theme::dark().with_overrides(&over);
        assert_eq!(t.accent, Color::Rgb(1, 2, 3));
        assert_eq!(t.user, Theme::dark().user); // untouched
    }
}
