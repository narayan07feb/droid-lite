// Loads extension.js with a fake `vscode` module and checks its code lenses against the real engine,
// then its engine and device handling against a fake engine and a fake adb.
// Run: DROID_LITE_ENGINE=../target/release/droid-lite node test/run.js
const Module = require('module');
const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'droid-lite-vscode-'));
const src = path.join(dir, 'app/src/main/java/demo');
fs.mkdirSync(src, { recursive: true });
fs.writeFileSync(path.join(dir, 'settings.gradle.kts'), 'include(":app")\n');
fs.writeFileSync(path.join(src, 'Api.kt'), 'package demo\n\ninterface Api {\n    fun load()\n}\n');
fs.writeFileSync(path.join(src, 'RealApi.kt'), 'package demo\n\nclass RealApi : Api {\n    override fun load() {}\n}\n');
fs.writeFileSync(path.join(src, 'Card.kt'),
  'package demo\n\n@Composable\nfun Card(names: MutableList<String>, title: String) {}\n');
process.env.DROID_LITE_PREVIEW_DIR = dir;

// a fake engine: logs each request; never answers screenshot, previews or the methods listed in
// silent.json; sends progress for previews (another file's first); ends with its stdin
const log = path.join(dir, 'requests.log');
const silent = path.join(dir, 'silent.json');
const progressFile = path.join(dir, 'progress.json'); // previews: n progress lines, one each `every` ms
const fakeEngine = path.join(dir, 'fake-engine');
const fakeAdb = path.join(dir, 'fake-adb');
fs.writeFileSync(fakeEngine, `#!/usr/bin/env node
const fs = require('fs');
require('readline').createInterface({ input: process.stdin }).on('line', (l) => {
  const { id, method, params } = JSON.parse(l);
  fs.appendFileSync(${JSON.stringify(log)}, JSON.stringify({ pid: process.pid, method, params }) + '\\n');
  const answers = { devices: [{ serial: 'S1', label: 'Fake (S1)', online: true }], adb: ${JSON.stringify(fakeAdb)}, lenses: [], stability: [] };
  const quiet = fs.existsSync(${JSON.stringify(silent)}) ? JSON.parse(fs.readFileSync(${JSON.stringify(silent)}, 'utf8')) : [];
  const prog = method === 'previews' && fs.existsSync(${JSON.stringify(progressFile)})
    && JSON.parse(fs.readFileSync(${JSON.stringify(progressFile)}, 'utf8'));
  if (prog) {
    let i = 0;
    const t = setInterval(() => {
      console.log(JSON.stringify({ event: 'preview_status', file: prog.other ? '/elsewhere/Other.kt' : fs.realpathSync(params.path), text: 'tick' }));
      if (++i === prog.n) clearInterval(t);
    }, prog.every);
  } else if (method === 'previews') {
    console.log(JSON.stringify({ event: 'preview_status', file: '/elsewhere/Other.kt', text: 'other file' }));
    console.log(JSON.stringify({ event: 'preview_status', file: fs.realpathSync(params.path), text: 'this file' }));
  }
  if (method !== 'screenshot' && method !== 'previews' && !quiet.includes(method)) console.log(JSON.stringify({ id, result: answers[method] ?? null }));
});
`, { mode: 0o755 });
// a fake adb: 1 MB on stderr (blocks unless it is read), then 25 lines on stdout in 5 writes
fs.writeFileSync(fakeAdb, `#!/usr/bin/env node
process.stderr.write('e'.repeat(1 << 20) + '\\n', () => {
  let i = 0;
  const t = setInterval(() => {
    process.stdout.write([0, 1, 2, 3, 4].map((k) => 'line ' + (i * 5 + k) + '\\n').join(''));
    if (++i === 5) { clearInterval(t); setTimeout(() => process.exit(0), 50); }
  }, 30);
});
`, { mode: 0o755 });
const requests = () => (fs.existsSync(log) ? fs.readFileSync(log, 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse) : []);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (what, ok, ms = 5000) => {
  for (const end = Date.now() + ms; !ok();) {
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await sleep(20);
  }
};
const cfg = { enginePath: path.resolve(process.env.DROID_LITE_ENGINE), logcatHistory: 500 };

