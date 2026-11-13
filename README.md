# Droid Lite

A light, Mac-first IDE for Android and Jetpack Compose code, written in Rust (egui).
It opens any Gradle Android project folder and indexes it in about a second:
a large app (578 modules, 11.7k files) takes 0.6 s.

## Install (Apple Silicon Mac)

1. Download `Droid-Lite-<version>-arm64.zip` from [Releases](https://github.com/narayan07feb/droid-lite/releases/latest).
2. Unzip it and drag **Droid Lite.app** to **Applications**.
3. The app is not signed with an Apple Developer ID, so macOS blocks the first open. Run this once in Terminal:

   ```sh
   xattr -dr com.apple.quarantine "/Applications/Droid Lite.app"
   ```

4. Open Droid Lite from Launchpad or Applications.

For a newer version, replace the app and run step 3 again.

## Run from source

```sh
cargo run --release -- /path/to/android/project
```

You can also press **Open project…**, or drop a folder on the window. The last project opens again on the next start.

To get a normal Mac app with the icon, run `scripts/bundle-mac.sh`, then copy `target/release/Droid Lite.app` to `/Applications`.

On macOS 26 and newer the window uses Liquid Glass: the side panels and bars are see-through, and the editor stays almost opaque.
Set `DROID_LITE_GLASS=0` for solid colours.

## Features

| Area | What you get |
|------|--------------|
| Keymaps | **Eclipse (macOS)** by default, or Android Studio (macOS): Help → Keymap. Help → Keyboard shortcuts lists every key. Eclipse: ⇧⌘T open type, ⇧⌘R open file, ⌘O outline, ⌘3 search everywhere, ⌘E recent, F3 or ⌘-click declaration, ⇧⌘G references, ⌥⌘W locate file, ⌃H find in files, ⌘L go to line, ⌘[ / ⌘] back and forward, ⌥⌘R rename, ⌥⌘L extract variable, ⇧⌘O organize imports, ⌘/ comment, ⌘D delete line, ⌥⌘↓ duplicate, ⌥↑/↓ move line, ⌃Space complete |
| Find / replace | ⌘F bar above the editor: case and whole-word toggles, Enter / ⇧Enter (Eclipse ⌘K / ⇧⌘K, Android Studio ⌘G / ⇧⌘G) step through matches, Replace and Replace all, all matches highlighted |
| Smart typing | Enter keeps the indent (one level more after `{` `(` `->`), brackets and quotes close themselves, typing a closer steps over it, Backspace removes an empty pair, the matching bracket is highlighted |
| ⌘ / Ctrl hover | Underlines the name and shows its declaration and "N usages in M files" (the declaration itself not counted) with the first usage lines |
| Gutter | Android Studio style icons: implemented-by (green, down) and implements / overrides (blue, up); click to jump or pick. Go to implementations: Eclipse ⌘T, Android Studio ⌥⌘B. Right-click menu on any name |
| Kotlin Multiplatform | Purple **E** gutter icon on an `expect` declaration (click: its `actual`s, each with its source set, e.g. `androidMain · …`, `iosMain · …`), **A** on an `actual` (click: the `expect`). Members of an `expect class` count too. Go to implementations (Eclipse ⌘T, Android Studio ⌥⌘B) jumps the same way. `expect` / `actual` are soft keywords: a variable or enum entry named `actual` stays a name |
| Compose stability | Inline badge after each `@Composable` (✓ skippable / ⚠ N unstable / ? unknown), and a hint on every parameter: a stable / unstable / runtime chip at the end of its line, or a coloured underline when several parameters share a line. Hover for the reason |
| Compile errors | Build (Eclipse ⌘B, Android Studio ⌘F9) compiles the file's module; errors and warnings from any Gradle run show as underlines in the editor and in Problems (all files) |
| Pull request and Git changes | Right panel → Pull request: the files the branch / pull request changed against its base, uncommitted files (stage, unstage, commit), branch switch. Click a file name to open it in the editor; "Diff" shows its changes. The panel resizes freely: long branch and file names are shortened. On a PR branch it asks which PR this one is based on (open PRs from `gh`, or type `#123` or a branch), so a stacked PR shows only its own files; the choice is kept per branch. Revert on each file (asks first): an uncommitted file goes back to the last commit (a new file is deleted); a PR file goes back to the base version as an uncommitted change |
| Find references | ⌘-click (or F3 / ⌘B) on a declaration opens a usages dropdown at the cursor, like Android Studio: `File.kt:line` and the code per usage, ↑ ↓ Enter to open, Esc to close, "Open in Find panel" for the full list; a single usage opens straight away. Filters in the dropdown and the Find panel, saved with the settings: **Hide tests** (`test`, `androidTest`, `commonTest`, `iosTest`… source sets, `*-test` / `test-*` modules, `*Test` / `*Spec` files), **Hide imports**, **Hide XML**, and a path box (`feature/` keeps only those paths, `-android-test` hides them). Every code reference in the project (strings and comments skipped) plus XML layouts and the manifest, with a count per file. kotlin-lsp adds what it finds, so a project it is still importing never shows an empty list |
| Project tree | Two views with a switch, remembered per project. **Android** (like Android Studio): per module `manifests`, `kotlin` with packages as dotted names, `res`, one group per source set (`kotlin (test)`, `kotlin (commonMain)`, `kotlin (iosMain)`). **Project**: the real folders without the `src/` level, single-child chains joined (`kotlin · com.app.feature`). Modules nest by Gradle path (`:feature:lending:data` → feature › lending › data), tests are green, platform source sets blue. **Filter files** at the top lists matching files with their folder |
| Locate file | ⌖ on the Project header, Eclipse ⌥⌘W, Android Studio ⌥F1, or right-click → Locate: opens the folders down to the open file in the Project tree and scrolls to it. The open file is always selected in the tree |
| Editing | Kotlin/Java colours, SF Mono, line numbers, current-line highlight, tabs, ⌘S save, ⌘W close tab |
| Autocomplete | Project symbols and keywords as you type (⌃Space to force). Adds the `import` for you. Type-aware items come from kotlin-lsp when it is installed |
| Library symbols | The project's dependencies (and theirs, from the `.pom` files) are read from the Gradle cache for Go to class, Go to symbol and autocomplete: full declarations from source jars, class names and Kotlin top-level functions from jars and aars. Each artifact is read once and kept in `~/Library/Caches/droid-lite/libs` (Gradle never changes a downloaded artifact), so the next start takes about 0.2 s instead of about 5 s on a large project. A changed file is read again. Delete that folder to start fresh |
| Compose analyser | The UI call tree of each composable. Parameter stability from skydoves' compose-stability-analyzer (`stabilityDump`) or Compose compiler reports, with a local guess when there is neither. Compose checks: `modifier` rules, naming, `mutableStateOf` without `remember`, effects without keys, `collectAsState` vs `collectAsStateWithLifecycle`, ViewModel parameters |
| Preview render | Runs the module's screenshot task (Google's Compose Preview Screenshot Testing, Paparazzi or Roborazzi) and shows the PNGs for the previews in the open file |
| Refactor | Rename (⇧F6) with a preview of every change; you can untick lines. kotlin-lsp does the rename when it runs, else a code-only text search (strings and comments are skipped). It also renames `Old.kt` when the class matches the file. Extract variable (⌥⌘V), optimize imports (⌃⌥O, keeps the order), and undo the last refactoring from the Refactor menu |
| Files on disk | The project folder is watched. An open file without edits reloads by itself; with edits you choose Reload or Keep mine. New and deleted files update the tree and the index. New stability dumps and compiler reports load by themselves |
| Git (GitLens-style) | Grey "author, time • summary" after the cursor's line; hover for the full commit, click for its diff. A blame column with author and age per block (Pull request tab → Blame column). The Pull request tab lists every commit that changed the open file (follows renames). Blame uses the editor text, so unsaved lines show "Not committed yet" |
| Run on a device | Device picker and **▶ App** in the toolbar: installs the debug build of the app module on the picked device (`:app:installDebug` with `ANDROID_SERIAL`), starts its launcher activity, and opens Logcat on `package:mine`. **Run task** next to it runs any Gradle task |
| Screenshot and screen recording | **📷** and **⏺** next to the device picker. A screenshot (`screencap`) or video (`screenrecord`, 3 minutes at most, no sound) of the picked device is saved to `~/Desktop` (or `DROID_LITE_CAPTURE_DIR`). A small window shows it, with **Copy** (the image to the clipboard) and **Show in Finder**. A recording is stopped by its own pid, pulled to the Mac, and only that one file is deleted from the device |
| Compose preview on a device | A green **▶** in the gutter next to each `@Preview`: installs the debug app and opens just that preview with `androidx.compose.ui.tooling.PreviewActivity`, like Android Studio's "Run preview". The app module needs `debugImplementation("androidx.compose.ui:ui-tooling")`; the message says so when it is missing |
| Logcat | Bottom **Logcat** tab, like Android Studio: device picker (USB or `adb connect`), live log with the last 2,000 lines, columns time / pid-tid / tag / app / level, Android Studio colours. Filter: `package:mine` (the project's `applicationId`s), `package:com.x`, `level:warn`, `tag:OkHttp`, `-tag:chatty`, free text. Pause, Scroll lock, Clear. Click a stack-trace line (`at …(File.kt:42)`) to open the file at that line. Keeps 50,000 lines. `adb` runs only while the tab is open, and never clears or resizes the phone's log buffer (Clear clears only the view), so other tools reading the same phone are not disturbed. `DROID_LITE_ADB` picks another adb |
| Gradle | Run any task (`assembleDebug`, `installDebug`, …), see the output live, and click `e:` compiler errors to open them |
| Performance | Bottom **Performance** tab: a graph of the last 240 frame times (lines at 16.7 ms and 100 ms; hover a bar for its slowest part), the median and slowest frame, the app's memory (as Activity Monitor shows it), how long the project took to open, and the time of each part of the last frame. A frame over 100 ms is also reported in the status bar |

## kotlin-lsp (live errors, type-aware completion)

Droid Lite starts JetBrains' kotlin-lsp when it finds one (`kotlin-lsp` on the PATH, in
`/opt/homebrew/bin`, or in `~/.local/bin`), with its heap capped (Settings, default 2g).
`DROID_LITE_LSP="cmd args"` picks another server.

