"""Generate approved v10 filled device symbols, tray masks, and native previews.
Python/Pillow are build tools only; the native app embeds alpha masks, not PNG decoders.
"""
from pathlib import Path
from io import BytesIO
import ctypes
import os
import struct
from PIL import Image, ImageDraw, ImageFont
from device_shapes import KINDS
from software_art import software_icon
from tray_art import tray as approved_tray, symbol as window_symbol, layers, generate as generate_tray

ROOT = Path(__file__).resolve().parents[1]
ASSETS = ROOT / "assets"
SIZES = (16, 20, 24, 28, 32, 40, 48, 56, 64, 256)
THEMES = {
    "light": {"foreground": "#332F2A", "fill": "#985536", "track": "#C8C2B9", "background": "#F1F0EE"},
    "dark": {"foreground": "#F0ECE6", "fill": "#E7B18B", "track": "#69655F", "background": "#222222"},
}


def tray(kind, percent, theme="dark", pixels=16):
    if not 0 <= percent <= 100: raise ValueError("Battery value must be 0..100")
    return approved_tray(kind, percent, theme, pixels)


def write_ico(path, frames):
    pngs = []
    for frame in frames:
        buf = BytesIO()
        frame.save(buf, format="PNG", optimize=True)
        pngs.append(buf.getvalue())
    offset = 6 + 16 * len(frames)
    payload = bytearray(struct.pack("<HHH", 0, 1, len(frames)))
    for image, png in zip(frames, pngs):
        n = image.width
        payload += struct.pack("<BBBBHHII", n if n < 256 else 0, n if n < 256 else 0,
                               0, 0, 1, 32, len(png), offset)
        offset += len(png)
    path.write_bytes(payload + b"".join(pngs))


def preview_font(size, candidates=None):
    if candidates is None:
        candidates = ("C:/Windows/Fonts/segoeui.ttf", "C:/Windows/Fonts/arial.ttf")
    for candidate in candidates:
        if Path(candidate).is_file():
            try:
                return ImageFont.truetype(str(candidate), size)
            except OSError:
                pass
    # The contact sheet is optional presentation; icon generation has no fonts.
    return ImageFont.load_default()


def preview():
    sheet = Image.new("RGB", (720, 310), THEMES["light"]["background"])
    d = ImageDraw.Draw(sheet)
    font = preview_font(12)
    for t, theme in enumerate(THEMES):
        y = t * 155
        d.rectangle((0, y, 719, y + 154), fill=THEMES[theme]["background"])
        for row, n in enumerate((16, 20, 24, 32)):
            yy = y + 10 + 36 * row
            d.text((10, yy), f"{n}px", font=font, fill=THEMES[theme]["foreground"])
            for col, (kind, value) in enumerate((("mouse", 90), ("keyboard", 75), ("headset", 60), ("gamepad", 45))):
                icon = tray(kind, value, theme, n)
                sheet.paste(icon, (90 + col * 150, yy), icon)
    sheet.save(ASSETS / "previews" / "tray-contact-sheet.png")
    atlas = Image.new("RGBA", (640, 384), "#FAF8F5")
    for index, kind in enumerate(KINDS):
        x, y = index % 4 * 160, index // 4 * 96
        atlas.alpha_composite(window_symbol(kind,56), (x + 15, y + 5))
        for t, theme in enumerate(THEMES):
            tile = Image.new("RGBA", (32, 32), THEMES[theme]["background"])
            tiny = tray(kind, 75, theme)
            tile.alpha_composite(tiny, (8, 8))
            atlas.alpha_composite(tile, (x + 80 + t * 34, y + 18))
        ImageDraw.Draw(atlas).text((x + 15, y + 72), kind, font=font, fill="#302D29")
    atlas.convert("RGB").save(ASSETS / "previews" / "device-symbol-atlas.png")
    proof = Image.new("RGB", (768, 128), "#F1F0EE")
    for t, theme in enumerate(THEMES):
        ImageDraw.Draw(proof).rectangle((t * 384, 0, (t + 1) * 384 - 1, 127), fill=THEMES[theme]["background"])
        for i, kind in enumerate(("mouse", "keyboard", "gamepad")):
            image = tray(kind, 75, theme).resize((128, 128), Image.Resampling.NEAREST)
            proof.paste(image, (t * 384 + i * 128, 0), image)
    proof.save(ASSETS / "previews" / "tray-16px-inspection.png")


def verify():
    for n in (28,35,42,56):
        expected=bytearray()
        for kind in KINDS:
            alpha,inner=layers(kind,n,standalone=True)
            assert alpha.getbbox() and inner.getbbox()
            for a,b in zip(alpha.tobytes(),inner.tobytes()):expected.extend((a,b))
        assert (ASSETS / "device-masks" / f"symbol-v10-{n}.bin").read_bytes()==expected
    for kind in KINDS:
        with Image.open(ASSETS / "icons" / "devices" / f"{kind}.ico") as icon:
            assert icon.ico.sizes() == {(n, n) for n in SIZES}
    count = len(KINDS) * len(SIZES)
    print(f"Pillow verified {len(KINDS)} filled standalone symbols at four DPI sizes and {count} ICO frames.")
    if os.name != "nt":
        print("Windows native decoding skipped: this host is not Windows.")
        return
    from ctypes import wintypes
    user = ctypes.WinDLL("user32", use_last_error=True)
    user.LoadImageW.argtypes = (wintypes.HINSTANCE, wintypes.LPCWSTR, wintypes.UINT, ctypes.c_int, ctypes.c_int, wintypes.UINT)
    user.LoadImageW.restype = wintypes.HANDLE
    user.DestroyIcon.argtypes = (wintypes.HANDLE,)
    user.DestroyIcon.restype = wintypes.BOOL
    for kind in KINDS:
        for n in SIZES:
            icon = user.LoadImageW(None, str(ASSETS / "icons" / "devices" / f"{kind}.ico"), 1, n, n, 0x10)
            assert icon, (kind, n, ctypes.get_last_error())
            assert user.DestroyIcon(icon)
    print(f"Windows LoadImageW verified {count} ICO frames; all handles destroyed.")


def main():
    generate_tray()
    for path in ("device-masks", "icons/devices", "previews", "generated"):
        (ASSETS / path).mkdir(parents=True, exist_ok=True)
    for kind in KINDS:
        write_ico(ASSETS / "icons" / "devices" / f"{kind}.ico", [tray(kind, 100, "light", n) for n in SIZES])
    # Existing resource compiler uses this compatibility filename for the EXE only.
    write_ico(ASSETS / "icons" / "full.ico", [software_icon(n) for n in SIZES])
    preview()
    verify()


if __name__ == "__main__":
    main()
