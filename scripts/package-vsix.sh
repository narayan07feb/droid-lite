#!/bin/sh
# Builds the VS Code / Cursor extension with the engine inside: vscode/droid-lite-<target>-<version>.vsix
set -eu
cd "$(dirname "$0")/.."
target=${1:-darwin-arm64}
cargo build --release
mkdir -p vscode/bin
cp target/release/droid-lite vscode/bin/droid-lite
cd vscode
DROID_LITE_ENGINE=bin/droid-lite node test/run.js
npx --yes @vscode/vsce@3.6.2 package --target "$target" --allow-missing-repository
