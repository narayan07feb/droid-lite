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
const readline = require('readline');

const userDir = () => path.join(os.homedir(), '.config', 'droid-lite', 'plugins');
const MAX_LOG = 20000; // lines kept per run in the test output

// plugin.json of `dir`, or of its droid-lite-plugin/ folder
function findPluginJson(dir) {
  return [path.join(dir, 'plugin.json'), path.join(dir, 'droid-lite-plugin', 'plugin.json')]
    .find((p) => fs.existsSync(p));
}

// every plugin: { name, folder, cwd, spec } or { error, folder }
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
      try {
        const spec = JSON.parse(fs.readFileSync(file, 'utf8'));
        if (!spec.name) throw new Error('no "name"');
        const real = fs.realpathSync(folder);
        const dir = spec.dir ? spec.dir.replace(/^~\//, `${os.homedir()}/`) : '.';
        out.push({ name: spec.name, folder: real, cwd: path.resolve(real, dir), spec });
      } catch (e) {
        out.push({ error: `${file}: ${e.message}`, folder });
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

// runs a command in its own process group (Stop reaches the tools it started), stdin empty
function spawn(cmd, plugin, project) {
  const [bin, ...args] = cmd;
  return cp.spawn(bin, args, {
    cwd: plugin.cwd, detached: true, stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env, DROID_LITE_PROJECT: project || '' },
  });
}

function stop(proc) {
  try { process.kill(-proc.pid, 'SIGTERM'); } catch { /* already ended */ }
}

// stdout of a command as JSON, with a time limit
function json(cmd, plugin, project, ms = 120000) {
  return new Promise((resolve, reject) => {
    const [bin, ...args] = cmd;
    cp.execFile(bin, args, {
      cwd: plugin.cwd, timeout: ms, maxBuffer: 32 << 20,
      env: { ...process.env, DROID_LITE_PROJECT: project || '' },
    }, (err, stdout) => {
      try { resolve(JSON.parse(stdout)); } catch { reject(err || new Error('no JSON from the plugin')); }
    });
  });
}

function location(project, file, line) {
  if (!file) return undefined;
  const uri = vscode.Uri.file(path.isAbsolute(file) ? file : path.join(project || '', file));
  return new vscode.Location(uri, new vscode.Position(Math.max((line || 1) - 1, 0), 0));
}

function setup(ctx, project) {
  const ctrl = vscode.tests.createTestController('droidLite.tests', 'Droid Lite tests');
  const sidebar = new vscode.EventEmitter();
  let plugins = [];
  const meta = new Map(); // test item id -> { plugin, caseId }
  const live = new Map(); // plugin name -> latest watch state
  let watchers = [];

  const addCases = (plugin, parent, cases) => {
    for (const c of cases || []) {
      const id = `${plugin.name}\u0000${c.id ?? c.label}`;
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
    plugins = readPlugins(project);
    meta.clear();
    ctrl.items.replace([]);
    await Promise.all(plugins.map(async (p) => {
      const label = p.name || path.basename(p.folder);
      const root = ctrl.createTestItem(`plugin:${label}`, label);
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
    sidebar.fire();
  }

  // one run of one case or group: JSON lines set the tree's icons and the results
  function runOne(run, item, token) {
    const { plugin: p, caseId } = meta.get(item.id) || {};
    return new Promise((resolve) => {
      if (!p?.spec.run || !caseId) { run.skipped(item); return resolve(); }
      run.started(item);
      const byCase = new Map();
      const walk = (i) => { const c = meta.get(i.id)?.caseId; if (c) byCase.set(c, i); i.children.forEach(walk); };
      walk(item);
      const ended = new Set();
      let lines = 0;
      const proc = spawn(argv(p.spec.run, p, project, caseId), p, project);
      token.onCancellationRequested(() => stop(proc));
      const onLine = (line) => {
        let m;
        try { m = JSON.parse(line); } catch { m = { log: line }; }
        if (typeof m !== 'object' || m === null) m = { log: line };
        if (m.log !== undefined || !m.status) {
          if (lines++ < MAX_LOG) run.appendOutput(`${m.log ?? line}\r\n`);
          return;
        }
        const target = byCase.get(m.case) || (m.case ? undefined : item);
        const text = [m.label, m.message].filter(Boolean).join(': ');
        if (text) run.appendOutput(`[${m.status}] ${text}${m.url ? ` ${m.url}` : ''}\r\n`, location(project, m.file, m.line), target);
        if (!target || !m.case) return; // a step's result: output only
        if (m.status === 'running') run.started(target);
        else if (m.status === 'passed') { run.passed(target); ended.add(target); }
        else if (m.status === 'skipped') { run.skipped(target); ended.add(target); }
        else if (m.status === 'failed') {
          const msg = new vscode.TestMessage(m.message || 'failed');
          msg.location = location(project, m.file, m.line);
          run.failed(target, msg);
          ended.add(target);
        }
      };
      readline.createInterface({ input: proc.stdout }).on('line', onLine);
      readline.createInterface({ input: proc.stderr }).on('line', onLine);
      proc.on('error', (e) => { run.errored(item, new vscode.TestMessage(e.message)); resolve(); });
      proc.on('close', (code) => {
        // the exit code decides for the case it ran when no final status came
        if (!ended.has(item)) {
          if (token.isCancellationRequested) run.skipped(item);
          else if (code === 0) run.passed(item);
          else run.failed(item, new vscode.TestMessage(`exit code ${code}`));
        }
        resolve();
      });
    });
  }

  ctrl.createRunProfile('Run', vscode.TestRunProfileKind.Run, async (request, token) => {
    // device tests are long and drive a phone: run only what was picked, never "everything"
    if (!request.include?.length) {
      vscode.window.showInformationMessage('Pick a case or group in the Test Explorer to run it.');
      return;
    }
    const run = ctrl.createTestRun(request);
    for (const item of request.include) {
      if (token.isCancellationRequested) break;
      await runOne(run, item, token);
    }
    run.end();
  }, true);
  ctrl.refreshHandler = load;

  // live state while the Droid Lite sidebar shows, one `watch` per plugin
  function startWatch() {
    stopWatch();
    for (const p of plugins.filter((x) => x.spec?.watch)) {
      const proc = spawn(argv(p.spec.watch, p, project), p, project);
      readline.createInterface({ input: proc.stdout }).on('line', (l) => {
        try { live.set(p.name, JSON.parse(l)); sidebar.fire(); } catch { /* not a state line */ }
      });
      proc.stderr.resume();
      watchers.push(proc);
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
        const rows = plugins.filter((p) => p.spec).map((p) => {
          const g = new vscode.TreeItem(p.name, vscode.TreeItemCollapsibleState.Expanded);
          g.plugin = p;
          return g;
        });
        rows.push(item('Add test plugin…', 'add', 'droidLite.addTestPlugin'));
        rows.push(item('Open Test Explorer', 'beaker', 'workbench.view.testing.focus'));
        return rows;
      }
      if (node.plugin && !node.view) {
        const p = node.plugin;
        const out = [];
        const state = live.get(p.name);
        if (state) {
          const s = item(state.title || 'Live', 'pulse', undefined, undefined, state.status);
          s.tooltip = (state.rows || []).map((r) => r.join(': ')).join('\n');
          out.push(s);
          for (const r of state.rows || []) out.push(item(r[0], 'blank', undefined, undefined, r[1]));
        }
        for (const v of p.spec.views || []) {
          const i = new vscode.TreeItem(v.name, vscode.TreeItemCollapsibleState.Collapsed);
          i.iconPath = new vscode.ThemeIcon('list-unordered');
          i.plugin = p; i.view = v;
          out.push(i);
        }
        for (const a of p.spec.actions || []) out.push(item(a.name, 'play', 'droidLite.testAction', [p, a]));
        for (const l of p.spec.links || []) out.push(item(l.name, 'link-external', 'vscode.open', [vscode.Uri.parse(l.url)]));
        out.push(item('Remove', 'trash', 'droidLite.removeTestPlugin', [p]));
        return out;
      }
      if (node.view) {
        try {
          const entries = await json(argv(node.view.run, node.plugin, project), node.plugin, project);
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

  const out = vscode.window.createOutputChannel('Droid Lite tests');
  ctx.subscriptions.push(
    ctrl, view, out,
    view.onDidChangeVisibility((e) => (e.visible ? startWatch() : stopWatch())),
    { dispose: stopWatch },
    vscode.commands.registerCommand('droidLite.testAction', (p, a) => {
      out.show(true);
      out.appendLine(`▶ ${p.name}: ${a.name}`);
      const proc = spawn(argv(a.run, p, project), p, project);
      let lines = 0;
      const log = (l) => { if (lines++ < MAX_LOG) out.appendLine(l); };
      readline.createInterface({ input: proc.stdout }).on('line', log);
      readline.createInterface({ input: proc.stderr }).on('line', log);
      proc.on('close', (code) => out.appendLine(`■ ${a.name} ended (exit code ${code})`));
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
      const links = fs.readdirSync(userDir()).map((n) => path.join(userDir(), n))
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

module.exports = { setup, readPlugins, argv, findPluginJson };
