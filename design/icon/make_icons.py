"""Drift's Windows icons from the Android launcher icon (android/app/src/main/res/drawable/ic_drop.xml).

The Android icon is a 108-unit adaptive icon whose inner 72 units show: a white drop (r 12 at 54,54)
with one satellite (r 4.5 at 70,62.5) on black. Windows gets the same shapes on the black rounded
square it already used; at tray sizes (<= 48 px) the drop is zoomed a little so it stays readable.

    python design/icon/make_icons.py
"""
from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[2]
ASSETS = ROOT / "ui" / "windows" / "Assets"
SS = 8  # supersampling

DROP = (54.0, 54.0, 12.0)
SATELLITE = (70.0, 62.5, 4.5)


def render(w: int, h: int | None = None, rounded: bool = True) -> Image.Image:
    h = h or w
    s = min(w, h)
    W, H = w * SS, h * SS
    img = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    if rounded:
        d.rounded_rectangle((0, 0, W - 1, H - 1), radius=int(0.22 * s * SS), fill=(0, 0, 0, 255))
    else:
        d.rectangle((0, 0, W, H), fill=(0, 0, 0, 255))
    # Android shows units 18..90 of the 108 grid; zoom in at small sizes
    zoom = 1.0 if s > 48 else 1.35
    unit = s * SS / 72.0 * zoom
    cx, cy = W / 2, H / 2
    for (x, y, r) in (DROP, SATELLITE):
        px, py, pr = cx + (x - 54) * unit, cy + (y - 54) * unit, r * unit
        d.ellipse((px - pr, py - pr, px + pr, py + pr), fill=(255, 255, 255, 255))
    return img.resize((w, h), Image.LANCZOS)


def main() -> None:
    sizes = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]
    imgs = [render(n) for n in sizes]
    imgs[-1].save(ASSETS / "Drift.ico", sizes=[(n, n) for n in sizes], append_images=imgs[:-1])
    imgs[-1].save(ASSETS / "Drift-256.png")
    tiles = ASSETS / "Tiles"
    render(150).save(tiles / "Square150x150Logo.png")
    render(44).save(tiles / "Square44x44Logo.png")
    render(50).save(tiles / "StoreLogo.png")
    render(310, 150, rounded=False).save(tiles / "Wide310x150Logo.png")
    # a preview sheet for review
    sheet = Image.new("RGBA", (sum(sizes) + 10 * len(sizes), 256), (128, 128, 128, 255))
    x = 0
    for n, im in zip(sizes, imgs):
        sheet.paste(im, (x, 0), im)
        x += n + 10
    sheet.save(Path(__file__).with_name("preview.png"))
    print("ok")


if __name__ == "__main__":
    main()
