// Test plugins in the Test Explorer: loads tests.js with a fake vscode, a fake HOME (so the user's
// own plugins are not read) and the repo's example plugin in a temporary project.
// Run: node test/plugins.js
const Module = require('module');
const assert = require('assert');
const crypto = require('crypto');
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
// a plugin whose run is one string, not a list
fs.mkdirSync(path.join(project, '.droid-lite', 'plugins', 'badrun'));
fs.writeFileSync(path.join(project, '.droid-lite', 'plugins', 'badrun', 'plugin.json'), JSON.stringify({ name: 'Bad run', run: './t.sh' }));
// a plugin whose watch ends at once (its pid goes to <plugin>/qwatch)
// (plugins keep their state outside their folder: any change in it asks for the OK again)
const quickDir = path.join(project, '.droid-lite', 'plugins', 'quick');
const quickState = path.join(tmp, 'quick-state');
fs.mkdirSync(quickDir);
fs.mkdirSync(quickState);
fs.writeFileSync(path.join(quickDir, 'plugin.json'), JSON.stringify({ name: 'Quick', watch: ['sh', '-c', 'echo $$ >> "$0/qwatch"', quickState] }));
// a plugin whose commands start child processes (their pids go to <plugin>/pids), watch every 0.2 s,
// count view runs, print a huge line and garbage places
const toolsDir = path.join(project, '.droid-lite', 'plugins', 'tools');
const toolsState = path.join(tmp, 'tools-state');
fs.mkdirSync(toolsDir);
fs.mkdirSync(toolsState);
const sh = (...a) => ['sh', '{plugin}/tools.sh', ...a, toolsState];
fs.writeFileSync(path.join(toolsDir, 'plugin.json'), JSON.stringify({
  name: 'Tools', list: sh('list'), run: ['sh', '{plugin}/tools.sh', 'run', toolsState, '{id}'], watch: sh('watch'),
  views: [{ name: 'Count', run: sh('count') }, { name: 'Hang', run: sh('hang') }],
  actions: [{ name: 'Slow', run: sh('slow') }, { name: 'Stubborn', run: sh('stubborn') }],
}));
fs.writeFileSync(path.join(toolsDir, 'tools.sh'), `#!/bin/sh
case "$1" in
  list) echo '[{"id":"group","label":"two children"},{"id":"term","label":"ignores TERM"},{"id":"left","label":"leaves a child"},
    {"id":"long","label":"one huge line"},{"id":"garbage","label":"garbage place","file":5,"line":"x"},{"id":"flood","label":"floods"},{"id":"big","label":"12 MB"}]' ;;
  run) case "$3" in
    group) sleep 300 & echo $! >> "$2/pids"; sleep 300 & echo $! >> "$2/pids"; wait ;;
    term) trap '' TERM; echo $$ >> "$2/pids"; sleep 300 & echo $! >> "$2/pids"; wait ;;
    left) sleep 300 & echo $! >> "$2/left"; exit 0 ;;
    long) head -c 200000 /dev/zero | tr '\\0' x; echo; echo after ;;
    garbage) echo '{"case":"garbage","status":"failed","message":{"a":1},"file":["x"],"line":{}}'; exit 1 ;;
    big) line=$(head -c 60000 /dev/zero | tr '\\0' x); i=0; while [ $i -lt 200 ]; do echo "$line"; i=$((i+1)); done; echo after ;;
    flood) seq 30000 | sed 's/.*/{"status":"passed","label":"step"}/'; seq 30000 | sed 's/.*/{"case":"flood","status":"failed","message":"m"}/'; exit 1 ;;
  esac ;;
  watch) echo $$ >> "$2/watch"; i=0; while :; do echo "{\\"title\\":\\"tick $i\\"}"; i=$((i+1)); sleep 0.2; done ;;
  count) echo x >> "$2/views"; echo '[{"label":"one","file":"a.kt","line":"zz"}]' ;;
  hang) sleep 300 & echo $! >> "$2/pids"; wait ;;
  slow) echo $$ >> "$2/pids"; sleep 300 & echo $! >> "$2/pids"; wait ;;
  stubborn) trap '' TERM; echo $$ >> "$2/pids"; sleep 300 & echo $! >> "$2/pids"; wait ;;
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
delete process.env.DROID_LITE_CONFIG_DIR; // its own check sets it
const real = (p) => fs.realpathSync.native(p);
const TRUST = "Allowing trusts this plugin's folder. Commands may also run project code outside it (for example a script in the repo); changes to that code do not ask again.";
// the OKs for project plugins, shared with the app:
// {"version":2,"approved":{"<project real path>":{"<plugin folder real path>":"<tree hash>"}}}
const store = path.join(home, '.config', 'droid-lite', 'approved-plugins.json');
const readStore = (file = store) => (fs.existsSync(file) ? JSON.parse(fs.readFileSync(file, 'utf8')) : {});
const writeStore = (s) => { fs.mkdirSync(path.dirname(store), { recursive: true }); fs.writeFileSync(store, JSON.stringify(s)); };
const okOf = (s, projectDir, folder) => s.approved?.[real(projectDir)]?.[real(folder)];
let hashOf; // the folder's tree hash (tests.treeHash, pinned by a fixed vector below)
// allows a project plugin as the extension would: <project>/.droid-lite/plugins/<name>
const approve = (folder) => {
  const s = readStore();
  const all = s.version === 2 ? s : { version: 2, approved: {} };
  const p = real(path.resolve(folder, '..', '..', '..'));
  all.approved[p] = { ...all.approved[p], [real(folder)]: hashOf(folder) };
  writeStore(all);
};

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
        return { add: (i) => m.set(i.id, i), delete: (id) => m.delete(id), replace: (l) => { m.clear(); l.forEach((i) => m.set(i.id, i)); }, forEach: (f) => m.forEach((v) => f(v)), get: (id) => m.get(id), get size() { return m.size; } };
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
    showErrorMessage: (m) => { fake.err = m; },
    showWarningMessage: (m, o, ...buttons) => { fake.asked = { m, o, buttons }; return Promise.resolve(fake.answer); },
    showOpenDialog: () => Promise.resolve(fake.pick && [{ fsPath: fake.pick }]),
  },
  commands: { registerCommand: (n, f) => { commands[n] = f; return {}; } },
};
const load = Module._load;
Module._load = function (req, ...rest) { return req === 'vscode' ? fake : load.call(this, req, ...rest); };
const tests = require('../tests.js');
hashOf = (folder) => tests.treeHash(real(folder)).hash;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (what, ok, ms = 5000) => {
  for (const end = Date.now() + ms; !ok();) {
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await sleep(20);
  }
};
const pidsIn = (f) => (fs.existsSync(f) ? fs.readFileSync(f, 'utf8').trim().split('\n').filter(Boolean).map(Number) : []);
const alive = (pid) => { try { process.kill(pid, 0); return true; } catch { return false; } };
const pids = path.join(toolsState, 'pids');
// a plugin's id: its folder's real path
const idOf = (n) => fs.realpathSync(path.join(project, '.droid-lite', 'plugins', n));
let checks = 0;
const check = (name, ok) => { assert.ok(ok, name); checks++; };
// runs `f` with the clock an hour ahead: a folder written just now is read long after (its sig is
// kept, not racy)
const later = async (f) => {
  const realNow = Date.now;
  Date.now = () => realNow() + 3600000;
  try { return await f(); } finally { Date.now = realNow; }
};

(async () => {
  // the plugin folders are read, a broken plugin.json is an error with its path
  const found = tests.readPlugins(project);
  assert.deepStrictEqual(found.map((p) => p.name || 'ERR'), ['ERR', 'ERR', 'Example tests', 'Missing', 'Quick', 'Tools']);
  assert.ok(found[1].error.includes('broken/plugin.json'), found[1].error);
  // {plugin} is the folder's real path, as in the app (the temp folder is a link on macOS: /var -> /private/var)
  const exampleDir = path.join(project, '.droid-lite', 'plugins', 'example');
  check('{plugin} is the real folder', found[2].folder === real(exampleDir)
    && tests.argv(['{plugin}/x'], found[2], project)[0] === `${real(exampleDir)}/x`);

  // a plugin.json with a command that is not a list of strings, a bad name, entry or link is an
  // error with its path (a string `run` used to throw inside the run and hang the Test Explorer)
  // shared with the app's test: it refuses the same files with the same words
  const badSpecs = require('./bad-manifests.json');
  const badProject = path.join(tmp, 'bad');
  for (const [n, [spec]] of Object.entries(badSpecs)) {
    fs.mkdirSync(path.join(badProject, '.droid-lite', 'plugins', n), { recursive: true });
    fs.writeFileSync(path.join(badProject, '.droid-lite', 'plugins', n, 'plugin.json'), JSON.stringify(spec));
  }
  const good = path.join(badProject, '.droid-lite', 'plugins', 'zz-good');
  fs.mkdirSync(good);
  fs.writeFileSync(path.join(good, 'plugin.json'), JSON.stringify({ name: 'Good', dir: '..', list: ['a'], run: ['a', '{id}'], watch: ['w'],
    views: [{ name: 'V', run: ['v'] }], actions: [{ name: 'B', run: ['b'] }], links: [{ name: 'L', url: 'HTTPS://example.com' }] }));
  const read = tests.readPlugins(badProject);
  const wrong = Object.entries(badSpecs).filter(([n, [, msg]]) => {
    const p = read.find((x) => x.folder.endsWith(`/${n}`));
    return !p?.error || !p.error.endsWith(`${n}/plugin.json: ${msg}`);
  }).map(([n]) => `${n}: ${read.find((x) => x.folder.endsWith(`/${n}`))?.error}`);
  check('every bad plugin.json is an error with its path', wrong.length === 0 || assert.fail(wrong.join('\n')));
  check('a good plugin.json loads', read.find((x) => x.name === 'Good') && !read.find((x) => x.name === 'Good').error);
  assert.deepStrictEqual(tests.argv(['x', '{plugin}/a', '{id}', '{project}'], { folder: '/p' }, '/proj', 'case 1'), ['x', '/p/a', 'case 1', '/proj']);
  assert.ok(tests.findPluginJson(path.join(project, '.droid-lite', 'plugins', 'example')));

  // the project's plugins below are allowed (the OK itself is checked at the end)
  const pluginsOf = (p) => fs.readdirSync(path.join(p, '.droid-lite', 'plugins')).map((n) => path.join(p, '.droid-lite', 'plugins', n));
  pluginsOf(project).forEach(approve);
  const ctx = { subscriptions: [] };
  const api = tests.setup(ctx, () => project);
  await api.loaded;
  const ex = idOf('example');
  const root = ctrl.items.get(`plugin:${ex}`);
  assert.ok(root && !root.error && root.label === 'Example tests', 'example plugin listed');
  const login = root.children.get(`${ex}\u0000Login`);
  const ok = login.children.get(`${ex}\u0000ok`);
  const bad = login.children.get(`${ex}\u0000bad`);
  assert.strictEqual(ok.uri.fsPath, path.join(project, 'flows/login.yaml'), 'file relative to the project');
  assert.strictEqual(ok.range.start.line, 1, '1-based line 2 -> 0-based 1');
  assert.ok(ctrl.items.get(`plugin:${idOf('broken')}`).error, 'broken plugin shows its error');
  check('a plugin with a string run shows its error in the Test Explorer',
    /badrun\/plugin\.json: run must be a list of strings$/.test(ctrl.items.get(`plugin:${idOf('badrun')}`).error));
  check('and in the sidebar', (await tree.getChildren()).some((r) => r.iconPath?.id === 'warning'
    && /badrun\/plugin\.json: run must be a list of strings$/.test(r.label)));

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
    await profile({ include }, tok).catch((e) => run?.ev.push(['threw', e.message]));
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
  // Remove with no user plugins folder at all: the plugin comes with the project
  fake.info = '';
  await commands['droidLite.removeTestPlugin']((await tree.getChildren()).find((r) => r.label === 'Example tests').plugin);
  check('Remove without a user plugins folder says it comes with the project', /comes with the project/.test(fake.info));

  const tools = ctrl.items.get(`plugin:${idOf('tools')}`);
  const toolCase = (id) => tools.children.get(`${idOf('tools')}\u0000${id}`);
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
  // the tool it left keeps its group tracked (stopped on dispose below), until it ends
  const leftPid = pidsIn(path.join(toolsState, 'left')).at(-1);
  const leftGroup = Number(require('child_process').execFileSync('ps', ['-o', 'pgid=', '-p', String(leftPid)]).toString().trim());
  await sleep(tests.limits.linger * 2);
  check('a group whose leader ended stays tracked while a tool it started runs', alive(leftPid)
    && [...tests.procs].some((x) => x.pid === leftGroup));

  // one huge line is cut, the next line still comes
  ev = await go([toolCase('long')]);
  const outs = ev.filter((e) => e[0] === 'out').map((e) => e[1]);
  check('a huge line is cut', outs.every((l) => l.length <= (64 << 10) + 20) && outs.some((l) => l.endsWith('[line cut]'))
    && outs.includes('after'));

  // a missing command: reported once (errored, not also failed), nothing throws
  const missing = ctrl.items.get(`plugin:${idOf('missing')}`).children.get(`${idOf('missing')}\u0000m`);
  ev = await go([missing]);
  check('a missing command is reported once', JSON.stringify(quiet(ev).slice(1, -1).map((e) => e[0])) === '["errored"]');
  const rowsOf = async () => tree.getChildren();

  // a run that cannot start (a command that is not a list) is an error and the run still ends
  const toolsPlugin = (await rowsOf()).find((r) => r.label === 'Tools').plugin;
  const toolsRun = toolsPlugin.spec.run;
  toolsPlugin.spec.run = './t.sh';
  ev = await Promise.race([go([toolCase('long')]), sleep(3000).then(() => 'stuck')]);
  toolsPlugin.spec.run = toolsRun;
  check('a run that throws still ends', ev !== 'stuck' && ev.at(-1)[0] === 'end');
  check('and shows the error on its case', ev.some((e) => e[0] === 'errored' && e[1] === toolCase('long').id));

  // a flooding plugin: status lines count toward the output limit, a case ends once past it
  ev = await go([toolCase('flood')]);
  const floodOut = ev.filter((e) => e[0] === 'out').length;
  const floodFailed = ev.filter((e) => e[0] === 'failed').length;
  check('status lines count toward the output limit', floodOut <= 20001 && ev.some((e) => e[0] === 'out' && /more than 20000 lines/.test(e[1])));
  check('past the limit a case ends once', floodFailed === 1 && ev.at(-1)[0] === 'end');
  // a run's output is capped at 8 MB too (200 lines of 60 000 characters)
  ev = await go([toolCase('big')]);
  const bigOut = ev.filter((e) => e[0] === 'out').map((e) => e[1]);
  check('a run shows at most 8 MB of output', bigOut.join('').length <= 8 << 20 && !bigOut.includes('after')
    && bigOut.some((l) => /more than 20000 lines or 8 MB/.test(l)) && bigOut.length > 100);
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
  const viewRuns = () => pidsIn(path.join(toolsState, 'views')).length;
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
  tests.limits.json = 120000; // the list calls of the next load
  fs.rmSync(pids);
  // plugins added or removed: the watchers start again
  const firstWatch = pidsIn(path.join(toolsState, 'watch'));
  await api.load();
  await until('a new watcher', () => pidsIn(path.join(toolsState, 'watch')).length === 2);
  await until('the old watcher to end', () => !firstWatch.some(alive), 2000).catch(() => {});
  check('load restarts the watchers', firstWatch.length === 1 && !firstWatch.some(alive));

  // a plugin whose watch cannot start does not stop the others
  const watchFile = path.join(toolsState, 'watch');
  const example = (await rowsOf()).find((r) => r.label === 'Example tests').plugin;
  const exampleWatch = example.spec.watch;
  example.spec.watch = 'not a list';
  const watchCount = pidsIn(watchFile).length;
  visibility({ visible: true });
  await until('the Tools watcher', () => pidsIn(watchFile).length === watchCount + 1, 3000).catch(() => {});
  example.spec.watch = exampleWatch;
  check('one watch that cannot start does not stop the others', pidsIn(watchFile).length === watchCount + 1);

  // a watcher that ended is never signalled again (its group id may be another process's by then)
  const qwatch = path.join(quickState, 'qwatch');
  const quickGone = () => { const q = pidsIn(qwatch).at(-1); return q && ![...tests.procs].some((x) => x.pid === q); };
  await until('the quick watcher to end', quickGone, 3000);
  const quickPid = pidsIn(qwatch).at(-1);
  const signalled = [];
  const realKill = process.kill;
  process.kill = (pid, sig) => { signalled.push(pid); return realKill.call(process, pid, sig); };
  visibility({ visible: false });
  process.kill = realKill;
  check('an ended watcher is not signalled', signalled.length > 0 && !signalled.includes(-quickPid));

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

  // dispose: every process a plugin started ends (watch, a run, actions). A group already sent
  // SIGTERM (a cancelled run) gets SIGKILL at once; the others SIGTERM and a SIGKILL timer that
  // keeps the host up until it fires
  tests.limits.kill = 2000;
  visibility({ visible: true });
  await until('a watcher', () => pidsIn(watchFile).length === watchCount + 2);
  commands['droidLite.testAction'](toolsRow.plugin, slow);
  await until('the action', () => pidsIn(pids).length === 2);
  tok = token();
  running = go([toolCase('term')], tok);
  await until('the TERM-proof run', () => pidsIn(pids).length === 4);
  const cancelled = pidsIn(pids).slice(2);
  tok.cancel();
  commands['droidLite.testAction'](toolsRow.plugin, toolsRow.plugin.spec.actions[1]);
  await until('the TERM-proof action', () => pidsIn(pids).length === 6);
  const proof = pidsIn(pids).slice(4);
  const leftOver = [...pidsIn(pids), pidsIn(watchFile).at(-1), leftPid];
  const timers = [];
  const realTimeout = global.setTimeout;
  global.setTimeout = (...a) => { const t = realTimeout(...a); timers.push(t); return t; };
  ctx.subscriptions.forEach((d) => d.dispose?.());
  global.setTimeout = realTimeout;
  await sleep(400);
  check('dispose: a group already sent SIGTERM gets SIGKILL at once', !cancelled.some(alive));
  check('dispose: the others get SIGTERM first', proof.every(alive));
  check('dispose: their SIGKILL timer keeps the host up', timers.length && timers.every((t) => t.hasRef()));
  await Promise.race([running, sleep(3000)]);
  await until('every plugin process to end', () => !leftOver.some(alive), 4000).catch(() => {});
  check('dispose ends every plugin process, a tool left by an ended run too', leftOver.length === 8 && !leftOver.some(alive));
  await until('no tracked process', () => tests.procs.size === 0, 2000).catch(() => {});
  check('no tracked process left', tests.procs.size === 0);

  // the project is read on each load (a folder opened later); a user and a project plugin may share
  // a name: test items, live state, views and actions stay apart
  const project2 = path.join(tmp, 'project2');
  const dup = (folder) => {
    fs.mkdirSync(folder, { recursive: true });
    const say = (what) => ['sh', '-c', `echo "${what}"`, '{plugin}'];
    fs.writeFileSync(path.join(folder, 'plugin.json'), JSON.stringify({
      name: 'Dup', list: say('[{\\"id\\":\\"c\\",\\"label\\":\\"$0\\"}]'),
      watch: ['sh', '-c', 'echo "{\\"title\\":\\"$0\\"}"; sleep 300', '{plugin}'],
      views: [{ name: 'V', run: say('[{\\"label\\":\\"$0\\"}]') }],
      actions: [{ name: 'Go', run: ['sleep', '300'] }],
    }));
    return fs.realpathSync(folder);
  };
  const userDup = dup(path.join(home, '.config', 'droid-lite', 'plugins', 'dup'));
  const projectDup = dup(path.join(project2, '.droid-lite', 'plugins', 'dup'));
  let current = project;
  approve(path.join(project2, '.droid-lite', 'plugins', 'dup'));
  const ctx2 = { subscriptions: [] };
  const api2 = tests.setup(ctx2, () => current);
  await api2.loaded;
  const roots = () => { const r = []; ctrl.items.forEach((i) => r.push(i)); return r; };
  check('the first project\'s plugins load', roots().some((i) => i.label === 'Tools'));
  current = project2;
  await api2.load();
  check('a load reads the project again', !roots().some((i) => i.label === 'Tools')
    && roots().filter((i) => i.label === 'Dup').length === 2);
  check('same-name plugins get their own test items', [userDup, projectDup].every((f) =>
    ctrl.items.get(`plugin:${f}`)?.children.get(`${f}\u0000c`)?.label === f));
  const dupRows = (await tree.getChildren()).filter((r) => r.label === 'Dup');
  visibility({ visible: true });
  const rowOf = async (r) => tree.getChildren(r);
  let firstRows;
  for (const end = Date.now() + 5000; Date.now() < end; await sleep(50)) {
    firstRows = await Promise.all(dupRows.map(rowOf));
    if (firstRows.every((kids) => kids[0]?.iconPath.id === 'pulse')) break;
  }
  check('same-name plugins keep their own live state', dupRows.length === 2
    && firstRows.every((kids, i) => kids[0].label === dupRows[i].plugin.folder));
  const views = await Promise.all(firstRows.map((kids) => tree.getChildren(kids.find((k) => k.label === 'V'))));
  check('same-name plugins keep their own views', views.every((v, i) => v[0].label === dupRows[i].plugin.folder));
  commands['droidLite.testAction'](dupRows[0].plugin, dupRows[0].plugin.spec.actions[0]);
  const [a0, a1] = await Promise.all(dupRows.map(rowOf));
  check('same-name plugins keep their own actions', a0.some((i) => i.label === 'Stop Go') && a1.some((i) => i.label === 'Go'));
  ctx2.subscriptions.forEach((d) => d.dispose?.());
  await until('no tracked process', () => tests.procs.size === 0, 8000).catch(() => {});
  check('no plugin process left', tests.procs.size === 0);

  // overlapping loads (a refresh while a slow list runs): the older one ends last and writes nothing
  const project3 = path.join(tmp, 'project3');
  const raceDir = path.join(project3, '.droid-lite', 'plugins', 'race');
  fs.mkdirSync(raceDir, { recursive: true });
  const raceOut = path.join(tmp, 'race-out');
  const raceSpec = (v, wait) => JSON.stringify({ name: 'Race', list: ['sh', '-c', `sleep ${wait}; echo '[{"id":"c","label":"${v}"}]'`],
    run: ['sh', '-c', `echo ${v} >> "$0"`, raceOut] });
  fs.writeFileSync(path.join(raceDir, 'plugin.json'), raceSpec('old', 1));
  approve(raceDir);
  // a project plugin not allowed: each of its commands would leave a mark
  const gateDir = path.join(project3, '.droid-lite', 'plugins', 'gate');
  fs.mkdirSync(gateDir);
  const marks = path.join(tmp, 'gate-marks');
  const mark = (what) => ['sh', '-c', `echo ${what} >> "$0"; echo '[{"id":"g","label":"g"}]'`, marks];
  const gateSpec = { name: 'Gate', list: mark('list'), run: mark('run'), watch: mark('watch'),
    views: [{ name: 'V', run: mark('view') }], actions: [{ name: 'A', run: mark('action') }] };
  fs.writeFileSync(path.join(gateDir, 'plugin.json'), JSON.stringify(gateSpec));
  { // another project's entry (the app's)
    const s = readStore();
    writeStore({ ...s, approved: { ...s.approved, '/some/app': { '/some/app/.droid-lite/plugins/p': 'abc123' } } });
  }
  const ctx3 = { subscriptions: [] };
  const api3 = tests.setup(ctx3, () => project3);
  fs.writeFileSync(path.join(raceDir, 'plugin.json'), raceSpec('new', 0));
  approve(raceDir);
  await api3.load();
  await api3.loaded;
  const raceId = fs.realpathSync(raceDir);
  const raceCase = ctrl.items.get(`plugin:${raceId}`)?.children.get(`${raceId}\u0000c`);
  ev = await go([raceCase]);
  check('an older load that ends last does not win: the run uses the newest plugin.json', raceCase?.label === 'new'
    && fs.readFileSync(raceOut, 'utf8').trim() === 'new' && quiet(ev).some((e) => e[0] === 'passed'));

  // an unapproved project plugin: "Needs your OK" in the Test Explorer and the sidebar, nothing runs
  const gateId = fs.realpathSync(gateDir);
  const gateRoot = ctrl.items.get(`needs-ok:${gateId}`);
  check('a project plugin not allowed shows "Needs your OK" in the Test Explorer', gateRoot?.description === 'Needs your OK'
    && /Needs your OK/.test(gateRoot.error) && gateRoot.error.includes(TRUST) && !ctrl.items.get(`plugin:${gateId}`));
  const gateRow = (await tree.getChildren()).find((r) => r.label === 'Gate');
  check('and in the sidebar, with only Allow…', gateRow?.description === 'Needs your OK'
    && (await tree.getChildren(gateRow)).map((i) => `${i.label}>${i.command?.command}`).join() === 'Allow…>droidLite.allowTestPlugin');
  visibility({ visible: true }); // watchers start
  commands['droidLite.testAction']({ folder: gateId, name: 'Gate', spec: gateSpec }, gateSpec.actions[0]);
  ev = await go([gateRoot]);
  await sleep(500);
  check('a project plugin not allowed runs nothing (list, watch, action, run)', !fs.existsSync(marks)
    && quiet(ev).every((e) => e[0] !== 'started'));
  // Cancel allows nothing; Allow (from the Test Explorer item) shows the folder and every command
  fake.answer = undefined;
  await commands['droidLite.allowTestPlugin'](gateRow.pending);
  check('Cancel allows nothing', !okOf(readStore(), project3, gateDir) && !fs.existsSync(marks));
  fake.answer = 'Allow';
  await commands['droidLite.allowTestPlugin'](gateRoot);
  const { o: modal, buttons } = fake.asked;
  check('the OK is a modal with what it trusts, the folder and the commands', modal.modal && buttons.includes('Allow')
    && modal.detail.includes(TRUST) && modal.detail.includes(gateId)
    && ['list', 'run', 'watch'].every((k) => modal.detail.includes(`${k}: ${JSON.stringify(gateSpec[k])}`))
    && modal.detail.includes(JSON.stringify(gateSpec.views[0].run)) && modal.detail.includes(JSON.stringify(gateSpec.actions[0].run)));
  await until('the list to run', () => fs.existsSync(marks), 3000).catch(() => {});
  check('Allow runs list', fs.existsSync(marks) && fs.readFileSync(marks, 'utf8').includes('list')
    && ctrl.items.get(`plugin:${gateId}`)?.children.get(`${gateId}\u0000g`));
  const saved = readStore();
  check('the store (v2, 0600) keeps other entries, holds the tree hash per project, no temp or lock file left',
    saved.version === 2 && saved.approved['/some/app']['/some/app/.droid-lite/plugins/p'] === 'abc123'
    && okOf(saved, project3, raceDir) === hashOf(raceDir) && okOf(saved, project3, gateDir) === hashOf(gateDir)
    && (fs.statSync(store).mode & 0o777) === 0o600
    && fs.readdirSync(path.dirname(store)).every((n) => !n.endsWith('.tmp') && !n.endsWith('.lock')));
  // a changed plugin.json asks again
  await sleep(300); // the watch started by the load ends
  fs.rmSync(marks);
  fs.writeFileSync(path.join(gateDir, 'plugin.json'), `${JSON.stringify(gateSpec)} `);
  await api3.load();
  await sleep(500);
  check('a changed plugin.json asks again', ctrl.items.get(`needs-ok:${gateId}`) && !ctrl.items.get(`plugin:${gateId}`) && !fs.existsSync(marks));
  // a store that is not a JSON object is never overwritten (it may be the app's)
  fs.writeFileSync(store, 'not json');
  fake.err = '';
  await commands['droidLite.allowTestPlugin']((await tree.getChildren()).find((r) => r.label === 'Gate').pending);
  check('a broken store is not overwritten', fs.readFileSync(store, 'utf8') === 'not json' && /could not save the OK/.test(fake.err));
  fs.writeFileSync(store, JSON.stringify(saved));
  // the user's own plugins need no OK
  check('a user plugin needs no OK', !JSON.stringify(readStore()).includes(userDup) && ctrl.items.get(`plugin:${userDup}`)?.children.get(`${userDup}\u0000c`));
  const allowGate = async () => {
    fake.answer = 'Allow';
    await commands['droidLite.allowTestPlugin']((await tree.getChildren()).find((r) => r.label === 'Gate' && r.pending).pending);
  };
  const gateNeedsOk = () => !!ctrl.items.get(`needs-ok:${gateId}`) && !ctrl.items.get(`plugin:${gateId}`);
  // any file in the plugin's folder (a script it runs, in a subfolder too) is in the OK
  fs.mkdirSync(path.join(gateDir, 'lib'));
  fs.writeFileSync(path.join(gateDir, 'lib', 'helper.sh'), 'echo 1\n');
  await api3.load();
  check('a new script in the folder asks again', gateNeedsOk());
  await allowGate();
  check('Allow runs it again', ctrl.items.get(`plugin:${gateId}`) && !gateNeedsOk());
  fs.writeFileSync(path.join(gateDir, 'lib', 'helper.sh'), 'echo 2\n');
  await api3.load();
  check('a changed script in the folder asks again', gateNeedsOk());
  // an old (v1) store, a flat map folder -> hash, allows nothing; the first Allow replaces it with v2
  writeStore({ [gateId]: hashOf(gateDir) });
  await api3.load();
  check('a v1 store asks again', gateNeedsOk());
  await allowGate();
  check('Allow replaces a v1 store with v2', readStore().version === 2 && !(gateId in readStore())
    && okOf(readStore(), project3, gateDir) === hashOf(gateDir) && !gateNeedsOk());
  // the OK is checked again before every command (list, run, watch, views, actions), as in the app:
  // a file of the folder changed after the load runs nothing, and the plugin asks for the OK again
  const changedMsg = 'Gate changed since you allowed it; allow it again';
  const gatePlugin = async () => (await tree.getChildren()).find((r) => r.label === 'Gate' && r.plugin)?.plugin;
  const refusedBy = {
    list: async (p) => { try { await tests.json(tests.argv(p.spec.list, p, project3), p, project3); } catch (e) { return e.message; } },
    run: async () => (await go([ctrl.items.get(`plugin:${gateId}`).children.get(`${gateId}\u0000g`)])).find((e) => e[0] === 'errored')?.[2],
    watch: async () => { visibility({ visible: false }); visibility({ visible: true }); await sleep(300); return ctrl.items.get(`needs-ok:${gateId}`)?.error; },
    view: async (p) => {
      const row = (await tree.getChildren()).find((r) => r.plugin === p);
      const v = (await tree.getChildren(row)).find((i) => i.label === 'V');
      return (await tree.getChildren(v))[0]?.label;
    },
    action: async (p) => {
      commands['droidLite.testAction'](p, p.spec.actions[0]);
      await sleep(100);
      return output.find((l) => l.includes(changedMsg));
    },
  };
  for (const [k, refuse] of Object.entries(refusedBy)) {
    if (gateNeedsOk()) await allowGate();
    await sleep(400); // the list and watch the load started end
    fs.rmSync(marks, { force: true });
    output.length = 0;
    const p = await gatePlugin();
    fs.writeFileSync(path.join(gateDir, 'lib', 'helper.sh'), `echo ${k}\n`);
    const said = await refuse(p);
    await sleep(300);
    check(`${k}: a file changed since the OK runs nothing and asks for the OK again`, !fs.existsSync(marks)
      && String(said).includes(changedMsg) && gateNeedsOk() && ctrl.items.get(`needs-ok:${gateId}`).error.startsWith(changedMsg)
      && !(await tree.getChildren()).some((r) => r.label === 'Gate' && r.plugin));
  }
  await allowGate();
  check('allowing it again runs it', !gateNeedsOk() && okOf(readStore(), project3, gateDir) === hashOf(gateDir));
  // an OK is for one project: the same folder's OK under another project's key allows nothing
  const projectB = path.join(tmp, 'projectB');
  const bDir = path.join(projectB, '.droid-lite', 'plugins', 'gate');
  fs.cpSync(gateDir, bDir, { recursive: true });
  const bOk = () => tests.readPlugins(projectB).find((x) => x.folder === real(bDir));
  writeStore({ version: 2, approved: { [real(project3)]: { [real(bDir)]: hashOf(bDir) } } });
  check('an OK for project A does not allow a plugin of project B', bOk()?.needsOk && hashOf(bDir) === hashOf(gateDir));
  approve(bDir);
  check('its own project\'s OK does', bOk()?.spec && !bOk().needsOk);
  // a project plugin that links outside the project's .droid-lite/plugins can never run or be
  // allowed, even with an entry in the store
  const outDir = path.join(tmp, 'outside-plugin');
  fs.mkdirSync(outDir);
  fs.writeFileSync(path.join(outDir, 'plugin.json'), JSON.stringify({ name: 'Outside', list: mark('outside') }));
  const outLink = path.join(project3, '.droid-lite', 'plugins', 'outside');
  fs.symlinkSync(outDir, outLink);
  writeStore({ version: 2, approved: { [real(project3)]: { [gateId]: hashOf(gateDir), [real(outDir)]: hashOf(outDir) } } });
  fs.rmSync(marks, { force: true });
  await api3.load();
  const outId = real(outDir);
  check('a plugin linked outside the project is refused', /plugin\.json: links outside the project$/.test(ctrl.items.get(`plugin:${outId}`)?.error)
    && !ctrl.items.get(`needs-ok:${outId}`) && (await tree.getChildren()).some((r) => r.iconPath?.id === 'warning' && /links outside the project/.test(r.label)));
  fake.asked = undefined;
  await commands['droidLite.allowTestPlugin']({ folder: outId });
  await sleep(300);
  check('and cannot be allowed or run', !fake.asked && !(fs.existsSync(marks) && fs.readFileSync(marks, 'utf8').includes('outside')));
  fs.unlinkSync(outLink);
  // nor can a project whose .droid-lite (or plugins folder) links outside it
  const projectL = path.join(tmp, 'projectL');
  fs.mkdirSync(projectL);
  fs.symlinkSync(path.join(project3, '.droid-lite'), path.join(projectL, '.droid-lite'));
  check('a project whose .droid-lite links outside is refused', tests.readPlugins(projectL)
    .filter((x) => x.folder.startsWith(real(project3))).every((x) => /links outside the project$/.test(x.error)));
  const projectM = path.join(tmp, 'projectM');
  fs.mkdirSync(path.join(projectM, '.droid-lite'), { recursive: true });
  fs.symlinkSync(path.join(project3, '.droid-lite', 'plugins'), path.join(projectM, '.droid-lite', 'plugins'));
  const linkedPlugins = tests.readPlugins(projectM).filter((x) => x.folder.startsWith(real(project3)));
  check('a project whose .droid-lite/plugins links outside is refused', linkedPlugins.length > 1
    && linkedPlugins.every((x) => /links outside the project$/.test(x.error)));
  // a link inside the folder, more than 2000 files or 8 MiB: too risky or too big to check
  const projectT = path.join(tmp, 'projectT');
  const tPlugin = (n, files) => {
    const d = path.join(projectT, '.droid-lite', 'plugins', n);
    fs.mkdirSync(d, { recursive: true });
    fs.writeFileSync(path.join(d, 'plugin.json'), JSON.stringify({ name: n }));
    files(d);
    return real(d);
  };
  const tLink = tPlugin('link', (d) => { fs.mkdirSync(path.join(d, 'sub')); fs.symlinkSync('/etc/hosts', path.join(d, 'sub', 'h')); });
  const tMany = tPlugin('many', (d) => { for (let i = 0; i < 2000; i++) fs.writeFileSync(path.join(d, `f${i}`), ''); });
  const tBig = tPlugin('big', (d) => fs.writeFileSync(path.join(d, 'blob'), Buffer.alloc((8 << 20) + 1)));
  const tFit = tPlugin('fit', (d) => { for (let i = 0; i < 1999; i++) fs.writeFileSync(path.join(d, `f${i}`), ''); });
  const tOf = (id) => tests.readPlugins(projectT).find((x) => x.folder === id);
  check('a link inside a plugin folder is refused', /contains links$/.test(tOf(tLink)?.error));
  check('more than 2000 files or 8 MiB is refused', /too big to check$/.test(tOf(tMany)?.error) && /too big to check$/.test(tOf(tBig)?.error)
    && tOf(tFit)?.needsOk);
  // the tree hash: fixed vectors (recomputed with Python's hashlib), the same in the app; the
  // permission bits are part of it
  const writeMode = (f, text, mode) => { fs.writeFileSync(f, text); fs.chmodSync(f, mode); };
  const vec = path.join(tmp, 'vector');
  fs.mkdirSync(path.join(vec, 'sub'), { recursive: true });
  writeMode(path.join(vec, 'plugin.json'), '{"name":"V","list":["sh","list.sh"]}', 0o644);
  writeMode(path.join(vec, 'list.sh'), 'echo []\n', 0o755);
  writeMode(path.join(vec, 'sub', 'a.txt'), 'a', 0o600);
  // (and the app's own: '.' sorts before '/')
  const vec2 = path.join(tmp, 'vector2');
  fs.mkdirSync(path.join(vec2, 'a'), { recursive: true });
  writeMode(path.join(vec2, 'a.txt'), 'x', 0o644);
  writeMode(path.join(vec2, 'a', 'b'), '', 0o644);
  writeMode(path.join(vec2, 'plugin.json'), '{"name":"T"}', 0o644);
  check('the tree hash matches the shared vectors', tests.treeHash(real(vec)).hash === 'c8b8b3b7639db77b69849a4bdbd4858552d939e8f6a9e38e18d74f8ee18b1d1a'
    && tests.treeHash(real(vec2)).hash === 'b7b77b6b2174d5ec3f45452cb493a4a27fdce843557c41dcdecb1d7f98aff0bd');
  // chmod +x on an allowed script asks again
  const projectX = path.join(tmp, 'projectX');
  const xDir = path.join(projectX, '.droid-lite', 'plugins', 'x');
  fs.mkdirSync(xDir, { recursive: true });
  fs.writeFileSync(path.join(xDir, 'plugin.json'), JSON.stringify({ name: 'X', list: ['./go.sh'] }));
  writeMode(path.join(xDir, 'go.sh'), 'echo []\n', 0o644);
  approve(xDir);
  const xOf = () => tests.readPlugins(projectX).find((x) => x.folder === real(xDir));
  const xAllowed = !!xOf().spec;
  fs.chmodSync(path.join(xDir, 'go.sh'), 0o755);
  check('a changed file mode asks for the OK again', xAllowed && xOf().needsOk);
  // a store whose version is 2.0 is version 2 (as in the app); the text "2" is not
  const storeText = fs.readFileSync(store, 'utf8');
  approve(xDir);
  const v2 = readStore();
  fs.writeFileSync(store, JSON.stringify(v2).replace('"version":2', '"version":2.0'));
  const as20 = !!xOf().spec && fs.readFileSync(store, 'utf8').includes('"version":2.0');
  fs.writeFileSync(store, JSON.stringify({ ...v2, version: '2' }));
  check('a store with version 2.0 counts, "2" does not', as20 && xOf().needsOk);
  fs.writeFileSync(store, storeText);
  // a name that is not UTF-8 is refused with the app's words (APFS cannot even make one)
  let notUtf8;
  try { tests.utf8Name(Buffer.from([0x61, 0xff, 0x62, 0xe2, 0x82])); } catch (e) { notUtf8 = e.message; }
  check('a name that is not UTF-8 is refused', notUtf8 === 'a\ufffdb\ufffd is not a UTF-8 name'
    && tests.utf8Name(Buffer.from('plugin.json')).toString() === 'plugin.json');
  // a file that grew past the limit since the walk is refused before any byte of it is read
  const grown = path.join(tmp, 'grown');
  fs.mkdirSync(grown);
  fs.writeFileSync(path.join(grown, 'plugin.json'), '{"name":"G"}');
  fs.closeSync(fs.openSync(path.join(grown, 'blob'), 'w'));
  fs.truncateSync(path.join(grown, 'blob'), 32 << 20); // sparse: no disk
  const [realLstat, realReadSync, realFstat] = [fs.lstatSync, fs.readSync, fs.fstatSync];
  let readBytes = 0;
  fs.lstatSync = (...a) => { const st = realLstat(...a); if (String(a[0]).endsWith('/blob')) st.size = 1n; return st; };
  fs.readSync = (...a) => { const n = realReadSync(...a); readBytes += n; return n; };
  let grew;
  try { tests.treeHash(real(grown)); } catch (e) { grew = e.message; } finally { [fs.lstatSync, fs.readSync] = [realLstat, realReadSync]; }
  check('a file that grew past the limit is refused unread', grew === 'too big to check' && readBytes < 100);
  // a file whose length changes while it is read is a change (it may be allowed again), and is read
  // only one byte past the length it had
  fs.truncateSync(path.join(grown, 'blob'), 4 << 20);
  readBytes = 0;
  fs.fstatSync = (...a) => { const st = realFstat(...a); if (st.size === 4 << 20) st.size = 1000; return st; };
  fs.readSync = (...a) => { const n = realReadSync(...a); readBytes += n; return n; };
  let moved;
  try { tests.treeHash(real(grown)); } catch (e) { moved = e; } finally { [fs.fstatSync, fs.readSync] = [realFstat, realReadSync]; }
  check('a length that changes while read is a change', moved?.message === 'blob changed while it was read' && moved.changed && readBytes < 1100);
  // the hash reads a piece at a time: no update is a whole file
  let biggest = 0;
  const realCreateHash = crypto.createHash;
  crypto.createHash = (...a) => {
    const h = realCreateHash(...a);
    const update = h.update.bind(h);
    h.update = (d) => { biggest = Math.max(biggest, d.length); update(d); return h; };
    return h;
  };
  try { tests.treeHash(real(grown)); } finally { crypto.createHash = realCreateHash; }
  check('the tree hash is fed a piece at a time', biggest > 0 && biggest <= 64 << 10);
  // a FIFO swapped in for a file after the walk never makes the hash wait for a writer: a change
  // (a child process: a wait would freeze this one)
  const fifoDir = path.join(tmp, 'fifo');
  fs.mkdirSync(fifoDir);
  fs.writeFileSync(path.join(fifoDir, 'plugin.json'), '{"name":"F"}');
  fs.writeFileSync(path.join(fifoDir, 'plain'), 'x');
  require('child_process').execFileSync('mkfifo', [path.join(fifoDir, 'x')]);
  const fifoChild = path.join(tmp, 'fifo-child.js');
  fs.writeFileSync(fifoChild, `const Module = require('module');
const fs = require('fs');
const load = Module._load;
Module._load = function (r, ...a) { return r === 'vscode' ? {} : load.call(this, r, ...a); };
const t = require(process.argv[2]);
const lstat = fs.lstatSync;
// the walk saw a file there; the FIFO took its place since
fs.lstatSync = (p, ...a) => lstat(String(p).endsWith('/x') ? process.argv[3] + '/plain' : p, ...a);
try { t.treeHash(process.argv[3]); console.log('hashed'); } catch (e) { console.log(e.message, !!e.changed); }
`);
  const fifoRun = require('child_process').spawnSync(process.execPath, [fifoChild, path.join(__dirname, '..', 'tests.js'), real(fifoDir)],
    { timeout: 5000, encoding: 'utf8' });
  check('a FIFO in place of a file is a change and never waits', (fifoRun.stdout || '').trim() === 'x is no longer a file true'
    || assert.fail(JSON.stringify([fifoRun.stdout, fifoRun.stderr, fifoRun.signal])));
  // keys are real paths with the on-disk case (a project opened as /x/caseproj is /x/CaseProj)
  const caseDir = path.join(tmp, 'CaseProj');
  fs.mkdirSync(path.join(caseDir, '.droid-lite', 'plugins', 'Cased'), { recursive: true });
  fs.writeFileSync(path.join(caseDir, '.droid-lite', 'plugins', 'Cased', 'plugin.json'), JSON.stringify({ name: 'Cased' }));
  const lower = path.join(tmp, 'caseproj');
  if (fs.existsSync(lower)) { // a case-insensitive disk (macOS default)
    const c = tests.readPlugins(lower).find((x) => x.name === 'Cased');
    check('keys use the on-disk case', c?.needsOk && c.project === real(caseDir) && c.project.endsWith('/CaseProj')
      && c.folder === path.join(real(caseDir), '.droid-lite', 'plugins', 'Cased'));
  } else console.log('skip: the disk is case-sensitive');
  // writes take <store>.lock, a file the app creates the same way (create_new): an Allow waits while
  // another process (the app) holds it, then keeps its entry
  const lock = `${store}.lock`;
  const P = real(project3);
  const before = readStore();
  fs.writeFileSync(lock, '');
  let done = false;
  const pending = tests.approve(P, '/x/a', 'ha').then(() => { done = true; });
  await sleep(300);
  check('an Allow waits for the lock', !done && !readStore().approved[P]['/x/a']);
  writeStore({ ...before, approved: { ...before.approved, [P]: { ...before.approved[P], '/x/b': 'hb' } } }); // the other process's write
  fs.unlinkSync(lock);
  await pending;
  check('and keeps what the lock holder wrote', readStore().approved[P]['/x/a'] === 'ha' && readStore().approved[P]['/x/b'] === 'hb' && !fs.existsSync(lock));
  // the lock holds the holder's token; the holder removes only its own lock (one taken away as stale
  // and made again by another process meanwhile is not its)
  const realRename = fs.renameSync;
  let heldToken;
  fs.renameSync = (a, b) => { realRename(a, b); if (b === store) { heldToken = fs.readFileSync(lock, 'utf8'); fs.writeFileSync(lock, '4242 another'); } };
  try { await tests.approve(P, '/x/t', 'ht'); } finally { fs.renameSync = realRename; }
  check('the holder removes only its own lock', new RegExp(`^${process.pid} [0-9a-f]{16}$`).test(heldToken)
    && fs.readFileSync(lock, 'utf8') === '4242 another' && readStore().approved[P]['/x/t'] === 'ht');
  fs.unlinkSync(lock);
  // two processes see one stale lock; the first takes it away and locks; the second, late, must
  // not delete that fresh lock
  fs.writeFileSync(lock, '1 old');
  fs.utimesSync(lock, new Date(Date.now() - 60000), new Date(Date.now() - 60000));
  const seen = fs.lstatSync(lock, { bigint: true });
  tests.removeStale(lock, seen);
  const firstTook = !fs.existsSync(lock);
  fs.writeFileSync(lock, '7 fresh', { flag: 'wx' });
  tests.removeStale(lock, seen);
  check('two takers of one stale lock never delete the fresh one', firstTook && fs.readFileSync(lock, 'utf8') === '7 fresh'
    && fs.readdirSync(path.dirname(lock)).every((n) => !n.includes('.stale.')));
  fs.unlinkSync(lock);
  // the stale lock is looked at again right before it is taken away: a fresh lock in its place is
  // never renamed (another process could lock in the gap)
  const oldLock = () => {
    fs.writeFileSync(lock, '1 old');
    fs.utimesSync(lock, new Date(Date.now() - 60000), new Date(Date.now() - 60000));
    return fs.lstatSync(lock, { bigint: true });
  };
  let seenOld = oldLock();
  fs.unlinkSync(lock);
  fs.writeFileSync(lock, '2 fresh');
  const steps = [];
  tests.removeStale(lock, seenOld, (st) => { steps.push(st); if (st === 'put back') fs.writeFileSync(lock, '3 third'); });
  check('a stale lock made fresh meanwhile is not touched', steps.length === 0 && fs.readFileSync(lock, 'utf8') === '2 fresh');
  fs.unlinkSync(lock);
  // renamed aside, it turns out to be another's fresh lock and a newer one took its place: it cannot
  // be put back, so it stays aside, never deleted
  seenOld = oldLock();
  const realError = console.error;
  console.error = () => {};
  try {
    tests.removeStale(lock, seenOld, (st) => {
      if (st === 'rename') { fs.unlinkSync(lock); fs.writeFileSync(lock, '2 fresh'); } else fs.writeFileSync(lock, '3 third');
    });
  } finally { console.error = realError; }
  const asideLocks = fs.readdirSync(path.dirname(lock)).filter((n) => n.includes('.stale.'));
  check('a fresh lock that cannot be put back is never deleted', fs.readFileSync(lock, 'utf8') === '3 third' && asideLocks.length === 1
    && fs.readFileSync(path.join(path.dirname(lock), asideLocks[0]), 'utf8') === '2 fresh');
  asideLocks.forEach((n) => fs.unlinkSync(path.join(path.dirname(lock), n)));
  fs.unlinkSync(lock);
  // a lock dated far in the future (a clock set back) is stale too
  fs.writeFileSync(lock, '1 skewed');
  fs.utimesSync(lock, new Date(Date.now() + 120000), new Date(Date.now() + 120000));
  const aheadT0 = Date.now();
  await tests.approve(P, '/x/ahead', 'hf');
  check('a lock 2 minutes ahead is stale', Date.now() - aheadT0 < 2000 && readStore().approved[P]['/x/ahead'] === 'hf' && !fs.existsSync(lock));
  // the store's other top-level keys are kept, as in the app
  writeStore({ ...readStore(), note: 'keep me' });
  await tests.approve(P, '/x/keys', 'hk');
  check('Allow keeps the store\'s other top-level keys', readStore().note === 'keep me' && readStore().approved[P]['/x/keys'] === 'hk');
  // whatever is in the way of the lock (an old folder that cannot be removed, a dangling link)
  // waits and gives up in time, never spins (a child process: a spin would freeze this one), and
  // is left alone
  const lockChild = path.join(tmp, 'lock-child.js');
  fs.writeFileSync(lockChild, `const Module = require('module');
const load = Module._load;
Module._load = function (r, ...a) { return r === 'vscode' ? {} : load.call(this, r, ...a); };
const t = require(process.argv[2]);
t.limits.lock = 300;
t.approve('/l', '/l/.droid-lite/plugins/p', 'h').then(() => console.log('allowed'), (e) => console.log(e.message));
`);
  const odd = (n, make) => {
    const cfgL = path.join(tmp, `lock-${n}`);
    fs.mkdirSync(cfgL);
    const l = path.join(cfgL, 'approved-plugins.json.lock');
    make(l);
    const t0 = Date.now();
    const r = require('child_process').spawnSync(process.execPath, [lockChild, path.join(__dirname, '..', 'tests.js')],
      { env: { ...process.env, DROID_LITE_CONFIG_DIR: cfgL }, timeout: 8000, encoding: 'utf8' });
    return { out: (r.stdout || '').trim(), ms: Date.now() - t0, left: (() => { try { fs.lstatSync(l); return true; } catch { return false; } })() };
  };
  for (const [n, make] of [
    ['dir', (l) => { fs.mkdirSync(l); fs.utimesSync(l, new Date(Date.now() - 60000), new Date(Date.now() - 60000)); }],
    ['link', (l) => fs.symlinkSync(path.join(tmp, 'nowhere'), l)],
    // a stale lock file in a folder that does not let it be renamed or removed
    ['stuck', (l) => {
      fs.writeFileSync(l, '1 old');
      fs.utimesSync(l, new Date(Date.now() - 60000), new Date(Date.now() - 60000));
      fs.chmodSync(path.dirname(l), 0o555);
    }],
  ]) {
    const r = odd(n, make);
    fs.chmodSync(path.join(tmp, `lock-${n}`), 0o755);
    check(`a lock in the way (${n}) is held, gives up in time, is left alone`,
      /is held by another Droid Lite; try again$/.test(r.out) && r.ms < 6000 && r.left || assert.fail(JSON.stringify(r)));
  }
  // a lock left by a process that died (older than 30 s) is removed
  fs.writeFileSync(lock, '');
  fs.utimesSync(lock, new Date(Date.now() - 60000), new Date(Date.now() - 60000));
  const staleT0 = Date.now();
  await tests.approve(P, '/x/c', 'hc');
  check('a stale lock is removed', Date.now() - staleT0 < 2000 && readStore().approved[P]['/x/c'] === 'hc' && !fs.existsSync(lock));
  // a lock that is never removed: it gives up after about 5 s and leaves the other's lock alone
  fs.writeFileSync(lock, '');
  const giveUpT0 = Date.now();
  const gaveUp = await tests.approve(P, '/x/z', 'hz').then(() => '', (e) => e.message);
  check('a held lock gives up after about 5 s', /is held by another Droid Lite; try again$/.test(gaveUp)
    && Date.now() - giveUpT0 >= 5000 && Date.now() - giveUpT0 < 7000 && fs.existsSync(lock) && !readStore().approved[P]['/x/z']);
  fs.unlinkSync(lock);
  // two Allows at once in one process, and two processes allowing 20 folders each: every entry kept
  await Promise.all([tests.approve(P, '/x/d', 'hd'), tests.approve('/y', '/y/.droid-lite/plugins/e', 'he')]);
  check('two interleaved Allows keep both', readStore().approved[P]['/x/d'] === 'hd' && readStore().approved['/y']['/y/.droid-lite/plugins/e'] === 'he');
  const child = path.join(tmp, 'allow-child.js');
  fs.writeFileSync(child, `const Module = require('module');
const load = Module._load;
Module._load = function (r, ...a) { return r === 'vscode' ? {} : load.call(this, r, ...a); };
const t = require(process.argv[2]);
(async () => { for (let i = 0; i < 20; i++) await t.approve('/two', '/two/.droid-lite/plugins/' + process.argv[3] + i, 'h'); })()
  .catch((e) => { console.error(e); process.exit(1); });
`);
  const codes = await Promise.all(['a', 'b'].map((k) => new Promise((resolve) => {
    require('child_process').spawn(process.execPath, [child, path.join(__dirname, '..', 'tests.js'), k], { stdio: 'inherit' }).on('exit', resolve);
  })));
  const two = readStore().approved['/two'] || {};
  check('two processes allowing at once keep every entry', codes.every((c) => c === 0) && Object.keys(two).length === 40
    && readStore().approved[P]['/x/a'] === 'ha');
  // $DROID_LITE_CONFIG_DIR is the config folder: user plugins and the store
  const cfg = path.join(tmp, 'cfg');
  fs.mkdirSync(path.join(cfg, 'plugins', 'mine'), { recursive: true });
  fs.writeFileSync(path.join(cfg, 'plugins', 'mine', 'plugin.json'), JSON.stringify({ name: 'Mine' }));
  const homeStore = fs.readFileSync(store, 'utf8');
  process.env.DROID_LITE_CONFIG_DIR = cfg;
  await api3.load();
  check('DROID_LITE_CONFIG_DIR: its plugins folder and store are read', roots().some((i) => i.label === 'Mine')
    && !roots().some((i) => i.label === 'Dup') && gateNeedsOk());
  await allowGate();
  check('and Allow writes there', okOf(readStore(path.join(cfg, 'approved-plugins.json')), project3, gateDir) === hashOf(gateDir)
    && fs.readFileSync(store, 'utf8') === homeStore && !gateNeedsOk());
  // an empty one counts as unset (as in the app): ~/.config/droid-lite
  process.env.DROID_LITE_CONFIG_DIR = '';
  await api3.load();
  check('an empty DROID_LITE_CONFIG_DIR counts as unset', roots().some((i) => i.label === 'Dup') && !roots().some((i) => i.label === 'Mine'));
  delete process.env.DROID_LITE_CONFIG_DIR;
  ctx3.subscriptions.forEach((d) => d.dispose?.());
  await until('no tracked process', () => tests.procs.size === 0, 8000).catch(() => {});
  check('no plugin process left after the OK checks', tests.procs.size === 0);

  // a project plugin's folder is checked again while a command of it runs: a running watch stops
  // when a file of the folder changes, and the plugin asks for the OK again; when its folder can no
  // longer be checked (too big), it is an error with no OK to give. Its commands get
  // PYTHONDONTWRITEBYTECODE (Python writes no __pycache__ into the folder)
  const projectW = path.join(tmp, 'projectW');
  const wDir = path.join(projectW, '.droid-lite', 'plugins', 'w');
  const wPid = path.join(tmp, 'w-pid');
  fs.mkdirSync(wDir, { recursive: true });
  fs.writeFileSync(path.join(wDir, 'plugin.json'), JSON.stringify({ name: 'W',
    list: ['sh', '-c', 'echo "[{\\"id\\":\\"c\\",\\"label\\":\\"py=$PYTHONDONTWRITEBYTECODE\\"}]"'],
    watch: ['sh', '-c', 'echo $$ > "$0"; while :; do echo "{}"; sleep 0.2; done', wPid] }));
  approve(wDir);
  tests.limits.recheck = 300;
  const ctxW = { subscriptions: [] };
  const apiW = tests.setup(ctxW, () => projectW);
  await apiW.loaded;
  const wId = real(wDir);
  check('a project plugin\'s commands get PYTHONDONTWRITEBYTECODE=1', ctrl.items.get(`plugin:${wId}`)?.children.get(`${wId}\u0000c`)?.label === 'py=1');
  visibility({ visible: true });
  await until('the W watch', () => pidsIn(wPid).length === 1);
  const wWatch = pidsIn(wPid)[0];
  fs.writeFileSync(path.join(wDir, 'notes.txt'), 'new');
  await until('the check', () => ctrl.items.get(`needs-ok:${wId}`), 3000).catch(() => {});
  await until('the W watch to end', () => !alive(wWatch), 3000).catch(() => {});
  check('a running watch stops when its folder changes, and the plugin asks again', !alive(wWatch)
    && ctrl.items.get(`needs-ok:${wId}`)?.error.startsWith('W changed since you allowed it; allow it again'));
  fake.answer = 'Allow';
  fs.rmSync(wPid, { force: true });
  await commands['droidLite.allowTestPlugin']({ folder: wId }); // its load starts the watch again
  await until('the W watch again', () => pidsIn(wPid).length === 1);
  const wWatch2 = pidsIn(wPid)[0];
  fs.writeFileSync(path.join(wDir, 'blob'), Buffer.alloc((8 << 20) + 1));
  await until('the check', () => ctrl.items.get(`plugin:${wId}`)?.error, 3000).catch(() => {});
  await until('the W watch to end', () => !alive(wWatch2), 3000).catch(() => {});
  fake.asked = undefined;
  await commands['droidLite.allowTestPlugin']({ folder: wId });
  check('a folder that can no longer be checked is an error, with no OK to give', !alive(wWatch2)
    && /w\/plugin\.json: W changed since you allowed it and cannot run now \(too big to check\)$/.test(ctrl.items.get(`plugin:${wId}`)?.error)
    && !ctrl.items.get(`needs-ok:${wId}`) && !fake.asked
    && (await tree.getChildren()).some((r) => r.iconPath?.id === 'warning' && /cannot run now/.test(r.label)));
  ctxW.subscriptions.forEach((d) => d.dispose?.());
  await until('no tracked process', () => tests.procs.size === 0, 8000).catch(() => {});
  check('no plugin process left after the watch checks', tests.procs.size === 0);

  // an allowed plugin whose folder looks as it did at the last full read is not read again: the
  // load hashes it once (the spawn of its list right after reads nothing), and a check reads it
  // again only when a file looks changed
  const projectS = path.join(tmp, 'projectS');
  const sDir = path.join(projectS, '.droid-lite', 'plugins', 's');
  fs.mkdirSync(sDir, { recursive: true });
  fs.writeFileSync(path.join(sDir, 'plugin.json'), JSON.stringify({ name: 'S', list: ['sh', '-c', 'echo []'] }));
  fs.writeFileSync(path.join(sDir, 'l.sh'), 'echo []\n');
  approve(sDir);
  let reads = 0;
  const countReads = (f) => {
    const before = fs.readSync;
    reads = 0;
    fs.readSync = (...a) => { reads++; return before(...a); };
    try { return f(); } finally { fs.readSync = before; }
  };
  const sPlugin = await later(() => tests.readPlugins(projectS).find((x) => x.name === 'S'));
  const oneHash = (countReads(() => tests.treeHash(real(sDir))), reads);
  countReads(() => [1, 2, 3].map(() => tests.changed(sPlugin)));
  const unchangedReads = reads;
  fs.writeFileSync(path.join(sDir, 'l.sh'), 'echo []\n'); // the same bytes again
  const same = await later(() => countReads(() => tests.changed(sPlugin)));
  const sameReads = reads;
  countReads(() => tests.changed(sPlugin));
  const afterReads = reads;
  check('a check reads the folder again only when a file looks changed', oneHash > 0 && unchangedReads === 0 && same === undefined
    && sameReads === oneHash && afterReads === 0);
  fs.writeFileSync(path.join(sDir, 'l.sh'), 'rm -r ~\n');
  check('and other bytes are refused', /S changed since you allowed it; allow it again/.test(tests.changed(sPlugin)?.why));
  fs.writeFileSync(path.join(sDir, 'l.sh'), 'echo []\n');
  approve(sDir);
  const ctxS = { subscriptions: [] };
  const realReadSync2 = fs.readSync;
  let loadReads = 0;
  fs.readSync = (...a) => { loadReads++; return realReadSync2(...a); };
  try {
    await later(async () => {
      const apiS = tests.setup(ctxS, () => projectS);
      await apiS.loaded;
    });
  } finally { fs.readSync = realReadSync2; }
  countReads(() => tests.readPlugins(projectS));
  const readOnly = reads; // the plugins read (one hash of S), and no list spawned
  check('a load hashes an allowed plugin once', (loadReads === readOnly && readOnly < 2 * oneHash)
    || assert.fail(`${loadReads} reads; reading the plugins alone is ${readOnly}, one hash ${oneHash}`));
  ctxS.subscriptions.forEach((d) => d.dispose?.());
  // a file gone or locked for a moment is a change that may be allowed again, never broken
  fs.chmodSync(path.join(sDir, 'l.sh'), 0o000);
  const locked = tests.changed(tests.readPlugins(projectS).find((x) => x.name === 'S') || sPlugin);
  fs.chmodSync(path.join(sDir, 'l.sh'), 0o644);
  const sNow = tests.readPlugins(projectS).find((x) => x.name === 'S');
  fs.renameSync(sDir, `${sDir}-away`);
  const gone = tests.changed(sNow);
  fs.renameSync(`${sDir}-away`, sDir);
  check('a file locked or a folder gone for a moment asks again, not broken', /allow it again$/.test(locked?.why) && !locked.broken
    && /allow it again$/.test(gone?.why) && !gone.broken && tests.changed(sNow) === undefined);
  // a plugin folder named `..x` is inside the plugins folder (path components, as in the app)
  const projectD = path.join(tmp, 'projectD');
  fs.mkdirSync(path.join(projectD, '.droid-lite', 'plugins', '..x'), { recursive: true });
  fs.writeFileSync(path.join(projectD, '.droid-lite', 'plugins', '..x', 'plugin.json'), JSON.stringify({ name: 'Dots' }));
  const dots = tests.readPlugins(projectD).find((x) => x.folder.endsWith('..x'));
  check('a plugin folder named ..x is inside', dots?.needsOk && dots.name === 'Dots' || assert.fail(JSON.stringify(dots)));
  // ~/ at the start of a command is the home folder, as in the app
  fs.writeFileSync(path.join(home, 'droid-lite-list.sh'), '#!/bin/sh\necho \'[{"id":"h","label":"from home"}]\'\n', { mode: 0o755 });
  const tilde = { name: 'T', folder: tmp, cwd: tmp, spec: {} };
  const homeCases = await tests.json(['~/droid-lite-list.sh'], tilde, project);
  check('~/ in a command is the home folder', homeCases?.[0]?.id === 'h');

  // racy-git rule: a sig taken in the tick of a write is not kept. On a volume with coarse stamps
  // (played here: lstat gives times in steps of 10^15 ns) a same-size rewrite in that tick, mtime set
  // back, changes no stamp at all, and must still be found
  const projectR = path.join(tmp, 'projectR');
  const rDir = path.join(projectR, '.droid-lite', 'plugins', 'r');
  fs.mkdirSync(rDir, { recursive: true });
  fs.writeFileSync(path.join(rDir, 'plugin.json'), JSON.stringify({ name: 'R', list: ['sh', '-c', 'echo []'] }));
  fs.writeFileSync(path.join(rDir, 'l.sh'), 'echo []\n');
  approve(rDir);
  const plainLstat = fs.lstatSync;
  const step = 1000000000000000n;
  fs.lstatSync = (...a) => {
    const st = plainLstat(...a);
    if (st && typeof st.mtimeNs === 'bigint') {
      st.mtimeNs -= st.mtimeNs % step;
      st.ctimeNs -= st.ctimeNs % step;
    }
    return st;
  };
  const nowR = Date.now;
  let rRacy;
  let rAfter;
  try {
    const tick = tests.treeStat(real(rDir)).newest;
    Date.now = () => Number((tick + 500000000n) / 1000000n); // read 0.5 s into the tick
    rRacy = tests.readPlugins(projectR).find((x) => x.name === 'R');
    const lsh = path.join(rDir, 'l.sh');
    const { mtime } = fs.statSync(lsh);
    fs.writeFileSync(lsh, 'rm -r ~\n');
    fs.utimesSync(lsh, mtime, mtime);
    check('a same-size rewrite in the tick keeps every coarse stamp', tests.treeStat(real(rDir)).newest === tick);
    rAfter = tests.changed(rRacy);
  } finally { fs.lstatSync = plainLstat; Date.now = nowR; }
  check('a sig taken in the tick of a write is not kept, and the rewrite is found', rRacy?.hash && rRacy.sig === ''
    && /R changed since you allowed it; allow it again/.test(rAfter?.why));
  const settledNow = BigInt(Date.now()) * 1000000n;
  check('a stamp within 2 s of the read, or after it, is racy', tests.settled('s', settledNow - 1000000000n, settledNow) === ''
    && tests.settled('s', settledNow + 5n, settledNow) === '' && tests.settled('s', settledNow - 3000000000n, settledNow) === 's');
  // the sig holds each file's change time: a same-size rewrite whose mtime is set back still changes it
  // (its mtime a whole second, so utimes can set it back exactly)
  const lsh2 = path.join(rDir, 'l.sh');
  const mtime2 = Math.floor(Date.now() / 1000) - 100;
  fs.writeFileSync(lsh2, 'echo []\n');
  fs.utimesSync(lsh2, mtime2, mtime2);
  approve(rDir);
  const rPlugin = await later(() => tests.readPlugins(projectR).find((x) => x.name === 'R'));
  fs.writeFileSync(lsh2, 'rm -r ~\n');
  fs.utimesSync(lsh2, mtime2, mtime2);
  check('a same-size rewrite with its mtime set back is found by the change time', rPlugin?.sig
    && /R changed since you allowed it; allow it again/.test(tests.changed(rPlugin)?.why));

  // Add: a different tool with a name already taken gets -2, one already added is not added twice, a
  // dangling link at the name is skipped (as in the app)
  const userPlugins = path.join(home, '.config', 'droid-lite', 'plugins');
  const toolA = path.join(tmp, 'toolA');
  const toolB = path.join(tmp, 'toolB', 'droid-lite-plugin');
  fs.mkdirSync(toolA, { recursive: true });
  fs.mkdirSync(toolB, { recursive: true });
  fs.writeFileSync(path.join(toolA, 'plugin.json'), JSON.stringify({ name: 'Same Tool' }));
  fs.writeFileSync(path.join(toolB, 'plugin.json'), JSON.stringify({ name: 'Same Tool' }));
  const linkOf = (n) => { try { return fs.readlinkSync(path.join(userPlugins, n)); } catch { return undefined; } };
  fake.err = '';
  fake.pick = toolA;
  await commands['droidLite.addTestPlugin']();
  fake.pick = path.dirname(toolB);
  await commands['droidLite.addTestPlugin']();
  check('a different tool with the same name is added as -2', linkOf('same-tool') === real(toolA) && linkOf('same-tool-2') === real(toolB));
  fake.asked = undefined;
  fake.pick = toolA;
  await commands['droidLite.addTestPlugin']();
  check('a tool already added is not added twice', /Same Tool is already added/.test(fake.asked?.m) && !linkOf('same-tool-3'));
  const toolC = path.join(tmp, 'toolC');
  fs.mkdirSync(toolC);
  fs.writeFileSync(path.join(toolC, 'plugin.json'), JSON.stringify({ name: 'Dang' }));
  fs.symlinkSync(path.join(tmp, 'gone'), path.join(userPlugins, 'dang'));
  fake.pick = toolC;
  await commands['droidLite.addTestPlugin']();
  check('a dangling link at the name is skipped, no throw', linkOf('dang-2') === real(toolC) && linkOf('dang') === path.join(tmp, 'gone') && !fake.err);
  fs.writeFileSync(path.join(toolC, 'plugin.json'), JSON.stringify({ name: 'Bad', run: 'x' }));
  await commands['droidLite.addTestPlugin']();
  check('a plugin.json the app refuses is not added', /run must be a list of strings/.test(fake.err) && !linkOf('bad'));
  for (const n of ['same-tool', 'same-tool-2', 'dang', 'dang-2']) fs.unlinkSync(path.join(userPlugins, n));

  fs.rmSync(tmp, { recursive: true, force: true });
  console.log(`ok: test plugins, ${checks} checks`);
})().catch((e) => {
  console.error(e);
  // a failed check leaves no plugin process behind
  tests.procs.forEach((p) => { try { process.kill(-p.pid, 'SIGKILL'); } catch { /* ended */ } });
  pidsIn(path.join(quickState, 'qwatch')).forEach((pid) => { try { process.kill(pid, 'SIGKILL'); } catch { /* ended */ } });
  pidsIn(path.join(tmp, 'w-pid')).forEach((pid) => { try { process.kill(pid, 'SIGKILL'); } catch { /* ended */ } });
  for (const f of ['pids', 'left', 'watch']) {
    pidsIn(path.join(toolsState, f)).forEach((pid) => { try { process.kill(pid, 'SIGKILL'); } catch { /* ended */ } });
  }
  process.exit(1);
});