Install with `brew install JetBrains/utils/kotlin-lsp`. If Homebrew refuses (it needs the newest
Xcode command line tools), install the standalone build instead:

```sh
# from https://github.com/Kotlin/kotlin-lsp/releases: the macOS arm64 .sit (a zip), checksum checked
unzip kotlin-server-<version>-aarch64.sit -d ~/.local/share/kotlin-lsp
ln -sf ~/.local/share/kotlin-lsp/kotlin-server-<version>/bin/intellij-server ~/.local/bin/kotlin-lsp
```

## How it works

- `lexer.rs`, `kotlin.rs`: a token scanner for Kotlin declarations. The tree-sitter Kotlin grammars misread
  `@Preview(...) private fun`, which Compose code uses everywhere, so the scanner works on tokens and brace depth.
- `index.rs`: the symbol index, fuzzy search (nucleo) and definition lookup.
- `compose.rs`: the UI tree, Compose checks and stability.
- `refactor.rs`: rename search, optimize imports, extract variable.
- `git.rs`: blame, history and commit diffs from the `git` command.
- `watch.rs`: the folder watcher. `glass.rs`: Liquid Glass.
- `gradle.rs`, `lsp.rs`: Gradle runs and the LSP client.
- `app.rs`, `app/`: the window, the refactoring UI and the reload logic.

