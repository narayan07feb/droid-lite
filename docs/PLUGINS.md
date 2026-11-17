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

> **Warning: commands run in the plugin folder unless you set `dir`.** Any file a command writes
> to its working folder (a log, a cache, `.pytest_cache`, a lock) lands in the plugin folder. For
> a project plugin that is a change to the folder: what runs stops and the plugin asks for the OK
> again. Set `"dir"` (for example `".."` or the project) or write to an absolute path outside the
> plugin folder.

## Where plugins live

```
~/.config/droid-lite/plugins/<name>/plugin.json     # yours, for every project (Add plugin puts links here)
<project>/.droid-lite/plugins/<name>/plugin.json    # shared with the project's repo
```

Droid Lite reads them when a project opens, and again when you press ⟳ in the Tests tab.
A broken `plugin.json` shows as an error in the tab, with its path.

## Project plugins need your OK once

A plugin that comes with a project (`<project>/.droid-lite/plugins/`) runs commands from a repo you
may have just cloned, so nothing of it runs until you allow it. The Tests tab (and VS Code's Test
Explorer and Droid Lite sidebar) shows it as **Needs your OK**, with its folder and every command
it would run; **Allow** runs it from then on.

- **The OK is for one project.** Allowing a plugin in one project does not allow it in another,
  even when both reach the same plugin folder.
- **The OK is for the whole plugin folder.** A change to any file in it (`plugin.json`, a script,
  a new or deleted file, a changed permission such as `chmod +x`) asks again. Every file counts,
  hidden ones too (`.DS_Store` included): nothing in the folder is left out.
