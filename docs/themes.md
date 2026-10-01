# Themes

A theme changes how the editor and the Hub look: colours, type, corners, and
pictures or moving shaders behind any panel you like. A theme is one file you
can share, and both programs read the same one.

Pick one in the editor under **Edit ⏵ Preferences**, or in the Hub under
**Settings**. Choosing in either changes the other within a second.

## The built-in themes

| Theme | What it is for |
|---|---|
| Floptle Dark | The default. Near-black, hairlines, one teal accent. The same look as fopull.com. |
| Floptle Light | The same, for bright rooms. |
| Midnight, Slate | Two softer darks: deep navy, and grey-blue. |
| Carbon (OLED) | True black, for OLED screens. |
| High Contrast, High Contrast Light | Maximum legibility: bold edges, larger text, Atkinson Hyperlegible, no animation. |
| Galaxy | Panels over a slowly turning spiral nebula. |
| Aurora | Northern lights behind deep teal glass. |
| Retrowave | A neon sunset over an endless grid, square corners. |
| Terminal | Green phosphor, everything in mono, scanlines. |
| Paper | Warm cream and ink: a light theme with less glare. |

## Effects and what they cost

Moving themes are cheap on purpose. Each shader a theme uses is drawn once per
update into a texture at half the window's resolution, at most 30 times a
second, and only where a visible panel shows it. Every panel then shows its
part of that texture like any other image. On a desktop GPU a built-in
backdrop takes between 0.06 and 0.2 ms each time it is drawn.

Under **Effects** in the theme settings:

- **Moving**: images, and shaders animate.
- **Still**: images, and each shader drawn once and held. No cost per frame.
- **Off**: colours only. The theme's colours, type and shape still apply.

**Backdrop resolution** (25 to 100%) and **Backdrop rate** (15, 30 or 60 per
second) trade sharpness and smoothness for GPU time. In the editor, **Hold
backdrops still while the game plays** is on by default, so a theme never
competes with your game.

## Making your own

Choose a theme and press **Customize…**. The theme editor changes the window
as you go: every colour, the three typefaces and text size, corner radius,
line width and spacing, and a backdrop and picture for each region. **Save
theme** puts it in your themes folder and chooses it.

You can also write one by hand. **Open themes folder** shows where themes
live (`~/.config/floptle/themes` on Linux, `%APPDATA%\floptle\themes` on
Windows, `~/Library/Application Support/floptle/themes` on macOS). A theme is
a folder holding a `theme.ron`:

```ron
(
    format: 1,
    id: "sunset",
    name: "Sunset",
    author: "You",
    extends: "floptle-dark",
    colors: (
        accent: "#ff8a3d",
        on_accent: "#1f0d00",
        ground: "#1a1012",
    ),
    shape: (radius: 10),
)
```

Save the file while the theme is chosen and the window redraws in it. If the
file has a mistake, the last version that loaded stays on screen and the
settings say which line is wrong.

Every key is optional. Anything you leave out comes from the theme you
`extends` (one of the built-ins; Floptle Dark if you name none). So a theme can
be three lines that change the accent, or a file that sets everything.

### Colours

A colour is `"#rrggbb"`, `"#rrggbbaa"`, `"rgba(r, g, b, a)"`, or another token
of the same theme: `"$accent"`, or `"$accent/45%"` for it at 45% opacity.

| Token | Used for |
|---|---|
| `ground` | the window behind everything |
| `surface` | a panel, a card, a window |
| `surface_2` | a header row, a chip, a resting button |
| `well` | text fields and code, a step below the ground |
| `hairline`, `hairline_quiet` | the 1px lines between surfaces, and inside them |
| `text`, `dim`, `faint` | words, secondary text, labels and fine print |
| `accent` | the one primary action, things switched on, focus |
| `accent_hi` | links, hover on the accent |
| `accent_wash`, `accent_edge` | a selected row's fill, and the edge of something hovered |
| `on_accent` | text on a filled accent button |
| `backdrop` | what sits under a translucent panel |
| `selection` | selected text |

If you set `accent`, you must set `on_accent` too, so a filled button is always
readable. The three status colours (good, warn, bad) are not part of a theme:
they mean the same thing everywhere. A theme whose text is hard to read on its
ground (under 4.5:1) still loads, with a warning.

`code` sets the script editor's colours: `background`, `gutter`, `keyword`,
`api`, `string`, `number`, `comment`, `text`, `current_line`.

### Type and shape

