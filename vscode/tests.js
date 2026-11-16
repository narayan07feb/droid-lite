// Droid Lite test plugins (docs/PLUGINS.md) in VS Code's Test Explorer: the same plugin.json files
// the app reads (<config>/plugins/<name>/ and <project>/.droid-lite/plugins/<name>/), so a plugin
// added in either one shows in both. Cases come from `list`, runs from `run` (JSON lines), extra
// lists (`views`), buttons (`actions`), `links` and the live state (`watch`) go in the Droid Lite
// sidebar. A project plugin runs nothing until the user allows it once for that project (shared
// with the app: <config>/approved-plugins.json, see `approve`); any change to a file in its folder
// asks again, checked before every command it runs. The user's own plugins need no OK. <config> is
// $DROID_LITE_CONFIG_DIR (when set and not empty, as in the app), else ~/.config/droid-lite.
const vscode = require('vscode');
const cp = require('child_process');
const crypto = require('crypto');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { EventEmitter } = require('events');
const { PassThrough } = require('stream');

const configDir = () => process.env.DROID_LITE_CONFIG_DIR || path.join(os.homedir(), '.config', 'droid-lite');
const userDir = () => path.join(configDir(), 'plugins');
const storeFile = () => path.join(configDir(), 'approved-plugins.json');
const realpath = (p) => fs.realpathSync.native(p); // the on-disk case, like the app
const isObject = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
const inside = (dir, p) => { const r = path.relative(dir, p); return !!r && !r.startsWith('..') && !path.isAbsolute(r); };
// what a trust OK covers (the Allow modal and "Needs your OK")
const TRUST = "Allowing trusts this plugin's folder. Commands may also run project code outside it (for example a script in the repo); changes to that code do not ask again.";
const MAX_TREE_FILES = 2000;
const MAX_TREE_BYTES = 8 << 20;

const utf8 = new TextDecoder('utf-8', { fatal: true });
// a file name as the tree hash takes it: refused when it is not UTF-8 (the app says the same)
function utf8Name(n) {
  try { utf8.decode(n); } catch { throw new Error(`${n.toString('utf8')} is not a UTF-8 name`); }
  return n;
}

// at most `max` bytes of the open file `fd` (a file that grew since the walk is never read whole)
function readUpTo(fd, max) {
  const chunks = [];
  const buf = Buffer.alloc(Math.min(max, 64 << 10));
  let n = 0;
  for (let r; n < max && (r = fs.readSync(fd, buf, 0, Math.min(buf.length, max - n), null)) > 0; n += r) {
    chunks.push(Buffer.from(buf.subarray(0, r)));
  }
  return Buffer.concat(chunks, n);
}

// the tree hash of a plugin folder (shared with the app, byte for byte): SHA-256 over every regular
// file under it, sorted by relative path ('/' separators) compared as UTF-8 bytes; per file its
// path, one 0 byte, its length (8 bytes, big-endian), its permission bits (mode & 0o777, 2 bytes,
// big-endian) and its bytes. Lowercase hex. A link anywhere inside, a name that is not UTF-8, or
// more than 2000 files or 8 MiB, throws. Also gives the plugin.json bytes it hashed, so what the
// user sees is what the OK covers.
function treeHash(folder) {
  const files = []; // [relative path, absolute path], as bytes
  let total = 0;
  const walk = (abs, rel) => {
    for (const n of fs.readdirSync(abs, { encoding: 'buffer' }).map(utf8Name)) {
      const a = Buffer.concat([abs, Buffer.from(path.sep), n]);
      const r = rel ? Buffer.concat([rel, Buffer.from('/'), n]) : n;
      const st = fs.lstatSync(a);
      if (st.isSymbolicLink()) throw new Error('contains links');
      if (st.isDirectory()) walk(a, r);
      else if (st.isFile() && (files.push([r, a]) > MAX_TREE_FILES || (total += st.size) > MAX_TREE_BYTES)) {
        throw new Error('too big to check');
      }
    }
  };
  walk(Buffer.from(folder), null);
  files.sort(([a], [b]) => Buffer.compare(a, b));
  const h = crypto.createHash('sha256');
  let manifest;
  total = 0;
  for (const [r, a] of files) {
    let fd;
    try { fd = fs.openSync(a, fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW); } catch (e) {
      throw e.code === 'ELOOP' ? new Error('contains links') : e;
    }
    let bytes;
    let mode;
    try {
      mode = fs.fstatSync(fd).mode & 0o777;
      bytes = readUpTo(fd, MAX_TREE_BYTES - total + 1);
    } finally { fs.closeSync(fd); }
    if ((total += bytes.length) > MAX_TREE_BYTES) throw new Error('too big to check'); // grew since the walk
    const head = Buffer.alloc(10);
    head.writeBigUInt64BE(BigInt(bytes.length));
    head.writeUInt16BE(mode, 8);
    h.update(r).update(Buffer.of(0)).update(head).update(bytes);
    if (r.toString() === 'plugin.json') manifest = bytes;
  }
  return { hash: h.digest('hex'), manifest };
}

