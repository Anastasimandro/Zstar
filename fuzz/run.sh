#!/usr/bin/env bash
# Runs one fuzz target for a fixed time: ./run.sh <target> <seconds>
# Used by CI and by hand, so the limits live in one place.
#   timeout 10 s per input, 512 MiB RSS: going over either is a bug to fix, not to raise.
set -euo pipefail
cd "$(dirname "$0")"
target=${1:?usage: run.sh <target> <seconds>}
seconds=${2:?usage: run.sh <target> <seconds>}

# ASan keeps 256 MiB of freed memory by default, which counts as RSS and would eat half of
# the 512 MiB limit below before the code under test allocates anything.
export ASAN_OPTIONS="${ASAN_OPTIONS:-quarantine_size_mb=16}"

# cargo-fuzz has no --locked: fail here if the lock file would have to change.
cargo fetch --locked --manifest-path Cargo.toml
python3 seeds.py
mkdir -p "corpus/$target" artifacts

# Corpus first (written to), seeds second (read only).
cargo fuzz run "$target" "corpus/$target" "seeds-unpacked/$target" -- \
  -max_total_time="$seconds" \
  -timeout=10 \
  -rss_limit_mb=512 \
  -max_len=65536 \
  -dict=dictionaries/magic.dict \
  -print_final_stats=1