const commands = {};
const emitters = [];
const changed_fire = () => emitters.forEach((e) => e.fire());
let provider;
let tree;
let status;
const errors = [];
const docChanges = [];
const docCloses = [];
const folderChanges = [];
const activeEditors = [];
let folders;
let folderReads = 0;
const watchers = [];
const output = [];
const panels = [];
let everything = ''; // all output, clears included
let clears = 0;
class Pos { constructor(l, c) { this.line = l; this.character = c; } }
const fake = {
  EventEmitter: class {
    constructor() {
      this.ls = [];
      this.event = (l) => { this.ls.push(l); return { dispose: () => { this.ls = this.ls.filter((x) => x !== l); } }; };
      emitters.push(this);
    }
    fire(v) { this.ls.forEach((l) => l(v)); }
  },
  MarkdownString: class { constructor(v) { this.value = v; } },
  Position: Pos,
  Range: class {
    constructor(a, b, c, d) {
      if (a instanceof Pos) { this.start = a; this.end = b; } else { this.start = new Pos(a, b); this.end = new Pos(c ?? a, d ?? b); }
    }
  },
  Location: class { constructor(uri, pos) { this.uri = uri; this.range = pos; } },
  CodeLens: class { constructor(range, command) { this.range = range; this.command = command; } },
  Uri: { file: (p) => ({ fsPath: p }) },
  StatusBarAlignment: { Left: 1 },
  ProgressLocation: { Notification: 15 },
  ViewColumn: { Beside: -2 },
  workspace: {
    textDocuments: [],
    get workspaceFolders() { folderReads++; return folders; },
    getConfiguration: () => ({ get: (k) => cfg[k] }),
    onDidChangeTextDocument: (l) => { docChanges.push(l); return { dispose() {} }; },
    onDidCloseTextDocument: (l) => { docCloses.push(l); return { dispose() {} }; },
    onDidChangeWorkspaceFolders: (l) => { folderChanges.push(l); return { dispose() {} }; },
    onDidSaveTextDocument: () => ({}),
    createFileSystemWatcher: (glob, noCreate, noChange, noDelete) => {
      const w = { glob, noCreate, noChange, noDelete, ls: {}, dispose() {} };
      for (const k of ['Create', 'Change', 'Delete']) w[`onDid${k}`] = (l) => { w.ls[k] = l; return {}; };
      watchers.push(w);
      return w;
    },
  },
  languages: {
    registerCodeLensProvider: (sel, p) => {
      assert.strictEqual(sel.pattern, '**/*.{kt,kts,java}');
      provider = p;
      return { dispose() {} };
    },
  },
  window: {
    activeTextEditor: undefined,
    state: { focused: false },
    onDidChangeActiveTextEditor: (l) => { activeEditors.push(l); return {}; },
    createWebviewPanel: () => {
      const p = {
        posted: [],
        visible: true,
        webview: { cspSource: 'vscode-resource:', asWebviewUri: (u) => u.fsPath, postMessage(m) { p.posted.push(m); } },
        reveal() {}, onDidDispose() {}, dispose() {},
        onDidChangeViewState(l) { p.viewState = l; return {}; },
      };
      panels.push(p);
      return p;
    },
    showErrorMessage: (m) => { errors.push(m); },
    createTextEditorDecorationType: (o) => ({ o, dispose() {} }),
    createStatusBarItem: () => (status = { show() { this.shown = true; }, hide() {}, dispose() {} }),
    createOutputChannel: () => ({
      clear() { output.length = 0; clears++; },
      append(d) { output.push(d); everything += d; },
      appendLine(d) { output.push(`${d}\n`); },
      show() {},
      dispose() {},
    }),
    registerTreeDataProvider: (id, p) => { tree = p; return { dispose() {} }; },
  },
  TreeItem: class { constructor(label, state) { this.label = label; this.state = state; } },
  TreeItemCollapsibleState: { Expanded: 2 },
  ThemeColor: class { constructor(id) { this.id = id; } },
  ThemeIcon: class { constructor(id) { this.id = id; } },
  commands: { registerCommand: (n, f) => { commands[n] = f; return { dispose() {} }; } },
};
folders = [{ uri: { fsPath: dir } }];
const load = Module._load;
Module._load = function (req, ...rest) { return req === 'vscode' ? fake : load.call(this, req, ...rest); };

