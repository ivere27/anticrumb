//! Terminal palettes; descriptions and upstream references are in docs/themes.md.
use crossterm::style::Color;
#[derive(Clone, Copy)]
pub struct Theme {
    pub id: &'static str,
    pub name: &'static str,
    pub background: Color,
    pub text: Color,
    pub muted: Color,
    pub subtle: Color,
    pub accent: Color,
    pub heading: Color,
    pub code: Color,
    pub highlight: Color,
    pub danger: Color,
    pub selected_text: Color,
    pub selected_background: Color,
}
const fn rgb(hex: u32) -> Color {
    Color::Rgb {
        r: (hex >> 16) as u8,
        g: (hex >> 8) as u8,
        b: hex as u8,
    }
}
#[allow(clippy::too_many_arguments)]
const fn palette(
    id: &'static str,
    name: &'static str,
    bg: u32,
    fg: u32,
    muted: u32,
    subtle: u32,
    accent: u32,
    heading: u32,
    code: u32,
    highlight: u32,
    danger: u32,
) -> Theme {
    Theme {
        id,
        name,
        background: rgb(bg),
        text: rgb(fg),
        muted: rgb(muted),
        subtle: rgb(subtle),
        accent: rgb(accent),
        heading: rgb(heading),
        code: rgb(code),
        highlight: rgb(highlight),
        danger: rgb(danger),
        selected_text: rgb(bg),
        selected_background: rgb(accent),
    }
}
pub const THEMES: &[Theme] = &[
    Theme {
        id: "terminal",
        name: "Terminal default",
        background: Color::Reset,
        text: Color::Reset,
        muted: rgb(0x9ca3af),
        subtle: rgb(0x52525b),
        accent: rgb(0x60a5fa),
        heading: rgb(0x60a5fa),
        code: rgb(0x67e8f9),
        highlight: rgb(0x4ade80),
        danger: rgb(0xf87171),
        selected_text: Color::Black,
        selected_background: rgb(0x60a5fa),
    },
    Theme {
        // Distinct sea blues and harbor lights on the terminal's own background.
        id: "busan-night",
        name: "Busan Night",
        background: Color::Reset,
        text: rgb(0xd8dee9),
        muted: rgb(0xaab8ca),
        subtle: rgb(0x7f91a6),
        accent: rgb(0x62bfff),
        heading: rgb(0xf4c46e),
        code: rgb(0x5edbc4),
        highlight: rgb(0x9ed66f),
        danger: rgb(0xf58c9b),
        selected_text: rgb(0x102332),
        selected_background: rgb(0x62bfff),
    },
    palette(
        "monokai", "Monokai", 0x272822, 0xf8f8f2, 0xa6a28c, 0x49483e, 0x66d9ef, 0xfd971f, 0xe6db74,
        0xa6e22e, 0xf92672,
    ),
    palette(
        "nord", "Nord", 0x2e3440, 0xeceff4, 0xd8dee9, 0x4c566a, 0x88c0d0, 0x81a1c1, 0x88c0d0,
        0xa3be8c, 0xbf616a,
    ),
    palette(
        "catppuccin-latte",
        "Catppuccin Latte",
        0xeff1f5,
        0x4c4f69,
        0x6c6f85,
        0xbcc0cc,
        0x1e66f5,
        0x7287fd,
        0x179299,
        0x40a02b,
        0xd20f39,
    ),
];
pub fn find(id: &str) -> Option<Theme> {
    THEMES.iter().find(|t| t.id == id).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curated_themes_have_unique_ids_and_busan_monokai_palettes() {
        let ids: Vec<&str> = THEMES.iter().map(|t| t.id).collect();
        assert_eq!(
            ids,
            [
                "terminal",
                "busan-night",
                "monokai",
                "nord",
                "catppuccin-latte"
            ]
        );
        let busan = find("busan-night").unwrap();
        assert_eq!(busan.background, Color::Reset);
        assert_eq!(busan.accent, rgb(0x62bfff));
        assert_eq!(busan.code, rgb(0x5edbc4));
        let monokai = find("monokai").unwrap();
        assert_eq!(monokai.background, rgb(0x272822));
        assert_eq!(monokai.highlight, rgb(0xa6e22e));
    }
}
