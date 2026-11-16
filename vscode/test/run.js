// Loads extension.js with a fake `vscode` module and checks its code lenses against the real engine.
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

const commands = {};
const emitters = [];
const changed_fire = () => emitters.forEach((e) => e.fire());
let provider;
let tree;
let status;
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
  workspace: {
    workspaceFolders: [{ uri: { fsPath: dir } }],
    getConfiguration: () => ({ get: (k) => (k === 'enginePath' ? path.resolve(process.env.DROID_LITE_ENGINE) : 500) }),
    onDidChangeTextDocument: () => ({ dispose() {} }),
    onDidSaveTextDocument: () => ({}),
    createFileSystemWatcher: () => ({ onDidCreate: () => ({}), onDidChange: () => ({}), onDidDelete: () => ({}), dispose() {} }),
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
    createWebviewPanel: () => { throw new Error('not in this test'); },
    createTextEditorDecorationType: (o) => ({ o, dispose() {} }),
    createStatusBarItem: () => (status = { show() { this.shown = true; }, hide() {}, dispose() {} }),
    createOutputChannel: () => ({ dispose() {} }),
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
  for (const s of ctx.subscriptions) s.dispose?.();
  fs.rmSync(dir, { recursive: true, force: true });
  console.log('ok: 11 checks');
})().catch((e) => { console.error(e); process.exit(1); });
