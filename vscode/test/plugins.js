// Test plugins in the Test Explorer: loads tests.js with a fake vscode, a fake HOME (so the user's
// own plugins are not read) and the repo's example plugin in a temporary project.
// Run: node test/plugins.js
const Module = require('module');
const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'droid-lite-plugins-'));
const home = path.join(tmp, 'home');
const project = path.join(tmp, 'project');
fs.mkdirSync(home, { recursive: true });
fs.mkdirSync(path.join(project, '.droid-lite', 'plugins'), { recursive: true });
fs.cpSync(path.join(__dirname, '..', '..', 'docs', 'plugin-example'), path.join(project, '.droid-lite', 'plugins', 'example'), { recursive: true });
fs.mkdirSync(path.join(project, '.droid-lite', 'plugins', 'broken'));
fs.writeFileSync(path.join(project, '.droid-lite', 'plugins', 'broken', 'plugin.json'), '{ not json');
process.env.HOME = home;

class Pos { constructor(l, c) { this.line = l; this.character = c; } }
const commands = {};
let ctrl;
let profile;
const fake = {
  Position: Pos,
  Range: class { constructor(a, b) { this.start = a; this.end = b; } },
  Location: class { constructor(uri, pos) { this.uri = uri; this.range = { start: pos }; } },
  Uri: { file: (p) => ({ fsPath: p }), parse: (u) => ({ url: u }) },
  EventEmitter: class { constructor() { this.event = () => ({}); } fire() {} },
  ThemeIcon: class { constructor(id) { this.id = id; } },
  TreeItem: class { constructor(label, state) { this.label = label; this.state = state; } },
  TreeItemCollapsibleState: { Expanded: 2, Collapsed: 1 },
  TestMessage: class { constructor(m) { this.message = m; } },
  TestRunProfileKind: { Run: 1 },
  tests: {
    createTestController: () => {
      const items = new Map();
      const coll = () => {
        const m = new Map();
        return { add: (i) => m.set(i.id, i), replace: (l) => { m.clear(); l.forEach((i) => m.set(i.id, i)); }, forEach: (f) => m.forEach((v) => f(v)), get: (id) => m.get(id), get size() { return m.size; } };
      };
      ctrl = {
        items: coll(),
        createTestItem: (id, label, uri) => { const i = { id, label, uri, children: coll() }; items.set(id, i); return i; },
        createRunProfile: (_n, _k, run) => { profile = run; },
        createTestRun: () => {
          const ev = [];
          return { ev, started: (i) => ev.push(['started', i.id]), passed: (i) => ev.push(['passed', i.id]), failed: (i, m) => ev.push(['failed', i.id, m.message]), skipped: (i) => ev.push(['skipped', i.id]), errored: (i, m) => ev.push(['errored', i.id, m.message]), appendOutput: (t) => ev.push(['out', t.trim()]), end: () => ev.push(['end']) };
        },
        dispose() {},
      };
      return ctrl;
    },
  },
  window: {
    createTreeView: () => ({ onDidChangeVisibility: () => ({}), dispose() {} }),
    createOutputChannel: () => ({ appendLine() {}, show() {}, dispose() {} }),
    showInformationMessage: (m) => { fake.info = m; },
  },
  commands: { registerCommand: (n, f) => { commands[n] = f; return {}; } },
};
const load = Module._load;
Module._load = function (req, ...rest) { return req === 'vscode' ? fake : load.call(this, req, ...rest); };
const tests = require('../tests.js');

(async () => {
  // the plugin folders are read, a broken plugin.json is an error with its path
  const found = tests.readPlugins(project);
  assert.deepStrictEqual(found.map((p) => p.name || 'ERR'), ['ERR', 'Example tests']);
  assert.ok(found[0].error.includes('broken/plugin.json'), found[0].error);
  assert.deepStrictEqual(tests.argv(['x', '{plugin}/a', '{id}', '{project}'], { folder: '/p' }, '/proj', 'case 1'), ['x', '/p/a', 'case 1', '/proj']);
  assert.ok(tests.findPluginJson(path.join(project, '.droid-lite', 'plugins', 'example')));

  const ctx = { subscriptions: [] };
  const api = tests.setup(ctx, project);
  await api.load();
  const root = ctrl.items.get('plugin:Example tests');
  assert.ok(root && !root.error, 'example plugin listed');
  const login = root.children.get('Example tests\u0000Login');
  const ok = login.children.get('Example tests\u0000ok');
  const bad = login.children.get('Example tests\u0000bad');
  assert.strictEqual(ok.uri.fsPath, path.join(project, 'flows/login.yaml'), 'file relative to the project');
  assert.strictEqual(ok.range.start.line, 1, '1-based line 2 -> 0-based 1');
  assert.ok(ctrl.items.get('plugin:broken').error, 'broken plugin shows its error');

  const go = async (include) => {
    let run;
    const orig = ctrl.createTestRun;
    ctrl.createTestRun = (r) => (run = orig(r));
    await profile({ include }, { isCancellationRequested: false, onCancellationRequested: () => {} });
    ctrl.createTestRun = orig;
    return run ? run.ev : [];
  };
  // a passing case: the exit code decides when no final status names it; step results go to output
  let ev = await go([ok]);
  assert.deepStrictEqual(ev.filter((e) => e[0] !== 'out'), [['started', ok.id], ['passed', ok.id], ['end']]);
  assert.ok(ev.some((e) => e[0] === 'out' && e[1] === '[passed] step 1'), JSON.stringify(ev));
  // a failing case: exit code 1
  ev = await go([bad]);
  assert.deepStrictEqual(ev.filter((e) => e[0] !== 'out'), [['started', bad.id], ['failed', bad.id, 'exit code 1'], ['end']]);
  // a group without an id cannot run on its own
  ev = await go([login]);
  assert.deepStrictEqual(ev, [['skipped', login.id], ['end']]);
  // "run all" with nothing picked never starts every device test
  ev = await go(undefined);
  assert.deepStrictEqual(ev, []);
  assert.ok(fake.info.includes('Pick a case'), fake.info);
  assert.ok(commands['droidLite.addTestPlugin'] && commands['droidLite.removeTestPlugin'] && commands['droidLite.testAction']);
  fs.rmSync(tmp, { recursive: true, force: true });
  console.log('ok: test plugins');
})().catch((e) => { console.error(e); process.exit(1); });
