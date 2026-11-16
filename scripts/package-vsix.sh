#!/bin/sh
# Builds the VS Code / Cursor extension with the engine inside: vscode/droid-lite-<target>-<version>.vsix
set -eu
cd "$(dirname "$0")/.."
# the engine is built for this machine's OS and CPU, so the package can only be for it: a passed
# target that names another one is refused (the package would carry an engine that cannot run)
case "$(uname -s) $(uname -m)" in
  "Darwin arm64") host=darwin-arm64 ;;
  "Darwin x86_64") host=darwin-x64 ;;
  "Linux x86_64") host=linux-x64 ;;
  "Linux aarch64" | "Linux arm64") host=linux-arm64 ;;
  *) host= ;;
esac
[ -n "$host" ] || { echo "package-vsix.sh: no VS Code target for $(uname -s) $(uname -m)" >&2; exit 1; }
target=${1:-$host}
[ "$target" = "$host" ] || {
  echo "package-vsix.sh: $target is not this machine ($host); build on a $target machine" >&2
  exit 1
}
cargo build --release
mkdir -p vscode/bin
cp target/release/droid-lite vscode/bin/droid-lite
cd vscode
DROID_LITE_ENGINE=bin/droid-lite node test/run.js
node test/plugins.js
npx --yes @vscode/vsce@3.6.2 package --target "$target" --allow-missing-repository
