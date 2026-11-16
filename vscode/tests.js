// Droid Lite test plugins (docs/PLUGINS.md) in VS Code's Test Explorer: the same plugin.json files
// the app reads (~/.config/droid-lite/plugins/<name>/ and <project>/.droid-lite/plugins/<name>/),
// so a plugin added in either one shows in both. Cases come from `list`, runs from `run` (JSON
// lines), extra lists (`views`), buttons (`actions`), `links` and the live state (`watch`) go in
// the Droid Lite sidebar.
const vscode = require('vscode');
const cp = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { EventEmitter } = require('events');
const { PassThrough } = require('stream');

const userDir = () => path.join(os.homedir(), '.config', 'droid-lite', 'plugins');
const MAX_LOG = 20000; // lines kept per run in the test output
const MAX_LOG_CHARS = 8 << 20; // and characters (about 8 MB) in all
const MAX_LINE = 64 << 10; // longest line kept (chars); the rest of a longer one is dropped
const limits = { json: 120000, grace: 500, kill: 5000 }; // ms: list/view time, output after exit, TERM -> KILL
const procs = new Set(); // every plugin process group still running (runs, actions, watch, list, views)

// plugin.json of `dir`, or of its droid-lite-plugin/ folder
function findPluginJson(dir) {
  return [path.join(dir, 'plugin.json'), path.join(dir, 'droid-lite-plugin', 'plugin.json')]
    .find((p) => fs.existsSync(p));
}

