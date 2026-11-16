// Droid Lite for VS Code / Cursor: a thin client of `droid-lite serve` (JSON lines over stdio).
const vscode = require('vscode');
const cp = require('child_process');
const path = require('path');
const fs = require('fs');
const readline = require('readline');
const preview = require('./preview');

let engine; // { proc, pending, nextId }
let ready; // Promise of the initialize reply
let indexing; // that promise while the engine has not answered it (one initialize at a time)
let recording; // serial while a screen recording runs (UNKNOWN: record_start did not answer)
let saving = false; // a recording is being saved (record_stop may take ~16 min): device calls wait
const UNKNOWN = '?';
let logcat; // running `adb logcat` process
let onStopped = () => {}; // the current engine stopped (set in activate)
let onIndexed = () => {}; // an initialize answered (set in activate)
let onIndexing = () => {}; // an initialize was sent or answered (set in activate)
const changed = new vscode.EventEmitter();
// many index changes in a row (a branch switch) repaint and refresh the lenses once
let changeTimer;
const changedSoon = () => { clearTimeout(changeTimer); changeTimer = setTimeout(() => changed.fire(), 200); };
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
  const e = { proc, pending, nextId: 1 };
  readline.createInterface({ input: proc.stdout }).on('line', (line) => {
    let msg;
    try { msg = JSON.parse(line); } catch { return; }
    if (msg.event) { events.fire(msg); return; }
    const p = pending.get(msg.id);
    if (!p) return;
    pending.delete(msg.id);
    msg.error === undefined ? p.resolve(msg.result) : p.reject(new Error(msg.error));
  });
  // the next call starts a new engine and indexes again
  const stop = (why) => {
    for (const p of pending.values()) p.reject(new Error(why));
    pending.clear();
    if (engine === e) { engine = undefined; ready = undefined; recording = undefined; onStopped(); }
  };
  proc.on('exit', () => stop('Droid Lite engine stopped'));
  proc.on('error', (err) => stop(`Droid Lite engine could not run: ${err.message}`));
  proc.stdin.on('error', (err) => stop(`Droid Lite engine stopped: ${err.message}`));
  engine = e;
}

// longest wait for an answer (ms), so no waiting call outlives a stuck engine; others wait 15 s.
// initialize: how long each project call waits for the index; the initialize itself is never sent
// again while the engine works on it (a big project may take minutes)
const limits = {
  initialize: 600000, ping: 15000, update: 15000, remove: 15000, lenses: 15000, stability: 15000, implementations: 15000,
  previews: 180000, devices: 60000, adb: 60000, screenshot: 60000, record_start: 60000, record_stop: 1080000, // engine worst case ~976 s (pull capped at 15 min)
};
// a previews call starts its limit again on each progress line, for any file (a first run downloads
// and compiles; a file queued behind another waits while that one draws), up to this cap (ms)
const caps = { previews: 900000 };
// how often the device in the status bar is looked up (ms)
const every = { devices: 20000 };

function call(ctx, method, params = {}) {
  if (!engine) {
    try { start(ctx); } catch (err) { return Promise.reject(err); }
    ready = undefined;
  }
  const e = engine;
  return new Promise((resolve, reject) => {
    const id = e.nextId++;
    const ms = limits[method] || 15000;
    const cap = caps[method];
    const t0 = Date.now();
    let timer;
    const arm = () => {
      clearTimeout(timer);
      const left = cap ? cap - (Date.now() - t0) : ms;
      timer = setTimeout(() => {
        e.pending.delete(id);
        progress?.dispose();
        reject(new Error(`Droid Lite engine did not answer ${method} in ${(left < ms ? cap : ms) / 1000} s`));
      }, Math.min(ms, left));
    };
    const progress = cap && events.event((m) => { if (m.event === 'preview_status') arm(); });
    if (method !== 'initialize') arm();
    const done = (f) => (v) => { clearTimeout(timer); progress?.dispose(); f(v); };
    e.pending.set(id, { resolve: done(resolve), reject: done(reject) });
    e.proc.stdin.write(JSON.stringify({ id, method, params }) + '\n');
  });
}

// the project folder (the one with settings.gradle), kept until the workspace folders change
let rootDir = null;
function root() {
  if (rootDir === null) {
    const folders = vscode.workspace.workspaceFolders || [];
    const gradle = folders.find((f) =>
      ['settings.gradle', 'settings.gradle.kts'].some((n) => fs.existsSync(path.join(f.uri.fsPath, n))));
    rootDir = (gradle || folders[0])?.uri.fsPath;
  }
  return rootDir;
}

