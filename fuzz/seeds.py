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
# The real RAR fixtures are seeds too, read straight from tests/fixtures/rar/ (no second copy).
rar_target = out / "rar_list_extract"
for rar in sorted((root.parent / "tests" / "fixtures" / "rar").glob("*.rar")):
    rar_target.mkdir(parents=True, exist_ok=True)
    (rar_target / f"fixture_{rar.stem}").write_bytes(rar.read_bytes())
    count += 1
print(f"unpacked {count} seeds into {out}", file=sys.stderr)