// why a plugin.json cannot be used, or undefined: every command is a list of strings (a plain
// string would fail later, inside a run or a watcher), links open only http(s) pages
function problem(spec) {
  const argv = (v) => Array.isArray(v) && v.length > 0 && v.every((a) => typeof a === 'string');
  if (!spec || typeof spec !== 'object' || Array.isArray(spec)) return 'not a JSON object';
  if (typeof spec.name !== 'string' || !spec.name) return 'name must be a string';
  if (spec.dir !== undefined && typeof spec.dir !== 'string') return 'dir must be a string';
  for (const k of ['list', 'run', 'watch']) {
    if (spec[k] !== undefined && !argv(spec[k])) return `${k} must be a list of strings`;
  }
  for (const k of ['views', 'actions', 'links']) {
    if (spec[k] === undefined) continue;
    if (!Array.isArray(spec[k]) || !spec[k].every((x) => x && typeof x === 'object' && typeof x.name === 'string')) {
      return `${k} must be a list of objects with a name`;
    }
    for (const x of spec[k]) {
      if (k === 'links' && !(typeof x.url === 'string' && /^https?:\/\//i.test(x.url))) return `links: ${x.name}: url must be an http or https link`;
      if (k !== 'links' && !argv(x.run)) return `${k}: ${x.name}: run must be a list of strings`;
    }
  }
}

// true while a run may still show one more line (MAX_LOG lines, MAX_LOG_CHARS characters)
function budget() {
  let n = 0;
  let chars = 0;
  return (line) => ++n <= MAX_LOG && (chars += line.length) <= MAX_LOG_CHARS;
}

// every plugin: { name, folder, cwd, spec } or { error, folder }; `folder` (its real path) is its
// id, two plugins may share a name
function readPlugins(project) {
  const roots = [userDir(), project && path.join(project, '.droid-lite', 'plugins')].filter(Boolean);
  const out = [];
  for (const root of roots) {
    let names = [];
    try { names = fs.readdirSync(root); } catch { continue; }
    for (const n of names.sort()) {
      const folder = path.join(root, n);
      const file = path.join(folder, 'plugin.json');
      if (!fs.existsSync(file)) continue;
      let real = folder;
      try {
        real = fs.realpathSync(folder);
        const spec = JSON.parse(fs.readFileSync(file, 'utf8'));
        const bad = problem(spec);
        if (bad) throw new Error(bad);
        const dir = spec.dir ? spec.dir.replace(/^~\//, `${os.homedir()}/`) : '.';
        out.push({ name: spec.name, folder: real, cwd: path.resolve(real, dir), spec });
      } catch (e) {
        out.push({ error: `${file}: ${e.message}`, folder: real });
      }
    }
  }
  return out;
}

// an argv with {plugin}, {project} and {id} filled in
function argv(cmd, plugin, project, id = '') {
  return cmd.map((a) => a.replaceAll('{plugin}', plugin.folder).replaceAll('{project}', project || '')
    .replaceAll('{id}', id));
}

// runs a command in its own process group (Stop reaches the tools it started), stdin empty.
// `onEnd(code, error)` runs once: after the exit plus a short grace for the last output (a tool
// it left running may hold the pipes), or when it could not start.
function spawn(cmd, plugin, project, onEnd = () => {}) {
  const [bin, ...args] = Array.isArray(cmd) ? cmd.map(String) : [];
  let proc;
  try {
    proc = cp.spawn(bin, args, {
      cwd: plugin.cwd, detached: true, stdio: ['ignore', 'pipe', 'pipe'],
      env: { ...process.env, DROID_LITE_PROJECT: project || '' },
    });
  } catch (err) { // no command at all: fails like a missing one
    proc = Object.assign(new EventEmitter(), { stdout: new PassThrough(), stderr: new PassThrough() });
    proc.stdout.end();
    proc.stderr.end();
    process.nextTick(() => { proc.emit('error', err); proc.emit('close'); });
  }
  let ended = false;
  const end = (code, err) => { if (!ended) { ended = true; onEnd(code, err); } };
  procs.add(proc);
  proc.on('close', () => procs.delete(proc));
  proc.on('error', (err) => end(undefined, err));
  proc.on('exit', (code, signal) => setTimeout(() => {
    proc.stdout.destroy();
    proc.stderr.destroy();
    end(code ?? signal);
  }, limits.grace));
  return proc;
}

// SIGTERM to the whole group, SIGKILL after limits.kill if any of it is still there. `final` (the
// extension goes away): a group already sent SIGTERM gets SIGKILL now, and the SIGKILL timer of
// the others keeps the host's event loop up until it fires (an unref'd one is lost when the host exits)
function stop(proc, final = false) {
  if (!proc.pid) return;
  const signal = (s) => { try { process.kill(-proc.pid, s); return true; } catch { return false; /* ended */ } };
  if (final && proc.stopping) { signal('SIGKILL'); return; }
  if (!signal('SIGTERM')) return;
  proc.stopping = true;
  const t = setTimeout(() => signal('SIGKILL'), limits.kill);
  if (!final) t.unref();
}

// lines of a stream; a line longer than MAX_LINE is cut there (memory stays bounded)
function lines(stream, onLine) {
  let buf = '';
  let cut = false; // dropping the rest of a long line
  stream.setEncoding('utf8');
  stream.on('data', (d) => {
    const parts = (buf + d).split(/\r?\n/);
    buf = parts.pop();
    for (const l of parts) {
      if (!cut) onLine(l);
      cut = false;
    }
    if (buf.length > MAX_LINE) {
      if (!cut) onLine(`${buf.slice(0, MAX_LINE)} … [line cut]`);
      buf = '';
      cut = true;
    }
  });
  stream.on('end', () => { if (buf && !cut) onLine(buf); });
}

// stdout of a command as JSON, with a time limit that ends its whole process group
function json(cmd, plugin, project, ms = limits.json) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    let why; // set when it was stopped
    const proc = spawn(cmd, plugin, project, (code, err) => {
      clearTimeout(timer);
      if (err || why) return reject(err || new Error(why));
      try { resolve(JSON.parse(Buffer.concat(chunks).toString('utf8'))); } catch {
        reject(new Error(code ? `exit code ${code}` : 'no JSON from the plugin'));
      }
    });
    const timer = setTimeout(() => { why = `no answer in ${ms / 1000} s`; stop(proc); }, ms);
    proc.stdout.on('data', (d) => {
      size += d.length;
      if (size <= 32 << 20) chunks.push(d);
      else if (!why) { why = 'more than 32 MB of output'; stop(proc); }
    });
    proc.stderr.resume();
  });
}

// a place in a file, or none when `file` or `line` is not usable
function location(project, file, line) {
  if (typeof file !== 'string' || !file) return undefined;
  const n = line === undefined || line === null ? 1 : Number(line);
  if (!Number.isFinite(n)) return undefined;
  const uri = vscode.Uri.file(path.isAbsolute(file) ? file : path.join(project || '', file));
  return new vscode.Location(uri, new vscode.Position(Math.max(Math.floor(n) - 1, 0), 0));
}

