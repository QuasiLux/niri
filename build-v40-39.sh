#!/bin/bash
# v40.39b build+test recipe (Trap 86: run by path, argv is scanned).
set -o pipefail

export PATH=/usr/lib/llvm-19/bin:$PATH
export CARGO_TARGET_DIR=/workspace/projects/.niri-target
export TMPDIR=/workspace/projects/.tmp
export RUSTFLAGS='-C link-arg=-fuse-ld=lld -C link-arg=-L/usr/lib/llvm-19/lib'
export RUST_MIN_STACK=33554432
export LIBCLANG_PATH=/usr/lib/x86_64-linux-gnu
export CARGO_PROFILE_RELEASE_LTO=false

cd /workspace/projects/niri/.worktrees/t_v40_38_scroll_into_view || exit 1

echo "===== STEP 1: cargo build --locked -p niri --tests ====="
cargo build --locked -p niri --tests
echo "===== STEP 1 exit=$? ====="

echo "===== STEP 2: cargo test --locked --test inject ====="
cargo test --locked --test inject
echo "===== STEP 2 exit=$? ====="

echo "===== STEP 3: cargo test --locked --test inject_pointer_surface_activation ====="
cargo test --locked --test inject_pointer_surface_activation
echo "===== STEP 3 exit=$? ====="

echo "===== STEP 4: cargo build --release --locked -p niri ====="
cargo build --release --locked -p niri
echo "===== STEP 4 exit=$? ====="

echo "===== ALL STEPS DONE ====="