// the engine reads UTF-8: a lone surrogate (half an emoji mid-edit) becomes U+FFFD
const wellFormed = (s) => (s.toWellFormed ? s.toWellFormed()
  : s.replace(/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/g, '\uFFFD'));

// `p`, or an error after `ms`
const within = (p, ms, method) => new Promise((resolve, reject) => {
  const t = setTimeout(() => reject(new Error(`Droid Lite engine did not answer ${method} in ${ms / 1000} s`)), ms);
  p.then((v) => { clearTimeout(t); resolve(v); }, (e) => { clearTimeout(t); reject(e); });
});

// index updates waiting to go to the engine, one request at a time (a lens or badge call waits
// behind one at most). Removes go first, up to `chunk` paths in one request (a branch switch
// deletes thousands); then updates, oldest first. A newer update replaces a waiting one for its
// path; a removed folder takes the waiting requests under it along, and a remove under a folder
// that waits to be removed is dropped. A path is looked up by its folders (O(depth)), never by a
// scan of the queue. `text` is a function read when the update goes, not on every keystroke.
function writeQueue(send, chunk = 5000) {
  const updates = new Map(); // path -> text function (or undefined), oldest first
  const removes = new Set();
  const below = new Map(); // folder -> the waiting paths under it
  const folders = (p) => { // every folder above an absolute path, nearest first (not the disk root)
    const out = [];
    for (let i = p.lastIndexOf(path.sep); i > 0; i = p.lastIndexOf(path.sep, i - 1)) out.push(p.slice(0, i));
    return out;
  };
  const waits = (p) => updates.has(p) || removes.has(p);
  const track = (p, on) => {
    for (const d of folders(p)) {
      let s = below.get(d);
      if (on) { if (!s) below.set(d, (s = new Set())); s.add(p); } else if (s) { s.delete(p); if (!s.size) below.delete(d); }
    }
  };
  const take = (p, f) => { f(); if (!waits(p)) track(p, false); };
  let sending = false;
  let soon = false;
  const pump = () => {
    soon = false;
    if (sending) return;
    let req;
    if (removes.size) {
      const paths = [];
      for (const p of removes) { if (paths.length === chunk) break; paths.push(p); }
      for (const p of paths) take(p, () => removes.delete(p));
      req = send('remove', { paths });
    } else if (updates.size) {
      const [p, text] = updates.entries().next().value;
      take(p, () => updates.delete(p));
      req = send('update', { path: p, ...(text && { text: wellFormed(text()) }) });
    } else return;
    sending = true;
    Promise.resolve(req).catch(() => {}).finally(() => { sending = false; pump(); });
  };
  return (p, method, text) => {
    if (method === 'remove') {
      for (const q of [p, ...(below.get(p) || [])]) take(q, () => { updates.delete(q); removes.delete(q); });
      if (folders(p).some((d) => removes.has(d))) return; // a folder above it is removed first
      removes.add(p);
      track(p, true);
    } else {
      if (!waits(p)) track(p, true);
      updates.delete(p); // to the end: the newest goes last
      updates.set(p, text);
    }
    if (!soon) { soon = true; setImmediate(pump); } // after the other requests of this batch of events
  };
}

// project calls wait for the index; device tools do not
function project(ctx, method, params) {
  if (!ready) {
    const dir = root();
    const r = dir ? call(ctx, 'initialize', { root: dir }) : Promise.reject(new Error('Open a folder first'));
    ready = r;
    if (dir) { indexing = r; onIndexing(); }
    const settled = () => { if (indexing === r) { indexing = undefined; onIndexing(); } };
    // a failed index is not kept: the next project call tries again (an engine that stops fails it)
    r.then(() => { settled(); changed.fire(); if (ready === r) onIndexed(); },
      () => { settled(); if (ready === r) ready = undefined; });
  }
  return within(ready, limits.initialize, 'initialize').then(() => call(ctx, method, params));
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
  const funs = await project(ctx, 'stability', { path: doc.uri.fsPath, text: wellFormed(doc.getText()) }).catch(() => []);
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
      const links = await project(ctx, 'lenses', { path: doc.uri.fsPath, text: wellFormed(doc.getText()) }).catch(() => []);
      return links.map((l) => new vscode.CodeLens(new vscode.Range(l.line, 0, l.line, 0), {
        title: l.title, command: 'droidLite.goTo', arguments: [doc.uri, l.line, l.targets],
      }));
    },
  };
}

