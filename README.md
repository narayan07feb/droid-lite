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

You can also press **Open project…**, or drop a folder on the window. The last project opens again on the next start. To work on two projects at once, press **+** next to the project name, ⇧⌘N, or Help → **New window…**: it asks for a project and opens it in its own window (a separate Droid Lite process, with its own tabs and builds).

To get a normal Mac app with the icon, run `scripts/bundle-mac.sh`, then copy `target/release/Droid Lite.app` to `/Applications`.


## Features

| Area | What you get |
|------|--------------|
| Keymaps | **Eclipse (macOS)** by default, or Android Studio (macOS): Help → Keymap. Help → Keyboard shortcuts lists every key. Eclipse: ⇧⌘T open type, ⇧⌘R open file, ⌘O outline, ⌘3 search everywhere, ⌘E recent, F3 or ⌘-click declaration, ⇧⌘G references, ⌥⌘W locate file, ⌃H find in files, ⌘L go to line, ⌘[ / ⌘] back and forward, ⌥⌘R rename, ⌥⌘L extract variable, ⇧⌘O organize imports, ⌘/ comment, ⌘D delete line, ⌥⌘↓ duplicate, ⌥↑/↓ move line, ⌃Space complete, ⇧⌘N new window |
| Find action | ⇧⌘A lists every command by name (rename, logcat, run app, focus mode…) with its shortcut; Enter runs it |
| Panels and focus | ⌘1 Project, ⌘4 bottom panel, ⌘9 Compose / Pull request slide in and out (also the three buttons at the left of the status bar). ⇧⌘F12 focus mode hides them all; ⌘1 / ⌘4 / ⌘9 from focus mode opens just that one. ⌃R runs the app on the device, ⌃⇧R the Gradle task |
| Notes | When a build or a test run ends and its tab is not on screen, a short note slides in at the bottom right (✓ or the failure, with the time). Click it to open the result. It goes after 5 s |
| Welcome screen | With no project open: an Open button and the last 8 projects |
| Indent guides and sticky header | Thin lines at each indent level. When you scroll inside a block, the lines that open it (class, fun, `Column {`, an XML tag) stay at the top of the editor; click one to jump there. Both read the indentation, so they work for Kotlin, Java, Gradle scripts and XML |
| Look and motion | Android Studio's new UI: project, branch and navigation left, search in the middle, Gradle task and device run right. Tabs slide their selection, buttons and rows fade on hover, jumps glide to their line. Motion draws frames only while it moves, so an idle window costs nothing |
| Find / replace | ⌘F bar above the editor: case and whole-word toggles, Enter / ⇧Enter (Eclipse ⌘K / ⇧⌘K, Android Studio ⌘G / ⇧⌘G) step through matches, Replace and Replace all, all matches highlighted |
| Smart typing | Enter keeps the indent (one level more after `{` `(` `->`), brackets and quotes close themselves, typing a closer steps over it, Backspace removes an empty pair, the matching bracket is highlighted |
| ⌘ / Ctrl hover | Underlines the name and shows its declaration and "N usages in M files" (the declaration itself not counted) with the first usage lines |
| Links above declarations | No gutter icons: a row of small grey links sits above each declaration (above its annotations), like VS Code's CodeLens. "Implemented in 2 classes", "Implements / extends Repo", "Overrides Repo.load", "▶ Run test", "▶ Run preview on the device", "Launcher activity", Kotlin Multiplatform's "Actual in androidMain, iosMain" / "Expected in commonMain", and the Hilt links. Click one to jump, pick or run. The rows get their own space, so the code never moves sideways; they are laid out once per change, not every frame. Colour swatches sit after their line. Go to implementations: Eclipse ⌘T, Android Studio ⌥⌘B. Right-click menu on any name |
| Hilt links | On a `@Provides` / `@Binds` function or an `@Inject constructor` class: "Injected in N places" (the `@Inject constructor` parameters and `@Inject` fields of that type). On an `@Inject constructor` class: "Dependencies: Repo, Api" opens what provides them; on an `@Inject` field: "Provided by provideRepo" |
| Kotlin Multiplatform | "Actual in androidMain, iosMain" above an `expect` declaration (click: its `actual`s, each with its source set, e.g. `androidMain · …`, `iosMain · …`), "Expected in commonMain" above an `actual` (click: the `expect`). Members of an `expect class` count too. Go to implementations (Eclipse ⌘T, Android Studio ⌥⌘B) jumps the same way. `expect` / `actual` are soft keywords: a variable or enum entry named `actual` stays a name |
| Compose stability | Inline badge after each `@Composable` (✓ skippable / ⚠ N unstable / ? unknown), and a hint on every parameter: a stable / unstable / runtime chip at the end of its line, or a coloured underline when several parameters share a line. Hover for the reason |
| Compile errors | Build (Eclipse ⌘B, Android Studio ⌘F9) compiles the file's module; errors and warnings from any Gradle run show as underlines in the editor and in Problems (all files) |
| Pull request and Git changes | Right panel → Pull request: the files the branch / pull request changed against its base, uncommitted files (stage, unstage, commit), branch switch. Click a file name to open it in the editor; "Diff" shows its changes. The panel resizes freely: long branch and file names are shortened. On a PR branch it asks which PR this one is based on (open PRs from `gh`, or type `#123` or a branch), so a stacked PR shows only its own files; the choice is kept per branch. Revert on each file (asks first): an uncommitted file goes back to the last commit (a new file is deleted); a PR file goes back to the base version as an uncommitted change |
| Find references | ⌘-click (or F3 / ⌘B) on a declaration opens a usages dropdown at the cursor, like Android Studio: `File.kt:line` and the code per usage, ↑ ↓ Enter to open, Esc to close, "Open in Find panel" for the full list; a single usage opens straight away. Filters in the dropdown and the Find panel, saved with the settings: **Hide tests** (`test`, `androidTest`, `commonTest`, `iosTest`… source sets, `*-test` / `test-*` modules, `*Test` / `*Spec` files), **Hide imports**, **Hide XML**, and a path box (`feature/` keeps only those paths, `-android-test` hides them). Every code reference in the project (strings and comments skipped) plus XML layouts and the manifest, with a count per file. Only places that mean the same declaration count: a member counts where its receiver is declared as that class (or one above or below it: `dao: UserDao`, `val dao = UserDao()`, `by viewModels<T>()`) or inside that class, so `cache.insert()` is not a usage of `UserDao.insert`; a top-level function counts where it is imported or in its package; overrides and other declarations of the name do not count. The ⌘-hover count and the rename text search (without kotlin-lsp) follow the same rule; in rename, places that look like another declaration start unticked. kotlin-lsp adds what it finds, so a project it is still importing never shows an empty list |
| Project tree | Two views with a switch, remembered per project. **Android** (like Android Studio): per module `manifests`, `kotlin` with packages as dotted names, `res`, one group per source set (`kotlin (test)`, `kotlin (commonMain)`, `kotlin (iosMain)`). **Project**: the real folders without the `src/` level, single-child chains joined (`kotlin · com.app.feature`). Modules nest by Gradle path (`:feature:lending:data` → feature › lending › data), tests are green, platform source sets blue. **Filter files** at the top lists matching files with their folder |
| Locate file | ⌖ on the Project header, Eclipse ⌥⌘W, Android Studio ⌥F1, or right-click → Locate: opens the folders down to the open file in the Project tree and scrolls to it. The open file is always selected in the tree |
| Editing | Kotlin/Java colours, SF Mono, line numbers, current-line highlight, tabs, ⌘S save, ⌘W close tab |
| Autocomplete | Project symbols and keywords as you type (⌃Space to force). Adds the `import` for you. Type-aware items come from kotlin-lsp when it is installed |
| Library symbols | The project's dependencies (and theirs, from the `.pom` files) are read from the Gradle cache for Go to class, Go to symbol and autocomplete: full declarations from source jars, class names and Kotlin top-level functions from jars and aars. Each artifact is read once and kept in `~/Library/Caches/droid-lite/libs` (Gradle never changes a downloaded artifact), so the next start takes about 0.2 s instead of about 5 s on a large project. A changed file is read again. Delete that folder to start fresh |
| Compose analyser | The UI call tree of each composable. Parameter stability from skydoves' compose-stability-analyzer (`stabilityDump`) or Compose compiler reports, with a local guess when there is neither. Compose checks: `modifier` rules, naming, `mutableStateOf` without `remember`, effects without keys, `collectAsState` vs `collectAsStateWithLifecycle`, ViewModel parameters |
| Preview render | Runs the module's screenshot task (Google's Compose Preview Screenshot Testing, Paparazzi or Roborazzi) and shows the PNGs for the previews in the open file |
| Refactor | Rename (⇧F6) with a preview of every change; you can untick lines. kotlin-lsp does the rename when it runs, else a code-only text search (strings and comments are skipped). It also renames `Old.kt` when the class matches the file. Extract variable (⌥⌘V), optimize imports (⌃⌥O, keeps the order), and undo the last refactoring from the Refactor menu |
| Files on disk | The project folder is watched. An open file without edits reloads by itself; with edits you choose Reload or Keep mine. New and deleted files update the tree and the index. New stability dumps and compiler reports load by themselves |
| Git (GitLens-style) | Grey "author, time • summary" after the cursor's line; hover for the full commit, click for its diff. A blame column with author and age per block (Pull request tab → Blame column). The Pull request tab lists every commit that changed the open file (follows renames). Blame uses the editor text, so unsaved lines show "Not committed yet". Change bars next to the line numbers (green added, blue changed, red deleted): bright for changes not committed yet (click to revert one), dim for what this branch changed since its base, the parent branch or PR set in the Pull request tab (else develop / main), so committed PR work stays visible |
| Run on a device | Device picker, build variant picker (`debug`, `release`, custom build types × product flavours, read from the app module's build file and kept per project) and **▶ App** in the toolbar: installs the picked variant of the app module on the picked device (`:app:installDebug`, `:app:installFreeRelease`… with `ANDROID_SERIAL`), starts its launcher activity, and opens Logcat on `package:mine`. **Run task** next to it runs any Gradle task |
| Screenshot and screen recording | **📷** and **⏺** next to the device picker. A screenshot (`screencap`) or video (`screenrecord`, 3 minutes at most, no sound) of the picked device is saved to `~/Desktop` (or `DROID_LITE_CAPTURE_DIR`). A small window shows it, with **Copy** (the image to the clipboard) and **Show in Finder**. A recording is stopped by its own pid, pulled to the Mac, and only that one file is deleted from the device |
| Compose preview on a device | "▶ Run preview on the device" above each `@Preview`: installs the debug app and opens just that preview with `androidx.compose.ui.tooling.PreviewActivity`, like Android Studio's "Run preview". The app module needs `debugImplementation("androidx.compose.ui:ui-tooling")`; the message says so when it is missing |
| ▶ on JUnit tests | "▶ Run test" above each `@Test` function and each class with tests, in `test` / `androidTest` source sets. Unit tests run `:module:test<Variant>UnitTest --tests com.app.FooTest.method` (plain `test` in a non-Android module); `androidTest` runs `connected<Variant>AndroidTest` for that class or method on the picked device. When the Build tab is not on screen, a note says "Tests passed" or "2 tests failed" |
| Quick fixes (⌥Enter) | On a Compose check: wrap `mutableStateOf` in `remember { }`, add `modifier: Modifier = Modifier` (as the first optional parameter, before a trailing lambda), default a Modifier parameter, give an effect the key `Unit`, use `collectAsStateWithLifecycle()`. Also: convert `{ return x }` to an expression body, import a class or function the file does not know yet (from the project or a library), and extract a string resource: a string in a composable becomes `stringResource(R.string.x)` (with `import <namespace>.R` when needed), a `text` / `hint` / `contentDescription` attribute in XML becomes `@string/x`; the string goes into the module's `src/main/res/values/strings.xml`. Each fix adds its imports. Also in the right-click menu |
| Live templates | In Kotlin files, type the name and press Tab (or pick it in the completion list): `comp` @Composable function, `prev` @Preview, `vm` ViewModel, `rem` `var x by remember { mutableStateOf() }`, `le` LaunchedEffect, `col` / `row` / `box`, `lazy` LazyColumn with `items`, `logd`, `tag`, `toast`. The cursor goes where the name goes, and the imports are added |
| New Kotlin file | ⌘N, the **+** on the Project header, or right-click a file in the tree → New Kotlin file here: Composable screen (with a preview), ViewModel (with a UI state class), class, data class, interface, object, enum, Activity (Compose; also added to the module's manifest), Fragment, or an empty file. The package comes from the folder |
| Android icons | Files in the tree, on tabs and in the filter list show what they hold: Activity, Fragment, ViewModel, Service, Receiver, Application, Room DAO / database, composables, tests, interface, object |
| Manifest links | A link above each Activity, Service, Receiver, Provider or Application class that a manifest declares ("Launcher activity" for the one with the launcher intent filter). Click it to open its entry in `AndroidManifest.xml` |
| Crash note | While Logcat streams (its tab is picked, even when the bottom panel is hidden), a crash of your app (`FATAL EXCEPTION`) shows a note: "App crashed: NullPointerException: … at Home.kt:42". Click it to open that line |
| Lint and detekt in Problems | After a Gradle run (`lint`, `detekt`, or any task that runs them), the reports it wrote (`build/reports/lint-results-*.xml`, `build/reports/detekt/*.xml`) show in Problems and as underlines, as `[lint HardcodedText] …` / `[detekt LongMethod] …`, errors and warnings apart. Only reports written by that run are read, so old ones never come back |
| Translations | Project menu → Translations…, or the action palette: every string resource × every `values-xx` folder in one table, with a filter and "Only missing". Missing ones are red; `translatable="false"` strings show "not translated". Click a cell to edit it; Enter saves it in that folder's `strings.xml` (a new entry goes into `res/values-xx/strings.xml` next to the default one). `'`, `&` and `<` are escaped for you |
| Emulators | The device picker lists your Android Virtual Devices (read in the background with `emulator -list-avds`); click one to start it. `DROID_LITE_EMULATOR` picks another emulator binary |
| Deep links | Device picker → Open a deep link…, or the action palette: the links from the manifests' `VIEW` intent filters (`demo://home`, `https://demo.app/item`), or any link you type, opened in the app on the picked device (`am start -W -a VIEW -d '…' <package>`) |
| Logcat | Bottom **Logcat** tab, like Android Studio: device picker (USB or `adb connect`), live log with the last 2,000 lines, columns time / pid-tid / tag / app / level, Android Studio colours. Filter: `package:mine` (the project's `applicationId`s), `package:com.x`, `level:warn`, `tag:OkHttp`, `-tag:chatty`, free text. Pause, Scroll lock, Clear. Click a stack-trace line (`at …(File.kt:42)`) to open the file at that line. Keeps 50,000 lines. `adb` runs only while the tab is open, and never clears or resizes the phone's log buffer (Clear clears only the view), so other tools reading the same phone are not disturbed. `DROID_LITE_ADB` picks another adb |
| Gradle | Run any task (`assembleDebug`, `installDebug`, …), see the output live, and click `e:` compiler errors to open them |
| Performance | Bottom **Performance** tab: a graph of the last 240 frame times (lines at 16.7 ms and 100 ms; hover a bar for its slowest part), the median and slowest frame, the app's memory (as Activity Monitor shows it), how long the project took to open, and the time of each part of the last frame. A frame over 100 ms is also reported in the status bar |
| Test plugins | Bottom **Tests** tab for any test tool that follows the plugin format ([docs/PLUGINS.md](docs/PLUGINS.md)): its cases as a tree with a filter and ▶ per case or group, live pass / fail, results that open their file or link, the run's log, a live state panel (for example the phone and screen of a device test), extra views (failures, …), action buttons and links. A "▶ Run …" link also shows above lines a case names. To add one, press **Add plugin…** in the Tests tab and pick the tool's folder (its `plugin.json` or `droid-lite-plugin/` folder); a project can also ship plugins in `.droid-lite/plugins/<name>/` |

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
- `watch.rs`: the folder watcher.
- `gradle.rs`, `lsp.rs`: Gradle runs and the LSP client.
- `plugins.rs`, `app/tests_ui.rs`: test plugins (the contract is in [docs/PLUGINS.md](docs/PLUGINS.md)) and the Tests tab.
- `app.rs`, `app/`: the window, the refactoring UI and the reload logic.

`cargo test -- --test-threads=1` runs the unit tests and end-to-end tests. The end-to-end tests drive the real
app with key presses and clicks (egui_kittest) on a fixture project with git history, a fake `gradlew` and a fake
Kotlin LSP (`tests/fixtures/fake_lsp.py`). They cover navigation, completion with imports, save, the Compose panel,
preview render, Gradle errors, rename/extract/optimize imports and undo, reload on disk change, git blame and history,
LSP completion/diagnostics/rename, test plugins (add, run from the list and the links above lines, views, actions, live state, remove), and a memory test that repeats real work 15 times and fails if the live heap grows.

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

`DROID_LITE_SHOT=out.png` saves a window screenshot once indexing is done, then quits.
Use it with `DROID_LITE_OPEN=<file>`, `DROID_LITE_SEARCH=<query>` or `DROID_LITE_BLAME=1` for UI checks.

## Limits (v0.1)

- Live preview while you type is not built. The preview path renders through Gradle (a few seconds).
- Without kotlin-lsp there is no member completion after `.`, and there are no compile errors in the editor.
- No debugger, Layout Inspector or emulator manager yet.
- Rename without kotlin-lsp matches by name, so two different symbols with the same name both show up in the preview. Untick the wrong ones.
- Optimize imports does not sort imports. This avoids fights with ktlint's import order.