- **The OK is checked before every command, and while one runs.** Droid Lite reads the folder again
  right before each command the plugin runs (`list`, `run`, `watch`, a view, an action), and every
  few seconds while one of them still runs. When it changed since you allowed it, nothing new runs
  and what still runs (a `watch`, a run, an action) stops: you see "changed since you allowed it;
  allow it again" and the plugin shows **Needs your OK** again. When the folder can no longer be
  checked at all (below), the plugin shows the error instead, with no OK to give. A file that is
  gone or cannot be read for a moment while the folder is read (an editor's save, a `git
  checkout`), or something other than a file in a file's place, counts as a change: it asks for the
  OK again, it is not an error. A folder whose every file still looks the same as at the last full
  read (path, size, times, inode, mode) is not read again, so a check while nothing changed is
  quick.
- **Nothing of a plugin not allowed is clickable.** Its links do not show until you allow it.
- **Not for code outside the folder.** Allowing trusts this plugin's folder. Its commands may also
  run project code outside it (for example a script in the repo, or `dir` pointing elsewhere);
  changes to that code do not ask again.

Some project plugins are refused (shown as an error in the tab, never runnable, never approvable):

- a name in its folder that is not UTF-8 ("is not a UTF-8 name");

- its folder links outside the project's `.droid-lite/plugins` folder, or the project's
  `.droid-lite` or `.droid-lite/plugins` folder links outside the project ("links outside the
  project");
- a link (symlink) anywhere inside its folder ("contains links");
- more than 2000 files or 8 MiB in its folder ("too big to check").

Plugins you add yourself (`~/.config/droid-lite/plugins/`, **Add plugin…**) need no OK.

**For authors of project plugins:** every file in the plugin folder is part of the OK. Keep the
plugin's own state, cache and log files outside its folder (for example under the project's build
folder, or `~/Library/Caches/<your tool>`). A plugin that writes into its own folder changes it, so
it is stopped, refused at its next command and asked about again on every load. Every command of a
project plugin runs with `PYTHONDONTWRITEBYTECODE=1`, so Python writes no `__pycache__` into it; a
tool that writes other files there (or Finder's `.DS_Store`) still changes the folder.

The OKs are kept in `approved-plugins.json` in the config folder (`$DROID_LITE_CONFIG_DIR` when it
is set and not empty, else `~/.config/droid-lite`), shared by the app and the VS Code extension:

```json
{"version": 2, "approved": {"<project real path>": {"<plugin folder real path>": "<folder hash>"}}}
```

Real paths are the paths on disk with links resolved. The folder hash is SHA-256 (lowercase hex)
over every regular file in the plugin folder, sorted by path relative to it (`/` between folders,
compared as UTF-8 bytes); for each file: its path, one zero byte, its length as 8 bytes big-endian,
its permission bits (`mode & 0o777`) as 2 bytes big-endian, then its bytes. (OKs saved before the
permission bits were part of the hash ask again once.) `"version"` is the number 2 (`2.0` counts
too, the text `"2"` does not). A file of any other shape (the older flat map too) holds no OKs, so
you are asked again, and the first **Allow** replaces it. A file that is not a JSON object is never overwritten:
fix or delete it. Delete an entry to be asked again.

A write takes the lock file `approved-plugins.json.lock` next to it: created only when it does not
exist, with the holder's token in it (its process id and a random number), tried again with a short
pause for about 5 s, and removed after the write only while it still holds that token. A lock file
more than 30 s old (or more than 30 s in the future: a clock set back) was left by a process that
stopped: it is looked at again, renamed to a name of its own, then deleted only when it is still
that same file. When it turns out to be another's fresh lock, it is put back; when that is not
possible (a newer lock took its place), it is left under its `.stale.` name, never deleted. So two
processes never delete each other's fresh lock. Anything else in its place (a folder, a link) counts as held and is left alone.

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
| `dir` | no | Where the commands run. Relative to the plugin folder's real path (links resolved, then `..` taken by its text); `~/` is your home. Default: the plugin folder (see the warning above: files written there change a project plugin) |
| `list` | no | Prints the cases (below). Runs when the project opens and on ⟳ |
| `run` | no | Runs one case or group. `{id}` is its id |
| `watch` | no | Runs while the Tests tab shows; prints the live state, one JSON line per change |
| `views` | no | Extra lists (failures, history, …), each a command that prints entries |
| `actions` | no | Buttons; each runs like `run` and reports the same way |
| `links` | no | Links that open in the browser: `http://` and `https://` only (the app drops any other; the VS Code extension shows the plugin.json as an error) |

Each command is an argv array, not a shell line, so spaces in an id are safe. In every argument:
`{plugin}` is the plugin folder's real path (links resolved), `{project}` the open project, `{id}`
the case id. A program that starts with `~/` is in your home folder. The command also
gets `DROID_LITE_PROJECT` in its environment. Its stdin is empty: never wait for typed input.

**Stop** sends `SIGTERM` to the command's process group, so the tools it started get it too, and
`SIGKILL` 5 s later if any of it is still there. If your plugin starts work in another session (a
detached process), stop that work in a `SIGTERM` handler. `list` and a view get 120 s to answer;
then their process group is killed.

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
  is also listed under the run's results; a click opens its `file` at `line`, else its `url`
  (`http` or `https` only).
- Results do not need a `case`: steps or sub-tests can report with only a `label`.
- When the command ends and the case it ran never got a final status, the exit code decides:
  0 is passed, anything else failed.

## watch: the live state

One JSON object per line, each time the state changes:

```json
{"title": "Login on Pixel 8", "status": "running", "rows": [["Screen", "OTP"], ["Step", "4 of 9"]]}
```

The newest line replaces the panel. Droid Lite starts `watch` when the Tests tab shows and stops
it when the tab hides, so it costs nothing in the background. A `watch` that ends is started again
after a pause (1 s, then doubled each time it ends soon, up to 30 s).

## views: lists

A JSON array of entries, with the same fields as a result line:
`label`, `status`, `message`, `file`, `line`, `url`.

## Example

[plugin-example/](plugin-example/) is a small shell plugin that shows every part. Copy it to
`~/.config/droid-lite/plugins/example/` to try it.
