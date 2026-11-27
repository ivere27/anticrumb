//! Terminal palettes; descriptions and upstream references are in docs/themes.md.
use crossterm::style::Color;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyntaxPalette {
    pub keyword: Color,
    pub string: Color,
    pub comment: Color,
    pub ty: Color,
    pub function: Color,
    pub constant: Color,
    pub operator: Color,
    pub property: Color,
    pub macro_call: Color,
    pub diff_added_bg: Color,
    pub diff_removed_bg: Color,
    pub match_fg: Color,
    pub match_bg: Color,
}

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
    pub syntax: SyntaxPalette,
}
const fn rgb(hex: u32) -> Color {
    Color::Rgb {
        r: (hex >> 16) as u8,
        g: (hex >> 8) as u8,
        b: hex as u8,
    }
}
#[allow(clippy::too_many_arguments)]
const fn syntax_palette(
    keyword: u32,
    string: u32,
    comment: u32,
    ty: u32,
    function: u32,
    constant: u32,
    operator: u32,
    property: u32,
    macro_call: u32,
    diff_added_bg: u32,
    diff_removed_bg: u32,
    match_fg: u32,
    match_bg: u32,
) -> SyntaxPalette {
    SyntaxPalette {
        keyword: rgb(keyword),
        string: rgb(string),
        comment: rgb(comment),
        ty: rgb(ty),
        function: rgb(function),
        constant: rgb(constant),
        operator: rgb(operator),
        property: rgb(property),
        macro_call: rgb(macro_call),
        diff_added_bg: rgb(diff_added_bg),
        diff_removed_bg: rgb(diff_removed_bg),
        match_fg: rgb(match_fg),
        match_bg: rgb(match_bg),
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
    syntax: SyntaxPalette,
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
        syntax,
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
        syntax: syntax_palette(
            0xc084fc, 0x86efac, 0x6b7280, 0xfbbf24, 0x60a5fa, 0xfb923c, 0x94a3b8, 0x67e8f9,
            0xf472b6, 0x132a1e, 0x32181d, 0x0b0f17, 0xfacc15,
        ),
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
        syntax: syntax_palette(
            0xc792ea, 0x9ed66f, 0x7f91a6, 0xf4c46e, 0x62bfff, 0xf79d65, 0x89ddff, 0x5edbc4,
            0xf58c9b, 0x142b23, 0x331b23, 0x102332, 0xf4c46e,
        ),
    },
    palette(
        "monokai",
        "Monokai",
        0x272822,
        0xf8f8f2,
        0xa6a28c,
        0x49483e,
        0x66d9ef,
        0xfd971f,
        0xe6db74,
        0xa6e22e,
        0xf92672,
        syntax_palette(
            0xf92672, 0xe6db74, 0x75715e, 0x66d9ef, 0xa6e22e, 0xae81ff, 0xf92672, 0xfd971f,
            0x66d9ef, 0x24331c, 0x3d1f26, 0x272822, 0xe6db74,
        ),
    ),
    palette(
        "nord",
        "Nord",
        0x2e3440,
        0xeceff4,
        0xd8dee9,
        0x4c566a,
        0x88c0d0,
        0x81a1c1,
        0x88c0d0,
        0xa3be8c,
        0xbf616a,
        syntax_palette(
            0x81a1c1, 0xa3be8c, 0x616e88, 0x8fbcbb, 0x88c0d0, 0xb48ead, 0x81a1c1, 0xd8dee9,
            0xd08770, 0x27382e, 0x3e2930, 0x2e3440, 0xebcb8b,
        ),
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
        syntax_palette(
            0x8839ef, 0x40a02b, 0x8c8fa1, 0xdf8e1d, 0x1e66f5, 0xfe640b, 0x04a5e5, 0x179299,
            0xd20f39, 0xd8eedb, 0xf5d8de, 0x4c4f69, 0xf6d58b,
        ),
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
