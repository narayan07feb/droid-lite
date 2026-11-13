# Droid Lite plugins

A plugin connects a test tool (or any checker) to the **Tests** tab. Droid Lite does not load
code: it runs the commands the plugin names and reads the JSON they print. Any language works.

## Add a plugin

1. Open the **Tests** tab and press **Add plugin…**.
2. Pick the test tool's folder. Droid Lite takes `plugin.json` from that folder, or from its
   `droid-lite-plugin/` folder.

That is all. Droid Lite links the folder into `~/.config/droid-lite/plugins/`, so a `git pull`
of the tool also updates its plugin. **Remove** in the tab drops only the link, never the folder.

**For tool authors:** keep the plugin in your repo as `droid-lite-plugin/plugin.json` with
`"dir": ".."`, and every user adds it by picking your repo's folder.

## Where plugins live

```
~/.config/droid-lite/plugins/<name>/plugin.json     # yours, for every project (Add plugin puts links here)
<project>/.droid-lite/plugins/<name>/plugin.json    # shared with the project's repo
```

Droid Lite reads them when a project opens, and again when you press ⟳ in the Tests tab.
A broken `plugin.json` shows as an error in the tab, with its path.

## plugin.json

```json
{
  "name": "My tests",
  "dir": "~/code/my-test-tool",
  "list":  ["python3", "{plugin}/plugin.py", "list"],
  "run":   ["python3", "{plugin}/plugin.py", "run", "{id}"],
  "watch": ["python3", "{plugin}/plugin.py", "watch"],
  "views":   [{"name": "Failures", "run": ["python3", "{plugin}/plugin.py", "failures"]}],
  "actions": [{"name": "Doctor",   "run": ["python3", "{plugin}/plugin.py", "doctor"]}],
  "links":   [{"name": "Dashboard", "url": "http://127.0.0.1:8080"}]
}
```

| Key | Needed | What it is |
|---|---|---|
| `name` | yes | The name in the tab |
| `dir` | no | Where the commands run. Relative to the plugin folder; `~/` is your home. Default: the plugin folder |
| `list` | no | Prints the cases (below). Runs when the project opens and on ⟳ |
| `run` | no | Runs one case or group. `{id}` is its id |
| `watch` | no | Runs while the Tests tab shows; prints the live state, one JSON line per change |
| `views` | no | Extra lists (failures, history, …), each a command that prints entries |
| `actions` | no | Buttons; each runs like `run` and reports the same way |
| `links` | no | Links that open in the browser |

Each command is an argv array, not a shell line, so spaces in an id are safe. In every argument:
`{plugin}` is the plugin folder, `{project}` the open project, `{id}` the case id. The command also
gets `DROID_LITE_PROJECT` in its environment. Its stdin is empty: never wait for typed input.

**Stop** sends `SIGTERM` to the command's process group, so the tools it started get it too. If
your plugin starts work in another session (a detached process), stop that work in a `SIGTERM`
handler.

## list: the cases

A JSON array. A case with `children` is a group. A case or group runs only when it has an `id`.

```json
[
  {"label": "Login", "id": "login", "children": [
    {"id": "login_ok", "label": "Valid user", "detail": "shown on hover",
     "file": "flows/login.yaml", "line": 3}
  ]}
]
```

`label` defaults to `id`. `file` is relative to the project (or absolute); with it, the case gets a
▶ in the editor gutter on `line` (1-based; 0 or no line means line 1). Click the label to open the
file. When one line has several cases, the gutter ▶ shows them in the list to choose.

## run and actions: JSON lines

Every stdout or stderr line is one of:

```json
{"case": "login_ok", "status": "running"}
{"case": "login_ok", "label": "Valid user", "status": "failed", "message": "Button 'Next' not found",
 "file": "flows/login.yaml", "line": 7, "url": "http://127.0.0.1:8080/failures/F-12"}
{"log": "a log line"}
any other text is a log line too
```

- `status`: `running`, `passed`, `failed` or `skipped`.
- A line with `case` sets that case's icon in the tree. Every line with a status except `running`
  is also listed under the run's results; a click opens its `file` at `line`, else its `url`.
- Results do not need a `case`: steps or sub-tests can report with only a `label`.
- When the command ends and the case it ran never got a final status, the exit code decides:
  0 is passed, anything else failed.

## watch: the live state

One JSON object per line, each time the state changes:

```json
{"title": "Login on Pixel 8", "status": "running", "rows": [["Screen", "OTP"], ["Step", "4 of 9"]]}
```

The newest line replaces the panel. Droid Lite starts `watch` when the Tests tab shows and stops
it when the tab hides, so it costs nothing in the background.

## views: lists

A JSON array of entries, with the same fields as a result line:
`label`, `status`, `message`, `file`, `line`, `url`.

## Example

[plugin-example/](plugin-example/) is a small shell plugin that shows every part. Copy it to
`~/.config/droid-lite/plugins/example/` to try it.