// projectOf(): the project folder, read again on each load
function setup(ctx, projectOf) {
  let project = projectOf();
  const ctrl = vscode.tests.createTestController('droidLite.tests', 'Droid Lite tests');
  const sidebar = new vscode.EventEmitter();
  let plugins = [];
  const meta = new Map(); // test item id -> { plugin, caseId }
  // keyed by the plugin's folder (its id), never its name
  const live = new Map(); // plugin -> latest watch state
  const pluginRows = new Map(); // plugin -> its sidebar row (to refresh only that row)
  const viewCache = new Map(); // plugin + view name -> its entries until that view is refreshed
  const actions = new Map(); // plugin + action name -> its running process
  let watchers = [];
  let watching = false; // the sidebar shows
  const fireRow = (folder) => { const r = pluginRows.get(folder); if (r) sidebar.fire(r); };

  const addCases = (plugin, parent, cases) => {
    for (const c of cases || []) {
      const id = `${plugin.folder}\u0000${c.id ?? c.label}`;
      const loc = location(project, c.file, c.line);
      const item = ctrl.createTestItem(id, c.label || c.id, loc?.uri);
      if (loc) item.range = new vscode.Range(loc.range.start, loc.range.start);
      item.description = c.detail;
      meta.set(id, { plugin, caseId: c.id });
      parent.add(item);
      addCases(plugin, item.children, c.children);
    }
  };

  async function load() {
    project = projectOf();
    plugins = readPlugins(project);
    meta.clear();
    ctrl.items.replace([]);
    await Promise.all(plugins.map(async (p) => {
      const label = p.name || path.basename(p.folder);
      const root = ctrl.createTestItem(`plugin:${p.folder}`, label);
      ctrl.items.add(root);
      if (p.error) { root.error = p.error; return; }
      if (!p.spec.list) return;
      root.busy = true;
      try {
        addCases(p, root.children, await json(argv(p.spec.list, p, project), p, project));
      } catch (e) {
        root.error = `list: ${e.message}`;
      }
      root.busy = false;
    }));
    viewCache.clear();
    for (const folder of live.keys()) if (!plugins.some((p) => p.folder === folder)) live.delete(folder);
    sidebar.fire();
    if (watching) startWatch(); // plugins came or went
  }

  // one run of one case or group: JSON lines set the tree's icons and the results
  function runOne(run, item, token) {
    const { plugin: p, caseId } = meta.get(item.id) || {};
    return new Promise((resolve) => {
      if (!p?.spec.run || !caseId) { run.skipped(item); return resolve(); }
      try { start(resolve); } catch (e) { run.errored(item, new vscode.TestMessage(e.message)); resolve(); }
    });
    function start(resolve) {
      run.started(item);
      const byCase = new Map();
      const walk = (i) => { const c = meta.get(i.id)?.caseId; if (c) byCase.set(c, i); i.children.forEach(walk); };
      walk(item);
      const ended = new Set();
      const room = budget(); // every line counts, status lines too: a flooding plugin cannot grow memory
      let told = false;
      let cancel;
      const proc = spawn(argv(p.spec.run, p, project, caseId), p, project, (code, err) => {
        cancel.dispose(); // a later cancel never signals a pid that is free again
        if (err) run.errored(item, new vscode.TestMessage(err.message));
        // the exit code decides for the case it ran when no final status came
        else if (!ended.has(item)) {
          if (token.isCancellationRequested) run.skipped(item);
          else if (code === 0) run.passed(item);
          else run.failed(item, new vscode.TestMessage(`exit code ${code}`));
        }
        resolve();
      });
      cancel = token.onCancellationRequested(() => stop(proc));
      const onLine = (line) => {
        let m;
        try { m = JSON.parse(line); } catch { m = { log: line }; }
        if (typeof m !== 'object' || m === null) m = { log: line };
        const full = !room(line);
        if (full && !told) {
          told = true;
          run.appendOutput(`[more than ${MAX_LOG} lines or ${MAX_LOG_CHARS >> 20} MB: the rest is not shown]\r\n`);
        }
        if (m.log !== undefined || !m.status) {
          if (!full) run.appendOutput(`${m.log ?? line}\r\n`);
          return;
        }
        const target = byCase.get(m.case) || (m.case ? undefined : item);
        const text = [m.label, m.message].filter(Boolean).join(': ');
        if (text && !full) run.appendOutput(`[${m.status}] ${text}${m.url ? ` ${m.url}` : ''}\r\n`, location(project, m.file, m.line), target);
        if (!target || !m.case) return; // a step's result: output only
        if (full && ended.has(target)) return; // past the limit a case ends once
        if (m.status === 'running') run.started(target);
        else if (m.status === 'passed') { run.passed(target); ended.add(target); }
        else if (m.status === 'skipped') { run.skipped(target); ended.add(target); }
        else if (m.status === 'failed') {
          const msg = new vscode.TestMessage(String(m.message || 'failed'));
          msg.location = location(project, m.file, m.line);
          run.failed(target, msg);
          ended.add(target);
        }
      };
      lines(proc.stdout, onLine);
      lines(proc.stderr, onLine);
    }
  }

  ctrl.createRunProfile('Run', vscode.TestRunProfileKind.Run, async (request, token) => {
    // device tests are long and drive a phone: run only what was picked, never "everything"
    if (!request.include?.length) {
      vscode.window.showInformationMessage('Pick a case or group in the Test Explorer to run it.');
      return;
    }
    const run = ctrl.createTestRun(request);
    try {
      for (const item of request.include) {
        if (token.isCancellationRequested) break;
        await runOne(run, item, token);
      }
    } finally {
      run.end(); // else the Test Explorer shows it running forever
    }
  }, true);
  ctrl.refreshHandler = load;

  // live state while the Droid Lite sidebar shows, one `watch` per plugin; a line redraws only
  // that plugin's row
  function startWatch() {
    stopWatch();
    for (const p of plugins.filter((x) => x.spec?.watch)) {
      try { // one plugin that cannot watch does not stop the others
        const proc = spawn(argv(p.spec.watch, p, project), p, project);
        lines(proc.stdout, (l) => {
          let state;
          try { state = JSON.parse(l); } catch { return; /* not a state line */ }
          if (state && typeof state === 'object') { live.set(p.folder, state); fireRow(p.folder); }
        });
        proc.stderr.resume();
        watchers.push(proc);
        // an ended watcher is never signalled: its group id may belong to another process by then
        proc.on('close', () => { watchers = watchers.filter((w) => w !== proc); });
      } catch (e) {
        live.set(p.folder, { title: 'Live', status: `watch: ${e.message}` });
      }
    }
  }
  function stopWatch() {
    watchers.forEach(stop);
    watchers = [];
  }

  // sidebar: per plugin its live state, views, actions and links
  const tree = {
    onDidChangeTreeData: sidebar.event,
    getTreeItem: (e) => e,
    async getChildren(node) {
      const item = (label, icon, command, args, description) => {
        const i = new vscode.TreeItem(label);
        i.iconPath = new vscode.ThemeIcon(icon);
        if (command) i.command = { command, title: label, arguments: args };
        i.description = description;
        return i;
      };
      if (!node) {
        const rows = plugins.filter((p) => p.error).map((p) => item(p.error, 'warning'));
        rows.push(...plugins.filter((p) => p.spec).map((p) => {
          const g = new vscode.TreeItem(p.name, vscode.TreeItemCollapsibleState.Expanded);
          g.plugin = p;
          pluginRows.set(p.folder, g);
          return g;
        }));
        rows.push(item('Add test plugin…', 'add', 'droidLite.addTestPlugin'));
        rows.push(item('Open Test Explorer', 'beaker', 'workbench.view.testing.focus'));
        return rows;
      }
      if (node.plugin && !node.view) {
        const p = node.plugin;
        const out = [];
        const state = live.get(p.folder);
        if (state) {
          const stateRows = (Array.isArray(state.rows) ? state.rows : []).map((r) => (Array.isArray(r) ? r : [r]).map(String));
          const s = item(String(state.title || 'Live'), 'pulse', undefined, undefined, state.status);
          s.tooltip = stateRows.map((r) => r.join(': ')).join('\n');
          out.push(s);
          for (const r of stateRows) out.push(item(r[0], 'blank', undefined, undefined, r[1]));
        }
        for (const v of p.spec.views || []) {
          const i = new vscode.TreeItem(v.name, vscode.TreeItemCollapsibleState.Collapsed);
          i.iconPath = new vscode.ThemeIcon('list-unordered');
          i.contextValue = 'droidLiteView'; // its inline Refresh button
          i.plugin = p; i.view = v;
          out.push(i);
        }
        for (const a of p.spec.actions || []) {
          out.push(actions.has(`${p.folder}\u0000${a.name}`)
            ? item(`Stop ${a.name}`, 'debug-stop', 'droidLite.stopTestAction', [p, a], 'running')
            : item(a.name, 'play', 'droidLite.testAction', [p, a]));
        }
        for (const l of p.spec.links || []) out.push(item(l.name, 'link-external', 'vscode.open', [vscode.Uri.parse(l.url)]));
        out.push(item('Remove', 'trash', 'droidLite.removeTestPlugin', [p]));
        return out;
      }
      if (node.view) {
        const key = `${node.plugin.folder}\u0000${node.view.name}`;
        if (!viewCache.has(key)) viewCache.set(key, json(argv(node.view.run, node.plugin, project), node.plugin, project));
        try {
          const entries = await viewCache.get(key);
          if (!Array.isArray(entries)) throw new Error('not a JSON list');
          return entries.map((e) => {
            const icon = { failed: 'error', passed: 'pass', skipped: 'circle-slash', running: 'sync' }[e.status] || 'circle-small';
            const loc = location(project, e.file, e.line);
            const args = loc ? [loc.uri, { selection: loc.range }] : e.url ? [vscode.Uri.parse(e.url)] : undefined;
            const i = item(e.label || '', icon, args ? 'vscode.open' : undefined, args, e.message);
            i.tooltip = [e.label, e.message, e.url].filter(Boolean).join('\n');
            return i;
          });
        } catch (e) {
          return [item(`${node.view.name}: ${e.message}`, 'warning')];
        }
      }
      return [];
    },
  };
  const view = vscode.window.createTreeView('droidLite.testPlugins', { treeDataProvider: tree });
  watching = Boolean(view.visible);

  const out = vscode.window.createOutputChannel('Droid Lite tests');
  ctx.subscriptions.push(
    ctrl, view, out,
    view.onDidChangeVisibility((e) => { watching = e.visible; if (watching) startWatch(); else stopWatch(); }),
    // every process group a plugin started ends with the extension
    { dispose: () => { watching = false; procs.forEach((p) => stop(p, true)); } },
    vscode.commands.registerCommand('droidLite.testAction', (p, a) => {
      const key = `${p.folder}\u0000${a.name}`;
      out.show(true);
      if (actions.has(key)) return out.appendLine(`${a.name} is still running`);
      out.appendLine(`▶ ${p.name}: ${a.name}`);
      const room = budget();
      const proc = spawn(argv(a.run, p, project), p, project, (code, err) => {
        actions.delete(key);
        out.appendLine(err ? `■ ${a.name} could not start: ${err.message}` : `■ ${a.name} ended (exit code ${code})`);
        fireRow(p.folder);
      });
      actions.set(key, proc);
      fireRow(p.folder);
      const log = (l) => { if (room(l)) out.appendLine(l); };
      lines(proc.stdout, log);
      lines(proc.stderr, log);
    }),
    vscode.commands.registerCommand('droidLite.stopTestAction', (p, a) => {
      const proc = actions.get(`${p.folder}\u0000${a.name}`);
      if (proc) stop(proc);
    }),
    vscode.commands.registerCommand('droidLite.refreshTestView', (node) => {
      viewCache.delete(`${node.plugin.folder}\u0000${node.view.name}`);
      sidebar.fire(node);
    }),
    // Add: link the tool's plugin folder into the shared folder the app reads too
    vscode.commands.registerCommand('droidLite.addTestPlugin', async () => {
      const [dir] = await vscode.window.showOpenDialog({
        canSelectFolders: true, canSelectFiles: false, openLabel: 'Add plugin',
        title: "Pick the test tool's folder (with plugin.json or droid-lite-plugin/)",
      }) || [];
      if (!dir) return;
      const file = findPluginJson(dir.fsPath);
      if (!file) return vscode.window.showErrorMessage(`No plugin.json in ${dir.fsPath} or its droid-lite-plugin/ folder.`);
      let name;
      try { name = JSON.parse(fs.readFileSync(file, 'utf8')).name; } catch (e) { return vscode.window.showErrorMessage(`${file}: ${e.message}`); }
      const link = path.join(userDir(), String(name || path.basename(dir.fsPath)).toLowerCase().replace(/[^a-z0-9]+/g, '-'));
      fs.mkdirSync(userDir(), { recursive: true });
      if (fs.existsSync(link)) return vscode.window.showWarningMessage(`${name} is already added.`);
      fs.symlinkSync(path.dirname(file), link);
      await load();
      vscode.window.showInformationMessage(`Added ${name}: its cases are in the Test Explorer.`);
    }),
    // Remove drops only the link, never the tool's folder
    vscode.commands.registerCommand('droidLite.removeTestPlugin', async (p) => {
      let names = [];
      try { names = fs.readdirSync(userDir()); } catch { /* no user plugins: it comes with the project */ }
      const links = names.map((n) => path.join(userDir(), n))
        .filter((l) => { try { return fs.realpathSync(l) === p.folder; } catch { return false; } });
      if (!links.length) return vscode.window.showInformationMessage(`${p.name} comes with the project (.droid-lite/plugins): remove it there.`);
      const ok = await vscode.window.showWarningMessage(`Remove ${p.name}? Only the link goes; the tool's folder stays.`, { modal: true }, 'Remove');
      if (ok !== 'Remove') return;
      links.forEach((l) => fs.unlinkSync(l));
      await load();
    }),
  );
  load();
  return { load, readPlugins };
}

module.exports = { setup, readPlugins, argv, findPluginJson, location, limits, procs };
