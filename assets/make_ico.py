"""Generate app.ico from icon.png.

icon.png is the single source of truth for the app's icon; this produces the
multi-size .ico that Windows actually wants — build.rs embeds it in the exe,
and installer.iss uses it for setup.exe. Re-run it whenever icon.png changes:

    python assets/make_ico.py

Windows picks a different size from the .ico depending on where it's drawing
(16px in the titlebar, 32px in the taskbar, 256px in Explorer's large view), so
a single-size .ico looks blurry or aliased in most places. Pillow downscales
from the source for each one.

The entries are written as uncompressed BMP/DIB (`bitmap_format="bmp"`), which
matters more than it looks. Pillow's default is to PNG-compress *every* frame,
and Windows only reliably decodes PNG-compressed entries at 256x256 — the shell
surfaces that use the smaller sizes (the taskbar, and a pinned shortcut in
particular) draw an all-PNG icon as a blank placeholder instead. BMP costs a
larger file and nothing else.
"""

from pathlib import Path

from PIL import Image

SIZES = [16, 24, 32, 48, 64, 128, 256]

here = Path(__file__).parent
src = here / "icon.png"
dst = here / "app.ico"

img = Image.open(src).convert("RGBA")
if img.width != img.height:
    raise SystemExit(f"icon must be square, got {img.width}x{img.height}")

img.save(dst, format="ICO", sizes=[(s, s) for s in SIZES], bitmap_format="bmp")
print(f"wrote {dst} ({dst.stat().st_size:,} bytes) with sizes {SIZES}")
