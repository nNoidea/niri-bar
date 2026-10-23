#!/usr/bin/env python3
"""Generate `sample_app.asar`, the committed test fixture for the ASAR icon parser.

The fixture is a completely synthetic Electron-style archive: it contains no
personal information and resembles what a real packed app looks like on disk.

Layout produced (matches `build_fake_asar` in `src/icon/mod.rs`):

    [u32 LE 4][u32 LE u1][u32 LE u2][u32 LE json_len]
    JSON header (zero-padded to 4-byte boundary)
    payload blob (files referenced by string offsets into this blob)

Header integers follow Electron's chromium Pickle layout:
    u1 = 4 + (4 + padded_json_len)
    u2 = 4 + padded_json_len

Re-run this script to regenerate (output is deterministic for a given zlib build):

    python3 src/icon/testdata/generate_sample_asar.py
"""
import json
import os
import struct
import zlib


def make_png(width: int, height: int, r: int, g: int, b: int) -> bytes:
    """Minimal deterministic PNG: 8-bit RGBA, fully saturated solid colour."""
    def chunk(kind: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + kind
            + data
            + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
        )

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    row = b"\x00" + bytes([r, g, b, 0xFF]) * width
    idat = zlib.compress(row * height, 9)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", idat)
        + chunk(b"IEND", b"")
    )


def main() -> None:
    icon = make_png(24, 24, 0x33, 0x66, 0x99)  # top-level fast-path icon
    icon2 = make_png(32, 32, 0x77, 0x88, 0x99)  # build/ fallback icon
    package_json = json.dumps(
        {
            "name": "sample-electron-app",
            "productName": "Sample Electron App",
            "version": "1.0.0",
            "main": "main.js",
            "icon": "build/icon.png",
        },
        separators=(",", ":"),
        sort_keys=True,
    ).encode()

    # Payload blob: concatenate all file contents and record string offsets.
    payload = b""
    offsets = {}
    for name, blob in (("icon.png", icon), ("package.json", package_json), ("build/icon.png", icon2)):
        offsets[name] = str(len(payload))
        payload += blob

    # node_modules noise: real packed apps ship thousands of these entries and
    # never our icons; the selective parser must skip them in microseconds.
    node_modules = {
        f"pkg_{i}_{name}": {"files": {"index.js": {"size": 96 + i, "offset": "0"}}}
        for i, name in enumerate(("bytebuffer", "debug", "left-pad", "ms", "optipng"))
    }

    header = {
        "files": {
            **node_modules,
            "package.json": {"size": len(package_json), "offset": offsets["package.json"]},
            "icon.png": {"size": len(icon), "offset": offsets["icon.png"]},
            "build": {
                "files": {
                    "icon.png": {"size": len(icon2), "offset": offsets["build/icon.png"]}
                }
            },
        }
    }
    files_json = json.dumps(header, separators=(",", ":")).encode()
    str_len = len(files_json)
    padded_len = (str_len + 3) & ~3
    u2 = 4 + padded_len
    u1 = 4 + u2

    out = struct.pack("<IIII", 4, u1, u2, str_len) + files_json
    out += b"\x00" * (16 + padded_len - len(out))
    out += payload

    dest = os.path.join(os.path.dirname(__file__), "sample_app.asar")
    with open(dest, "wb") as fh:
        fh.write(out)
    print(f"wrote {dest} ({len(out)} bytes)")


if __name__ == "__main__":
    main()
