#!/bin/sh
# Builds the VS Code / Cursor extension with the engine inside: vscode/droid-lite-<target>-<version>.vsix
set -eu
cd "$(dirname "$0")/.."
# the engine is built for this Mac's CPU, so the label follows it unless a target is passed
case "$(uname -m)" in
  arm64) host=darwin-arm64 ;;
  x86_64) host=darwin-x64 ;;
  *) host= ;;
esac
target=${1:-$host}
[ -n "$target" ] || { echo "package-vsix.sh: no VS Code target for $(uname -m); pass one" >&2; exit 1; }
cargo build --release
mkdir -p vscode/bin
cp target/release/droid-lite vscode/bin/droid-lite
cd vscode
DROID_LITE_ENGINE=bin/droid-lite node test/run.js
node test/plugins.js
npx --yes @vscode/vsce@3.6.2 package --target "$target" --allow-missing-repository