```ron
fonts: (
    ui: "IBM Plex Sans",           // or "fonts/MyFace.ttf" inside the theme
    mono: "IBM Plex Mono",
    display: "Bricolage Grotesque", // titles
    ui_weight: 400,
    display_weight: 640,
    size: 13,
),
shape: (radius: 6, radius_small: 4, widget_radius: 4, stroke: 1, density: 1.0,
        shadow: (blur: 16, offset: (0, 6), color: "#00000066")),
motion: (animation_ms: 120),
```

Built-in faces: `IBM Plex Sans`, `IBM Plex Mono`, `Bricolage Grotesque`,
`Atkinson Hyperlegible`, `Ubuntu`, `Hack`. A `.ttf` or `.otf` inside the theme
works too. The editor's icons always draw, whatever face you choose.

### Backdrops for each region

`surfaces` gives any region its own layers, drawn bottom to top, then the
region's `fill` over them. Over layers, the fill is a veil: give it some
transparency or nothing shows through.

```ron
surfaces: {
    "ground": (layers: [Shader(shader: "builtin:galaxy")]),
    "tab.inspector": (fill: "#0a0a1480", layers: [
        Shader(shader: "builtin:galaxy"),
        Image(path: "images/hero.png", fit: Contain, align: BottomRight, opacity: 0.4),
    ]),
    "menu_bar": (layers: [Gradient(stops: [(0.0, "#ff8a3d40"), (1.0, "#00000000")], angle: 0)]),
},
```

| Region | Is |
|---|---|
| `ground` | the window behind everything |
| `panel` | every docked panel's body (takes `ground`'s layers if it has none) |
| `tab.<name>` | one editor panel: `tab.inspector`, `tab.hierarchy`, `tab.assets`, `tab.console`, `tab.scripting`, `tab.packages`… (takes `panel`'s) |
| `menu_bar`, `tab_bar` | the editor's menu bar, and the strips of tabs |
| `code_editor` | the script editor's text area |
| `hub.header`, `hub.content` | the Hub's top bar and main area |

A region with no `layers` uses its parent's. `layers: []` means none here.

The layers:

- `Shader(shader, opacity, speed, scale, colors, params, image, blend)`: a
  moving picture. `builtin:galaxy`, `builtin:aurora`, `builtin:grid`,
  `builtin:scanlines`, `builtin:drift`, `builtin:waves`, `builtin:starfield`,
  or a `.wgsl` file in the theme (below).
- `Image(path, fit, align, scale, offset, opacity, tint, blend, space, pixelated)`:
  `fit` is `Cover`, `Contain`, `Stretch`, `Tile` or `Natural`; `align` is
  `Center`, `TopLeft`, `BottomRight` and so on. `space: Window` lays the
  picture out across the whole window, so neighbouring panels show one
  picture between them.
- `Gradient(stops, angle, radial, opacity, blend, space)`
- `Solid(color)`

`blend: Add` adds light instead of covering: good for glows and sparkle.

### Writing a shader

A theme's `.wgsl` file defines one function:

```wgsl
fn backdrop(uv: vec2<f32>, px: vec2<f32>) -> vec4<f32> {
    // uv: 0 to 1 across the window. px: the same, in points.
    let wave = 0.5 + 0.5 * sin(uv.x * 8.0 + bd.time);
    return vec4<f32>(mix(bd.color2.rgb, bd.color0.rgb, wave * 0.3), 1.0);
}
```

It returns a colour with straight alpha. It can read `bd.time`, `bd.window`,
`bd.resolution`, `bd.scale`, `bd.color0` to `bd.color3` (the layer's
`colors`), `bd.params0` and `bd.params1` (its `params`), and `bd.pointer`;
sample `bd_image` with `bd_sampler`; and call `bd_hash`, `bd_noise`, `bd_fbm`
and `bd_rot`. The picture spans the window, so every panel showing it shows
one continuous scene. A shader that does not compile is reported with the
compiler's message, and its panels show their colour instead.

## Sharing

**Export…** saves the chosen theme, with its pictures, shaders and fonts, as a
`.floptletheme` file. To add one, drop it on the editor or Hub window, or use
**Import…** in the editor. A theme added again with the same id replaces the
old one.

A theme can also ship in a package: put it at `themes/<id>/theme.ron` (or
`themes/<name>.floptletheme`) at the package root. Once the package is
installed in a project, its themes appear in the picker. Choosing one copies it
into your themes folder, so it stays yours in every project and in the Hub.
The catalogue lists such packages under **themes**.

Only share pictures and fonts you have the right to share.
