// Droid Lite for VS Code / Cursor: a thin client of `droid-lite serve` (JSON lines over stdio).
const vscode = require('vscode');
const cp = require('child_process');
const path = require('path');
const fs = require('fs');
const readline = require('readline');
const preview = require('./preview');

let engine; // { proc, pending, nextId }
let ready; // Promise of the initialize reply
let recording; // serial while a screen recording runs
let logcat; // running `adb logcat` process
const changed = new vscode.EventEmitter();
const events = new vscode.EventEmitter(); // engine lines without an id (preview progress)

function enginePath(ctx) {
  const set = vscode.workspace.getConfiguration('droidLite').get('enginePath');
  return set || path.join(ctx.extensionPath, 'bin', 'droid-lite');
}

function start(ctx) {
  const bin = enginePath(ctx);
  if (!fs.existsSync(bin)) throw new Error(`Droid Lite engine not found: ${bin}`);
  const proc = cp.spawn(bin, ['serve'], { stdio: ['pipe', 'pipe', 'ignore'] });
  const pending = new Map();
  readline.createInterface({ input: proc.stdout }).on('line', (line) => {
    let msg;
    try { msg = JSON.parse(line); } catch { return; }
    if (msg.event) { events.fire(msg); return; }
    const p = pending.get(msg.id);
    if (!p) return;
    pending.delete(msg.id);
    msg.error === undefined ? p.resolve(msg.result) : p.reject(new Error(msg.error));
  });
  proc.on('exit', () => {
    for (const p of pending.values()) p.reject(new Error('Droid Lite engine stopped'));
    pending.clear();
    if (engine && engine.proc === proc) engine = undefined;
  });
  engine = { proc, pending, nextId: 1 };
}

function call(ctx, method, params = {}) {
  if (!engine) {
    start(ctx);
    ready = undefined;
  }
  const e = engine;
  return new Promise((resolve, reject) => {
    const id = e.nextId++;
    e.pending.set(id, { resolve, reject });
    e.proc.stdin.write(JSON.stringify({ id, method, params }) + '\n');
  });
}

function root() {
  const folders = vscode.workspace.workspaceFolders || [];
  const gradle = folders.find((f) =>
    ['settings.gradle', 'settings.gradle.kts'].some((n) => fs.existsSync(path.join(f.uri.fsPath, n))));
  return (gradle || folders[0])?.uri.fsPath;
}

// project calls wait for the index; device tools do not
function project(ctx, method, params) {
  if (!ready) {
    const dir = root();
    ready = dir ? call(ctx, 'initialize', { root: dir }) : Promise.reject(new Error('Open a folder first'));
    ready.then(() => changed.fire(), () => {});
  }
  return ready.then(() => call(ctx, method, params));
}

const loc = (h) => new vscode.Location(vscode.Uri.file(h.path), new vscode.Position(h.line, h.col));

async function goTo(uri, line, targets) {
  const locs = targets.map(loc);
  if (locs.length === 1) {
    await vscode.window.showTextDocument(locs[0].uri, { selection: locs[0].range });
  } else {
    await vscode.commands.executeCommand('editor.action.showReferences', uri, new vscode.Position(line, 0), locs);
  }
}

// Droid Lite's colours: a badge after the `fun` line and after each parameter (or a line under its
// name when it shares a line), the reasons on hover
const GREEN = '#5FB865';
const RED = '#E05050';
const ORANGE = '#E0A030';
const GREY = '#8C8C8C';
const look = { stable: ['stable', GREEN], unstable: ['unstable', RED], runtime: ['runtime', ORANGE], unknown: ['unknown', GREY] };

function summary(f) {
  const n = (v) => f.params.filter((p) => p.verdict === v).length;
  const unstable = n('unstable');
  const unknown = n('unknown') + n('runtime');
  if (unstable) return [`⚠ ${unstable} unstable`, ORANGE];
  if (unknown) return [`? ${unknown} unknown`, GREY];
  return [f.skippable ? '✓ skippable' : '✓ stable parameters', GREEN];
}

