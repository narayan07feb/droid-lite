# Droid Lite for VS Code and Cursor

Android tools from [Droid Lite](https://github.com/narayan07feb/droid-lite), in VS Code, Cursor and other VS Code based editors. The Droid Lite engine is inside the extension: nothing else to install.

## Features

- **Code links** above Kotlin and Java declarations: `↓ Implemented in 2 classes`, `↑ Overrides Api.load`, `↑ Api`, `Actual in androidMain, iosMain`. Click one to jump, or to see the list.
- **Compose stability** above each composable: `✓ skippable`, `⚠ 1 unstable: names`. Click it for each parameter and the reason. It reads the Compose compiler reports when the project has them, else it guesses.
- **Device tools** (Command Palette, "Droid Lite"): screenshot, screen recording (a status bar button stops it), logcat in the Output panel.
- **Pull requests**: installs GitHub Pull Requests with it, for "Created by me", "Waiting for my review" and side by side diffs.

The links follow unsaved edits. Needs `adb` for the device tools (`ANDROID_HOME`, the default Mac SDK folder, or the PATH). Captures go to the Desktop.

## Install

1. Download `droid-lite-darwin-arm64-<version>.vsix` (Mac with Apple Silicon).
2. In VS Code or Cursor: Extensions panel, `...`, **Install from VSIX...**. Or run `code --install-extension <file>` (Cursor: `cursor --install-extension <file>`).
