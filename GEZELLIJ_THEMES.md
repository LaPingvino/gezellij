# Gezellig Themes

Two warm, low-blue-light themes for Gezellij, built for the kind of session that
stays open all day: `gezellig-dark` (candlelight on dark wood) and
`gezellig-light` (warm paper and ink). *Gezellig* is the Dutch word for the
cosy, unhurried, company-of-friends feeling — the themes aim at that, not at
neon.

Part of [GEZELLIJ_PLAN.md](GEZELLIJ_PLAN.md) Phase 5: *Custom "Gezellig" themes
(warm, cozy color palettes for long terminal sessions)*.

## Selecting a theme

The themes ship inside the binary. `zellij-utils/src/consts.rs` embeds the whole
theme directory with `include_dir!`:

```rust
pub static ZELLIJ_DEFAULT_THEMES: Dir = include_dir!("$CARGO_MANIFEST_DIR/assets/themes");
```

and `setup.rs::get_default_themes()` walks every file in it, so **any new
`.kdl` file dropped into `zellij-utils/assets/themes/` is picked up
automatically at the next build** — no registration list to edit.

Pick one in `config.kdl`:

```kdl
theme "gezellig-dark"
```

Or let Gezellij follow the palette the host terminal reports over CSI 2031 /
DSR 997. Both must be set — if either is missing, the static `theme` stays
authoritative (see the doc comments on `theme_dark` / `theme_light` in
`zellij-utils/src/input/options.rs`):

```kdl
theme_dark "gezellig-dark"
theme_light "gezellig-light"
```

Or from the command line, without touching the config:

```console
$ zellij options --theme gezellig-dark
```

To iterate on a copy without rebuilding, point `theme_dir` at a directory
holding the `.kdl` files:

```kdl
theme_dir "/home/you/.config/zellij/themes"
theme "gezellig-dark"
```

## Palette rationale

* **Low blue light.** Every hue sits on the warm half of the wheel. There is no
  pure blue anywhere in either palette; the coolest accent is a desaturated
  pine-teal that reads as "green with a memory of blue". Blue light is the part
  of the spectrum that keeps you alert at 23:00, and a multiplexer is exactly
  the surface you stare at for eight hours.
* **Candlelight, not amber filter.** The dark background is a deep warm charcoal
  with a brown bias (`#241f1b`) rather than a neutral grey or a tinted black.
  Foreground text is unbleached cream (`#ede0cf`) instead of white, which takes
  the glare off without losing contrast.
* **Accents from one family.** Terracotta, moss, pine and dusty plum, with
  honey-amber reserved for "this is the thing you selected" — selected ribbons,
  selected frames, table titles. They are related enough to feel like one room
  and distinct enough to tell apart at a glance.
* **Gentle selection.** Selection is a warmer, slightly lighter shade of the
  background (`#3a322b` on dark, `#e6d9c3` on light), not an inverted block.
  Selection should feel like the light moving, not like a flashbulb.
* **Contrast that still works.** Warm and low-contrast are not the same thing.
  Body text runs 11-12.5:1, every accent on the background clears 4.5:1, and
  frames clear 3:1 (see the ratios below).

The light variant is the same room by daylight: cream paper (`#f6eee0`), dark
warm ink (`#3b3129`), and the same accent family darkened so it holds up on a
bright ground.

## Colour tables

### gezellig-dark

| Role | Hex | RGB | Used for |
|---|---|---|---|
| Background | `#241f1b` | 36 31 27 | text/table/list/frame background |
| Selection | `#3a322b` | 58 50 43 | `text_selected`, `list_selected`, `table_cell_selected` |
| Foreground | `#ede0cf` | 237 224 207 | body text, `player_10` |
| Muted tan | `#c6b49c` | 198 180 156 | `ribbon_unselected` background |
| Honey amber | `#e8b455` | 232 180 85 | `ribbon_selected` bg, `table_title`, `frame_selected` |
| Terracotta | `#d2793f` | 210 121 63 | `emphasis_0`, `frame_highlight` |
| Pine | `#7a9e91` | 122 158 145 | `emphasis_1` |
| Moss | `#9aa95c` | 154 169 92 | `emphasis_2`, `exit_code_success` |
| Dusty plum | `#be96b4` | 190 150 180 | `emphasis_3` |
| Clay rose | `#d66f60` | 214 111 96 | `exit_code_error` |
| Wood grain | `#806f5e` | 128 111 94 | `frame_unselected` |
| Warm honey | `#e0c482` | 224 196 130 | multiplayer accent |

