#!/usr/bin/env python3
"""Generate assets/scenes/ui_demo.ron.

Hand-writing scene RON is fine for five nodes and a mistake waiting to happen
for fifty, because `parent` is a POSITIONAL index into the node list — insert
one node near the top and every parent below it is silently wrong. So the
demo's tree is written as a tree here and the indices are computed.
"""
import io

nodes = []  # each: dict with name, parent (index or None), matter, ui, layer, scripts


def node(name, parent=None, matter="Empty", ui=None, layer=None, scripts=None):
    nodes.append(
        dict(name=name, parent=parent, matter=matter, ui=ui, layer=layer, scripts=scripts or [])
    )
    return len(nodes) - 1


def ron_num(v):
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, int):
        return str(v)
    return f"{v:.4g}" if v != int(v) else f"{float(v):.1f}"


# `ElementSpec` fields that are `Option<...>`. Scene RON is parsed WITHOUT
# implicit-some, so these have to be written `Some((...))` — and forgetting one
# is a parse error thirty lines from where you'd look.
OPTIONAL = {
    "stack", "shape", "text", "image", "slider", "part", "mask", "scroll",
    "scrollbar", "nav", "field", "repeater", "gradient", "shadow", "glow",
    "grain",
}


def ron(v, indent, key=None):
    pad = " " * indent
    if isinstance(v, dict):
        if not v:
            return "()"
        out = "(\n"
        for k, x in v.items():
            out += f"{pad}    {k}: {ron(x, indent + 4, k)},\n"
        body = out + pad + ")"
        return f"Some({body})" if key in OPTIONAL else body
    if isinstance(v, tuple):
        return "(" + ", ".join(ron(x, indent) for x in v) + ")"
    if isinstance(v, list):
        if not v:
            return "[]"
        out = "[\n"
        for x in v:
            out += f"{pad}    {ron(x, indent + 4)},\n"
        return out + pad + "]"
    if isinstance(v, Raw):
        return v.s
    if isinstance(v, str):
        return '"' + v.replace('"', '\\"') + '"'
    if isinstance(v, bool):
        return "true" if v else "false"
    return ron_num(v)


class Raw:
    def __init__(self, s):
        self.s = s

    def __repr__(self):
        return self.s


def free(x, y):
    return Raw(f"Free(pos: ({x:.1f}, {y:.1f}))")


def pin(anchor, x=0.0, y=0.0):
    return Raw(f"Pin(anchor: {anchor}, offset: ({x:.1f}, {y:.1f}))")


def stretch(minv, maxv, m):
    return Raw(
        f"Stretch(min: ({minv[0]:.2f}, {minv[1]:.2f}), max: ({maxv[0]:.2f}, {maxv[1]:.2f}), "
        f"margin: ({m[0]:.1f}, {m[1]:.1f}, {m[2]:.1f}, {m[3]:.1f}))"
    )


def size(w, h):
    return Raw(f"({w}, {h})")


FIX = lambda v: f"Fixed({v:.1f})"
PCT = lambda v: f"Pct({v:.2f})"
FIT = "Fit"
GROW = lambda v=1.0: f"Grow({v:.1f})"


def text(s, sz=18.0, color=(0.9, 0.92, 0.96, 1.0), align="Start", valign="Center", **kw):  # noqa
    d = dict(text=s, size=sz, color=Raw(f"({color[0]}, {color[1]}, {color[2]}, {color[3]})"),
             align=Raw(align), valign=Raw(valign), fit=False)
    d.update(kw)
    return d


def shape(fill=(0, 0, 0, 0), radius=None, border=None, border_color=(0, 0, 0, 0), **kw):
    # `fill` and `border_color` have no serde default, so both are always
    # written even when they are transparent.
    d = dict(fill=Raw(f"({fill[0]}, {fill[1]}, {fill[2]}, {fill[3]})"))
    if radius is not None:
        d["radius"] = radius
    if border is not None:
        d["border"] = border
    d["border_color"] = Raw(
        f"({border_color[0]}, {border_color[1]}, {border_color[2]}, {border_color[3]})"
    )
    d.update(kw)
    return d


def stack(dirn="Column", gap=8.0, pad=0.0, justify="Start", align="Start"):
    return dict(dir=Raw(dirn), gap=gap, pad=pad, justify=Raw(justify), align=Raw(align))


# ---------------------------------------------------------------------------
# The scene
# ---------------------------------------------------------------------------

