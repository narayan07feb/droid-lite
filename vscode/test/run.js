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
let provider;
let tree;
class Pos { constructor(l, c) { this.line = l; this.character = c; } }
const fake = {
  EventEmitter: class { constructor() { this.event = () => {}; } fire() {} },
  Position: Pos,
  Range: class { constructor(a, b) { this.start = new Pos(a, b); } },
  Location: class { constructor(uri, pos) { this.uri = uri; this.range = pos; } },
  CodeLens: class { constructor(range, command) { this.range = range; this.command = command; } },
  Uri: { file: (p) => ({ fsPath: p }) },
  StatusBarAlignment: { Left: 1 },
  ProgressLocation: { Notification: 15 },
  workspace: {
    workspaceFolders: [{ uri: { fsPath: dir } }],
    getConfiguration: () => ({ get: (k) => (k === 'enginePath' ? path.resolve(process.env.DROID_LITE_ENGINE) : 500) }),
    onDidChangeTextDocument: () => ({ dispose() {} }),
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
    createStatusBarItem: () => ({ show() {}, hide() {}, dispose() {} }),
    createOutputChannel: () => ({ dispose() {} }),
    registerTreeDataProvider: (id, p) => { tree = p; return { dispose() {} }; },
  },
  TreeItem: class { constructor(label) { this.label = label; } },
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
  assert.deepStrictEqual(await titles(doc('Card.kt')), [[3, '⚠ 1 unstable: names']]);
  // unsaved text is used: a new line moves the lens
  assert.deepStrictEqual(await titles(doc('Api.kt', 'package demo\n\n\ninterface Api {\n    fun load()\n}\n')),
    [[3, '↓ Implemented in 1 class'], [4, '↓ Implemented in 1 class']]);
  // a lens target opens the file
  const [lens] = await provider.provideCodeLenses(doc('Api.kt'));
  assert.strictEqual(lens.command.command, 'droidLite.goTo');
  assert.strictEqual(lens.command.arguments[2][0].path, fs.realpathSync(path.join(src, 'RealApi.kt')));
  // every tool in the sidebar runs a registered command
  const items = tree.getChildren();
  assert.strictEqual(items.length, 6);
  for (const i of items) assert.ok(commands[i.command.command], i.command.command);
  for (const s of ctx.subscriptions) s.dispose?.();
  fs.rmSync(dir, { recursive: true, force: true });
  console.log('ok: 8 checks');
})().catch((e) => { console.error(e); process.exit(1); });
