# Terminal Themes

Use `/theme` to pick a palette: Up/Down selects, Enter saves, Esc cancels. `/theme ID` selects directly. The choice persists across launches; RGB-capable terminals give the best results.

| ID | Palette |
|---|---|
| `terminal` | Terminal defaults |
| `busan-night` | Soft gray text, distinct sea blues, and warm harbor lights on the terminal default background |
| `monokai` | Dark charcoal with bright accents |
| `nord` | [Nord colors](https://www.nordtheme.com/docs/colors-and-palettes/) |
| `catppuccin-latte` | [Catppuccin Latte](https://github.com/catppuccin/palette/blob/main/palette.json), light |

Busan Night is designed for dark terminal backgrounds. It inherits the terminal background, including any configured transparency. Blue accents, teal code, amber headings, green additions, and coral errors distinguish text roles. Selected items use dark text on a bright blue background.

Palette definitions: [theme.rs](../src/tui/theme.rs).