### gezellig-light

| Role | Hex | RGB | Used for |
|---|---|---|---|
| Background | `#f6eee0` | 246 238 224 | text/table/list/frame background |
| Selection | `#e6d9c3` | 230 217 195 | `text_selected`, `list_selected`, `table_cell_selected` |
| Foreground | `#3b3129` | 59 49 41 | body text, `ribbon_selected` base, `player_10` |
| Deep bark | `#4a3e34` | 74 62 52 | `ribbon_unselected` background |
| Amber | `#e0b454` | 224 180 84 | `ribbon_selected` background |
| Ochre | `#8a5c10` | 138 92 16 | `table_title`, `frame_selected` |
| Terracotta | `#a64f20` | 166 79 32 | `emphasis_0`, `frame_highlight` |
| Pine | `#2f7062` | 47 112 98 | `emphasis_1` |
| Moss | `#607028` | 96 112 40 | `emphasis_2`, `exit_code_success` |
| Plum | `#805276` | 128 82 118 | `emphasis_3` |
| Brick | `#a63e30` | 166 62 48 | `exit_code_error` |
| Weathered wood | `#928068` | 146 128 104 | `frame_unselected` |
| Honey | `#b07d1a` | 176 125 26 | multiplayer accent |

## Contrast ratios (WCAG 2.x, base against its painted background)

Where a block sets `background 0` (frames, titles, exit codes — the background
is not painted there) the ratio is measured against the theme background.

| Block | dark | light |
|---|---|---|
| `text_unselected` | 12.56 | 11.00 |
| `text_unselected` emphasis 0/1/2/3 | 5.09 / 5.54 / 6.38 / 6.38 | 4.85 / 5.04 / 4.74 / 5.38 |
| `text_selected`, `list_selected`, `table_cell_selected` | 9.67 | 9.10 |
| `ribbon_selected` | 8.62 | 6.53 |
| `ribbon_unselected` | 8.09 | 8.98 |
| `table_title`, `frame_selected` | 8.62 | 5.04 |
| `frame_highlight` | 5.09 | 4.85 |
| `frame_unselected` | 3.38 | 3.31 |
| `exit_code_success` | 6.38 | 4.74 |
| `exit_code_error` | 4.90 | 5.44 |

Every text role clears AA (4.5:1); frame lines clear the 3:1 non-text
threshold. Only `base`-against-its-painted-background pairs were measured (plus
the `text_unselected` emphases); ribbon emphasis colours sit on the ribbon
background and were not measured, as in the upstream themes.

## Format notes

Both files use the newer per-component style format that the parser in
`zellij-utils/src/kdl/mod.rs` (`Themes::from_kdl` /
`Themes::style_declaration_from_node`) accepts, with every supported block
present — including the optional `frame_unselected`, which most shipped themes
omit:

`text_unselected`, `text_selected`, `ribbon_selected`, `ribbon_unselected`,
`table_title`, `table_cell_selected`, `table_cell_unselected`, `list_selected`,
`list_unselected`, `frame_unselected`, `frame_selected`, `frame_highlight`,
`exit_code_success`, `exit_code_error`, `multiplayer_user_colors`.

Each style block carries `base`, `background` and `emphasis_0`-`emphasis_3`.
Colours are written as RGB triples (`232 180 85`), matching every other shipped
theme; the parser also accepts `"#rrggbb"`, `"#rgb"` and a single 0-255 ANSI
index, and `0` is used for "not painted" the way the existing themes do.
Unlike the other themes, all ten `multiplayer_user_colors` slots are filled
with a visible colour rather than left at `0` (ANSI black).
