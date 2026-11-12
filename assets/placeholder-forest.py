# Placeholder pixel-art forests for OAK (to be replaced with real art).
import random, sys
from PIL import Image
def forest(w, h, seed, moon=True, layers=3, stars=True, shades=None):
    r = random.Random(seed)
    im = Image.new('RGB', (w, h))
    px = im.load()
    top, bot = (13, 17, 16), (27, 36, 31)
    for y in range(h):
        t = y / h
        for x in range(w):
            # banded gradient with ordered dithering
            d = ((x & 1) * 2 + (y & 1)) / 4 - 0.375
            k = min(1, max(0, t + d * 0.12))
            k = round(k * 6) / 6
            px[x, y] = tuple(int(a + (b - a) * k) for a, b in zip(top, bot))
    if stars:
        for _ in range(w * h // 140):
            x, y = r.randrange(w), r.randrange(int(h * 0.55))
            px[x, y] = r.choice([(70, 78, 72), (95, 102, 94), (140, 146, 132)])
    if moon:
        cx, cy, rad = int(w * 0.78), int(h * 0.2), max(3, h // 12)
        for y in range(cy - rad, cy + rad + 1):
            for x in range(cx - rad, cx + rad + 1):
                if (x - cx) ** 2 + (y - cy) ** 2 <= rad * rad and 0 <= x < w and 0 <= y < h:
                    lit = (x - cx + rad * 0.45) ** 2 + (y - cy - rad * 0.2) ** 2 > (rad * 0.8) ** 2
                    px[x, y] = (214, 210, 190) if lit else (24, 31, 28)
    shades = shades or [(32, 48, 42), (24, 40, 33), (16, 29, 23), (10, 19, 15)][-layers:]
    for li, col in enumerate(shades):
        base = int(h * (0.55 + 0.13 * li)) if layers > 1 else int(h * 0.62)
        hi = tuple(min(255, c + 12) for c in col)
        x = -r.randrange(6)
        while x < w + 8:
            th = r.randint(int(h * (0.22 + 0.08 * li)), int(h * (0.34 + 0.1 * li)))
            hw = max(2, th // 3)
            top_y = base - th
            for y in range(top_y, h):
                if y < base:
                    # tiers: a jagged triangle
                    f = (y - top_y) / max(1, base - top_y)
                    half = int(hw * f) + (1 if (y - top_y) % 4 == 3 else 0)
                    for dx in range(-half, half + 1):
                        xx = x + dx
                        if 0 <= xx < w:
                            px[xx, y] = hi if dx == -half and half > 1 else col
                else:
                    for dx in range(-1, 1):
                        if 0 <= x + dx < w and y < base + 2:
                            px[x + dx, y] = (14, 12, 10)
            x += r.randint(max(3, hw), hw * 2 + 2)
        # the layer's ground
        for y in range(base, h):
            for xx in range(w):
                if px[xx, y] != (14, 12, 10) or li == len(shades) - 1:
                    px[xx, y] = col
    return im
out = sys.argv[1]
# The tab strip's forest tab: a tab's shape (210 x 33 points).
forest(108, 17, 11, moon=True, layers=2, shades=[(52, 84, 64), (30, 56, 41)]).save(out + r'\forest-header.png')
forest(320, 180, 7, moon=True, layers=4).save(out + r'\forest-home.png')
