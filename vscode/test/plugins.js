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
// a plugin whose commands start child processes (their pids go to <plugin>/pids), watch every 0.2 s,
// count view runs, print a huge line and garbage places
const toolsDir = path.join(project, '.droid-lite', 'plugins', 'tools');
fs.mkdirSync(toolsDir);
const sh = (...a) => ['sh', '{plugin}/tools.sh', ...a, '{plugin}'];
fs.writeFileSync(path.join(toolsDir, 'plugin.json'), JSON.stringify({
  name: 'Tools', list: sh('list'), run: ['sh', '{plugin}/tools.sh', 'run', '{plugin}', '{id}'], watch: sh('watch'),
  views: [{ name: 'Count', run: sh('count') }, { name: 'Hang', run: sh('hang') }],
  actions: [{ name: 'Slow', run: sh('slow') }],
}));
fs.writeFileSync(path.join(toolsDir, 'tools.sh'), `#!/bin/sh
case "$1" in
  list) echo '[{"id":"group","label":"two children"},{"id":"term","label":"ignores TERM"},{"id":"left","label":"leaves a child"},
    {"id":"long","label":"one huge line"},{"id":"garbage","label":"garbage place","file":5,"line":"x"}]' ;;
  run) case "$3" in
    group) sleep 300 & echo $! >> "$2/pids"; sleep 300 & echo $! >> "$2/pids"; wait ;;
    term) trap '' TERM; echo $$ >> "$2/pids"; sleep 300 & echo $! >> "$2/pids"; wait ;;
    left) sleep 300 & echo $! >> "$2/left"; exit 0 ;;
    long) head -c 200000 /dev/zero | tr '\\0' x; echo; echo after ;;
    garbage) echo '{"case":"garbage","status":"failed","message":{"a":1},"file":["x"],"line":{}}'; exit 1 ;;
  esac ;;
  watch) echo $$ >> "$2/watch"; i=0; while :; do echo "{\\"title\\":\\"tick $i\\"}"; i=$((i+1)); sleep 0.2; done ;;
  count) echo x >> "$2/views"; echo '[{"label":"one","file":"a.kt","line":"zz"}]' ;;
  hang) sleep 300 & echo $! >> "$2/pids"; wait ;;
  slow) echo $$ >> "$2/pids"; sleep 300 & echo $! >> "$2/pids"; wait ;;
esac
`);
// a plugin whose commands do not exist
const missingDir = path.join(project, '.droid-lite', 'plugins', 'missing');
fs.mkdirSync(missingDir);
fs.writeFileSync(path.join(missingDir, 'plugin.json'), JSON.stringify({
  name: 'Missing', list: ['sh', '-c', 'echo \'[{"id":"m","label":"m"}]\''], run: ['/nonexistent/droid-lite-run', '{id}'],
  watch: ['/nonexistent/droid-lite-watch'], actions: [{ name: 'Gone', run: ['/nonexistent/droid-lite-action'] }],
}));
process.env.HOME = home;

class Pos { constructor(l, c) { this.line = l; this.character = c; } }
const commands = {};
let ctrl;
let profile;
let tree; // the sidebar
let visibility; // its visibility listener
const fired = []; // sidebar refreshes (the element, undefined = whole tree)
const output = [];
const fake = {
  Position: Pos,
  Range: class { constructor(a, b) { this.start = a; this.end = b; } },
  Location: class { constructor(uri, pos) { this.uri = uri; this.range = { start: pos }; } },
  Uri: { file: (p) => ({ fsPath: p }), parse: (u) => ({ url: u }) },
  EventEmitter: class { constructor() { this.event = () => ({}); } fire(e) { fired.push(e); } },
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
    createTreeView: (_id, o) => { tree = o.treeDataProvider; return { onDidChangeVisibility: (l) => { visibility = l; return {}; }, dispose() {} }; },
    createOutputChannel: () => ({ appendLine(l) { output.push(l); }, show() {}, dispose() {} }),
    showInformationMessage: (m) => { fake.info = m; },
  },
  commands: { registerCommand: (n, f) => { commands[n] = f; return {}; } },
};
const load = Module._load;
Module._load = function (req, ...rest) { return req === 'vscode' ? fake : load.call(this, req, ...rest); };
const tests = require('../tests.js');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (what, ok, ms = 5000) => {
  for (const end = Date.now() + ms; !ok();) {
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await sleep(20);
  }
};
const pidsIn = (f) => (fs.existsSync(f) ? fs.readFileSync(f, 'utf8').trim().split('\n').filter(Boolean).map(Number) : []);
const alive = (pid) => { try { process.kill(pid, 0); return true; } catch { return false; } };
const pids = path.join(toolsDir, 'pids');
let checks = 0;
const check = (name, ok) => { assert.ok(ok, name); checks++; };

