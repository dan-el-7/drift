"""Drift's install splash: Ink's idle stage as a seamless animated GIF.

Same recipe as the app (shapes blurred, summed, thresholded at 50% with a gentle slope), drawn at
2x and scaled down for clean edges. Every motion completes whole cycles in LOOP seconds, so the GIF
loops without a seam. Velopack shows it borderless while installing, with its progress line below.

    python installer/splash.py ui/windows/Assets/Fonts/Geist-Variable.ttf installer/splash.gif
"""
import math
import sys

from PIL import Image, ImageDraw, ImageFilter, ImageFont

W, H, SS = 380, 300, 2           # size (px) and supersampling
FPS, LOOP = 25, 2.4              # 60 frames
CX, CY = W / 2, 132
K = 1.7                          # the stage, scaled up (blur too, so the goo behaves the same)
INK, PAPER, G1 = 255, 0, 138


def stage(t):
    """The goo layer at time t (seconds), 2x, thresholded. Returns an L image."""
    m = Image.new("L", (W * SS, H * SS), 0)
    d = ImageDraw.Draw(m)

    def disc(x, y, r):
        if r > 0.3:
            d.ellipse(((x - r) * SS, (y - r) * SS, (x + r) * SS, (y + r) * SS), fill=255)

    w = 2 * math.pi / LOOP                      # one loop = one breath
    breath = 1 + 0.25 * (0.5 - 0.5 * math.cos(w * t))
    disc(CX, CY, 12 * K * breath)
    for i, k in enumerate((1, 2, -1)):          # lava-lamp satellites: whole turns per loop
        a = w * k * t + i * 2.1
        rr = (20 + 9 * math.sin(w * t + i)) * K
        disc(CX + math.cos(a) * rr, CY + math.sin(a) * rr, (4.5 + i) * K)
    # one ripple per loop: a liquid ring that thins until it evaporates
    u = (t % LOOP) / LOOP
    lw = 9 * K * (1 - u) ** 1.3
    r = (16 + u * 70) * K
    if lw > 0.2:
        d.ellipse(((CX - r) * SS, (CY - r) * SS, (CX + r) * SS, (CY + r) * SS), outline=255, width=max(1, int(lw * SS)))
    m = m.filter(ImageFilter.GaussianBlur(5.6 * K * SS))
    # threshold at 50%, slope 12 (the app's anti-aliased edge)
    return m.point(lambda v: max(0, min(255, int((v / 255 * 12 - 5.5) * 255))))


def frame(t, fonts):
    goo = stage(t).resize((W, H), Image.LANCZOS)
    img = Image.new("L", (W, H), PAPER)
    img.paste(INK, mask=goo)
    d = ImageDraw.Draw(img)
    mark, caption = fonts
    d.text((22, 22), "drift", font=mark, fill=INK)
    d.text((22, H - 44), "Installing", font=caption, fill=G1)
    return img


def main(font_path, out):
    mark = ImageFont.truetype(font_path, 17)
    mark.set_variation_by_axes([600])
    caption = ImageFont.truetype(font_path, 13)
    caption.set_variation_by_axes([400])
    n = int(FPS * LOOP)
    frames = [frame(i / FPS, (mark, caption)).convert("P", palette=Image.ADAPTIVE, colors=32) for i in range(n)]
    frames[0].save(out, save_all=True, append_images=frames[1:], duration=int(1000 / FPS), loop=0, optimize=True, disposal=1)
    print(f"{out}: {n} frames, {W}x{H}")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