node("Post Processing", matter=Raw(
    "PostProcess(enabled: true, bloom: true, bloom_threshold: 0.55, bloom_intensity: 0.30, "
    "vignette: true, vignette_strength: 0.45, vignette_radius: 0.55, ao: Off, ao_strength: 1.0, "
    "ao_radius: 0.7, posterize_bands: 0, posterize_dither: false)"))
node("Camera", matter=Raw("Camera(fov_y: 1.0, active: true)"))

LAYER = node(
    "Demo UI",
    layer=dict(design_height=720.0, reference_width=1280.0, scale_mode=Raw("Blend"),
               match_wh=0.5, z=10, enabled=True, space=Raw("Screen"), canvas_scale=0.01,
               nav_wrap=True, tooltip_delay=0.35),
    scripts=[dict(kind="ui_demo", enabled=True, params=[])],
)

# ---- backdrop -------------------------------------------------------------
node("Backdrop", LAYER,
     ui=dict(place=stretch((0, 0), (1, 1), (0, 0, 0, 0)), size=size(FIT, FIT),
             shape=shape((0.035, 0.040, 0.055, 1.0),
                         gradient=Raw("Some((kind: Radial, to: (0.010, 0.012, 0.020, 1.0), "
                                      "angle: 0.0, mid: 0.5))"))))

# ---- header ---------------------------------------------------------------
node("Title", LAYER,
     ui=dict(place=free(48, 40), size=size(FIX(600.0), FIX(48.0)),
             text=text("Floptle UI"), style="title"))
node("Subtitle", LAYER,
     ui=dict(place=free(50, 88), size=size(FIX(700.0), FIX(20.0)),
             text=text("styles · states · focus · fields · drag & drop · lists"),
             style="caption"))

# ---- left panel -----------------------------------------------------------
LEFT = node("Controls Panel", LAYER,
            ui=dict(place=free(48, 132), size=size(FIX(468.0), FIX(476.0)),
                    stack=stack("Column", 14.0, 20.0), style="panel"))

# tab strip
TABS = node("Tabs", LEFT,
            ui=dict(place=free(0, 0), size=size(PCT(1.0), FIX(40.0)),
                    stack=stack("Row", 4.0, 0.0)))
for i, label in enumerate(["Loadout", "Crew", "Log"]):
    node(f"Tab {label}", TABS,
         ui=dict(place=free(0, 0), size=size(GROW(1.0), FIX(40.0)),
                 text=text(label, align="Center"), button=True, focusable=True,
                 group="tabs", selected=(i == 0), style="tab",
                 tooltip=f"the {label.lower()} tab — a radio group, no script"))

node("Rule", LEFT,
     ui=dict(place=free(0, 0), size=size(PCT(1.0), FIX(1.0)),
             shape=shape((0.25, 0.28, 0.34, 1.0))))

# buttons
node("Play", LEFT,
     ui=dict(place=free(0, 0), size=size(PCT(1.0), FIX(58.0)),
             text=text("Launch", align="Center"), button=True, focusable=True,
             style="button/primary",
             tooltip="a primary button — hover, press and FOCUS are all the style's"),
     scripts=[dict(kind="ui_demo_button", enabled=True, params=[],
                   strs=[("says", "launched")])])
ROW2 = node("Button Row", LEFT,
            ui=dict(place=free(0, 0), size=size(PCT(1.0), FIX(46.0)),
                    stack=stack("Row", 10.0, 0.0)))
node("Options", ROW2,
     ui=dict(place=free(0, 0), size=size(GROW(1.0), FIX(46.0)),
             text=text("Options", align="Center"), button=True, focusable=True,
             style="button/ghost", tooltip="the quiet variant of the same button"),
     scripts=[dict(kind="ui_demo_button", enabled=True, params=[],
                   strs=[("says", "options")])])
node("Continue", ROW2,
     ui=dict(place=free(0, 0), size=size(GROW(1.0), FIX(46.0)),
             text=text("Continue", align="Center"), button=True, focusable=True,
             disabled=True, style="button/primary",
             tooltip="disabled: greyed by the style, and unreachable by the pad"))

# text field
node("Field Label", LEFT,
     ui=dict(place=free(0, 0), size=size(PCT(1.0), FIX(18.0)),
             text=text("CALL SIGN"), style="caption"))
node("Call Sign", LEFT,
     ui=dict(place=free(0, 0), size=size(PCT(1.0), FIX(52.0)),
             text=text("", sz=22.0), style="field",
             field=dict(placeholder="  type here…", max_len=12, upper=True),
             tooltip="a text field: caret, selection, clipboard, key repeat"),
     scripts=[dict(kind="ui_demo_field", enabled=True, params=[])])