// the store: {"version":2,"approved":{"<project real path>":{"<plugin folder real path>":"<tree hash>"}}}.
// Any other shape (an old flat map too) allows none. `strict`: a file that is not a JSON object
// throws (a write never drops what the app saved); another object is replaced on write.
function readStore(strict = false) {
  const empty = { version: 2, approved: {} };
  let store;
  try { store = JSON.parse(fs.readFileSync(storeFile(), 'utf8')); } catch (e) {
    if (e.code === 'ENOENT' || !strict) return empty;
    throw new Error(`${storeFile()}: ${e.message}`);
  }
  if (!isObject(store)) {
    if (strict) throw new Error(`${storeFile()} is not a JSON object`);
    return empty;
  }
  // the number 2 (2.0 too, as the app reads it; never the text "2")
  return store.version === 2 && isObject(store.approved) ? store : empty;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
// takes away the stale lock file `lock` that `seen` (an lstat with bigint) described: renamed to a
// name of its own first, then deleted only when what was renamed is still that file. Another
// process may have taken the stale one away and made a fresh lock meanwhile: that one is put back
// (unless a newer lock already took its place), so two processes taking away one stale lock never
// delete a fresh one. A new file may get a freed inode number again: its time tells it apart.
function removeStale(lock, seen) {
  const aside = `${lock}.stale.${process.pid}.${crypto.randomBytes(8).toString('hex')}`;
  try { fs.renameSync(lock, aside); } catch { return; }
  let now;
  try { now = fs.lstatSync(aside, { bigint: true }); } catch { /* gone */ }
  if (!(now && now.dev === seen.dev && now.ino === seen.ino && now.mtimeNs === seen.mtimeNs)) {
    try { fs.linkSync(aside, lock); } catch { /* a newer lock took its place */ }
  }
  try { fs.unlinkSync(aside); } catch { /* gone */ }
}

// runs `f` holding <store>.lock (created with 'wx', holding this holder's token: its pid and a
// random number; the app takes the same lock), waiting up to limits.lock ms. Every try that fails
// waits and checks the time, whatever is in the way: a lock file older than 30 s is from a process
// that died and is taken away (removeStale); anything else (a fresh lock, a folder, a link, a stale
// lock that could not be taken away) is held. The lock is removed after only while it holds this
// token.
async function locked(f) {
  const lock = `${storeFile()}.lock`;
  const token = `${process.pid} ${crypto.randomBytes(8).toString('hex')}`;
  fs.mkdirSync(path.dirname(lock), { recursive: true });
  for (let wait = 10, end = Date.now() + limits.lock; ; wait = Math.min(wait * 2, 250)) {
    let fd;
    try { fd = fs.openSync(lock, 'wx', 0o600); } catch (e) {
      if (e.code !== 'EEXIST') throw e;
    }
    if (fd !== undefined) {
      try { fs.writeSync(fd, token); } catch (e) {
        try { fs.unlinkSync(lock); } catch { /* gone */ }
        throw e;
      } finally { fs.closeSync(fd); }
      break;
    }
    let st;
    try { st = fs.lstatSync(lock, { bigint: true }); } catch { /* gone meanwhile */ }
    if (st?.isFile() && Date.now() - Number(st.mtimeMs) > 30000) removeStale(lock, st);
    if (Date.now() > end) throw new Error(`${lock} is held by another Droid Lite; try again`);
    await sleep(wait);
  }
  try { return f(); } finally {
    try { if (fs.readFileSync(lock, 'utf8') === token) fs.unlinkSync(lock); } catch { /* gone */ }
  }
}

// allows the plugin folder `folder` of project `project` (both real paths) with this tree hash,
// keeping every other entry: read, changed and written under the lock, through a 0600 temp file
// and a rename, so a reader (the app) never sees half a file
function approve(project, folder, hash) {
  return locked(() => {
    const file = storeFile();
    const store = readStore(true);
    const out = { version: 2, approved: store.approved };
    if (!isObject(out.approved[project])) out.approved[project] = {};
    out.approved[project][folder] = hash;
    const tmp = `${file}.${process.pid}.${crypto.randomBytes(6).toString('hex')}.tmp`;
    try {
      fs.writeFileSync(tmp, `${JSON.stringify(out, null, 2)}\n`, { mode: 0o600, flag: 'wx' });
      fs.renameSync(tmp, file);
    } catch (e) {
      try { fs.unlinkSync(tmp); } catch { /* not written */ }
      throw e;
    }
  });
}

// a project plugin as it is on disk now: { real, hash, manifest } or a thrown reason it can never
// run (its real folder is not inside the project's real .droid-lite/plugins, a link inside it, too big)
function inspect(project, folder) {
  const real = realpath(folder);
  const plugins = realpath(path.join(project, '.droid-lite', 'plugins'));
  if (!inside(realpath(project), plugins) || !inside(plugins, real)) throw new Error('links outside the project');
  return { real, ...treeHash(real) };
}

// why a loaded project plugin may not run now: its folder (read again, the same checks) is not the
// one allowed at load, as in the app: { why }, or { why, broken: true } when it can no longer be
// checked at all (links outside, too big) and so never run or be allowed; undefined when it is
// unchanged, and for the user's own plugins
function changed(plugin) {
  if (!plugin.hash) return undefined;
  try {
    if (inspect(plugin.project, plugin.folder).hash === plugin.hash) return undefined;
  } catch (e) {
    return { why: `${plugin.name} changed since you allowed it and cannot run now (${e.message})`, broken: true };
  }
  return { why: `${plugin.name} changed since you allowed it; allow it again` };
}
const MAX_LOG = 20000; // lines kept per run in the test output
const MAX_LOG_CHARS = 8 << 20; // and characters (about 8 MB) in all
const MAX_LINE = 64 << 10; // longest line kept (chars); the rest of a longer one is dropped
// ms: list/view time, output after exit, TERM -> KILL, the store's lock wait, the check of a project
// plugin's folder while a command of it runs
const limits = { json: 120000, grace: 500, kill: 5000, lock: 5000, recheck: 3000 };
const procs = new Set(); // every plugin process group still running (runs, actions, watch, list, views)

// plugin.json of `dir`, or of its droid-lite-plugin/ folder
function findPluginJson(dir) {
  return [path.join(dir, 'plugin.json'), path.join(dir, 'droid-lite-plugin', 'plugin.json')]
    .find((p) => fs.existsSync(p));
}

// why a plugin.json cannot be used, or undefined: every command is a list of strings (a plain
// string would fail later, inside a run or a watcher), links open only http(s) pages
function problem(spec) {
  const argv = (v) => Array.isArray(v) && v.length > 0 && v.every((a) => typeof a === 'string');
  if (!spec || typeof spec !== 'object' || Array.isArray(spec)) return 'not a JSON object';
  if (typeof spec.name !== 'string' || !spec.name) return 'name must be a string';
  if (spec.dir !== undefined && typeof spec.dir !== 'string') return 'dir must be a string';
  for (const k of ['list', 'run', 'watch']) {
    if (spec[k] !== undefined && !argv(spec[k])) return `${k} must be a list of strings`;
  }
  for (const k of ['views', 'actions', 'links']) {
    if (spec[k] === undefined) continue;
    if (!Array.isArray(spec[k]) || !spec[k].every((x) => x && typeof x === 'object' && typeof x.name === 'string')) {
      return `${k} must be a list of objects with a name`;
    }
    for (const x of spec[k]) {
      if (k === 'links' && !(typeof x.url === 'string' && /^https?:\/\//i.test(x.url))) return `links: ${x.name}: url must be an http or https link`;
      if (k !== 'links' && !argv(x.run)) return `${k}: ${x.name}: run must be a list of strings`;
    }
  }
}

// true while a run may still show one more line (MAX_LOG lines, MAX_LOG_CHARS characters)
function budget() {
  let n = 0;
  let chars = 0;
  return (line) => ++n <= MAX_LOG && (chars += line.length) <= MAX_LOG_CHARS;
}

// every plugin: { name, folder, cwd, spec }, { error, folder } or, a project plugin not allowed
// for this project (yet, or since a file in its folder changed), { name, folder, project, file,
// needsOk } with no spec, so nothing of it can run; `folder` (its real path) is its id, two plugins
// may share a name. A project plugin that links outside the project's .droid-lite/plugins, holds a
// link or is too big to check is an error: it can never run or be allowed.
function readPlugins(project) {
  const roots = [userDir(), project && path.join(project, '.droid-lite', 'plugins')].filter(Boolean);
  const approved = readStore().approved;
  const out = [];
  for (const root of roots) {
    const own = root === userDir();
    let names = [];
    try { names = fs.readdirSync(root); } catch { continue; }
    for (const n of names.sort()) {
      const folder = path.join(root, n);
      const file = path.join(folder, 'plugin.json');
      if (!fs.existsSync(file)) continue;
      let real = folder;
      try {
        real = realpath(folder);
        let bytes;
        let ok = own;
        let key;
        let hash;
        if (own) bytes = fs.readFileSync(file);
        else {
          key = realpath(project);
          const seen = inspect(project, folder);
          bytes = seen.manifest;
          if (!bytes) throw new Error('not a file');
          hash = seen.hash;
          ok = approved[key]?.[real] === hash;
        }
        const spec = JSON.parse(bytes.toString('utf8'));
        const bad = problem(spec);
        if (bad) throw new Error(bad);
        if (!ok) {
          out.push({ name: spec.name, folder: real, project: key, file, needsOk: true });
          continue;
        }
        const dir = spec.dir ? spec.dir.replace(/^~\//, `${os.homedir()}/`) : '.';
        // a project plugin keeps what it was allowed under: spawn checks it before every command
        out.push({ name: spec.name, folder: real, cwd: path.resolve(real, dir), spec, ...(hash && { project: key, hash }) });
      } catch (e) {
        out.push({ error: `${file}: ${e.message}`, folder: real });
      }
    }
  }
  return out;
}

// an argv with {plugin}, {project} and {id} filled in
function argv(cmd, plugin, project, id = '') {
  return cmd.map((a) => a.replaceAll('{plugin}', plugin.folder).replaceAll('{project}', project || '')
    .replaceAll('{id}', id));
}

// runs a command in its own process group (Stop reaches the tools it started), stdin empty.
// `onEnd(code, error)` runs once: after the exit plus a short grace for the last output (a tool
// it left running may hold the pipes), or when it could not start. A project plugin whose folder
// changed since it was allowed (`changed`) starts nothing: it fails like a missing command and
// `plugin.revoke` stops what of it still runs and marks it Needs your OK (or an error). A project
// plugin's Python writes no __pycache__ into its folder (that would ask for the OK again).
function spawn(cmd, plugin, project, onEnd = () => {}) {
  const [bin, ...args] = Array.isArray(cmd) ? cmd.map(String) : [];
  let proc;
  try {
    const change = changed(plugin);
    if (change) {
      plugin.revoke?.(change);
      throw new Error(change.why);
    }
    proc = cp.spawn(bin, args, {
      cwd: plugin.cwd, detached: true, stdio: ['ignore', 'pipe', 'pipe'],
      env: { ...process.env, DROID_LITE_PROJECT: project || '', ...(plugin.hash && { PYTHONDONTWRITEBYTECODE: '1' }) },
    });
  } catch (err) { // no command at all (or refused): fails like a missing one
    proc = Object.assign(new EventEmitter(), { stdout: new PassThrough(), stderr: new PassThrough() });
    proc.stdout.end();
    proc.stderr.end();
    process.nextTick(() => { proc.emit('error', err); proc.emit('close'); });
  }
  let ended = false;
  const end = (code, err) => { if (!ended) { ended = true; onEnd(code, err); } };
  proc.plugin = plugin;
  procs.add(proc);
  proc.on('close', () => procs.delete(proc));
  proc.on('error', (err) => end(undefined, err));
  proc.on('exit', (code, signal) => setTimeout(() => {
    proc.stdout.destroy();
    proc.stderr.destroy();
    end(code ?? signal);
  }, limits.grace));
  return proc;
}

// SIGTERM to the whole group, SIGKILL after limits.kill if any of it is still there. `final` (the
// extension goes away): a group already sent SIGTERM gets SIGKILL now, and the SIGKILL timer of
// the others keeps the host's event loop up until it fires (an unref'd one is lost when the host exits)
function stop(proc, final = false) {
  if (!proc.pid) return;
  const signal = (s) => { try { process.kill(-proc.pid, s); return true; } catch { return false; /* ended */ } };
  if (final && proc.stopping) { signal('SIGKILL'); return; }
  if (!signal('SIGTERM')) return;
  proc.stopping = true;
  const t = setTimeout(() => signal('SIGKILL'), limits.kill);
  if (!final) t.unref();
}

// lines of a stream; a line longer than MAX_LINE is cut there (memory stays bounded)
function lines(stream, onLine) {
  let buf = '';
  let cut = false; // dropping the rest of a long line
  stream.setEncoding('utf8');
  stream.on('data', (d) => {
    const parts = (buf + d).split(/\r?\n/);
    buf = parts.pop();
    for (const l of parts) {
      if (!cut) onLine(l);
      cut = false;
    }
    if (buf.length > MAX_LINE) {
      if (!cut) onLine(`${buf.slice(0, MAX_LINE)} … [line cut]`);
      buf = '';
      cut = true;
    }
  });
  stream.on('end', () => { if (buf && !cut) onLine(buf); });
}

// stdout of a command as JSON, with a time limit that ends its whole process group
function json(cmd, plugin, project, ms = limits.json) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    let why; // set when it was stopped
    const proc = spawn(cmd, plugin, project, (code, err) => {
      clearTimeout(timer);
      if (err || why) return reject(err || new Error(why));
      try { resolve(JSON.parse(Buffer.concat(chunks).toString('utf8'))); } catch {
        reject(new Error(code ? `exit code ${code}` : 'no JSON from the plugin'));
      }
    });
    const timer = setTimeout(() => { why = `no answer in ${ms / 1000} s`; stop(proc); }, ms);
    proc.stdout.on('data', (d) => {
      size += d.length;
      if (size <= 32 << 20) chunks.push(d);
      else if (!why) { why = 'more than 32 MB of output'; stop(proc); }
    });
    proc.stderr.resume();
  });
}

// a place in a file, or none when `file` or `line` is not usable
function location(project, file, line) {
  if (typeof file !== 'string' || !file) return undefined;
  const n = line === undefined || line === null ? 1 : Number(line);
  if (!Number.isFinite(n)) return undefined;
  const uri = vscode.Uri.file(path.isAbsolute(file) ? file : path.join(project || '', file));
  return new vscode.Location(uri, new vscode.Position(Math.max(Math.floor(n) - 1, 0), 0));
}

// projectOf(): the project folder, read again on each load
function setup(ctx, projectOf) {
  let project = projectOf();
  const ctrl = vscode.tests.createTestController('droidLite.tests', 'Droid Lite tests');
  const sidebar = new vscode.EventEmitter();
  let plugins = [];
  const meta = new Map(); // test item id -> { plugin, caseId }
  // keyed by the plugin's folder (its id), never its name
  const live = new Map(); // plugin -> latest watch state
  const pluginRows = new Map(); // plugin -> its sidebar row (to refresh only that row)
  const viewCache = new Map(); // plugin + view name -> its entries until that view is refreshed
  const actions = new Map(); // plugin + action name -> its running process
  let watchers = [];
  let watching = false; // the sidebar shows
  const fireRow = (folder) => { const r = pluginRows.get(folder); if (r) sidebar.fire(r); };

  const addCases = (plugin, parent, cases) => {
    for (const c of cases || []) {
      const id = `${plugin.folder}\u0000${c.id ?? c.label}`;
      const loc = location(project, c.file, c.line);
      const item = ctrl.createTestItem(id, c.label || c.id, loc?.uri);
      if (loc) item.range = new vscode.Range(loc.range.start, loc.range.start);
      item.description = c.detail;
      meta.set(id, { plugin, caseId: c.id });
      parent.add(item);
      addCases(plugin, item.children, c.children);
    }
  };

  // a project plugin not allowed: right-click > Allow… (package.json: testing/item/context)
  const needsOkItem = (p) => {
    const root = ctrl.createTestItem(`needs-ok:${p.folder}`, p.name || path.basename(p.folder));
    root.description = 'Needs your OK';
    root.error = `${p.why ? `${p.why}. ` : ''}Needs your OK: this project plugin runs nothing until you allow it (right-click > Allow…, or Allow… under Test plugins in the Droid Lite sidebar). ${TRUST} ${p.folder}`;
    return root;
  };
  // a loaded project plugin whose folder changed since the OK (found by spawn or the periodic
  // check): what of it still runs stops, and it needs the OK again, or is an error (no OK to give)
  // when its folder can no longer be checked
  const revoke = (p, { why, broken }) => {
    if (p.needsOk || p.error || !plugins.includes(p)) return;
    procs.forEach((x) => { if (x.plugin === p) stop(x); });
    const file = path.join(p.folder, 'plugin.json');
    delete p.spec;
    live.delete(p.folder);
    pluginRows.delete(p.folder);
    ctrl.items.delete(`plugin:${p.folder}`);
    if (broken) {
      p.error = `${file}: ${why}`;
      const root = ctrl.createTestItem(`plugin:${p.folder}`, p.name);
      root.error = p.error;
      ctrl.items.add(root);
    } else {
      Object.assign(p, { needsOk: true, file, why });
      ctrl.items.add(needsOkItem(p));
    }
    sidebar.fire();
  };
  // a project plugin's folder may change while a command of it runs (a watch runs for hours): it
  // is checked again every limits.recheck ms while one does
  const recheck = setInterval(() => {
    for (const p of plugins) {
      if (!p.hash || !p.spec || ![...procs].some((x) => x.plugin === p)) continue;
      const change = changed(p);
      if (change) revoke(p, change);
    }
  }, limits.recheck);
  recheck.unref?.();

  // loads may overlap (a refresh while a slow list runs): only the newest one writes its results
  let loads = 0;
  async function load() {
    const mine = ++loads;
    const stale = () => mine !== loads;
    project = projectOf();
    plugins = readPlugins(project);
    for (const p of plugins) if (p.hash) p.revoke = (why) => revoke(p, why);
    meta.clear();
    ctrl.items.replace([]);
    await Promise.all(plugins.map(async (p) => {
      const label = p.name || path.basename(p.folder);
      if (p.needsOk) {
        ctrl.items.add(needsOkItem(p));
        return;
      }
      const root = ctrl.createTestItem(`plugin:${p.folder}`, label);
      ctrl.items.add(root);
      if (p.error) { root.error = p.error; return; }
      if (!p.spec.list) return;
      root.busy = true;
      let cases;
      let error;
      try { cases = await json(argv(p.spec.list, p, project), p, project); } catch (e) { error = e; }
      if (stale()) return;
      if (error) root.error = `list: ${error.message}`;
      else addCases(p, root.children, cases);
      root.busy = false;
    }));
    if (stale()) return;
    viewCache.clear();
    for (const folder of live.keys()) if (!plugins.some((p) => p.folder === folder)) live.delete(folder);
    sidebar.fire();
    if (watching) startWatch(); // plugins came or went
  }

  // one run of one case or group: JSON lines set the tree's icons and the results
  function runOne(run, item, token) {
    const { plugin: p, caseId } = meta.get(item.id) || {};
    return new Promise((resolve) => {
      if (!p?.spec?.run || !caseId) { run.skipped(item); return resolve(); }
      try { start(resolve); } catch (e) { run.errored(item, new vscode.TestMessage(e.message)); resolve(); }
    });
    function start(resolve) {
      run.started(item);
      const byCase = new Map();
      const walk = (i) => { const c = meta.get(i.id)?.caseId; if (c) byCase.set(c, i); i.children.forEach(walk); };
      walk(item);
      const ended = new Set();
      const room = budget(); // every line counts, status lines too: a flooding plugin cannot grow memory
      let told = false;
      let cancel;
      const proc = spawn(argv(p.spec.run, p, project, caseId), p, project, (code, err) => {
        cancel.dispose(); // a later cancel never signals a pid that is free again
        if (err) run.errored(item, new vscode.TestMessage(err.message));
        // the exit code decides for the case it ran when no final status came
        else if (!ended.has(item)) {
          if (token.isCancellationRequested) run.skipped(item);
          else if (code === 0) run.passed(item);
          else run.failed(item, new vscode.TestMessage(`exit code ${code}`));
        }
        resolve();
      });
      cancel = token.onCancellationRequested(() => stop(proc));
      const onLine = (line) => {
        let m;
        try { m = JSON.parse(line); } catch { m = { log: line }; }
        if (typeof m !== 'object' || m === null) m = { log: line };
        const full = !room(line);
        if (full && !told) {
          told = true;
          run.appendOutput(`[more than ${MAX_LOG} lines or ${MAX_LOG_CHARS >> 20} MB: the rest is not shown]\r\n`);
        }
        if (m.log !== undefined || !m.status) {
          if (!full) run.appendOutput(`${m.log ?? line}\r\n`);
          return;
        }
        const target = byCase.get(m.case) || (m.case ? undefined : item);
        const text = [m.label, m.message].filter(Boolean).join(': ');
        if (text && !full) run.appendOutput(`[${m.status}] ${text}${m.url ? ` ${m.url}` : ''}\r\n`, location(project, m.file, m.line), target);
        if (!target || !m.case) return; // a step's result: output only
        if (full && ended.has(target)) return; // past the limit a case ends once
        if (m.status === 'running') run.started(target);
        else if (m.status === 'passed') { run.passed(target); ended.add(target); }
        else if (m.status === 'skipped') { run.skipped(target); ended.add(target); }
        else if (m.status === 'failed') {
          const msg = new vscode.TestMessage(String(m.message || 'failed'));
          msg.location = location(project, m.file, m.line);
          run.failed(target, msg);
          ended.add(target);
        }
      };
      lines(proc.stdout, onLine);
      lines(proc.stderr, onLine);
    }
  }

  ctrl.createRunProfile('Run', vscode.TestRunProfileKind.Run, async (request, token) => {
    // device tests are long and drive a phone: run only what was picked, never "everything"
    if (!request.include?.length) {
      vscode.window.showInformationMessage('Pick a case or group in the Test Explorer to run it.');
      return;
    }
    const run = ctrl.createTestRun(request);
    try {
      for (const item of request.include) {
        if (token.isCancellationRequested) break;
        await runOne(run, item, token);
      }
    } finally {
      run.end(); // else the Test Explorer shows it running forever
    }
  }, true);
  ctrl.refreshHandler = load;

  // live state while the Droid Lite sidebar shows, one `watch` per plugin; a line redraws only
  // that plugin's row
  function startWatch() {
    stopWatch();
    for (const p of plugins.filter((x) => x.spec?.watch)) {
      try { // one plugin that cannot watch does not stop the others
        const proc = spawn(argv(p.spec.watch, p, project), p, project);
        lines(proc.stdout, (l) => {
          let state;
          try { state = JSON.parse(l); } catch { return; /* not a state line */ }
          if (state && typeof state === 'object') { live.set(p.folder, state); fireRow(p.folder); }
        });
        proc.stderr.resume();
        watchers.push(proc);
        // an ended watcher is never signalled: its group id may belong to another process by then
        proc.on('close', () => { watchers = watchers.filter((w) => w !== proc); });
      } catch (e) {
        live.set(p.folder, { title: 'Live', status: `watch: ${e.message}` });
      }
    }
  }
  function stopWatch() {
    watchers.forEach(stop);
    watchers = [];
  }

  // sidebar: per plugin its live state, views, actions and links
  const tree = {
    onDidChangeTreeData: sidebar.event,
    getTreeItem: (e) => e,
    async getChildren(node) {
      const item = (label, icon, command, args, description) => {
        const i = new vscode.TreeItem(label);
        i.iconPath = new vscode.ThemeIcon(icon);
        if (command) i.command = { command, title: label, arguments: args };
        i.description = description;
        return i;
      };
      if (!node) {
        const rows = plugins.filter((p) => p.error).map((p) => item(p.error, 'warning'));
        rows.push(...plugins.filter((p) => p.needsOk).map((p) => {
          const g = new vscode.TreeItem(p.name, vscode.TreeItemCollapsibleState.Expanded);
          g.iconPath = new vscode.ThemeIcon('shield');
          g.description = 'Needs your OK';
          g.tooltip = `${p.why ? `${p.why}\n` : ''}${p.folder}\nThis project plugin runs nothing until you allow it. ${TRUST}`;
          g.pending = p;
          return g;
        }));
        rows.push(...plugins.filter((p) => p.spec).map((p) => {
          const g = new vscode.TreeItem(p.name, vscode.TreeItemCollapsibleState.Expanded);
          g.plugin = p;
          pluginRows.set(p.folder, g);
          return g;
        }));
        rows.push(item('Add test plugin…', 'add', 'droidLite.addTestPlugin'));
        rows.push(item('Open Test Explorer', 'beaker', 'workbench.view.testing.focus'));
        return rows;
      }
      if (node.pending) return [item('Allow…', 'unlock', 'droidLite.allowTestPlugin', [node.pending])];
      if (node.plugin && !node.plugin.spec) return []; // asks for the OK again
      if (node.plugin && !node.view) {
        const p = node.plugin;
        const out = [];
        const state = live.get(p.folder);
        if (state) {
          const stateRows = (Array.isArray(state.rows) ? state.rows : []).map((r) => (Array.isArray(r) ? r : [r]).map(String));
          const s = item(String(state.title || 'Live'), 'pulse', undefined, undefined, state.status);
          s.tooltip = stateRows.map((r) => r.join(': ')).join('\n');
          out.push(s);
          for (const r of stateRows) out.push(item(r[0], 'blank', undefined, undefined, r[1]));
        }
        for (const v of p.spec.views || []) {
          const i = new vscode.TreeItem(v.name, vscode.TreeItemCollapsibleState.Collapsed);
          i.iconPath = new vscode.ThemeIcon('list-unordered');
          i.contextValue = 'droidLiteView'; // its inline Refresh button
          i.plugin = p; i.view = v;
          out.push(i);
        }
        for (const a of p.spec.actions || []) {
          out.push(actions.has(`${p.folder}\u0000${a.name}`)
            ? item(`Stop ${a.name}`, 'debug-stop', 'droidLite.stopTestAction', [p, a], 'running')
            : item(a.name, 'play', 'droidLite.testAction', [p, a]));
        }
        for (const l of p.spec.links || []) out.push(item(l.name, 'link-external', 'vscode.open', [vscode.Uri.parse(l.url)]));
        out.push(item('Remove', 'trash', 'droidLite.removeTestPlugin', [p]));
        return out;
      }
      if (node.view) {
        const key = `${node.plugin.folder}\u0000${node.view.name}`;
        if (!viewCache.has(key)) viewCache.set(key, json(argv(node.view.run, node.plugin, project), node.plugin, project));
        try {
          const entries = await viewCache.get(key);
          if (!Array.isArray(entries)) throw new Error('not a JSON list');
          return entries.map((e) => {
            const icon = { failed: 'error', passed: 'pass', skipped: 'circle-slash', running: 'sync' }[e.status] || 'circle-small';
            const loc = location(project, e.file, e.line);
            const args = loc ? [loc.uri, { selection: loc.range }] : e.url ? [vscode.Uri.parse(e.url)] : undefined;
            const i = item(e.label || '', icon, args ? 'vscode.open' : undefined, args, e.message);
            i.tooltip = [e.label, e.message, e.url].filter(Boolean).join('\n');
            return i;
          });
        } catch (e) {
          return [item(`${node.view.name}: ${e.message}`, 'warning')];
        }
      }
      return [];
    },
  };
  const view = vscode.window.createTreeView('droidLite.testPlugins', { treeDataProvider: tree });
  watching = Boolean(view.visible);

  const out = vscode.window.createOutputChannel('Droid Lite tests');
  ctx.subscriptions.push(
    ctrl, view, out,
    view.onDidChangeVisibility((e) => { watching = e.visible; if (watching) startWatch(); else stopWatch(); }),
    // every process group a plugin started ends with the extension
    { dispose: () => { watching = false; clearInterval(recheck); procs.forEach((p) => stop(p, true)); } },
    vscode.commands.registerCommand('droidLite.testAction', (given, act) => {
      // only a loaded plugin that may run (a project plugin not allowed has no spec), its own action
      const p = plugins.find((x) => x.spec && x.folder === given?.folder);
      const a = p?.spec.actions?.find((x) => x.name === act?.name);
      if (!a) return;
      const key = `${p.folder}\u0000${a.name}`;
      out.show(true);
      if (actions.has(key)) return out.appendLine(`${a.name} is still running`);
      out.appendLine(`▶ ${p.name}: ${a.name}`);
      const room = budget();
      const proc = spawn(argv(a.run, p, project), p, project, (code, err) => {
        actions.delete(key);
        out.appendLine(err ? `■ ${a.name} could not start: ${err.message}` : `■ ${a.name} ended (exit code ${code})`);
        fireRow(p.folder);
      });
      actions.set(key, proc);
      fireRow(p.folder);
      const log = (l) => { if (room(l)) out.appendLine(l); };
      lines(proc.stdout, log);
      lines(proc.stderr, log);
    }),
    vscode.commands.registerCommand('droidLite.stopTestAction', (p, a) => {
      const proc = actions.get(`${p.folder}\u0000${a.name}`);
      if (proc) stop(proc);
    }),
    vscode.commands.registerCommand('droidLite.refreshTestView', (node) => {
      viewCache.delete(`${node.plugin.folder}\u0000${node.view.name}`);
      sidebar.fire(node);
    }),
    // Allow: shows the commands of the plugin.json on disk now and allows exactly the folder's tree
    // it read them from, for this project only; from the sidebar (the plugin) or the Test Explorer
    // (its test item)
    vscode.commands.registerCommand('droidLite.allowTestPlugin', async (arg) => {
      const folder = arg?.id?.startsWith?.('needs-ok:') ? arg.id.slice('needs-ok:'.length) : arg?.folder;
      const p = plugins.find((x) => x.needsOk && x.folder === folder);
      if (!p) return;
      let seen;
      let spec;
      try { // the folder as it is now: the OK covers exactly the tree shown
        seen = inspect(p.project, p.folder);
        spec = JSON.parse(seen.manifest.toString('utf8'));
        const bad = problem(spec);
        if (bad) throw new Error(bad);
      } catch (e) {
        return vscode.window.showErrorMessage(`${p.file}: ${e.message}`);
      }
      const cmd = (v) => JSON.stringify(v);
      const detail = [
        TRUST,
        '',
        `Folder: ${p.folder}`,
        ...(spec.dir ? [`Runs in: ${path.resolve(p.folder, spec.dir.replace(/^~\//, `${os.homedir()}/`))}`] : []),
        ...['list', 'run', 'watch'].filter((k) => spec[k]).map((k) => `${k}: ${cmd(spec[k])}`),
        ...(spec.views || []).map((v) => `view "${v.name}": ${cmd(v.run)}`),
        ...(spec.actions || []).map((x) => `action "${x.name}": ${cmd(x.run)}`),
      ].join('\n');
      const ok = await vscode.window.showWarningMessage(
        `Allow the project's test plugin "${spec.name}" to run these commands on your Mac?`,
        { modal: true, detail }, 'Allow');
      if (ok !== 'Allow') return;
      try { await approve(p.project, seen.real, seen.hash); } catch (e) {
        return vscode.window.showErrorMessage(`Droid Lite: could not save the OK: ${e.message}`);
      }
      await load();
    }),
    // Add: link the tool's plugin folder into the shared folder the app reads too
    vscode.commands.registerCommand('droidLite.addTestPlugin', async () => {
      const [dir] = await vscode.window.showOpenDialog({
        canSelectFolders: true, canSelectFiles: false, openLabel: 'Add plugin',
        title: "Pick the test tool's folder (with plugin.json or droid-lite-plugin/)",
      }) || [];
      if (!dir) return;
      const file = findPluginJson(dir.fsPath);
      if (!file) return vscode.window.showErrorMessage(`No plugin.json in ${dir.fsPath} or its droid-lite-plugin/ folder.`);
      let name;
      try { name = JSON.parse(fs.readFileSync(file, 'utf8')).name; } catch (e) { return vscode.window.showErrorMessage(`${file}: ${e.message}`); }
      const link = path.join(userDir(), String(name || path.basename(dir.fsPath)).toLowerCase().replace(/[^a-z0-9]+/g, '-'));
      fs.mkdirSync(userDir(), { recursive: true });
      if (fs.existsSync(link)) return vscode.window.showWarningMessage(`${name} is already added.`);
      fs.symlinkSync(path.dirname(file), link);
      await load();
      vscode.window.showInformationMessage(`Added ${name}: its cases are in the Test Explorer.`);
    }),
    // Remove drops only the link, never the tool's folder
    vscode.commands.registerCommand('droidLite.removeTestPlugin', async (p) => {
      let names = [];
      try { names = fs.readdirSync(userDir()); } catch { /* no user plugins: it comes with the project */ }
      const links = names.map((n) => path.join(userDir(), n))
        .filter((l) => { try { return realpath(l) === p.folder; } catch { return false; } });
      if (!links.length) return vscode.window.showInformationMessage(`${p.name} comes with the project (.droid-lite/plugins): remove it there.`);
      const ok = await vscode.window.showWarningMessage(`Remove ${p.name}? Only the link goes; the tool's folder stays.`, { modal: true }, 'Remove');
      if (ok !== 'Remove') return;
      links.forEach((l) => fs.unlinkSync(l));
      await load();
    }),
  );
  const loaded = load();
  return { load, readPlugins, loaded };
}

module.exports = { setup, readPlugins, argv, json, findPluginJson, location, limits, procs, treeHash, approve, removeStale, readUpTo, utf8Name };