const ext = require('../extension.js');
const preview = require('../preview.js');
const ctx = { subscriptions: [], extensionPath: path.join(__dirname, '..') };
ext.activate(ctx);

const doc = (file, text) => ({
  uri: { fsPath: path.join(src, file) },
  languageId: 'plaintext', // VS Code without a Kotlin extension
  getText: () => text ?? fs.readFileSync(path.join(src, file), 'utf8'),
});
const titles = async (d) => (await provider.provideCodeLenses(d)).map((l) => [l.range.start.line, l.command.title]);

(async () => {
  assert.deepStrictEqual(await titles(doc('Api.kt')),
    [[2, '↓ Implemented in 1 class'], [3, '↓ Implemented in 1 class']]);
  assert.deepStrictEqual(await titles(doc('RealApi.kt')), [[2, '↑ Api'], [3, '↑ Overrides Api.load']]);
  assert.deepStrictEqual(await titles(doc('Card.kt')), []);
  // unsaved text is used: a new line moves the lens
  assert.deepStrictEqual(await titles(doc('Api.kt', 'package demo\n\n\ninterface Api {\n    fun load()\n}\n')),
    [[3, '↓ Implemented in 1 class'], [4, '↓ Implemented in 1 class']]);
  // a lens target opens the file
  const [lens] = await provider.provideCodeLenses(doc('Api.kt'));
  assert.strictEqual(lens.command.command, 'droidLite.goTo');
  assert.strictEqual(lens.command.arguments[2][0].path, fs.realpathSync(path.join(src, 'RealApi.kt')));
  // stability: Droid Lite's badge after the fun line; both parameters share that line, so their
  // names are underlined (red: unstable, green: stable)
  const card = doc('Card.kt');
  const text = card.getText().split('\n');
  card.version = 1;
  card.lineCount = text.length;
  card.lineAt = (l) => ({ range: { end: new Pos(l, text[l].length) } });
  const painted = [];
  const editor = { document: card, setDecorations: (t, opts) => painted.push([t.o, opts]) };
  fake.window.activeTextEditor = editor;
  changed_fire();
  await new Promise((r) => setTimeout(r, 300));
  const got = painted.filter(([, o]) => o.length).map(([t, o]) => o.map((x) => [
    x.range.start.line, x.range.start.character, x.range.end.character,
    x.renderOptions?.after.contentText ?? t.textDecoration, t.after?.color]));
  assert.deepStrictEqual(got.flat(), [
    [3, 54, 54, '⚠ 1 unstable', '#E0A030'],
    [3, 9, 14, 'underline 1.5px #E05050', undefined],
    [3, 37, 42, 'underline 1.5px #5FB865', undefined],
  ]);
  // the tools: grouped in the sidebar, each runs a registered command; the status bar opens the menu
  const groups = tree.getChildren();
  assert.deepStrictEqual(groups.map((g) => g.label), ['Compose', 'Device', 'Project']);
  const leaves = groups.flatMap((g) => tree.getChildren(g));
  assert.deepStrictEqual(leaves.map((i) => i.label),
    ['Compose preview', 'Screenshot', 'Record screen', 'Show logcat', 'Index the project again']);
  for (const i of leaves) assert.ok(commands[i.command.command], i.command.command);
  assert.ok(status.shown && status.command === 'droidLite.menu' && commands['droidLite.menu'], 'status bar menu');
  let checks = 11;
  const check = (name, ok) => { assert.ok(ok, name); checks++; };

  // the preview page runs only its own script (a nonce, no 'unsafe-inline' script)
  const page = preview.html({ cspSource: 'vscode-resource:' });
  const nonce = /script-src 'nonce-([^']+)'/.exec(page)?.[1];
  check('preview CSP nonce', nonce && page.includes(`<script nonce="${nonce}">`)
    && !/script-src[^;"]*unsafe-inline/.test(page));
  check('a new nonce per page', !preview.html({ cspSource: 'x' }).includes(nonce));
  const pkg = require('../package.json');
  const props = pkg.contributes.configuration.properties;
  check('enginePath is a machine setting', props['droidLite.enginePath'].scope === 'machine');
  check('logcatMaxLines setting', props['droidLite.logcatMaxLines'].default === 20000);
  check('version 0.4.2', pkg.version === '0.4.2');

  // the rest runs on the fake engine
  cfg.enginePath = fakeEngine;
  await commands['droidLite.reindex']();
  await until('the fake engine', () => requests().some((r) => r.method === 'initialize'));
  const enginePid = requests()[0].pid;
  check('index requests reach the fake engine', requests().every((r) => r.pid === enginePid));

  // index updates: build/.gradle/.idea/.git skipped; deletes (files and folders) remove; only file: documents
  const files = watchers.find((w) => w.glob === '**/*.{kt,kts,java}');
  const all = watchers.find((w) => w.glob === '**/*');
  const api = path.join(src, 'Api.kt');
  files.ls.Change({ fsPath: path.join(dir, 'app/build/generated/source/Gen.kt') });
  files.ls.Change({ fsPath: path.join(dir, '.gradle/kotlin/X.kt') });
  files.ls.Create({ fsPath: path.join(src, 'New.kt') });
  // folders created, renamed or moved in (a dot in a folder name too); a new plain file is not sent
  for (const d of ['moved', 'com.example', '../../build/tmp']) fs.mkdirSync(path.join(src, d), { recursive: true });
  fs.writeFileSync(path.join(src, 'readme.txt'), 'x');
  for (const d of ['moved', 'com.example', 'readme.txt', '../../build/tmp']) all.ls.Create({ fsPath: path.join(src, d) });
  all.ls.Delete({ fsPath: path.join(src, 'old') });
  all.ls.Delete({ fsPath: path.join(src, 'old.dir') });
  all.ls.Delete({ fsPath: path.join(src, 'Gone.kt') });
  all.ls.Delete({ fsPath: path.join(dir, 'app/build') });
  all.ls.Delete({ fsPath: path.join(src, 'notes.txt') });
  // another folder of a multi-root workspace (one whose name starts like the root too) is not sent
  const outside = path.join(`${dir}-other`, 'A.kt');
  files.ls.Change({ fsPath: outside });
  files.ls.Create({ fsPath: '/elsewhere/B.kt' });
  all.ls.Delete({ fsPath: path.join(dir, '..', 'gone') });
  let reads = 0;
  const editDoc = (scheme, text, file = api) => ({
    uri: { scheme, fsPath: file }, getText: () => { reads++; return text; }, version: 2,
  });
  const change = (scheme, text, file) => docChanges.forEach((l) => l({ document: editDoc(scheme, text, file) }));
  const close = (scheme, file = api) => docCloses.forEach((l) => l(editDoc(scheme, 'never read', file)));
  const rootReads = folderReads;
  change('git', 'from git');
  await sleep(600);
  check('a git: document does not update', !requests().some((r) => r.params.path === api));
  change('file', 'package demo\n// 1\n');
  change('file', 'package demo\n// 2\n');
  // half an emoji (a lone surrogate) goes as U+FFFD: the engine reads UTF-8
  change('file', 'package demo\n// a\uD83Db\n', path.join(src, 'Emoji.kt'));
  change('file', 'package other\n', outside);
  check('no text read per keystroke', reads === 0);
  await sleep(600);
  check('text read once when the update goes', reads === 2);
  check('the project folder is not looked up per edit', folderReads === rootReads);
  const sent = requests().filter((r) => r.method === 'update' || r.method === 'remove')
    .map((r) => [r.method, path.relative(dir, r.params.path), r.params.text]);
  check('a create sends update', sent.some(([m, p, t]) => m === 'update' && p.endsWith('New.kt') && t === undefined));
  check('a created folder sends update (a dot in its name too)', ['demo/moved', 'demo/com.example'].every((f) =>
    sent.some(([m, p, t]) => m === 'update' && p.endsWith(f) && t === undefined)));
  check('a created plain file is not sent', !sent.some(([, p]) => p.endsWith('readme.txt')));
  check('build and .gradle paths are skipped', !sent.some(([, p]) => /(^|\/)(build|\.gradle)(\/|$)/.test(p)));
  check('every deleted path sends remove (folders with a dot too)', ['demo/old', 'demo/old.dir', 'Gone.kt', 'notes.txt']
    .every((f) => sent.some(([m, p]) => m === 'remove' && p.endsWith(f))));
  check('no update with empty text', !sent.some(([m, , t]) => m === 'update' && t === ''));
  check('paths outside the project folder are not sent', sent.length && !sent.some(([, p]) => p.startsWith('..') || path.isAbsolute(p)));
  check('a lone surrogate goes as U+FFFD', sent.some(([m, p, t]) => m === 'update' && p.endsWith('Emoji.kt')
    && t === 'package demo\n// a\uFFFDb\n'));
  check('the watchers split: files create/change, all paths create/delete',
    files.noDelete && !files.noCreate && !files.noChange && !all.noCreate && all.noChange && !all.noDelete);
  const apiUpdates = sent.filter(([, p]) => p.endsWith('Api.kt'));
  check('the newest file: edit wins',
    apiUpdates.length === 1 && apiUpdates[0][2] === 'package demo\n// 2\n');

  // a closed tab with unsaved text: the index goes back to the disk text (a plain update)
  const before = requests().length;
  change('file', 'package demo\n// unsaved\n');
  close('git');
  close('file');
  await sleep(600);
  const after = requests().slice(before).filter((r) => r.params.path === api);
  check('closing a tab sends update without text',
    after.length === 1 && after[0].method === 'update' && after[0].params.text === undefined);

  // a late watcher event (no text) for an open file with unsaved edits keeps those edits; closing
  // it (edits dropped) still goes back to the disk
  const dirtyText = 'package demo\n// dirty\n';
  const dirtyApi = { ...editDoc('file', dirtyText), isDirty: true, isClosed: false };
  fake.workspace.textDocuments = [dirtyApi];
  const beforeLate = requests().length;
  change('file', dirtyText);
  files.ls.Change({ fsPath: api });
  await sleep(600);
  const late = requests().slice(beforeLate).filter((r) => r.params.path === api);
  check('a late watcher event keeps the unsaved text', late.length === 1 && late[0].params.text === dirtyText);
  const beforeClose = requests().length;
  close('file');
  await sleep(600);
  const closed = requests().slice(beforeClose).filter((r) => r.params.path === api);
  check('closing an unsaved tab still sends no text', closed.length === 1 && closed[0].params.text === undefined);
  fake.workspace.textDocuments = [];

  // every engine call has a time limit: one never answered does not wait forever
  check('every engine method has a limit', ['initialize', 'ping', 'update', 'remove', 'lenses', 'stability',
    'implementations', 'previews', 'devices', 'adb', 'screenshot', 'record_start', 'record_stop']
    .every((m) => ext.limits[m] > 0) && ext.limits.initialize === 60000 && ext.limits.update === 15000
    && ext.limits.lenses === 15000 && ext.limits.record_stop === 900000);
  fs.writeFileSync(silent, '["lenses"]');
  ext.limits.lenses = 100;
  const lensResult = await Promise.race([provider.provideCodeLenses(doc('Api.kt')), sleep(2000).then(() => 'stuck')]);
  check('a never-answered lenses call ends', Array.isArray(lensResult));
  ext.limits.lenses = 15000;
  fs.rmSync(silent);

  // a device call that is never answered fails after its limit
  check('device calls wait 60 s at most', ext.limits.screenshot === 60000 && ext.limits.devices === 60000);
  ext.limits.screenshot = 100;
  await Promise.race([commands['droidLite.screenshot'](), sleep(2000)]);
  check('device call times out', errors.some((m) => /did not answer screenshot/.test(m)));

  // a preview that is never answered: the panel shows the error instead of "Drawing…"
  check('previews wait 180 s without progress, 15 min in all', ext.limits.previews === 180000 && ext.caps.previews === 900000);
  ext.limits.previews = 100;
  // the active editor is still Card.kt
  commands['droidLite.preview']();
  await until('the preview error', () => panels[0]?.posted.some((m) => m.type === 'error'), 2000)
    .catch(() => {});
  check('preview times out into the panel',
    panels[0]?.posted.some((m) => m.type === 'error' && /did not answer previews/.test(m.text)));
  // progress lines name their file (the real path): another file's progress is not shown
  const statuses = panels[0].posted.filter((m) => m.type === 'status').map((m) => m.text);
  check('only the drawn file\'s progress shows', statuses.includes('this file') && !statuses.includes('other file'));
  // a hidden panel draws nothing; it draws the active file when it shows again
  const panel = panels[0];
  const starts = () => panel.posted.filter((m) => m.type === 'start').map((m) => m.file);
  const drawn = starts().length;
  panel.visible = false;
  panel.viewState({ webviewPanel: panel });
  const apiDoc = { ...doc('Api.kt'), version: 1, lineCount: 6, lineAt: () => ({ range: { end: new Pos(0, 0) } }) };
  activeEditors.forEach((l) => l({ document: apiDoc, setDecorations() {} }));
  await sleep(50);
  check('a hidden panel does not draw', starts().length === drawn);
  panel.visible = true;
  panel.viewState({ webviewPanel: panel });
  check('the panel draws the active file when shown', starts().length === drawn + 1 && starts().at(-1) === 'Card.kt');
  await sleep(300); // the preview call times out

  // each progress line for the drawn file starts the previews limit again, up to the cap; another
  // file's progress does not
  const previewError = async (prog) => {
    fs.writeFileSync(progressFile, JSON.stringify(prog));
    const n = panel.posted.length;
    const t = Date.now();
    commands['droidLite.preview']();
    await until('the preview error', () => panel.posted.slice(n).some((m) => m.type === 'error'), 5000);
    fs.rmSync(progressFile);
    return [Date.now() - t, panel.posted.slice(n).find((m) => m.type === 'error').text];
  };
  ext.limits.previews = 300;
  const [withProgress] = await previewError({ n: 8, every: 100 }); // last line at 0.8 s: ends at about 1.1 s
  check('progress for the file starts the previews limit again', withProgress >= 900);
  const [otherProgress] = await previewError({ n: 8, every: 100, other: true });
  check("another file's progress does not", otherProgress < 750);
  ext.caps.previews = 600;
  const [capped, cappedText] = await previewError({ n: 20, every: 100 });
  check('previews end at the cap', capped < 1300 && /in 0\.6 s/.test(cappedText));
  ext.caps.previews = 900000;
  await sleep(1500); // the last progress lines

  // logcat: older lines cleared past logcatMaxLines; stderr is read (else the fake adb never ends)
  cfg.logcatMaxLines = 10;
  await commands['droidLite.logcat']();
  await until('logcat to stop', () => output.join('').includes('[logcat stopped]')).catch(() => {});
  const shown = output.join('');
  check('logcat stderr is drained', shown.includes('[logcat stopped]') && everything.includes('eeee'));
  const kept = shown.slice(shown.lastIndexOf('[older lines cleared]'));
  check('logcat is capped', clears > 1
    && shown.includes('[older lines cleared]') && kept.split('\n').length - 3 <= 10 && kept.includes('line 24'));

  // a recording ends with its engine: the status bar shows the phone again
  await commands['droidLite.recordStart']();
  check('recording shows in the status bar', status.command === 'droidLite.recordStop');

  // the engine stops: the next project call starts a new one and indexes again first
  // (with unsaved edits: the new engine gets them again; another folder's and a clean file's not)
  const dirtyRestart = 'package demo\n// after restart\n';
  const otherDoc = { ...editDoc('file', 'x', outside), isDirty: true };
  const cleanDoc = { ...editDoc('file', 'clean', path.join(src, 'RealApi.kt')), isDirty: false };
  fake.workspace.textDocuments = [{ ...editDoc('file', dirtyRestart), isDirty: true }, otherDoc, cleanDoc,
    { ...editDoc('git', 'git', api), isDirty: true }];
  process.kill(enginePid);
  await sleep(200);
  check('an engine stop ends the recording', status.command === 'droidLite.menu' && /device-mobile/.test(status.text));
  await provider.provideCodeLenses(doc('Api.kt'));
  const next = requests().filter((r) => r.pid !== enginePid).map((r) => r.method);
  check('a restarted engine indexes first', next[0] === 'initialize' && next.includes('lenses'));
  await sleep(600);
  const resent = requests().filter((r) => r.pid !== enginePid && r.method === 'update');
  check('a restarted engine gets the unsaved edits again', resent.length === 1 && resent[0].params.path === api
    && resent[0].params.text === dirtyRestart);
  fake.workspace.textDocuments = [];

  // an engine that stops reading: the next write fails (EPIPE) and fails every waiting call
  const deaf = path.join(dir, 'deaf-engine');
  // (it marks when its stdin is closed: a slow start under load must not make the writes succeed)
  const deafMark = path.join(dir, 'deaf-closed');
  fs.writeFileSync(deaf, `#!/bin/sh\nexec 0<&-\ntouch '${deafMark}'\nsleep 5\n`, { mode: 0o755 });
  cfg.enginePath = deaf;
  errors.length = 0;
  let indexed = false;
  commands['droidLite.reindex']().then(() => { indexed = true; });
  await until('the deaf engine', () => fs.existsSync(deafMark));
  await Promise.race([commands['droidLite.screenshot'](), sleep(3000)]);
  check('a closed engine stdin fails the waiting calls', indexed && errors.some((m) => /stopped: .*EPIPE/.test(m)));

  // an engine that cannot start fails the call with a clear message, and the next call tries again
  const notRunnable = path.join(dir, 'not-runnable');
  fs.writeFileSync(notRunnable, 'x', { mode: 0o644 });
  cfg.enginePath = notRunnable;
  errors.length = 0;
  await Promise.race([commands['droidLite.reindex'](), sleep(3000)]);
  check('spawn failure rejects', errors.some((m) => /could not run/.test(m)));
  errors.length = 0;
  await Promise.race([commands['droidLite.screenshot'](), sleep(3000)]);
  check('spawn failure resets the engine', errors.some((m) => /could not run/.test(m)));

  // no folder: indexing fails and is not kept; a folder that appears is indexed
  cfg.enginePath = fakeEngine;
  folders = [];
  folderChanges.forEach((l) => l());
  errors.length = 0;
  await commands['droidLite.reindex']();
  check('no folder: indexing fails', errors.some((m) => /Open a folder first/.test(m)));
  const known = requests().length;
  folders = [{ uri: { fsPath: dir } }];
  folderChanges.forEach((l) => l());
  await until('indexing the new folder', () => requests().slice(known).some((r) => r.method === 'initialize'), 3000)
    .catch(() => {});
  check('a folder that appears is indexed', requests().slice(known).some((r) => r.method === 'initialize'
    && r.params.root === dir));

  // a missing engine binary: calls fail, activation (device lookup) does not throw
  cfg.enginePath = path.join(dir, 'no-such-engine');
  errors.length = 0;
  await commands['droidLite.reindex']();
  check('a missing engine fails the call', errors.some((m) => /engine not found/.test(m)));
  const ctx2 = { subscriptions: [], extensionPath: ctx.extensionPath };
  let threw;
  try { ext.activate(ctx2); } catch (e) { threw = e; }
  await sleep(100);
  check('activation with a missing engine does not throw', !threw);

  // the device poll skips a round while the last devices call still waits
  cfg.enginePath = fakeEngine;
  fs.writeFileSync(silent, '["devices"]');
  ext.every.devices = 50;
  fake.window.state.focused = true;
  const beforePoll = requests().length;
  const ctx3 = { subscriptions: [], extensionPath: ctx.extensionPath };
  ext.activate(ctx3);
  await sleep(600);
  const polls = requests().slice(beforePoll).filter((r) => r.method === 'devices').length;
  check('no new devices poll while one waits', polls === 1);
  fake.window.state.focused = false;
  ctx2.subscriptions.push(...ctx3.subscriptions);

  for (const s of [...ctx.subscriptions, ...ctx2.subscriptions]) s.dispose?.();
  fs.rmSync(dir, { recursive: true, force: true });
  console.log(`ok: ${checks} checks`);
})().catch((e) => { console.error(e); process.exit(1); });