`cargo test -- --test-threads=1` runs the unit tests and end-to-end tests. The end-to-end tests drive the real
app with key presses and clicks (egui_kittest) on a fixture project with git history, a fake `gradlew` and a fake
Kotlin LSP (`tests/fixtures/fake_lsp.py`). They cover navigation, completion with imports, save, the Compose panel,
preview render, Gradle errors, rename/extract/optimize imports and undo, reload on disk change, git blame and history,
LSP completion/diagnostics/rename, and a memory test that repeats real work 15 times and fails if the live heap grows.

## Performance checks

`droid-lite --bench <project>` times opening, indexing and each search
(`--features memstats` adds the memory each step keeps).

`scripts/perf_check.py <old-ref> [<new-ref>]` compares two versions: it builds both (kept in `target/perf`),
runs `--bench` on your project several times with the order swapped each round, and fails when a step is
more than 15 % and 2 ms slower, or keeps more than 5 % and 2 MB more memory.

Run it on every push with the pre-push hook:

```sh
git config core.hooksPath scripts/hooks
git config droidlite.perfProject /path/to/android/project
```

The hook checks only pushes that change `src`, `Cargo.*` or `assets`, and skips new branches.
`git push --no-verify` skips it once.

`DROID_LITE_SHOT=out.png` saves a window screenshot once indexing is done, then quits. `DROID_LITE_SHOT=out.png` saves a window screenshot once indexing is done, then quits.
Use it with `DROID_LITE_OPEN=<file>`, `DROID_LITE_SEARCH=<query>` or `DROID_LITE_BLAME=1` for UI checks.

## Limits (v0.1)

- Live preview while you type is not built. The preview path renders through Gradle (a few seconds).
- Without kotlin-lsp there is no member completion after `.`, and there are no compile errors in the editor.
- No debugger, Layout Inspector or emulator manager yet.
- Rename without kotlin-lsp matches by name, so two different symbols with the same name both show up in the preview. Untick the wrong ones.
- Optimize imports does not sort imports. This avoids fights with ktlint's import order.
