#!/usr/bin/env python3
"""Turns the committed hex seeds (fuzz/seeds/<target>/*.hex) into the binary files libFuzzer
reads (fuzz/seeds-unpacked/<target>/*). Archives are kept as hex text so that no binary
file is ever committed."""
import pathlib
import sys

root = pathlib.Path(__file__).resolve().parent
out = root / "seeds-unpacked"
count = 0
for hex_file in sorted((root / "seeds").glob("*/*.hex")):
    target = out / hex_file.parent.name
    target.mkdir(parents=True, exist_ok=True)
    data = bytes.fromhex("".join(hex_file.read_text().split()))
    (target / hex_file.stem).write_bytes(data)
    count += 1
print(f"unpacked {count} seeds into {out}", file=sys.stderr)
