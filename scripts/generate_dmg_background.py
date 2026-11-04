"""Render the macOS installer window background (requires Pillow).

The layout (window size, icon centres) comes from release.py so the artwork and
the Finder settings cannot drift apart. Writes 1x and @2x PNGs; dmgbuild joins
them into one HiDPI TIFF.

Finder draws icon labels black in Light mode and white in Dark mode, and the
artwork cannot tell which. Each icon therefore sits on a tile whose colour
around the label has a luminance near 0.18, where both black and white text
keep about 4.5:1 contrast.
"""
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter

import release

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "assets/installer"
WIDTH, HEIGHT = release.DMG_CANVAS
ICON = release.DMG_ICON_SIZE
TILE = (184, 204)  # width, height
TILE_TOP = 86  # tile top above the icon centre; leaves room for the label below
# Labels fall in the lower third, where the gradient reaches luminance ~0.18.
TILE_TOP_COLOR, TILE_BOTTOM_COLOR = (134, 138, 145), (110, 114, 121)
SUPERSAMPLE = 4


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def vertical_gradient(size, top, bottom):
    gradient = Image.linear_gradient("L").resize(size)
    return Image.composite(Image.new("RGB", size, bottom), Image.new("RGB", size, top), gradient)


def rounded_mask(size, box, radius, scale):
    """An antialiased mask; `box` and `radius` are in points, `size` in pixels."""
    factor = scale * SUPERSAMPLE
    mask = Image.new("L", (size[0] * SUPERSAMPLE, size[1] * SUPERSAMPLE), 0)
    ImageDraw.Draw(mask).rounded_rectangle([v * factor for v in box], radius * factor, fill=255)
    return mask.resize(size, Image.Resampling.LANCZOS)


def render(scale):
    size = (WIDTH * scale, HEIGHT * scale)
    s = lambda value: value * scale  # noqa: E731
    canvas = vertical_gradient(size, (38, 40, 45), (16, 17, 19)).convert("RGBA")

    # A soft light from above, as on a studio backdrop.
    glow = Image.new("L", size, 0)
    ImageDraw.Draw(glow).ellipse([s(-80), s(-260), s(WIDTH + 80), s(250)], fill=46)
    glow = glow.filter(ImageFilter.GaussianBlur(s(90)))
    canvas = Image.composite(Image.new("RGBA", size, (255, 255, 255, 255)), canvas, glow)

    # The Frames mark, two overlapping frames, drawn large and barely there.
    motif = Image.new("L", (size[0] * 2, size[1] * 2), 0)
    draw = ImageDraw.Draw(motif)
    middle = release.DMG_ICON_Y + TILE[1] / 2 - TILE_TOP
    for dx, dy in ((-40, -36), (40, 36)):
        cx, cy = WIDTH / 2 + dx, middle + dy
        box = [(cx - 128) * scale * 2, (cy - 128) * scale * 2, (cx + 128) * scale * 2, (cy + 128) * scale * 2]
        draw.rounded_rectangle(box, s(30) * 2, outline=255, width=s(2) * 2)
    motif = motif.resize(size, Image.Resampling.LANCZOS).point(lambda v: v * 14 // 255)
    canvas = Image.composite(Image.new("RGBA", size, (255, 255, 255, 255)), canvas, motif)

    for cx in (release.DMG_APP_X, release.DMG_APPLICATIONS_X):
        cy = release.DMG_ICON_Y
        box = (cx - TILE[0] / 2, cy - TILE_TOP, cx + TILE[0] / 2, cy - TILE_TOP + TILE[1])
        radius = 42
        # Shadow under the tile.
        shadow = rounded_mask(size, (box[0], box[1] + 10, box[2], box[3] + 14), radius, scale)
        shadow = shadow.filter(ImageFilter.GaussianBlur(s(16))).point(lambda v: v * 150 // 255)
        canvas = Image.composite(Image.new("RGBA", size, (0, 0, 0, 255)), canvas, shadow)
        # Brushed graphite; the label band stays near luminance 0.18.
        mask = rounded_mask(size, box, radius, scale)
        face = Image.new("RGB", size)
        y0, y1 = round(s(box[1])), round(s(box[3]))
        face.paste(vertical_gradient((size[0], y1 - y0), TILE_TOP_COLOR, TILE_BOTTOM_COLOR), (0, y0))
        canvas = Image.composite(face.convert("RGBA"), canvas, mask)
        # A hairline highlight along the top edge, as light catching a bevel.
        inner = rounded_mask(size, (box[0], box[1] + 1.5, box[2], box[3] + 1.5), radius, scale)
        rim = ImageChops.subtract(mask, inner).point(lambda v: v * 110 // 255)
        canvas = Image.composite(Image.new("RGBA", size, (255, 255, 255, 255)), canvas, rim)

    # The drag arrow: a line that fades in, ending in an open chevron.
    arrow = Image.new("L", (size[0] * SUPERSAMPLE // scale, size[1] * SUPERSAMPLE // scale), 0)
    a = lambda value: value * SUPERSAMPLE  # noqa: E731
    draw = ImageDraw.Draw(arrow)
    y = release.DMG_ICON_Y - 8
    left = release.DMG_APP_X + TILE[0] / 2 + 20
    right = release.DMG_APPLICATIONS_X - TILE[0] / 2 - 20
    steps = 48
    for i in range(steps):
        x0 = left + (right - 3 - left) * i / steps
        x1 = left + (right - 3 - left) * (i + 1) / steps
        draw.line([a(x0), a(y), a(x1), a(y)], fill=round(40 + 175 * (i + 1) / steps), width=a(2))
    for dy in (-9, 9):
        draw.line([a(right - 9), a(y + dy), a(right), a(y)], fill=215, width=a(2))
    for point in ((right - 9, y - 9), (right - 9, y + 9), (right, y)):
        draw.ellipse([a(point[0] - 1), a(point[1] - 1), a(point[0] + 1), a(point[1] + 1)], fill=215)
    arrow = arrow.resize(size, Image.Resampling.LANCZOS)
    canvas = Image.composite(Image.new("RGBA", size, (236, 238, 242, 255)), canvas, arrow)

    # Fine grain keeps the large gradients from banding.
    noise = Image.effect_noise(size, 6).point(lambda v: 128 + (v - 128) // 2)
    canvas = ImageChops.add(canvas.convert("RGB"), Image.merge("RGB", (noise,) * 3), offset=-128)
    return canvas


def main():
    OUTPUT.mkdir(parents=True, exist_ok=True)
    render(1).save(OUTPUT / "dmg-background.png", optimize=True)
    render(2).save(OUTPUT / "dmg-background@2x.png", optimize=True)


if __name__ == "__main__":
    main()