(async () => {
  // the plugin folders are read, a broken plugin.json is an error with its path
  const found = tests.readPlugins(project);
  assert.deepStrictEqual(found.map((p) => p.name || 'ERR'), ['ERR', 'Example tests', 'Missing', 'Tools']);
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

  // a cancellation token: cancel() runs its listeners; `ls` holds those not disposed
  const token = () => {
    const t = { isCancellationRequested: false, ls: new Set() };
    t.onCancellationRequested = (l) => { t.ls.add(l); return { dispose: () => t.ls.delete(l) }; };
    t.cancel = () => { t.isCancellationRequested = true; t.ls.forEach((l) => l()); };
    return t;
  };
  const go = async (include, tok = token()) => {
    let run;
    const orig = ctrl.createTestRun;
    ctrl.createTestRun = (r) => (run = orig(r));
    await profile({ include }, tok);
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

  const tools = ctrl.items.get('plugin:Tools');
  const toolCase = (id) => tools.children.get(`Tools\u0000${id}`);
  const quiet = (ev) => ev.filter((e) => e[0] !== 'out');

  // garbage file/line: no place, no throw
  check('garbage places are skipped', tests.location('/p', 5, 1) === undefined && tests.location('/p', 'a.kt', 'x') === undefined
    && tests.location('/p', 'a.kt', {}) === undefined && tests.location('/p', 'a.kt').range.start.line === 0);
  check('a case with a garbage place loads', !tools.error && toolCase('garbage') && !toolCase('garbage').uri);
  ev = await go([toolCase('garbage')]);
  check('a result with a garbage place is reported', quiet(ev).some((e) => e[0] === 'failed' && e[1] === toolCase('garbage').id));

  // cancel: SIGTERM reaches every process of the run's group; the listener goes with the run
  let tok = token();
  let running = go([toolCase('group')], tok);
  await until('two children', () => pidsIn(pids).length === 2);
  const group = pidsIn(pids);
  tok.cancel();
  ev = await running;
  await until('the group to end', () => !group.some(alive), 3000).catch(() => {});
  check('cancel ends every process of the group', !group.some(alive));
  check('the cancel listener is disposed when the run ends', tok.ls.size === 0);
  check('a cancelled case is skipped', quiet(ev).some((e) => e[0] === 'skipped' && e[1] === toolCase('group').id));

  // a group that ignores SIGTERM gets SIGKILL
  fs.rmSync(pids);
  tests.limits.kill = 1000;
  tok = token();
  running = go([toolCase('term')], tok);
  await until('the TERM-proof group', () => pidsIn(pids).length === 2);
  const stubborn = pidsIn(pids);
  tok.cancel();
  await sleep(500);
  check('TERM alone leaves it', stubborn.every(alive));
  await Promise.race([running, sleep(4000)]);
  await until('SIGKILL', () => !stubborn.some(alive), 3000).catch(() => {});
  check('SIGKILL ends a group that ignores TERM', !stubborn.some(alive));
  fs.rmSync(pids);

  // a run ends with its process even when a child it left keeps the output open
  const t0 = Date.now();
  ev = await Promise.race([go([toolCase('left')]), sleep(4000).then(() => 'stuck')]);
  check('a run settles on exit, not on closed output', ev !== 'stuck' && Date.now() - t0 < 3000
    && quiet(ev).some((e) => e[0] === 'passed'));
  pidsIn(path.join(toolsDir, 'left')).forEach((pid) => { try { process.kill(pid, 'SIGKILL'); } catch { /* ended */ } });

  // one huge line is cut, the next line still comes
  ev = await go([toolCase('long')]);
  const outs = ev.filter((e) => e[0] === 'out').map((e) => e[1]);
  check('a huge line is cut', outs.every((l) => l.length <= (64 << 10) + 20) && outs.some((l) => l.endsWith('[line cut]'))
    && outs.includes('after'));

  // a missing command: reported once (errored, not also failed), nothing throws
  const missing = ctrl.items.get('plugin:Missing').children.get('Missing\u0000m');
  ev = await go([missing]);
  check('a missing command is reported once', JSON.stringify(quiet(ev).slice(1, -1).map((e) => e[0])) === '["errored"]');
  const rowsOf = async () => tree.getChildren();
  const missingPlugin = (await rowsOf()).find((r) => r.label === 'Missing').plugin;
  commands['droidLite.testAction'](missingPlugin, missingPlugin.spec.actions[0]);
  await until('the missing action', () => output.some((l) => /Gone could not start/.test(l)), 2000).catch(() => {});
  check('a missing action command is reported, no throw', output.some((l) => /Gone could not start/.test(l)));

  // watch: lines update the live state and refresh only the plugin's row; views run once until refreshed
  const row = (await rowsOf()).find((r) => r.label === 'Tools');
  fired.length = 0;
  visibility({ visible: true });
  await until('watch lines', () => fired.length >= 6);
  // the example plugin watches too: its own row
  check('watch refreshes only the plugin rows', fired.every((e) => e === row || (e?.plugin && e.label === 'Example tests')));
  const kids = async () => tree.getChildren(row);
  check('watch lines update the live state', /^tick [1-9]/.test((await kids())[0].label));
  const countView = (await kids()).find((i) => i.label === 'Count');
  const viewRuns = () => pidsIn(path.join(toolsDir, 'views')).length;
  const entries = await tree.getChildren(countView);
  await tree.getChildren(countView);
  check('a view runs once until refreshed', viewRuns() === 1 && entries[0].label === 'one');
  commands['droidLite.refreshTestView'](countView);
  await tree.getChildren(countView);
  check('refreshing a view runs it again', viewRuns() === 2);
  // a view that never answers: its whole group ends at the time limit
  tests.limits.json = 300;
  const hang = await tree.getChildren((await kids()).find((i) => i.label === 'Hang'));
  const hung = pidsIn(pids);
  await until('the hung view to end', () => !hung.some(alive), 2000).catch(() => {});
  check('a timed-out view ends its group', /no answer/.test(hang[0].label) && hung.length === 1 && !hung.some(alive));
  fs.rmSync(pids);
  // plugins added or removed: the watchers start again
  const firstWatch = pidsIn(path.join(toolsDir, 'watch'));
  await api.load();
  await until('a new watcher', () => pidsIn(path.join(toolsDir, 'watch')).length === 2);
  await until('the old watcher to end', () => !firstWatch.some(alive), 2000).catch(() => {});
  check('load restarts the watchers', firstWatch.length === 1 && !firstWatch.some(alive));

  // an action shows Stop while it runs, and Stop ends its group
  const toolsRow = (await rowsOf()).find((r) => r.label === 'Tools');
  const slow = toolsRow.plugin.spec.actions[0];
  commands['droidLite.testAction'](toolsRow.plugin, slow);
  await until('the action', () => pidsIn(pids).length === 2);
  const action = pidsIn(pids);
  check('a running action shows Stop', (await tree.getChildren(toolsRow)).some((i) => i.label === 'Stop Slow'
    && i.command.command === 'droidLite.stopTestAction'));
  commands['droidLite.stopTestAction'](toolsRow.plugin, slow);
  await until('the action to end', () => output.some((l) => /Slow ended/.test(l)), 3000).catch(() => {});
  check('Stop ends the action group', !action.some(alive) && output.some((l) => /Slow ended/.test(l)));
  check('an ended action shows Run again', (await tree.getChildren(toolsRow)).some((i) => i.label === 'Slow'));
  fs.rmSync(pids);

  // dispose: every process a plugin started ends (watch and a running action)
  commands['droidLite.testAction'](toolsRow.plugin, slow);
  await until('the action', () => pidsIn(pids).length === 2);
  const leftOver = [...pidsIn(pids), ...pidsIn(path.join(toolsDir, 'watch'))];
  ctx.subscriptions.forEach((d) => d.dispose?.());
  await until('every plugin process to end', () => !leftOver.some(alive), 3000).catch(() => {});
  check('dispose ends every plugin process', leftOver.length === 4 && !leftOver.some(alive));
  await until('no tracked process', () => tests.procs.size === 0, 2000).catch(() => {});
  check('no tracked process left', tests.procs.size === 0);

  fs.rmSync(tmp, { recursive: true, force: true });
  console.log(`ok: test plugins, ${checks} checks`);
})().catch((e) => {
  console.error(e);
  // a failed check leaves no plugin process behind
  tests.procs.forEach((p) => { try { process.kill(-p.pid, 'SIGKILL'); } catch { /* ended */ } });
  for (const f of ['pids', 'left', 'watch']) {
    pidsIn(path.join(toolsDir, f)).forEach((pid) => { try { process.kill(pid, 'SIGKILL'); } catch { /* ended */ } });
  }
  process.exit(1);
});
