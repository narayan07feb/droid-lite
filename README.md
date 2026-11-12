# Droid Lite

A light, Mac-first IDE for Android and Jetpack Compose code, written in Rust (egui).
It opens any Gradle Android project folder and indexes it in about a second:
a large app (578 modules, 11.7k files) takes 0.6 s.

## Run

```sh
cargo run --release -- /path/to/android/project
```

You can also press **Open project…**, or drop a folder on the window. The last project opens again on the next start.

## Features

| Area | What you get |
|------|--------------|
| Navigation | ⌘O class, ⇧⌘O file, ⌥⌘O symbol, ⇧⇧ search everywhere, ⌘E recent files, ⌘F12 file structure, ⌘B or ⌘-click go to declaration, ⌘[ / ⌘] back and forward, ⌥F7 find usages, ⇧⌘F find in files |
| Editing | Kotlin/Java colours, line numbers, tabs, ⌘S save, ⌘W close tab |
| Autocomplete | Project symbols and keywords as you type (⌃Space to force). Adds the `import` for you. Type-aware items come from kotlin-lsp when it is installed |
| Compose analyser | The UI call tree of each composable. Parameter stability from skydoves' compose-stability-analyzer (`stabilityDump`) or Compose compiler reports, with a local guess when there is neither. Compose checks: `modifier` rules, naming, `mutableStateOf` without `remember`, effects without keys, `collectAsState` vs `collectAsStateWithLifecycle`, ViewModel parameters |
| Preview render | Runs the module's screenshot task (Google's Compose Preview Screenshot Testing, Paparazzi or Roborazzi) and shows the PNGs for the previews in the open file |
| Gradle | Run any task (`assembleDebug`, `installDebug`, …), see the output live, and click `e:` compiler errors to open them |

## Type-aware completion (optional)

```sh
brew install JetBrains/utils/kotlin-lsp
```

Droid Lite starts `kotlin-lsp --stdio` (or `kotlin-language-server`) when it finds one.
Set `DROID_LITE_LSP="cmd args"` to use another server. Without a server, completion and
go-to-declaration use the built-in index.

## How it works

- `lexer.rs`, `kotlin.rs`: a token scanner for Kotlin declarations. The tree-sitter Kotlin grammars misread
  `@Preview(...) private fun`, which Compose code uses everywhere, so the scanner works on tokens and brace depth.
- `index.rs`: the symbol index, fuzzy search (nucleo) and definition lookup.
- `compose.rs`: the UI tree, Compose checks and stability.
- `gradle.rs`, `lsp.rs`: Gradle runs and the LSP client.
- `app.rs`: the window.

`cargo test` runs the unit tests. `DROID_LITE_SHOT=out.png` saves a window screenshot once indexing is done, then quits.
Use it with `DROID_LITE_OPEN=<file>` or `DROID_LITE_SEARCH=<query>` for UI checks.

## Limits (v0.1)

- Live preview while you type is not built. The preview path renders through Gradle (a few seconds).
- Without kotlin-lsp there is no member completion after `.`, and there are no compile errors in the editor.
- No debugger, Layout Inspector, emulator manager or refactorings yet.
- The editor reloads a file only when you open it again. It does not watch files that change on disk.