function hover(f) {
  const yn = (b) => (b === true ? 'yes' : b === false ? 'no' : '?');
  const md = new vscode.MarkdownString(
    `**${f.function}** skippable: ${yn(f.skippable)} · restartable: ${yn(f.restartable)}\n\n| Parameter | Stability | Why |\n|---|---|---|\n`
    + f.params.map((p) => `| \`${p.name}\` | ${p.verdict} | ${(p.reason || '').replace(/\|/g, '\\|')} |`).join('\n')
    + `\n\n${f.source === 'guess'
      ? '_Local guess: run the stabilityDump task or build with Compose compiler reports for exact results._'
      : `_From ${path.basename(f.source)}_`}`);
  return md;
}

const badgeTypes = new Map(); // colour → decoration type (badge after the line)
const lineTypes = new Map(); // colour → decoration type (line under a name)
function badgeType(color) {
  if (!badgeTypes.has(color)) {
    badgeTypes.set(color, vscode.window.createTextEditorDecorationType({
      after: {
        color,
        backgroundColor: `${color}24`,
        margin: '0 0 0 1.5em',
        textDecoration: 'none; border-radius: 6px; padding: 0 6px; font-size: 90%',
      },
    }));
  }
  return badgeTypes.get(color);
}
function lineType(color) {
  if (!lineTypes.has(color)) {
    lineTypes.set(color, vscode.window.createTextEditorDecorationType({
      textDecoration: `underline 1.5px ${color}`,
    }));
  }
  return lineTypes.get(color);
}

async function paintStability(ctx, editor) {
  if (!editor || !/\.kts?$/.test(editor.document.uri.fsPath)) return;
  const doc = editor.document;
  const version = doc.version;
  const funs = await project(ctx, 'stability', { path: doc.uri.fsPath, text: doc.getText() }).catch(() => []);
  if (doc.version !== version) return; // a newer paint follows
  const badges = new Map();
  const lines = new Map();
  const add = (map, color, opt) => (map.get(color) || map.set(color, []).get(color)).push(opt);
  const end = (line) => doc.lineAt(Math.min(line, doc.lineCount - 1)).range.end;
  for (const f of funs) {
    const [text, color] = summary(f);
    const tip = hover(f);
    add(badges, color, { range: new vscode.Range(end(f.line), end(f.line)), hoverMessage: tip, renderOptions: { after: { contentText: text } } });
    for (const p of f.params) {
      if (p.line === undefined) continue;
      const [word, pc] = look[p.verdict] || look.unknown;
      const shared = p.line === f.line || f.params.filter((q) => q.line === p.line).length > 1;
      if (shared) {
        add(lines, pc, { range: new vscode.Range(p.line, p.col, p.line, p.end), hoverMessage: tip });
      } else {
        add(badges, pc, { range: new vscode.Range(end(p.line), end(p.line)), hoverMessage: tip, renderOptions: { after: { contentText: word } } });
      }
    }
  }
  for (const [color, t] of badgeTypes) if (!badges.has(color)) editor.setDecorations(t, []);
  for (const [color, t] of lineTypes) if (!lines.has(color)) editor.setDecorations(t, []);
  for (const [color, opts] of badges) editor.setDecorations(badgeType(color), opts);
  for (const [color, opts] of lines) editor.setDecorations(lineType(color), opts);
}

function lenses(ctx) {
  return {
    onDidChangeCodeLenses: changed.event,
    async provideCodeLenses(doc) {
      const links = await project(ctx, 'lenses', { path: doc.uri.fsPath, text: doc.getText() }).catch(() => []);
      return links.map((l) => new vscode.CodeLens(new vscode.Range(l.line, 0, l.line, 0), {
        title: l.title, command: 'droidLite.goTo', arguments: [doc.uri, l.line, l.targets],
      }));
    },
  };
}

