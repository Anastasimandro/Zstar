#!/usr/bin/env bash
# Peak RSS and wall time per format on one deterministic 1 GiB file (release build).
# Usage: scripts/bench-1gib.sh [format...]   (default: every format the CLI accepts below)
# Env:   BENCH_DIR  work directory with ~3 GiB free (default: ${TMPDIR:-/tmp}/zstar-bench)
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
work=${BENCH_DIR:-${TMPDIR:-/tmp}/zstar-bench}
mkdir -p "$work/in"
big="$work/in/big.bin"

if [ ! -f "$big" ] || [ "$(stat -c %s "$big")" != 1073741824 ]; then
  # Half word-salad text (compresses ~3x), half seeded pseudo-random bytes (incompressible).
  python3 - "$big" <<'PY'
import random, sys
rng = random.Random(1234)
words = ["".join(rng.choices("abcdefghijklmnopqrstuvwxyz", k=rng.randint(2, 9))) for _ in range(3000)]
chunk = 8 << 20
with open(sys.argv[1], "wb") as f:
    for i in range(128):
        if i % 2 == 0:
            text = bytearray()
            while len(text) < chunk:
                text += (" ".join(rng.choices(words, k=2000)) + "\n").encode()
            f.write(text[:chunk])
        else:
            f.write(rng.randbytes(chunk))
PY
fi

# Wall seconds and peak RSS (KiB) of one command. Peak RSS is VmHWM of the child itself,
# polled until it exits: getrusage(RUSAGE_CHILDREN) would also count the launcher's memory.
measure() {
  python3 -c 'import subprocess, sys, time
t = time.time()
p = subprocess.Popen(sys.argv[1:], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
peak = 0
while p.poll() is None:
    try:
        for line in open(f"/proc/{p.pid}/status"):
            if line.startswith("VmHWM:"):
                peak = max(peak, int(line.split()[1]))
    except OSError:
        pass
    time.sleep(0.005)
if p.returncode:
    sys.exit(f"command failed: {p.returncode}")
print(f"{time.time() - t:.2f} {peak}")' "$@"
}

cargo build --release --locked -p zstar-cli --manifest-path "$root/Cargo.toml" >&2
zstar="$root/target/release/zstar"
formats=("$@")
[ ${#formats[@]} -gt 0 ] || formats=(zip tar tar.gz tar.zst gz zst)

printf '| format | compress s | compress MiB | extract s | extract MiB | archive MiB |\n|---|---|---|---|---|---|\n'
for f in "${formats[@]}"; do
  out="$work/out.$f"; dest="$work/dest"
  rm -rf "$out" "$dest"
  src="$work/in"; case "$f" in gz|zst) src="$big";; esac
  c=$(measure "$zstar" compress --format "$f" "$out" "$src")
  x=$(measure "$zstar" extract -o "$dest" "$out")
  set -- $c; cs=$1; cm=$(( $2 / 1024 ))
  set -- $x; xs=$1; xm=$(( $2 / 1024 ))
  printf '| %s | %s | %s | %s | %s | %s |\n' "$f" "$cs" "$cm" "$xs" "$xm" "$(( $(stat -c %s "$out") / 1048576 ))"
  rm -rf "$out" "$dest"
done