# slider
node("Slider Label", LEFT,
     ui=dict(place=free(0, 0), size=size(PCT(1.0), FIX(18.0)),
             text=text("THRUST"), style="caption"))
SL = node("Thrust", LEFT,
          ui=dict(place=free(0, 0), size=size(PCT(1.0), FIX(20.0)),
                  shape=shape((0.03, 0.04, 0.06, 1.0), radius=10.0, border=1.0,
                              border_color=(0.25, 0.28, 0.34, 1.0)),
                  slider=Raw("Some((value: 0.62, min: 0.0, max: 1.0, dir: Row, "
                             "flip: false, interact: true))"),
                  tooltip="drag me — an ordinary slider, track + fill + handle"))
node("Thrust Fill", SL,
     ui=dict(place=free(0, 0), size=size(PCT(1.0), PCT(1.0)),
             part=Raw("Some(Fill)"),
             shape=shape((0.38, 0.78, 0.96, 1.0), radius=10.0)))
node("Thrust Handle", SL,
     ui=dict(place=free(0, 0), size=size(FIX(20.0), FIX(20.0)),
             part=Raw("Some(Handle)"),
             shape=shape((0.90, 0.93, 0.97, 1.0), radius=10.0)))

# toggles — `toggle: true` flips `selected`, and the style's `selected` block
# is what "on" looks like. No script, and no tick mark the engine drew.
node("Toggle Label", LEFT,
     ui=dict(place=free(0, 0), size=size(PCT(1.0), FIX(18.0)),
             text=text("SYSTEMS"), style="caption"))
CHIPS = node("Chips", LEFT,
             ui=dict(place=free(0, 0), size=size(PCT(1.0), FIX(38.0)),
                     stack=stack("Row", 8.0, 0.0)))
for label, on in [("Shields", True), ("Cloak", False), ("Beacon", True)]:
    node(f"Chip {label}", CHIPS,
         ui=dict(place=free(0, 0), size=size(GROW(1.0), FIX(38.0)),
                 text=text(label, sz=16.0, align="Center"), button=True, focusable=True,
                 toggle=True, selected=on, style="chip",
                 tooltip=f"toggle {label.lower()} — `toggle: true`, no script"))

# ---- right panel: a repeater-driven list ----------------------------------
RIGHT = node("Manifest Panel", LAYER,
             ui=dict(place=free(548, 132), size=size(FIX(400.0), FIX(274.0)),
                     stack=stack("Column", 10.0, 18.0), style="panel"))
node("Manifest Title", RIGHT,
     ui=dict(place=free(0, 0), size=size(PCT(1.0), FIX(20.0)),
             text=text("MANIFEST"), style="caption"))
VIEW = node("Manifest View", RIGHT,
            ui=dict(place=free(0, 0), size=size(PCT(1.0), FIX(208.0)),
                    style="panel/sunken",
                    scroll=Raw("Some((speed: 40.0, offset: 0.0, offset_x: 0.0, drag: false))")))
node("Manifest List", VIEW,
     ui=dict(place=free(8, 8), size=size(FIX(326.0), FIT),
             stack=stack("Column", 6.0, 0.0),
             repeater=dict(template="DemoRow", count=0)))
BAR = node("Manifest Bar", VIEW,
           ui=dict(place=pin("TopRight", -14.0, 8.0), size=size(FIX(6.0), FIX(192.0)),
                   style="scroll/track",
                   scrollbar=dict(target="Manifest View", axis=Raw("Column"))))
node("Manifest Thumb", BAR,
     ui=dict(place=free(0, 0), size=size(FIX(6.0), FIX(40.0)),
             part=Raw("Some(Handle)"), style="scroll/thumb"))

# ---- drag & drop ----------------------------------------------------------
DND = node("Bay Panel", LAYER,
           ui=dict(place=free(548, 452), size=size(FIX(400.0), FIX(156.0)),
                   stack=stack("Column", 12.0, 18.0), style="panel"))
node("Bay Title", DND,
     ui=dict(place=free(0, 0), size=size(PCT(1.0), FIX(20.0)),
             text=text("CARGO BAY  ·  drag an item into a slot"), style="caption"))
SLOTS = node("Slots", DND,
             ui=dict(place=free(0, 0), size=size(PCT(1.0), FIX(88.0)),
                     stack=stack("Row", 10.0, 0.0)))
