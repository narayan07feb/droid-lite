# Droid Lite for VS Code and Cursor

Android tools from [Droid Lite](https://github.com/narayan07feb/droid-lite) in VS Code, Cursor and
other VS Code based editors (Windsurf, Antigravity). The Droid Lite engine is inside the extension
(about 8 MB): nothing else to install. It is the same engine as the Droid Lite app, so a fix in one
shows up in both.

## Install

1. Get `droid-lite-darwin-arm64-<version>.vsix` (Mac with Apple Silicon). To build it yourself, run
   `scripts/package-vsix.sh` in the Droid Lite repo: it builds the engine for this Mac, runs the
   extension tests and writes the file into `vscode/`. It refuses a target that is not this Mac.
2. Install it:
   - VS Code or Cursor: Extensions panel → `...` → **Install from VSIX...** → pick the file.
   - Terminal: `code --install-extension <file>` (Cursor: `cursor --install-extension <file>`).
3. Open the Android project folder (the one with `settings.gradle(.kts)`) and click **Trust** when
   asked. In Restricted Mode VS Code keeps the extension off, because it runs the engine and `adb`.
4. After installing a newer version: ⌘⇧P → **Reload Window**.

## Where to find things

| Place | What is there |
|---|---|
| **Droid Lite icon** (left bar) → **Tools** | Compose preview, Screenshot, Record screen / Stop recording, Show logcat / Stop logcat, Index the project again |
| **Droid Lite icon** → **Test plugins** | Per test plugin: its **Tests** (cases with ▶ / ■), live state, views, actions, links, Allow… / Remove; **Add test plugin…** |
| **Testing view** (flask icon, left bar) | The same test cases, VS Code style: run, stop, results, ▶ in the editor gutter |
| **Status bar** (bottom left) | The connected phone; click for every tool. Red while recording: click to stop and save |
| **Editor title** (top right of a `.kt` file) | The preview icon: opens the Compose Preview panel beside the code |
| **Command Palette** (⌘⇧P) | Every command, starting with "Droid Lite:" |

## Features

### Compose preview

- Opens beside the editor and draws every `@Preview` of the open file: Light and Dark, font
  scales, screen sizes and the other multipreviews, like Android Studio.
- `@PreviewParameter` previews draw one card per value of the provider (at most 20).
- No Gradle run. A warm preview worker compiles the file and the project code it needs, and draws
  with layoutlib (the renderer Android Studio uses). First picture about 20–30 s, later ones a few
  seconds. The panel shows each step.
- Drawn again on save, and when you switch to another `.kt` file while the panel is visible.
- **Cache**: an unchanged file shows its pictures at once. At most 20 pictures in all, none older
  than 30 minutes. Any change to the file, the project code it uses, its resources or the renderer
  draws it again.
- **Renderer**: 16 files (14 jars and layoutlib's data), taken from the Gradle cache when they are
  there, else downloaded once in the background (about 175 MB, checksums checked, resumes after a
  drop). Offline with pieces missing, the card names them. On disk: about 230–290 MB.
- The project's own libraries come from the Gradle cache, so the project must have been built once.
- Code that only Gradle generates (data binding, view binding, BuildConfig, KSP) cannot compile in
  the preview; the card shows the error.
- At most 2 preview workers run (about 1 GB each while drawing); an idle one exits after 10 minutes.

### Code links

Above Kotlin and Java declarations: `↓ Implemented in 2 classes`, `↑ Overrides Api.load`, `↑ Api`,
`Actual in androidMain, iosMain`. Click one to jump, or to pick from the list. They follow unsaved
edits.

### Compose stability

A coloured badge after each composable's `fun` line and each parameter line, like the Droid Lite
app: green stable, red unstable, orange runtime, grey unknown, and `✓ skippable`. Hover for the
reasons. It reads the Compose compiler reports when the project has them, else it guesses (and
says so).

### Device tools

- **Screenshot** and **screen recording** of the connected phone (or a pick list when several are
  connected). Files go to the Desktop. A recording stays on the phone until a full, size-checked
  copy is on the Mac.
- **Logcat** in the Output panel ("Logcat"); the newest 20000 lines are kept.
- Needs `adb`: `ANDROID_HOME`, the default Mac SDK folder, or the PATH. The status bar checks the
  phone every 20 s, only while the window has focus.

### Test tools (test plugins)

A test plugin connects a test tool, for example a Maestro e2e tool, in the
[Droid Lite plugin format](https://github.com/narayan07feb/droid-lite/blob/master/docs/PLUGINS.md).
The same plugins work in the Droid Lite app.

- **Where plugins come from**: `~/.config/droid-lite/plugins/` (yours, for every project; the
  folder follows `DROID_LITE_CONFIG_DIR`) and `<project>/.droid-lite/plugins/` (shipped with a
  project). A plugin added in the Droid Lite app shows here too, and the other way round.
- **Add**: Droid Lite icon → Test plugins → **Add test plugin…** → pick the tool's folder (with
  `plugin.json` or a `droid-lite-plugin/` folder). **Remove** drops only the link, never the folder.
- **Cases** show in two places, using one runner: the **Testing view** (flask icon, with ▶ in the
  editor gutter on a case's file) and the Droid Lite sidebar under each plugin's **Tests (N)**, as
  in the Droid Lite app: ▶ and ■ on each case and group that has an id, the last result's icon,
  the failure on hover, and a click opens the case's file.
- **Run all** with nothing picked does nothing on purpose: device tests are long, so pick a case or
  a group.
- Stop ends the whole process group (SIGTERM, then SIGKILL after 5 s). Output is capped (20000
  lines, about 8 MB per run, 64 KB per line).
- **Live state, views and actions** (for example Failures, Changes, Doctor) are in the sidebar;
  views can be refreshed, actions can be stopped. Links open only `http` and `https` URLs.

#### Project plugins need your OK once

A plugin that comes with a project runs commands from a repo you may have just cloned, so nothing
of it runs until you allow it:

- It shows as **Needs your OK**. **Allow…** shows its folder and every command it would run.
- The OK covers every file in the plugin's folder (a hash of names, sizes, modes and bytes), for
  this project only. Any change to a file there asks again, and the OK is checked again before
  every command.
- Refused: a plugin folder that links outside the project, contains links, or is too big to check
  (more than 2000 files or 8 MB).
- Allowing trusts this plugin's folder. Its commands may also run project code outside it (for
  example a script in the repo); changes to that code do not ask again.
- OKs are kept in `~/.config/droid-lite/approved-plugins.json`, shared with the Droid Lite app.
- Plugins you add yourself (`~/.config/droid-lite/plugins/`) need no OK.

### Pull requests

The extension brings GitHub Pull Requests with it, for "Created by me", "Waiting for my review",
checking out a PR and side by side diffs.

## Commands

| Command | What it does |
|---|---|
| Droid Lite: Tools… | Opens the tools menu (same as the status bar button) |
| Droid Lite: Compose Preview | Opens the Compose Preview panel beside the editor |
| Droid Lite: Device Screenshot | Saves a screenshot of the phone to the Desktop |
| Droid Lite: Start / Stop Screen Recording | Records the phone's screen; stop saves the video to the Desktop |
| Droid Lite: Show / Stop Logcat | Streams the phone's log into the "Logcat" output |
| Droid Lite: Index the Project Again | Restarts the engine and indexes the project from scratch |
| Droid Lite: Add Test Plugin… | Links a test tool's plugin folder (shared with the Droid Lite app) |
| Run / Stop, Allow…, Refresh, Remove | On test cases, plugins and views in the sidebar (right-click or the inline buttons) |

## Settings

| Setting | Default | What it does |
|---|---|---|
| `droidLite.enginePath` | empty | Another `droid-lite` binary to use (machine setting only) |
| `droidLite.logcatHistory` | 500 | Logcat lines shown from before the stream starts |
| `droidLite.logcatMaxLines` | 20000 | Most logcat lines kept; older lines are cleared |

Recommended for Android projects: turn off VS Code's own Gradle import, which starts a Gradle
daemon of several GB on every folder open. In your user settings:

```json
"java.import.gradle.enabled": false,
"gradle.autoDetect": "off"
```

## Troubleshooting

| What you see | Why, and what to do |
|---|---|
| Nothing from Droid Lite at all | The folder is not trusted (Restricted Mode): click **Restricted Mode** in the status bar → **Trust** |
| A new version does not show | ⌘⇧P → **Reload Window** |
| No test cases | Open the Testing view (flask icon) or Droid Lite → Test plugins → the plugin's **Tests**. A project plugin shows **Needs your OK** until you allow it |
| `.kt` files show as "Plain Text" | VS Code has no Kotlin language without a Kotlin extension. Droid Lite works anyway (it goes by file name); add a Kotlin extension only for syntax colours |
| A preview card says what is missing | Build the project once with Gradle (libraries come from the Gradle cache), or connect to the internet for the one-time renderer download |
| "Skipped: mac-guard says memory is critical" | The Mac is short of memory or disk, so no preview worker is started. Free some and save the file again |
| The first preview is slow | The first draw compiles the project code the file needs (20–30 s); later draws take seconds |

## Limits

- Mac with Apple Silicon only, for now.
- Live preview while typing is not built: previews draw on open and on save.
- Each request to the engine has a time limit (for example 15 s for links and badges, 3 minutes
  for a preview, extended while it reports progress). An engine that does not answer is restarted.
