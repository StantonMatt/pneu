#!/usr/bin/env python3
"""One-off: python3 desktop/packaging/windows/generate-assets.py (Pillow).

The main PNG is the checked-in export of assets/macos/AppIcon.svg. The small
master was rendered once with resvg-py from assets/macos/AppIcon-16.svg, changing
its viewBox to "72 72 880 880" at 1024x1024. No SVG renderer is needed to regenerate
assets or build the package. Keep the tile/Store exports on their original grid.
"""
from pathlib import Path
from PIL import Image

root = Path(__file__).resolve().parent
master = Image.open(root / '../../assets/macos/AppIcon.iconset/icon_512x512@2x.png').convert('RGBA')
app = master.crop((72, 72, 952, 952))
small = Image.open(root / 'AppIcon-small.png').convert('RGBA')
for name, size in [('Square44x44Logo', 44), ('Square150x150Logo', 150), ('StoreLogo', 50)]:
    source = app if name == 'Square44x44Logo' else master
    for scale in (100, 125, 150, 200, 400):
        source.resize(((size * scale + 50) // 100,) * 2, Image.Resampling.LANCZOS).save(
            root / f'Assets/{name}.scale-{scale}.png')
for size in (16, 20, 24, 30, 32, 36, 40, 48, 60, 64, 72, 80, 96, 256):
    source = small if size <= 20 else app
    icon = source.resize((size, size), Image.Resampling.LANCZOS)
    for suffix in ('', '_altform-unplated'):
        icon.save(root / f'Assets/Square44x44Logo.targetsize-{size}{suffix}.png')
frames = [
    (small if n == 16 else app).resize((n, n), Image.Resampling.LANCZOS)
    for n in (16, 24, 32, 48, 64, 128, 256)
]
frames[-1].save(root / 'Pneu.ico', sizes=[frame.size for frame in frames], append_images=frames[:-1])
