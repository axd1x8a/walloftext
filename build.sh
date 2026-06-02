#!/usr/bin/env bash
set -e

if [ ! -f static/unifont.wtfont ]; then
  echo "Generating font atlas..."
  cargo run -p font-gen -- --font tools/font-gen/assets/UnifontExMono.ttf
fi

echo "Building WASM frontend..."
# wasm-pack build --target web frontend/ --out-dir ../static --no-typescript --release

cargo build --target wasm32-unknown-unknown -p walloftext-frontend
wasm-bindgen --keep-debug --target web --out-dir static ./target/wasm32-unknown-unknown/debug/walloftext_frontend.wasm --no-typescript

# echo "Building backend..."
# cargo build -p walloftext-backend

# # echo "Starting server..."
# ./target/debug/walloftext
