"""Regenerate the original Morpheus app icon with Pillow.

Run `python3 scripts/create_icons.py`; macOS additionally uses iconutil for ICNS.
Generated icons are checked in so package builds do not need Python.
"""

from pathlib import Path
from PIL import Image, ImageDraw


ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "assets"
OUT.mkdir(exist_ok=True)

SIZE = 1024
image = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
draw = ImageDraw.Draw(image)
draw.rounded_rectangle((20, 20, 1004, 1004), radius=235, fill="#10131B")


def stroke(points, color, width=92):
    draw.line(points, fill=color, width=width, joint="curve")
    radius = width // 2
    for x, y in (points[0], points[-1]):
        draw.ellipse((x-radius, y-radius, x+radius, y+radius), fill=color)


# Two nested paths form an M that reads clearly at notification-icon sizes.
stroke([(255, 724), (255, 326), (510, 555)], "#A88CFF")
stroke([(510, 555), (769, 326), (769, 724)], "#78C8B1")
draw.ellipse((474, 518, 546, 590), fill="#EAFBF5")

image.save(OUT / "icon.png")
image.save(OUT / "icon.ico", sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])

iconset = OUT / "icon.iconset"
iconset.mkdir(exist_ok=True)
for logical, scale in ((16, 1), (16, 2), (32, 1), (32, 2), (128, 1), (128, 2), (256, 1), (256, 2), (512, 1), (512, 2)):
    size = logical * scale
    suffix = "@2x" if scale == 2 else ""
    resized = image.resize((size, size), Image.Resampling.LANCZOS)
    resized.save(iconset / f"icon_{logical}x{logical}{suffix}.png")

print(f"Generated icons in {OUT}")
