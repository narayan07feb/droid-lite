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

// a fake engine: logs each request; never answers screenshot or previews; ends with its stdin
const log = path.join(dir, 'requests.log');
const fakeEngine = path.join(dir, 'fake-engine');
const fakeAdb = path.join(dir, 'fake-adb');
fs.writeFileSync(fakeEngine, `#!/usr/bin/env node
const fs = require('fs');
require('readline').createInterface({ input: process.stdin }).on('line', (l) => {
  const { id, method, params } = JSON.parse(l);
  fs.appendFileSync(${JSON.stringify(log)}, JSON.stringify({ pid: process.pid, method, params }) + '\\n');
  const answers = { devices: [{ serial: 'S1', label: 'Fake (S1)', online: true }], adb: ${JSON.stringify(fakeAdb)}, lenses: [], stability: [] };
  if (method !== 'screenshot' && method !== 'previews') console.log(JSON.stringify({ id, result: answers[method] ?? null }));
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
const watchers = [];
const output = [];
const panels = [];
let everything = ''; // all output, clears included
let clears = 0;
class Pos { constructor(l, c) { this.line = l; this.character = c; } }
const fake = {
  EventEmitter: class {
    constructor() { this.ls = []; this.event = (l) => { this.ls.push(l); return {}; }; emitters.push(this); }
    fire() { this.ls.forEach((l) => l()); }
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
    workspaceFolders: [{ uri: { fsPath: dir } }],
    getConfiguration: () => ({ get: (k) => cfg[k] }),
    onDidChangeTextDocument: (l) => { docChanges.push(l); return { dispose() {} }; },
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
    onDidChangeActiveTextEditor: () => ({}),
    createWebviewPanel: () => {
      const p = {
        posted: [],
        webview: { cspSource: 'vscode-resource:', asWebviewUri: (u) => u.fsPath, postMessage(m) { p.posted.push(m); } },
        reveal() {}, onDidDispose() {}, dispose() {},
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
  check('version 0.3.2', pkg.version === '0.3.2');

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
  all.ls.Delete({ fsPath: path.join(src, 'old') });
  all.ls.Delete({ fsPath: path.join(src, 'Gone.kt') });
  all.ls.Delete({ fsPath: path.join(dir, 'app/build') });
  all.ls.Delete({ fsPath: path.join(src, 'notes.txt') });
  const change = (scheme, text) => docChanges.forEach((l) => l({ document: {
    uri: { scheme, fsPath: api }, getText: () => text, version: 2,
  } }));
  change('git', 'from git');
  await sleep(600);
  check('a git: document does not update', !requests().some((r) => r.params.path === api));
  change('file', 'package demo\n// 1\n');
  change('file', 'package demo\n// 2\n');
  await sleep(600);
  const sent = requests().filter((r) => r.method === 'update' || r.method === 'remove')
    .map((r) => [r.method, path.relative(dir, r.params.path), r.params.text]);
  check('a create sends update', sent.some(([m, p, t]) => m === 'update' && p.endsWith('New.kt') && t === undefined));
  check('build and .gradle paths are skipped', !sent.some(([, p]) => /(^|\/)(build|\.gradle)(\/|$)/.test(p)));
  check('deletes send remove, for a folder too', sent.some(([m, p]) => m === 'remove' && p.endsWith('demo/old'))
    && sent.some(([m, p]) => m === 'remove' && p.endsWith('Gone.kt')));
  check('a deleted non-source file is not sent', !sent.some(([, p]) => p.endsWith('notes.txt')));
  check('no update with empty text', !sent.some(([m, , t]) => m === 'update' && t === ''));
  check('the watchers split: files create/change, all paths delete',
    files.noDelete && !files.noCreate && !files.noChange && all.noCreate && all.noChange && !all.noDelete);
  const apiUpdates = sent.filter(([, p]) => p.endsWith('Api.kt'));
  check('the newest file: edit wins',
    apiUpdates.length === 1 && apiUpdates[0][2] === 'package demo\n// 2\n');

  // a device call that is never answered fails after its limit
  check('device calls wait 60 s at most', ext.limits.screenshot === 60000 && ext.limits.devices === 60000);
  ext.limits.screenshot = 100;
  await Promise.race([commands['droidLite.screenshot'](), sleep(2000)]);
  check('device call times out', errors.some((m) => /did not answer screenshot/.test(m)));

  // a preview that is never answered: the panel shows the error instead of "Drawing…"
  check('previews wait 180 s at most', ext.limits.previews === 180000);
  ext.limits.previews = 100;
  // the active editor is still Card.kt
  commands['droidLite.preview']();
  await until('the preview error', () => panels[0]?.posted.some((m) => m.type === 'error'), 2000)
    .catch(() => {});
  check('preview times out into the panel',
    panels[0]?.posted.some((m) => m.type === 'error' && /did not answer previews/.test(m.text)));

  // logcat: older lines cleared past logcatMaxLines; stderr is read (else the fake adb never ends)
  cfg.logcatMaxLines = 10;
  await commands['droidLite.logcat']();
  await until('logcat to stop', () => output.join('').includes('[logcat stopped]')).catch(() => {});
  const shown = output.join('');
  check('logcat stderr is drained', shown.includes('[logcat stopped]') && everything.includes('eeee'));
  const kept = shown.slice(shown.lastIndexOf('[older lines cleared]'));
  check('logcat is capped', clears > 1
    && shown.includes('[older lines cleared]') && kept.split('\n').length - 3 <= 10 && kept.includes('line 24'));

  // the engine stops: the next project call starts a new one and indexes again first
  process.kill(enginePid);
  await sleep(200);
  await provider.provideCodeLenses(doc('Api.kt'));
  const next = requests().filter((r) => r.pid !== enginePid).map((r) => r.method);
  check('a restarted engine indexes first', next[0] === 'initialize' && next.includes('lenses'));

  // an engine that stops reading: the next write fails (EPIPE) and fails every waiting call
  const deaf = path.join(dir, 'deaf-engine');
  fs.writeFileSync(deaf, '#!/bin/sh\nexec 0<&-\nsleep 3\n', { mode: 0o755 });
  cfg.enginePath = deaf;
  errors.length = 0;
  let indexed = false;
  commands['droidLite.reindex']().then(() => { indexed = true; });
  await sleep(300);
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

  for (const s of ctx.subscriptions) s.dispose?.();
  fs.rmSync(dir, { recursive: true, force: true });
  console.log(`ok: ${checks} checks`);
})().catch((e) => { console.error(e); process.exit(1); });
