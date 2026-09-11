#!/usr/bin/env python3
"""Generate every icon the binaries and installers embed from the one logo.

    python3 scripts/brand/make-icons.py

Reads `branding/floptle-logo.png` (the full logo: the mark above the wordmark,
white line art with its shadow, on transparency) and writes, all committed:

    branding/icon-<size>.png   the MARK on a dark rounded tile, 16..1024
    branding/floptle.ico       Windows, every size in one file (build.rs embeds it)
    branding/floptle.icns      macOS, for a Floptle.app if one is ever bundled

The mark alone: at 32 px a wordmark is a smudge. And a tile behind it: the
logo is white on nothing, which is invisible on a light taskbar. The tile is
the editor's ground, so the icon reads as the app it opens.

Deterministic — the same input writes the same bytes — so a regenerated set
diffs as nothing. Needs Pillow.
"""

from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[2]
SRC = ROOT / "branding" / "floptle-logo.png"
OUT = ROOT / "branding"

# The mark's box inside the source, measured from its alpha (the wordmark sits
# below y=605 and is cropped away).
MARK_BOX = (138, 127, 649, 600)
TILE = (0x16, 0x16, 0x1a, 255)
SIZES = [16, 24, 32, 48, 64, 128, 256, 512, 1024]


def tile(size: int) -> Image.Image:
    """A rounded dark square, drawn at 4x and downsampled so the corners are smooth."""
    s = size * 4
    im = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    ImageDraw.Draw(im).rounded_rectangle((0, 0, s - 1, s - 1), radius=int(s * 0.22), fill=TILE)
    return im.resize((size, size), Image.LANCZOS)


def icon(mark: Image.Image, size: int) -> Image.Image:
    out = tile(size)
    # The mark fills ~72% of the tile, centred; at tiny sizes a little more,
    # or the strokes vanish.
    fill = 0.80 if size <= 32 else 0.72
    w, h = mark.size
    scale = size * fill / max(w, h)
    m = mark.resize((max(1, round(w * scale)), max(1, round(h * scale))), Image.LANCZOS)
    x = (size - m.width) // 2
    y = (size - m.height) // 2
    out.alpha_composite(m, (x, y))
    return out


def main() -> None:
    src = Image.open(SRC).convert("RGBA")
    mark = src.crop(MARK_BOX)
    icons = {s: icon(mark, s) for s in SIZES}
    for s, im in icons.items():
        im.save(OUT / f"icon-{s}.png", optimize=True)
    # Pillow writes every listed size into the one .ico from the largest.
    icons[256].save(
        OUT / "floptle.ico",
        sizes=[(s, s) for s in SIZES if s <= 256],
        append_images=[icons[s] for s in SIZES if s < 256],
    )
    icons[1024].save(OUT / "floptle.icns")
    print("wrote", ", ".join(f"icon-{s}.png" for s in SIZES), "floptle.ico floptle.icns")


if __name__ == "__main__":
    main()
