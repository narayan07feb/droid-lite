# Droid Lite for VS Code and Cursor

Android tools from [Droid Lite](https://github.com/narayan07feb/droid-lite), in VS Code, Cursor and other VS Code based editors. The Droid Lite engine is inside the extension: nothing else to install.

## Features

- **Compose preview** beside the editor (the preview icon at the top right of a `.kt` file): no Gradle run, drawn again on save, unchanged files at once from the cache (20 pictures, 30 minutes). The renderer comes from the Gradle cache, or is downloaded once (about 175 MB) with progress in the panel.
- **Code links** above Kotlin and Java declarations: `↓ Implemented in 2 classes`, `↑ Overrides Api.load`, `↑ Api`, `Actual in androidMain, iosMain`. Click one to jump, or to see the list.
- **Compose stability** like Droid Lite: a coloured badge after each `fun` line and parameter line (stable, unstable, runtime, unknown), the reasons on hover. It reads the Compose compiler reports when the project has them, else it guesses.
- **Device tools**: screenshot, screen recording and logcat. The status bar button shows the phone and opens every tool (it turns red while recording: click to stop and save); the Droid Lite icon in the left bar lists them in groups.
- **Pull requests**: installs GitHub Pull Requests with it, for "Created by me", "Waiting for my review" and side by side diffs.

The links follow unsaved edits. Needs `adb` for the device tools (`ANDROID_HOME`, the default Mac SDK folder, or the PATH). Captures go to the Desktop.

## Install

1. Download `droid-lite-darwin-arm64-<version>.vsix` (Mac with Apple Silicon).
   To build it yourself: `scripts/package-vsix.sh` in the Droid Lite repo.
2. In VS Code or Cursor: Extensions panel, `...`, **Install from VSIX...**. Or run `code --install-extension <file>` (Cursor: `cursor --install-extension <file>`).
3. Open the Android project folder and click **Trust**: in Restricted Mode the extension stays off.
4. After installing a newer version: ⌘⇧P → **Reload Window**.