async function pickDevice(ctx) {
  const online = (await call(ctx, 'devices')).filter((d) => d.online);
  if (!online.length) throw new Error('No device is connected (adb devices shows none online)');
  if (online.length === 1) return online[0].serial;
  const pick = await vscode.window.showQuickPick(online.map((d) => ({ label: d.label, serial: d.serial })),
    { title: 'Device' });
  return pick?.serial;
}

function revealFile(file, what) {
  vscode.window.showInformationMessage(`${what} saved: ${path.basename(file)}`, 'Open', 'Show in Finder')
    .then((a) => {
      if (a === 'Open') vscode.env.openExternal(vscode.Uri.file(file));
      if (a === 'Show in Finder') vscode.commands.executeCommand('revealFileInOS', vscode.Uri.file(file));
    });
}

function guard(fn) {
  return async (...args) => {
    try { await fn(...args); } catch (e) { vscode.window.showErrorMessage(`Droid Lite: ${e.message}`); }
  };
}

function activate(ctx) {
  // by file name: VS Code has no Kotlin language without a Kotlin extension
  const sel = { scheme: 'file', pattern: '**/*.{kt,kts,java}' };
  const out = vscode.window.createOutputChannel('Logcat', 'log');
  // always visible: the phone in the status bar, a click opens every tool (stops a recording)
  const main = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 100);
  let device = 'Droid Lite';
  const treeChanged = new vscode.EventEmitter();
  const refresh = () => {
    main.text = recording ? '$(record) Stop recording' : `$(device-mobile) ${device}`;
    main.tooltip = recording ? `Recording ${recording}: click to stop and save` : 'Droid Lite tools';
    main.command = recording ? 'droidLite.recordStop' : 'droidLite.menu';
    main.backgroundColor = recording ? new vscode.ThemeColor('statusBarItem.errorBackground') : undefined;
    main.show();
    treeChanged.fire();
  };
  const findDevice = () => call(ctx, 'devices').then((all) => {
    const on = all.filter((d) => d.online);
    device = on.length === 1 ? on[0].label.replace(/ \(.*\)$/, '') : on.length ? `${on.length} devices` : 'No device';
    refresh();
  }, () => {});
  const tool = (label, command, icon) => ({ label, command, icon });
  const tools = () => [
    ['Compose', [tool('Compose preview', 'droidLite.preview', 'open-preview')]],
    ['Device', [
      tool('Screenshot', 'droidLite.screenshot', 'device-camera'),
      recording ? tool('Stop recording and save', 'droidLite.recordStop', 'debug-stop')
        : tool('Record screen', 'droidLite.recordStart', 'record'),
      logcat ? tool('Stop logcat', 'droidLite.logcatStop', 'close') : tool('Show logcat', 'droidLite.logcat', 'output'),
    ]],
    ['Project', [tool('Index the project again', 'droidLite.reindex', 'refresh')]],
  ];
  const tree = {
    onDidChangeTreeData: treeChanged.event,
    getTreeItem: (e) => e,
    getChildren: (group) => {
      if (group) return group.tools;
      return tools().map(([name, list]) => {
        const g = new vscode.TreeItem(name, vscode.TreeItemCollapsibleState.Expanded);
        if (name === 'Device') g.description = device;
        g.tools = list.map((t) => {
          const i = new vscode.TreeItem(t.label);
          i.command = { command: t.command, title: t.label };
          i.iconPath = new vscode.ThemeIcon(t.icon);
          return i;
        });
        return g;
      });
    },
  };
  const timer = setInterval(() => { if (vscode.window.state.focused) findDevice(); }, 20000);
  findDevice();
  refresh();

  // keep the index in step with unsaved edits and files changed outside the editor
  const timers = new Map();
  const update = (p, text) => {
    if (!ready) return;
    clearTimeout(timers.get(p));
    timers.set(p, setTimeout(() => {
      timers.delete(p);
      project(ctx, 'update', text === undefined ? { path: p } : { path: p, text })
        .then(() => changed.fire(), () => {});
    }, 400));
  };
  paintStability(ctx, vscode.window.activeTextEditor);
  preview.setup(ctx, project, events);
  const watcher = vscode.workspace.createFileSystemWatcher('**/*.{kt,kts,java}');

  ctx.subscriptions.push(
    vscode.window.registerTreeDataProvider('droidLite.tools', tree),
    main,
    { dispose: () => clearInterval(timer) },
    vscode.commands.registerCommand('droidLite.menu', guard(async () => {
      findDevice();
      const pick = await vscode.window.showQuickPick(
        tools().flatMap(([, list]) => list).map((t) => ({ label: `$(${t.icon}) ${t.label}`, command: t.command })),
        { title: `Droid Lite · ${device}` });
      if (pick) await vscode.commands.executeCommand(pick.command);
    })),
    vscode.languages.registerCodeLensProvider(sel, lenses(ctx)),
    vscode.window.onDidChangeActiveTextEditor((e) => paintStability(ctx, e)),
    changed.event(() => paintStability(ctx, vscode.window.activeTextEditor)),
    vscode.workspace.onDidChangeTextDocument((e) => {
      if (/\.(kt|kts|java)$/.test(e.document.uri.fsPath)) update(e.document.uri.fsPath, e.document.getText());
    }),
    watcher,
    watcher.onDidCreate((u) => update(u.fsPath)),
    watcher.onDidChange((u) => update(u.fsPath)),
    watcher.onDidDelete((u) => update(u.fsPath, '')),
    out,
    vscode.commands.registerCommand('droidLite.goTo', guard(goTo)),
    vscode.commands.registerCommand('droidLite.reindex', guard(async () => {
      // a running recording ends with the engine
      engine?.proc.stdin.end();
      engine = undefined;
      ready = undefined;
      recording = undefined;
      refresh();
      await project(ctx, 'ping');
    })),
    vscode.commands.registerCommand('droidLite.screenshot', guard(async () => {
      const serial = await pickDevice(ctx);
      if (serial) revealFile(await call(ctx, 'screenshot', { serial }), 'Screenshot');
    })),
    vscode.commands.registerCommand('droidLite.recordStart', guard(async () => {
      const serial = await pickDevice(ctx);
      if (!serial) return;
      await call(ctx, 'record_start', { serial });
      recording = serial;
      refresh();
    })),
    vscode.commands.registerCommand('droidLite.recordStop', guard(async () => {
      if (!recording) throw new Error('No recording is running');
      recording = undefined;
      refresh();
      const file = await vscode.window.withProgress(
        { location: vscode.ProgressLocation.Notification, title: 'Saving the recording…' },
        () => call(ctx, 'record_stop'));
      revealFile(file, 'Recording');
    })),
    vscode.commands.registerCommand('droidLite.logcat', guard(async () => {
      const serial = await pickDevice(ctx);
      if (!serial) return;
      logcat?.kill();
      const adb = await call(ctx, 'adb');
      const history = vscode.workspace.getConfiguration('droidLite').get('logcatHistory');
      out.clear();
      out.show(true);
      const proc = cp.spawn(adb, ['-s', serial, 'logcat', '-v', 'threadtime', '-T', String(history)]);
      proc.stdout.setEncoding('utf8').on('data', (d) => out.append(d));
      proc.on('exit', () => { if (logcat === proc) { logcat = undefined; out.appendLine('[logcat stopped]'); refresh(); } });
      logcat = proc;
      refresh();
    })),
    vscode.commands.registerCommand('droidLite.logcatStop', () => logcat?.kill()),
    { dispose: () => { logcat?.kill(); engine?.proc.stdin.end(); for (const t of [...badgeTypes.values(), ...lineTypes.values()]) t.dispose(); } },
  );
}

module.exports = { activate, deactivate() {} };
