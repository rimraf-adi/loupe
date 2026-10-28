#!/usr/bin/env bash
# Fetch benchmark corpora into ./bench-corpora (gitignored). Shallow clones only.
#
#   scripts/fetch-corpora.sh            # default set, tens of thousands of files at most
#   scripts/fetch-corpora.sh --large    # also fetch repos beyond the v1 scale target
set -euo pipefail

dest="$(cd "$(dirname "$0")/.." && pwd)/bench-corpora"
mkdir -p "$dest"

default=(
  "redis       https://github.com/redis/redis.git"                  # C
  "tokio       https://github.com/tokio-rs/tokio.git"               # Rust
  "typescript  https://github.com/microsoft/TypeScript.git"         # TS
)
large=(
  "rust        https://github.com/rust-lang/rust.git"               # large Rust
  "linux       https://github.com/torvalds/linux.git"               # very large C, > v1 scale
)

repos=("${default[@]}")
if [[ "${1:-}" == "--large" ]]; then
  repos+=("${large[@]}")
fi

for entry in "${repos[@]}"; do
  read -r name url _ <<<"$entry"
  if [[ -d "$dest/$name/.git" ]]; then
    echo "skip   $name (already present)"
    continue
  fi
  echo "clone  $name"
  # depth 1000 keeps enough history for the co-change miner and the localization eval.
  git clone --depth 1000 --single-branch "$url" "$dest/$name"
done