async function pickDevice(ctx) {
  // a device call would wait behind record_stop (minutes) and then fail
  if (saving) {
    vscode.window.showInformationMessage('Droid Lite: the recording is still being saved. Please wait until it is done.');
    return undefined;
  }
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
  const note = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 99);
  note.text = '$(sync~spin) Droid Lite: indexing…';
  note.tooltip = 'Droid Lite is reading the project: links and badges come when it is done';
  onIndexing = () => (indexing ? note.show() : note.hide());
  onIndexing();
  let device = 'Droid Lite';
  const treeChanged = new vscode.EventEmitter();
  const refresh = () => {
    main.text = recording ? '$(record) Stop recording' : `$(device-mobile) ${device}`;
    main.tooltip = recording === UNKNOWN ? 'A recording may be running (record_start did not answer): click to stop it'
      : recording ? `Recording ${recording}: click to stop and save` : 'Droid Lite tools';
    main.command = recording ? 'droidLite.recordStop' : 'droidLite.menu';
    main.backgroundColor = recording ? new vscode.ThemeColor('statusBarItem.errorBackground') : undefined;
    main.show();
    treeChanged.fire();
  };
  // one lookup at a time: a slow adb never piles up calls
  let looking;
  const findDevice = () => {
    if (saving) return Promise.resolve(); // it would wait behind record_stop
    looking ??= call(ctx, 'devices').then((all) => {
      const on = all.filter((d) => d.online);
      device = on.length === 1 ? on[0].label.replace(/ \(.*\)$/, '') : on.length ? `${on.length} devices` : 'No device';
      refresh();
    }, () => {}).finally(() => { looking = undefined; });
    return looking;
  };
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
  const timer = setInterval(() => { if (vscode.window.state.focused) findDevice(); }, every.devices);
  findDevice();
  refresh();
  onStopped = refresh; // a recording ends with its engine

  // keep the index in step with unsaved edits and files changed outside the editor (outputs and
  // tool folders never count); see writeQueue
  const timers = new Map();
  const enqueue = writeQueue((method, params) => project(ctx, method, params).then(changedSoon));
  const source = (p) => /\.(kt|kts|java)$/.test(p);
  // only paths under the project folder (another folder of a multi-root workspace is not indexed)
  const indexed = (p) => {
    const r = root() && path.relative(root(), p);
    return typeof r === 'string' && r !== '..' && !r.startsWith(`..${path.sep}`) && !path.isAbsolute(r)
      // the engine's skip list: build, node_modules and hidden folders (.gradle, .idea, .git, .cxx)
      && !/(^|[\\/])(build|node_modules|\.[^\\/]+)([\\/]|$)/.test(r);
  };
  const send = (p, method, text) => {
    if (!ready || !indexed(p)) return;
    clearTimeout(timers.get(p));
    timers.set(p, setTimeout(() => {
      timers.delete(p);
      enqueue(p, method, text);
    }, 400));
  };
  // an open file with unsaved edits: its text, so a late watcher event never undoes them
  const unsaved = (p) => vscode.workspace.textDocuments
    .find((d) => d.isDirty && !d.isClosed && d.uri.scheme === 'file' && d.uri.fsPath === p);
  // no text: the engine reads the disk (a folder: every source file under it)
  const update = (p, text) => {
    if (!ready || !indexed(p)) return; // no document lookup for build outputs and other folders
    const d = !text && unsaved(p);
    send(p, 'update', d ? () => d.getText() : text);
  };
  // a new engine (restart, Index again) knows only the disk: send every unsaved edit again
  onIndexed = () => {
    for (const d of vscode.workspace.textDocuments) {
      if (d.isDirty && d.uri.scheme === 'file' && source(d.uri.fsPath)) update(d.uri.fsPath);
    }
  };
  // any deleted path: a folder (its name may have a dot) takes every file under it out of the index
  const remove = (p) => send(p, 'remove');
  const isDir = (p) => { try { return fs.statSync(p).isDirectory(); } catch { return false; } };
  paintStability(ctx, vscode.window.activeTextEditor);
  preview.setup(ctx, project, events);
  // test tools in Droid Lite's plugin format (docs/PLUGINS.md), in the Test Explorer
  // (the project folder is read on each load: a folder opened later brings its project plugins)
  const testPlugins = vscode.tests && require('./tests').setup(ctx, root);
  const watcher = vscode.workspace.createFileSystemWatcher('**/*.{kt,kts,java}', false, false, true);
  const paths = vscode.workspace.createFileSystemWatcher('**/*', false, true, false);

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
    vscode.workspace.onDidChangeTextDocument(({ document: d }) => {
      // only files on disk: a git: diff side shares the path
      if (d.uri.scheme === 'file' && source(d.uri.fsPath)) update(d.uri.fsPath, () => d.getText());
    }),
    // a closed tab drops its unsaved text: the index goes back to the file on disk
    vscode.workspace.onDidCloseTextDocument((d) => {
      if (d.uri.scheme === 'file' && source(d.uri.fsPath)) send(d.uri.fsPath, 'update');
    }),
    vscode.workspace.onDidChangeWorkspaceFolders(() => {
      rootDir = null;
      testPlugins?.load();
      if (!ready) project(ctx, 'ping').catch(() => {}); // a folder to index may have appeared
    }),
    watcher,
    watcher.onDidCreate((u) => update(u.fsPath)),
    watcher.onDidChange((u) => update(u.fsPath)),
    paths,
    // a folder created, renamed or moved in: the engine indexes every source file under it
    paths.onDidCreate(({ fsPath: p }) => { if (ready && !source(p) && indexed(p) && isDir(p)) update(p); }),
    paths.onDidDelete((u) => remove(u.fsPath)),
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
      try {
        await call(ctx, 'record_start', { serial });
      } catch (e) {
        // it waited behind other device calls and may still start: Stop works either way
        if (!/did not answer|already running/.test(e.message)) throw e;
        recording = UNKNOWN;
        refresh();
        throw new Error(`${e.message}. A recording may be running: Stop recording ends it.`);
      }
      recording = serial;
      refresh();
    })),
    vscode.commands.registerCommand('droidLite.recordStop', guard(async () => {
      if (!recording) throw new Error('No recording is running');
      const was = recording;
      recording = undefined;
      refresh();
      let file;
      saving = true;
      try {
        file = await vscode.window.withProgress(
          { location: vscode.ProgressLocation.Notification, title: 'Saving the recording…' },
          () => call(ctx, 'record_stop'));
      } catch (e) {
        if (was === UNKNOWN && /no recording is running/.test(e.message)) {
          return vscode.window.showInformationMessage('Droid Lite: no recording was running.');
        }
        throw e;
      } finally {
        saving = false;
      }
      revealFile(file, 'Recording');
    })),
    vscode.commands.registerCommand('droidLite.logcat', guard(async () => {
      const serial = await pickDevice(ctx);
      if (!serial) return;
      logcat?.kill();
      const adb = await call(ctx, 'adb');
      const config = vscode.workspace.getConfiguration('droidLite');
      const max = config.get('logcatMaxLines') || 20000;
      out.clear();
      out.show(true);
      const proc = cp.spawn(adb, ['-s', serial, 'logcat', '-v', 'threadtime', '-T', String(config.get('logcatHistory'))]);
      // the channel keeps at most `max` lines; adb's errors go there too, so no pipe fills
      let lines = 0;
      const write = (d) => {
        const n = d.split('\n').length - 1;
        if (lines + n > max) {
          out.clear();
          out.appendLine('[older lines cleared]');
          lines = 0;
        }
        lines += n;
        out.append(d);
      };
      proc.stdout.setEncoding('utf8').on('data', write);
      proc.stderr.setEncoding('utf8').on('data', write);
      proc.on('exit', () => { if (logcat === proc) { logcat = undefined; out.appendLine('[logcat stopped]'); refresh(); } });
      proc.on('error', (err) => {
        if (logcat === proc) { logcat = undefined; refresh(); }
        out.appendLine(`[logcat could not run: ${err.message}]`);
        vscode.window.showErrorMessage(`Droid Lite: logcat could not run: ${err.message}`);
      });
      logcat = proc;
      refresh();
    })),
    vscode.commands.registerCommand('droidLite.logcatStop', () => logcat?.kill()),
    note,
    { dispose: () => { clearTimeout(changeTimer); logcat?.kill(); engine?.proc.stdin.end(); for (const t of [...badgeTypes.values(), ...lineTypes.values()]) t.dispose(); } },
  );
}

module.exports = { activate, deactivate() {}, limits, caps, every, writeQueue };
