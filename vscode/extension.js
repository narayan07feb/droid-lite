// Droid Lite for VS Code / Cursor: a thin client of `droid-lite serve` (JSON lines over stdio).
const vscode = require('vscode');
const cp = require('child_process');
const path = require('path');
const fs = require('fs');
const readline = require('readline');

let engine; // { proc, pending, nextId }
let ready; // Promise of the initialize reply
let recording; // serial while a screen recording runs
let logcat; // running `adb logcat` process
const changed = new vscode.EventEmitter();

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

function stabilityTitle(f) {
  const n = (v) => f.params.filter((p) => p.verdict === v).length;
  const unstable = n('unstable');
  const unknown = n('unknown') + n('runtime');
  if (unstable) return `⚠ ${unstable} unstable: ${f.params.filter((p) => p.verdict === 'unstable').map((p) => p.name).join(', ')}`;
  if (unknown) return `? ${unknown} unknown`;
  return f.skippable ? '✓ skippable' : '✓ stable parameters';
}

function showStability(f) {
  const items = f.params.map((p) => ({ label: `${p.name}: ${p.verdict}`, detail: p.reason || undefined }));
  const from = f.source === 'guess' ? 'Droid Lite guess (no compiler report)' : `report ${path.basename(f.source)}`;
  vscode.window.showQuickPick(items, { title: `${f.function}: parameter stability (${from})` });
}

function lenses(ctx) {
  return {
    onDidChangeCodeLenses: changed.event,
    async provideCodeLenses(doc) {
      const params = { path: doc.uri.fsPath, text: doc.getText() };
      const [links, stab] = await Promise.all([
        project(ctx, 'lenses', params).catch(() => []),
        /\.kts?$/.test(doc.uri.fsPath) ? project(ctx, 'stability', params).catch(() => []) : [],
      ]);
      const at = (line) => new vscode.Range(line, 0, line, 0);
      return [
        ...links.map((l) => new vscode.CodeLens(at(l.line), {
          title: l.title, command: 'droidLite.goTo', arguments: [doc.uri, l.line, l.targets],
        })),
        ...stab.map((f) => new vscode.CodeLens(at(f.line), {
          title: stabilityTitle(f), command: 'droidLite.stability', arguments: [f],
        })),
      ];
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
  const rec = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 100);
  rec.text = '$(record) Recording';
  rec.tooltip = 'Stop the screen recording';
  rec.command = 'droidLite.recordStop';
  const out = vscode.window.createOutputChannel('Logcat', 'log');
  // the Droid Lite icon in the activity bar: one click per tool
  const tools = [
    ['Device screenshot', 'droidLite.screenshot', 'device-camera'],
    ['Start screen recording', 'droidLite.recordStart', 'record'],
    ['Stop screen recording', 'droidLite.recordStop', 'debug-stop'],
    ['Show logcat', 'droidLite.logcat', 'output'],
    ['Stop logcat', 'droidLite.logcatStop', 'close'],
    ['Index the project again', 'droidLite.reindex', 'refresh'],
  ].map(([label, command, icon]) => {
    const item = new vscode.TreeItem(label);
    item.command = { command, title: label };
    item.iconPath = new vscode.ThemeIcon(icon);
    return item;
  });

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
  const watcher = vscode.workspace.createFileSystemWatcher('**/*.{kt,kts,java}');

  ctx.subscriptions.push(
    vscode.window.registerTreeDataProvider('droidLite.tools', { getTreeItem: (i) => i, getChildren: () => tools }),
    vscode.languages.registerCodeLensProvider(sel, lenses(ctx)),
    vscode.workspace.onDidChangeTextDocument((e) => {
      if (/\.(kt|kts|java)$/.test(e.document.uri.fsPath)) update(e.document.uri.fsPath, e.document.getText());
    }),
    watcher,
    watcher.onDidCreate((u) => update(u.fsPath)),
    watcher.onDidChange((u) => update(u.fsPath)),
    watcher.onDidDelete((u) => update(u.fsPath, '')),
    rec,
    out,
    vscode.commands.registerCommand('droidLite.goTo', guard(goTo)),
    vscode.commands.registerCommand('droidLite.stability', showStability),
    vscode.commands.registerCommand('droidLite.reindex', guard(async () => {
      // a running recording ends with the engine
      engine?.proc.stdin.end();
      engine = undefined;
      ready = undefined;
      recording = undefined;
      rec.hide();
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
      rec.show();
    })),
    vscode.commands.registerCommand('droidLite.recordStop', guard(async () => {
      if (!recording) throw new Error('No recording is running');
      recording = undefined;
      rec.hide();
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
      proc.on('exit', () => { if (logcat === proc) { logcat = undefined; out.appendLine('[logcat stopped]'); } });
      logcat = proc;
    })),
    vscode.commands.registerCommand('droidLite.logcatStop', () => logcat?.kill()),
    { dispose: () => { logcat?.kill(); engine?.proc.stdin.end(); } },
  );
}

module.exports = { activate, deactivate() {} };
