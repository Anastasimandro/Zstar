#!/usr/bin/env bash
# Metadata-heavy measurements: many empty or tiny files, before and after a change.
#   scripts/meta-bench.sh gen <dir>                       build the data sets (slow, ~3 GB of inodes)
#   scripts/meta-bench.sh run <label> <meta_bench> <dir> [reps]
#       <meta_bench> is the example built from the tree under test:
#         cargo build --release --locked -p zstar-core --example meta_bench
#       (copy crates/core/examples/meta_bench.rs into an older tree to measure it).
#       Archives for the extract runs are made once by the binary given and reused, so every
#       label extracts the same bytes. Prints one line per run. Env: ALLOC_SO=<alloc_count.so>
#       (built from scripts/alloc_count.c) adds the allocation-count runs; SKIP_1M=1 skips the
#       one-million-entry sets.
set -euo pipefail

gen() {
  local d=$1
  mkdir -p "$d"
  python3 - "$d" <<'PY'
import os, sys
root = sys.argv[1]
def flat(name, n):
    p = os.path.join(root, name)
    if os.path.isdir(p): return
    os.makedirs(p + ".tmp")
    for i in range(n):
        open(f"{p}.tmp/f{i:07d}", "wb").close()
    os.rename(p + ".tmp", p)
def tree(name, dirs, per, size):
    p = os.path.join(root, name)
    if os.path.isdir(p): return
    os.makedirs(p + ".tmp")
    body = b"x" * size
    for d in range(dirs):
        os.makedirs(f"{p}.tmp/d{d:04d}")
        for i in range(per):
            with open(f"{p}.tmp/d{d:04d}/f{i:05d}", "wb") as f:
                f.write(body)
    os.rename(p + ".tmp", p)
flat("flat100k", 100_000)
flat("flat1m", 1_000_000)
tree("tree1m", 1000, 1000, 0)
tree("tiny100k", 100, 1000, 64)
PY
}

one() { # one <tag> <cmd...>: run, print the harness line prefixed with the tag
  local tag=$1; shift
  local out
  out=$("$@" 2>&1 | tr '\n' ' ')
  echo "$LABEL $tag $out"
}

run() {
  LABEL=$1; local bin=$2 d=$3 reps=${4:-3}
  local w="$d/work"; mkdir -p "$w/archives"
  local sets=(flat100k tiny100k) sets1m=(flat1m tree1m)
  [ -n "${SKIP_1M:-}" ] || sets+=("${sets1m[@]}")
  # SETS="a b" picks the sets, PHASES="timed cancel alloc" the parts (to resume an interrupted run).
  [ -z "${SETS:-}" ] || read -ra sets <<<"$SETS"
  local phases=" ${PHASES:-timed cancel alloc} "
  # Archives to extract, made once.
  for s in "${sets[@]}"; do
    for f in zip tar.gz; do
      [ -f "$w/archives/$s.$f" ] || "$bin" create "$f" "$d/$s" "$w/archives/$s.$f" >/dev/null
    done
  done
  for s in "${sets[@]}"; do
    [[ $phases == *" timed "* ]] || break
    local n=$reps; case $s in *1m) n=${REPS_1M:-$reps};; esac
    for f in zip tar.gz; do
      for r in $(seq "$n"); do
        rm -f "$w/out"; one "create $f $s rep$r" "$bin" create "$f" "$d/$s" "$w/out"
      done
    done
    case $s in *100k) for r in $(seq "$n"); do rm -f "$w/out"
      one "create 7z $s rep$r" "$bin" create 7z "$d/$s" "$w/out"; done;; esac
    for f in zip tar.gz; do
      for r in $(seq "$n"); do
        rm -rf "$w/dest"; one "extract $f $s rep$r" "$bin" extract "$w/archives/$s.$f" "$w/dest"
      done
    done
    rm -rf "$w/dest" "$w/out"
  done
  # Cancellation 1.5 s after the start, while the tree is still being walked or planned.
  for s in "${sets[@]}"; do
    [[ $phases == *" cancel "* ]] || break
    case $s in *100k) ms=400;; *) ms=1500;; esac
    for r in 1 2 3; do
      rm -f "$w/out"; one "cancel-create zip $s at${ms}ms rep$r" "$bin" create zip "$d/$s" "$w/out" --cancel-ms $ms
      rm -rf "$w/dest"; one "cancel-extract zip $s at${ms}ms rep$r" "$bin" extract "$w/archives/$s.zip" "$w/dest" --cancel-ms $ms
    done
    rm -rf "$w/dest" "$w/out"
  done
  if [ -n "${ALLOC_SO:-}" ] && [[ $phases == *" alloc "* ]]; then
    for s in "${sets[@]}"; do
      for f in zip tar.gz; do
        rm -f "$w/out"; one "alloc create $f $s" env LD_PRELOAD="$ALLOC_SO" "$bin" create "$f" "$d/$s" "$w/out"
        rm -rf "$w/dest"; one "alloc extract $f $s" env LD_PRELOAD="$ALLOC_SO" "$bin" extract "$w/archives/$s.$f" "$w/dest"
      done
      rm -rf "$w/dest" "$w/out"
    done
  fi
}

case ${1:-} in
  gen) gen "$2" ;;
  run) shift; run "$@" ;;
  *) sed -n '2,12p' "$0"; exit 2 ;;
esac