CARGO = ["ORE", "FUEL", "", ""]
for i in range(4):
    s = node(f"Slot {i + 1}", SLOTS,
             ui=dict(place=free(0, 0), size=size(FIX(80.0), FIX(80.0)),
                     style="slot", button=True, drop_target=True,
                     tooltip=f"slot {i + 1}"),
             scripts=[dict(kind="ui_demo_slot", enabled=True, params=[])])
    # Every slot has a crate; the empty ones start hidden. Dropping moves the
    # LABEL, not the node — the engine moves nothing, and this is one of the
    # cheapest ways a game can decide what a drag looks like.
    node(f"Crate {i + 1}", s,
         ui=dict(place=pin("Center"), size=size(FIX(64.0), FIX(64.0)),
                 text=text(CARGO[i], sz=14.0, align="Center"),
                 style="item", button=True, draggable=True,
                 visible=bool(CARGO[i]),
                 tooltip="pick me up and drop me on an empty slot"))

# ---- status line + tooltip box --------------------------------------------
node("Status", LAYER,
     ui=dict(place=pin("BottomLeft", 50.0, -34.0), size=size(FIX(900.0), FIX(22.0)),
             text=text("focus with the arrows or a d-pad  ·  submit with Enter or A", sz=15.0, color=(0.56, 0.60, 0.68, 1.0))))
TIP = node("Tooltip", LAYER,
           ui=dict(place=free(0, 0), size=size(FIT, FIT), stack=stack("Column", 0.0, 8.0),
                   style="tooltip", visible=False, order=1000, tooltip_box=True))
node("Tooltip Text", TIP,
     ui=dict(place=free(0, 0), size=size(FIT, FIT), text=text(" ", sz=14.0)))

# ---- the crew panel, which this file does NOT describe ---------------------
# Deliberately empty: everything inside it is described in ui_demo.lua with
# `ui.make` and built from a table. This node is the frame — where the panel
# sits and how big it is — which is the part a designer wants to place by hand.
node("Crew Panel", LAYER,
     ui=dict(place=free(980.0, 132.0), size=size(FIX(252.0), FIX(476.0))))

# ---------------------------------------------------------------------------
# Emit
# ---------------------------------------------------------------------------

HEADER = """// The UI demo scene — press Play.
//
// GENERATED by scripts/gen_ui_demo.py, because `parent` is a POSITIONAL index
// into this list: insert one node near the top by hand and every parent below
// it silently points at the wrong thing. Edit it in the editor (that rewrites
// the indices correctly) or edit the generator; don't hand-splice nodes.
//
// It is an ORDINARY project scene using only things a project can use. There
// is no engine theme here: the whole look is assets/ui/demo.tokens.ron and
// assets/ui/demo.uistyle.ron, and deleting those two files leaves the demo
// working and grey.
"""

out = io.StringIO()
out.write(HEADER)
out.write("(\n    name: \"ui_demo\",\n    nodes: [\n")
for n in nodes:
    out.write("        (\n")
    out.write(f"            name: {ron(n['name'], 12)},\n")
    out.write("            transform: (\n")
    out.write("                translation: (0.0, 0.0, 0.0),\n")
    out.write("                rotation: (0.0, 0.0, 0.0, 1.0),\n")
    out.write("                scale: (1.0, 1.0, 1.0),\n")
    out.write("            ),\n")
    m = n["matter"]
    out.write(f"            matter: {m.s if isinstance(m, Raw) else m},\n")
    if n["scripts"]:
        out.write("            scripts: [\n")
        for s in n["scripts"]:
            out.write("                (\n")
            out.write(f"                    kind: {ron(s['kind'], 20)},\n")
            out.write("                    enabled: true,\n")
            out.write("                    params: [],\n")
            if s.get("strs"):
                out.write("                    strs: [\n")
                for k, v in s["strs"]:
                    out.write(f"                        ({ron(k, 24)}, {ron(v, 24)}),\n")
                out.write("                    ],\n")
            out.write("                ),\n")
        out.write("            ],\n")
    else:
        out.write("            scripts: [],\n")
    if n["parent"] is not None:
        out.write(f"            parent: Some({n['parent']}),\n")
    if n["layer"] is not None:
        out.write(f"            ui_layer: Some({ron(n['layer'], 12)}),\n")
    if n["ui"] is not None:
        out.write(f"            ui: Some({ron(n['ui'], 12)}),\n")
    out.write("        ),\n")
out.write("    ],\n)\n")

open("assets/scenes/ui_demo.ron", "w").write(out.getvalue())
print(f"wrote assets/scenes/ui_demo.ron — {len(nodes)} nodes")
