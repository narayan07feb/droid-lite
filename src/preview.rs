//! Compose previews in the IDE, drawn by the warm preview worker (`preview-worker/PreviewWorker.java`):
//! one JVM that compiles the open file and renders its `@Preview` functions with layoutlib. No Gradle.
//!
//! The UI thread only calls [`Client::request`] (a channel send) and [`Client::poll`] (a `try_recv`).
//! Everything slow runs on the client's own thread: the mac-guard check, starting the worker
//! (detached, in its own process group), the socket, and decoding the PNGs. A new request replaces
//! one that has not started yet. Caches live in `~/Library/Caches/droid-lite/previews` only.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

/// The worker's source, written next to the base pack: the single-file launcher compiles it at start.
const WORKER_SOURCE: &str = include_str!("../preview-worker/PreviewWorker.java");

/// What one render request asks for.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub file: PathBuf,
    /// The JVM class of the file's top-level functions (`com.app.ui.CardKt`).
    pub class: String,
    pub previews: Vec<String>,
    /// The caller's number for this request, given back in its answer (Done, Failed or Skipped).
    pub seq: u64,
    /// The project: its libraries come from the Gradle cache (read only), its own code the preview
    /// needs from its files.
    pub project: Option<ProjectRef>,
}

/// The Kotlin compiler a worker runs: the project's own version (its version catalog's `kotlin`),
/// so its libraries' metadata and compiler plugins (Compose, parcelize) match; jars from the Gradle
/// cache, read only.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Kotlin {
    pub version: String,
    /// The compiler and what it needs, for the worker's classpath.
    pub compiler: Vec<PathBuf>,
    pub compose_plugin: Option<PathBuf>,
    pub parcelize_plugin: Option<PathBuf>,
    pub serialization_plugin: Option<PathBuf>,
    /// What compiled code needs besides the libraries (the parcelize runtime).
    pub runtime: Vec<PathBuf>,
}

const DEFAULT_KOTLIN: &str = "2.2.20";

/// The version catalog's `kotlin = "…"`, else the base pack's version.
pub fn kotlin_version(root: Option<&Path>) -> String {
    root.and_then(|r| std::fs::read_to_string(r.join("gradle/libs.versions.toml")).ok())
        .and_then(|t| {
            t.lines().map(str::trim).find_map(|l| {
                let v = l.strip_prefix("kotlin")?.trim_start().strip_prefix('=')?.trim();
                v.strip_prefix('"')?.split('"').next().map(String::from)
            })
        })
        .unwrap_or_else(|| DEFAULT_KOTLIN.into())
}

pub fn kotlin(version: &str) -> Result<Kotlin, String> {
    let k = |a: &str| ("org.jetbrains.kotlin".to_string(), a.to_string(), Some(version.to_string()));
    let compiler = crate::libs::closure_archives(vec![k("kotlin-compiler-embeddable"), k("kotlin-stdlib")]);
    if !compiler.iter().any(|f| f.to_string_lossy().contains(&format!("kotlin-compiler-embeddable-{version}.jar"))) {
        return Err(format!("The Kotlin {version} compiler is not on this Mac yet (it comes with the project's first Gradle build, or the preview download)."));
    }
    let one = |a: &str| {
        crate::libs::closure_archives(vec![k(a)])
            .into_iter()
            .find(|f| f.to_string_lossy().contains(&format!("{a}-{version}.jar")))
    };
    Ok(Kotlin {
        version: version.into(),
        compiler,
        compose_plugin: one("kotlin-compose-compiler-plugin-embeddable"),
        parcelize_plugin: one("kotlin-parcelize-compiler"),
        serialization_plugin: one("kotlin-serialization-compiler-plugin-embeddable"),
        runtime: one("kotlin-parcelize-runtime").into_iter().collect(),
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectRef {
    pub root: PathBuf,
    pub scripts: Vec<PathBuf>,
    pub files: std::sync::Arc<Vec<PathBuf>>,
}

/// The project's own code a preview needs: the files (compiled once, cached by `key`) and the R sources.
struct Own {
    sources: Vec<String>,
    rjava: Vec<String>,
    key: String,
}

/// The pictures the file's preview functions ask for (Light and Dark, font scales, sizes…), from
/// their annotations and the project's annotation classes.
fn specs_for(text: &str, functions: &[String], classes: &std::collections::HashMap<String, String>) -> Vec<crate::preview_spec::Spec> {
    functions
        .iter()
        .flat_map(|f| {
            let at = [format!("fun {f}("), format!("fun {f} (")].iter().find_map(|p| text.find(p.as_str()));
            let ann = at.map(|i| crate::preview_spec::annotations_before(text, i)).unwrap_or_default();
            crate::preview_spec::specs(f, &ann, classes)
        })
        .collect()
}

/// Follows the open file's imports through the project (main sources only), writes the R classes of
/// the modules it uses, and hashes it all, so the worker compiles this code once per change.
fn own_code(pk: &crate::preview_src::Packages, open: &Path) -> Result<Own, String> {
    let text = std::fs::read_to_string(open).map_err(|e| e.to_string())?;
    let (files, rs) = pk.closure(open, &text, 1500)?;
    let mut h = fnv("own");
    for f in &files {
        let m = std::fs::metadata(f).ok();
        let stamp = m.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
        h ^= fnv(&format!("{}|{:?}|{:?}", f.display(), stamp, m.map(|m| m.len())));
        h = h.wrapping_mul(0x100000001b3);
    }
    let _ = rs; // the modules' R classes are built once per project (project_r), on the classpath
    let rjava: Vec<String> = Vec::new();
    Ok(Own {
        sources: files.iter().map(|f| f.to_string_lossy().into_owned()).collect(),
        rjava,
        key: format!("{h:016x}"),
    })
}

/// What comes back, by message only.
pub enum Event {
    /// The real step in words ("Compiling the project code (42 files)", "Drawing 2 of 3 previews")
    /// for the request of that file, with (done, total) when the count is known; or a reason it
    /// cannot run ("Skipped: …").
    Status(PathBuf, String, Option<(u64, u64)>),
    /// One picture or one error text per preview, in request order.
    Done { file: PathBuf, seq: u64, cards: Vec<(String, Result<egui::ColorImage, String>)> },
    /// The whole request failed (compile error, missing class): the text for the cards.
    Failed { file: PathBuf, seq: u64, error: String },
    /// The request was not drawn: a newer one replaced it, or there is no room now (the reason).
    Skipped { file: PathBuf, seq: u64, reason: String },
}

pub struct Client {
    tx: Sender<Request>,
    rx: Receiver<Event>,
}

/// Starts the client's thread: requests go in, events come out (a caller may block on them).
pub fn spawn(ctx: &egui::Context) -> (Sender<Request>, Receiver<Event>) {
    let (tx, jobs) = channel::<Request>();
    let (events, rx) = channel();
    let ctx = ctx.clone();
    std::thread::Builder::new()
        .name("preview-client".into())
        .spawn(move || run(jobs, events, ctx))
        .expect("preview thread");
    (tx, rx)
}

impl Client {
    pub fn new(ctx: &egui::Context) -> Client {
        let (tx, rx) = spawn(ctx);
        Client { tx, rx }
    }

    /// Never blocks.
    pub fn request(&self, r: Request) {
        let _ = self.tx.send(r);
    }

    /// Never blocks.
    pub fn poll(&self) -> Option<Event> {
        self.rx.try_recv().ok()
    }
}

/// `DROID_LITE_PREVIEW_DIR`, else `~/Library/Caches/droid-lite/previews` (kept short: a Unix socket
/// path may hold 104 bytes on macOS).
pub fn dir() -> PathBuf {
    std::env::var_os("DROID_LITE_PREVIEW_DIR").map_or_else(
        || {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join("Library/Caches/droid-lite/previews")
        },
        PathBuf::from,
    )
}

/// The worker's folder: its source (PreviewWorker.java) and layoutlib's unpacked runtime and resources
/// (`layoutlib/`). The renderer's jars are found or downloaded at every worker start
/// ([`crate::preview_base`]): Gradle's cache, else `m2/` next to it. Optional, from older packs:
///   compose/*.jar, compile/*.jar  Compose for a project that has none in the Gradle cache
///   compose-plugin.jar            the Compose compiler plugin when the project's Kotlin has none there
pub fn base_dir() -> PathBuf {
    dir().join("base")
}

/// Jars in `base/<sub>`, sorted (an older pack names android.jar zz-android.jar so it comes last).
fn base_jars(sub: &str) -> Vec<String> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(base_dir().join(sub))
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "jar")).collect())
        .unwrap_or_default();
    v.sort();
    v.iter().map(|p| p.to_string_lossy().into_owned()).collect()
}

const NO_COMPOSE: &str = "This project's Compose libraries are not in the Gradle cache (~/.gradle/caches) yet. \
Build the project once with Gradle (any task that compiles it), then open the preview again.";

/// Why previews must wait now ("" = go): mac-guard says memory is critical, or the disk is under 8 GB.
pub fn room() -> String {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let guard = std::env::var_os("DROID_LITE_MAC_GUARD")
        .map_or_else(|| home.join(".local/bin/mac-guard"), PathBuf::from);
    room_from(&guard, Duration::from_secs(3))
}

/// `guard status`, stopped after `limit`: no mac-guard, or one that hangs, says nothing against.
fn room_from(guard: &Path, limit: Duration) -> String {
    match crate::capture::output_within(Command::new(guard).arg("status"), limit) {
        Ok(out) => room_from_status(&String::from_utf8_lossy(&out.stdout)),
        Err(_) => String::new(),
    }
}

fn room_from_status(status: &str) -> String {
    if status.lines().any(|l| l.trim() == "level Critical") {
        return "Skipped: mac-guard says memory is critical. Try again when it is not.".into();
    }
    let disk = status
        .split("disk ")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|n| n.parse::<f64>().ok());
    match disk {
        Some(gb) if gb < 8.0 => format!("Skipped: only {gb:.0} GB free on the disk (needs 8 GB)."),
        _ => String::new(),
    }
}

/// The next request to draw: the newest one waiting. Every older one, and one there is no room
/// for now, is answered Skipped, so no caller waits for an answer that never comes.
fn next_job(jobs: &Receiver<Request>, send: &dyn Fn(Event), room: &dyn Fn() -> String) -> Option<Request> {
    loop {
        let mut r = jobs.recv().ok()?;
        while let Ok(newer) = jobs.try_recv() {
            send(Event::Skipped { file: r.file, seq: r.seq, reason: "replaced by a newer request".into() });
            r = newer;
        }
        let why = room();
        if why.is_empty() {
            return Some(r);
        }
        send(Event::Status(r.file.clone(), why.clone(), None));
        send(Event::Skipped { file: r.file, seq: r.seq, reason: why });
    }
}

/// The preview cache key: what is drawn (file, text, project code, specs, Kotlin version), the
/// libraries, the worker's source, the renderer and every resource and assets file.
fn cache_key(draw: &[&str], rt: &ProjectRuntime, worker: &str, renderer: &str, walks: &mut Walks) -> String {
    let mut parts = draw.to_vec();
    let (libs, worker, files) = (format!("{:08x}", rt.key()), format!("{:016x}", fnv(worker)), format!("{:016x}", rt.files_key(walks)));
    parts.extend([libs.as_str(), &worker, renderer, &files]);
    crate::preview_cache::key(&parts)
}

/// A status line, with (done, total) when the count is known.
type StatusLine<'a> = dyn Fn(&str, Option<(u64, u64)>) + 'a;

/// What the run loop reads once per project (the libraries, the packages) and the compilers it found.
#[derive(Default)]
struct Reads {
    libs: Option<(PathBuf, LibraryCode)>,
    packages: Option<(PathBuf, crate::preview_src::Packages)>,
    /// The project files the packages were read from; another list (files added or removed) reads them again.
    files: Option<std::sync::Arc<Vec<PathBuf>>>,
    kotlins: std::collections::HashMap<String, Result<Kotlin, String>>,
}

impl Reads {
    /// After a request that failed for want of libraries or a compiler (no Compose in the Gradle cache
    /// yet, no compiler, an import of a package the project does not have) the next one reads again,
    /// so "build once with Gradle, then open the preview again" works.
    fn forget_errors(&mut self) {
        self.libs = None;
        self.packages = None;
        self.files = None;
        self.kotlins.retain(|_, k| k.is_ok());
    }

    /// The project's libraries (once per project root) and its packages (again when files were
    /// added or removed since: a new screen, a moved folder).
    fn refresh(&mut self, p: &ProjectRef, status: &StatusLine<'_>) {
        if self.libs.as_ref().is_none_or(|(r0, _)| *r0 != p.root) {
            status("Reading the project's libraries", None);
            let progress = |i: usize, n: usize| status("Unpacking the project's libraries", Some((i as u64, n as u64)));
            self.libs = Some((p.root.clone(), library_classpath(&p.root, &p.scripts, &progress)));
            self.files = None;
        }
        if self.files.as_ref().is_none_or(|f| !std::sync::Arc::ptr_eq(f, &p.files) && **f != *p.files) {
            self.packages = Some((p.root.clone(), crate::preview_src::Packages::read(&p.files)));
            self.files = Some(p.files.clone());
        }
    }
}

/// Whether a failed request was about the libraries or the compiler (read them again for the next
/// one), not an ordinary compile error in the user's code: the missing Compose or compiler texts, or
/// an unresolved import of a package that is not the project's (a library not in the Gradle cache).
fn library_trouble(error: &str, unresolved_imports: &[String], packages: Option<&crate::preview_src::Packages>) -> bool {
    error == NO_COMPOSE
        || error.contains("is not on this Mac yet")
        || error.contains("is not in the Gradle cache yet")
        || unresolved_imports.iter().any(|i| packages.is_none_or(|p| p.package_for(i).is_none()))
}

fn run(jobs: Receiver<Request>, events: Sender<Event>, ctx: egui::Context) {
    let send = |e: Event| {
        let _ = events.send(e);
        ctx.request_repaint();
    };
    let mut state = State::default();
    serve_jobs(&jobs, &send, &room, &mut |r| state.draw(r, &send));
}

/// Draws each request the loop takes. A panic in one (a bug) answers that request Failed and the
/// loop goes on, so no caller waits for an answer that never comes.
fn serve_jobs(jobs: &Receiver<Request>, send: &dyn Fn(Event), room: &dyn Fn() -> String, draw: &mut dyn FnMut(&Request)) {
    while let Some(r) = next_job(jobs, send, room) {
        if let Err(p) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| draw(&r))) {
            let why = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
            send(Event::Failed { file: r.file.clone(), seq: r.seq, error: format!("internal error: {why}") });
        }
    }
}

/// What the run loop keeps between requests: the worker connection and what it read of the project.
#[derive(Default)]
struct State {
    conn: Option<(UnixStream, BufReader<UnixStream>)>,
    conn_version: String,
    id: u64,
    reads: Reads,
    /// The last request failed for want of libraries or a compiler: read them again.
    forget: bool,
    assets: Vec<String>,
    res_packages: Vec<String>,
    module_res: Vec<String>,
    exploded: Vec<String>,
    walks: Walks,
}

/// The resource and assets folders' listings, each kept while the folder's own time is unchanged
/// (a file added, removed or renamed in it changes that time): a cache hit then reads no folder
/// again, only each file's time and size (an edit in place). On BranchAndroid 25 ms → 5 ms.
#[derive(Default)]
struct Walks {
    /// Folder → (its time, sub-folders, files), sorted by name.
    dirs: std::collections::HashMap<PathBuf, (std::time::SystemTime, Vec<PathBuf>, Vec<PathBuf>)>,
    /// Folders read, for tests.
    reads: usize,
}

impl State {
    /// Draws one request and answers it once (Done or Failed).
    fn draw(&mut self, r: &Request, send: &dyn Fn(Event)) {
        if std::mem::take(&mut self.forget) {
            self.reads.forget_errors();
        }
        let fail = |error: String| Event::Failed { file: r.file.clone(), seq: r.seq, error };
        let (cp, own) = match &r.project {
            Some(p) => {
                self.reads.refresh(p, &|t, n| send(Event::Status(r.file.clone(), t.into(), n)));
                let lc = self.reads.libs.as_ref().map(|(_, l)| l.clone()).unwrap_or_default();
                let (mut cp, lib_r) = (lc.jars, lc.rjava);
                self.assets = lc.assets;
                // the project's own Compose runs the preview when the Gradle cache has it
                if cp.iter().any(|j| j.contains("/androidx.compose.runtime/")) {
                    match project_r(&self.reads.packages.as_ref().unwrap().1, &lib_r) {
                        Ok((rs, pkgs, res)) => {
                            cp.push(rs.to_string_lossy().into_owned());
                            self.res_packages = pkgs;
                            self.module_res = res;
                            self.exploded = lc.exploded.clone();
                        }
                        Err(e) => {
                            self.forget = true; // the libraries' R classes
                            return send(fail(e));
                        }
                    }
                } else {
                    cp.clear(); // not usable on its own: the base pack's Compose instead
                }
                match own_code(&self.reads.packages.as_ref().unwrap().1, &r.file) {
                    Ok(o) => (cp, Some(o)),
                    Err(e) => return send(fail(e)),
                }
            }
            None => (Vec::new(), None),
        };
        let version = kotlin_version(r.project.as_ref().map(|p| p.root.as_path()));
        let stand_in = std::env::var_os("DROID_LITE_PREVIEW_WORKER").is_some(); // tests: no compiler needed
        if cp.is_empty() && !stand_in && base_jars("compose").is_empty() {
            self.forget = true;
            return send(fail(NO_COMPOSE.into()));
        }
        let kt = match self.reads.kotlins.entry(version.clone()).or_insert_with(|| {
            if stand_in { Ok(Kotlin { version: version.clone(), ..Default::default() }) } else { kotlin(&version) }
        }) {
            Ok(k) => k.clone(),
            Err(e) => {
                let e = e.clone();
                self.forget = true;
                return send(fail(e));
            }
        };
        // never Kotlin's own compiler-side jars on the worker's classpath: the worker's compiler would
        // load them as plugins (a scripting plugin of another version crashed it); it brings its stdlib
        let text = std::fs::read_to_string(&r.file).unwrap_or_default();
        let specs = {
            let empty = std::collections::HashMap::new();
            let classes = self.reads.packages.as_ref().map_or(&empty, |(_, p)| &p.annotation_classes);
            specs_for(&text, &r.previews, classes)
        };
        let rt = ProjectRuntime {
            jars: cp.iter().filter(|j| !j.contains("/org.jetbrains.kotlin/")).cloned().collect(),
            assets: if cp.is_empty() { Vec::new() } else { self.assets.clone() },
            res_packages: if cp.is_empty() { Vec::new() } else { self.res_packages.clone() },
            module_res: if cp.is_empty() { Vec::new() } else { self.module_res.clone() },
            exploded: if cp.is_empty() { Vec::new() } else { self.exploded.clone() },
        };
        // the same file, project code, previews, libraries, worker, renderer and resources as a recent
        // draw: its pictures at once
        let cache_dir = dir().join("cache");
        let (file, specs_text) = (r.file.to_string_lossy(), format!("{specs:?}"));
        let draw = [file.as_ref(), &text, own.as_ref().map_or("", |o| o.key.as_str()), &specs_text, &version];
        let cache_key = cache_key(&draw, &rt, WORKER_SOURCE, &crate::preview_base::fingerprint(), &mut self.walks);
        if !stand_in && let Some(cards) = crate::preview_cache::lookup(&cache_dir, &cache_key, crate::preview_cache::now_secs()) {
            return send(Event::Done { file: r.file.clone(), seq: r.seq, cards });
        }
        let worker_id = format!("{version}-{:08x}", rt.key());
        if self.conn_version != worker_id {
            self.conn = None; // another project's Kotlin or libraries: its own worker
            self.conn_version = worker_id;
        }
        let sock = socket(&kt, &rt);
        // the worker may have ended (idle exit, a crash) or never started: start it and connect by
        // itself, up to 3 tries with a short back-off, then the error goes on the card
        let mut last_error = String::new();
        let mut base = None;
        let mut lost = false;
        for attempt in 0..3u32 {
            // a lost connection (a worker that exited when idle) connects again at once
            if attempt > 0 && !std::mem::take(&mut lost) {
                std::thread::sleep(Duration::from_millis(500 << attempt));
            }
            if self.conn.is_none() {
                match connect_or_start(send, &r.file, &sock, &kt, &rt, &mut base) {
                    Ok(c) => self.conn = Some(c),
                    Err(e) => {
                        self.forget |= e.contains("is not in the Gradle cache yet"); // the Compose compiler plugin
                        last_error = e;
                        if base.is_none() && !stand_in {
                            break; // the renderer's files are missing: the card says which, no retry
                        }
                        continue;
                    }
                }
            }
            send(Event::Status(r.file.clone(), format!("Sending {} preview(s) to the worker", r.previews.len()), None));
            self.id += 1;
            let (w, rd) = self.conn.as_mut().unwrap();
            match ask(w, rd, self.id, r, &specs, &cp, own.as_ref(), send) {
                Ok((e, unresolved)) => {
                    match &e {
                        Event::Done { file, cards, .. } if !stand_in => store_if_current(&cache_dir, &cache_key, file, &text, cards),
                        Event::Failed { error, .. } => self.forget = library_trouble(error, &unresolved, self.reads.packages.as_ref().map(|(_, p)| p)),
                        _ => {}
                    }
                    send(e);
                    last_error.clear();
                    break;
                }
                Err(NoAnswer::Replaced) => {
                    self.conn = None;
                    last_error = "The preview worker stopped: another window's request replaced this one".into();
                }
                Err(NoAnswer::Silent(e)) => {
                    // a hung worker would take the next connection too: stop it, so the next try
                    // starts a fresh one
                    self.conn = None;
                    stop_worker(&sock, STOP_GRACE);
                    last_error = format!("The preview worker stopped: {e}");
                }
                Err(NoAnswer::Lost(e)) => {
                    // only this connection: the next try connects again (or starts a worker)
                    self.conn = None;
                    lost = true;
                    last_error = format!("The preview worker closed the connection: {e}");
                }
            }
        }
        if !last_error.is_empty() {
            send(fail(last_error));
        }
    }
}

/// Stores the pictures under `key` only when the file still has the text the key was made from: the
/// worker reads the file itself, so after a save while it drew the pictures may show the new text,
/// and must not be kept under the old text's key.
fn store_if_current(cache_dir: &Path, key: &str, file: &Path, text: &str, cards: &crate::preview_cache::Cards) {
    if std::fs::read_to_string(file).is_ok_and(|now| now == text) {
        crate::preview_cache::store(cache_dir, key, file, cards, crate::preview_cache::now_secs());
    }
}

/// One socket per worker source, Kotlin version and libraries: after an update, or for a project on
/// another Kotlin, a new worker starts; an old one exits when idle.
fn socket(kt: &Kotlin, rt: &ProjectRuntime) -> PathBuf {
    let plugins = format!("{:?}{:?}{:?}", kt.compose_plugin, kt.parcelize_plugin, kt.serialization_plugin);
    dir().join(format!("w-{:08x}-{}-{:08x}.sock", (fnv(WORKER_SOURCE) ^ fnv(&plugins)) as u32, kt.version, rt.key()))
}

/// How long a stopped worker gets to end after SIGTERM before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// Stops the worker serving `sock` (it hung, closed the connection or sent garbage): SIGTERM, then
/// SIGKILL after `grace`, then its socket goes, so the next try starts a fresh worker instead of
/// connecting to the hung one. Found by its command line (the socket is its argument), like
/// [`stop_other_workers`]; worker.pid names only the newest worker.
fn stop_worker(sock: &Path, grace: Duration) {
    let mut pids = worker_pids(sock);
    for p in &pids {
        unsafe { libc::kill(*p, libc::SIGTERM) };
    }
    let until = Instant::now() + grace;
    while !pids.is_empty() && Instant::now() < until {
        pids.retain(|p| unsafe { libc::kill(*p, 0) } == 0);
        std::thread::sleep(Duration::from_millis(50));
    }
    for p in &pids {
        unsafe { libc::kill(*p, libc::SIGKILL) };
    }
    let _ = std::fs::remove_file(sock);
}

/// The processes whose command line names `sock` (never this one).
fn worker_pids(sock: &Path) -> Vec<i32> {
    let Ok(out) = Command::new("ps").args(["-axww", "-o", "pid=,command="]).output() else { return Vec::new() };
    let (sock, me) = (sock.to_string_lossy().into_owned(), std::process::id());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (pid, cmd) = l.trim_start().split_once(' ')?;
            let pid = pid.parse::<i32>().ok()?;
            (pid as u32 != me && format!(" {cmd} ").contains(&format!(" {sock} "))).then_some(pid)
        })
        .collect()
}

fn connect_or_start(
    send: &dyn Fn(Event),
    file: &Path,
    sock: &Path,
    kt: &Kotlin,
    rt: &ProjectRuntime,
    base: &mut Option<crate::preview_base::Base>,
) -> Result<(UnixStream, BufReader<UnixStream>), String> {
    if let Ok(s) = UnixStream::connect(sock) {
        let rd = BufReader::new(s.try_clone().map_err(|e| e.to_string())?);
        return Ok((s, rd));
    }
    // the renderer's files, checked at every worker start: one that went away is found again or
    // downloaded; never a start with a partial set
    if base.is_none() && std::env::var_os("DROID_LITE_PREVIEW_WORKER").is_none() {
        let status = |t: String, n| send(Event::Status(file.to_path_buf(), t, n));
        *base = Some(crate::preview_base::ensure(&base_dir(), &dir().join("m2"), &status)?);
    }
    send(Event::Status(file.to_path_buf(), "Starting the preview worker".into(), None));
    stop_other_workers(sock);
    let mut child = start_worker(sock, kt, rt, base.as_ref())?;
    let _ = std::fs::write(dir().join("worker.pid"), format!("{} {}", child.id(), sock.display()));
    let until = Instant::now() + START_TIMEOUT;
    while Instant::now() < until {
        if let Ok(s) = UnixStream::connect(sock) {
            std::thread::spawn(move || child.wait()); // reap it when it ends (idle exit)
            let rd = BufReader::new(s.try_clone().map_err(|e| e.to_string())?);
            return Ok((s, rd));
        }
        // it ended before it listened: say why now (the end of its log), do not wait the full minute
        if let Ok(Some(status)) = child.try_wait() {
            let log = std::fs::read_to_string(dir().join("worker.log")).unwrap_or_default();
            let tail: Vec<&str> = log.lines().rev().take(8).collect();
            return Err(format!(
                "The preview worker stopped at start ({status}):\n{}",
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = child.kill();
    let _ = child.wait(); // no zombie
    Err(format!(
        "The preview worker did not start in {} s. Its log: {}",
        START_TIMEOUT.as_secs(),
        dir().join("worker.log").display()
    ))
}

/// How long a starting worker may take to listen.
const START_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 60 });

/// Workers that may run at once (about 2 GB each): the one starting and one other, so two windows
/// on different projects (the app and VS Code) do not stop each other's worker mid-draw.
const MAX_WORKERS: usize = 2;

/// Before a new worker starts, the other preview workers of this preview folder (an older build's,
/// another project's or Kotlin version's) past [`MAX_WORKERS`] are asked to stop (SIGTERM), the
/// oldest first: the youngest is the one most likely still in use, and an unused one exits by
/// itself when idle. Found by their command line (worker source and socket), so older builds that
/// wrote no pidfile are found too.
fn stop_other_workers(keep: &Path) {
    let Ok(out) = Command::new("ps").args(["-axo", "pid=,etime=,command="]).output() else { return };
    let src = base_dir().join("PreviewWorker.java");
    let (src, keep, me) = (src.to_string_lossy().into_owned(), keep.to_string_lossy().into_owned(), std::process::id());
    let mut others = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(age)) = (it.next().and_then(|p| p.parse::<i32>().ok()), it.next().and_then(etime_secs)) else { continue };
        let cmd = it.collect::<Vec<_>>().join(" ");
        if pid as u32 != me && cmd.contains(&src) && !cmd.contains(&keep) {
            others.push((pid, age));
        }
    }
    let mut stopped = workers_to_stop(others);
    for p in &stopped {
        unsafe { libc::kill(*p, libc::SIGTERM) };
    }
    // give them a moment to go before the new one starts (memory)
    let until = Instant::now() + Duration::from_secs(5);
    while !stopped.is_empty() && Instant::now() < until {
        stopped.retain(|p| unsafe { libc::kill(*p, 0) } == 0);
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Of the other workers (pid, age in seconds), the ones to stop so that with the new one at most
/// [`MAX_WORKERS`] run: all but the youngest.
fn workers_to_stop(mut others: Vec<(i32, u64)>) -> Vec<i32> {
    others.sort_by_key(|w| w.1);
    others.into_iter().skip(MAX_WORKERS - 1).map(|w| w.0).collect()
}

/// `ps` elapsed time (`[[dd-]hh:]mm:ss`) in seconds.
fn etime_secs(s: &str) -> Option<u64> {
    let (days, rest) = s.split_once('-').map_or(Some((0, s)), |(d, r)| d.parse().ok().map(|d: u64| (d, r)))?;
    let secs = rest.split(':').try_fold(0u64, |acc, p| p.parse::<u64>().ok().map(|n| acc * 60 + n))?;
    Some(days * 86_400 + secs)
}

/// Starts the worker detached (its own process group, output to worker.log), so it outlives nothing
/// it should not and a restart of Droid Lite finds it still warm.
fn start_worker(
    sock: &Path,
    kt: &Kotlin,
    rt: &ProjectRuntime,
    renderer: Option<&crate::preview_base::Base>,
) -> Result<std::process::Child, String> {
    let d = dir();
    let base = base_dir();
    std::fs::create_dir_all(d.join("work")).map_err(|e| e.to_string())?;
    let log = std::fs::File::create(d.join("worker.log")).map_err(|e| e.to_string())?;
    let mut cmd = match std::env::var("DROID_LITE_PREVIEW_WORKER") {
        // tests: a stand-in worker taking the same two arguments
        Ok(c) => {
            let mut parts = c.split_whitespace();
            let mut cmd = Command::new(parts.next().unwrap_or("false"));
            cmd.args(parts);
            cmd
        }
        Err(_) => {
            let r = renderer.ok_or("The preview renderer's files were not checked.")?;
            std::fs::create_dir_all(&base).map_err(|e| e.to_string())?;
            let src = base.join("PreviewWorker.java");
            if std::fs::read_to_string(&src).ok().as_deref() != Some(WORKER_SOURCE) {
                std::fs::write(&src, WORKER_SOURCE).map_err(|e| e.to_string())?;
            }
            let paths = |v: &[PathBuf]| v.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join(":");
            let compose = kt.compose_plugin.clone().unwrap_or_else(|| base.join("compose-plugin.jar"));
            if !compose.is_file() {
                return Err(format!(
                    "The Compose compiler plugin for Kotlin {} is not in the Gradle cache yet (kotlin-compose-compiler-plugin-embeddable). \
                     Build the project once with Gradle, then open the preview again.",
                    kt.version
                ));
            }
            let plugins: Vec<PathBuf> = std::iter::once(compose)
                .chain(kt.parcelize_plugin.clone())
                .chain(kt.serialization_plugin.clone())
                .collect();
            // what the open file compiles against besides the project's libraries: android.jar, or an
            // older pack's compile/ folder (its Compose, android.jar last) for a project with none
            let android = r.android_jar.to_string_lossy().into_owned();
            let mut compile = if rt.jars.is_empty() { base_jars("compile").join(":") } else { android.clone() };
            if !kt.runtime.is_empty() {
                compile = format!("{compile}:{}", paths(&kt.runtime));
            }
            let mut cmd = Command::new(java());
            // headless and no Dock icon: layoutlib renders without a window, and a JVM that touches
            // AWT on macOS otherwise shows up in the Dock
            cmd.arg("-Djava.awt.headless=true")
                .arg("-Dapple.awt.UIElement=true")
                .arg("-Xmx2g")
                .arg("-XX:+UseSerialGC")
                .arg(format!("-Dpaparazzi.layoutlib.runtime.root={}", r.layoutlib_runtime.display()))
                .arg(format!("-Dpaparazzi.layoutlib.resources.root={}", r.layoutlib_resources.display()))
                .arg(format!("-Ddroidlite.plugins={}", paths(&plugins)))
                .arg(format!("-Ddroidlite.compile.cp={compile}"))
                .arg(format!("-Ddroidlite.assets={}", rt.assets.join(":")))
                .arg(format!("-Ddroidlite.res.packages={}", rt.res_packages.join(":")))
                .arg(format!("-Ddroidlite.res.modules={}", rt.module_res.join(":")))
                .arg(format!("-Ddroidlite.aar.dirs={}", rt.exploded.join(":")))
                .arg("-cp")
                // the project's Kotlin compiler (and its stdlib) first, then the renderer, then the
                // project's libraries (or an older pack's Compose), android.jar last (after layoutlib)
                .arg(format!(
                    "{}:{}:{}:{android}",
                    paths(&kt.compiler),
                    paths(&r.runtime),
                    if rt.jars.is_empty() { base_jars("compose").join(":") } else { rt.jars.join(":") },
                ))
                .arg(&src);
            cmd
        }
    };
    cmd.arg(sock)
        .arg(d.join("work"))
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .process_group(0);
    let _ = std::fs::remove_file(sock);
    cmd.spawn().map_err(|e| format!("Could not start the preview worker: {e}"))
}

/// The JVM for the worker: `DROID_LITE_JAVA`, else `$JAVA_HOME/bin/java`, else a JDK 17 (what layoutlib
/// is built for: macOS's java_home, then Homebrew's openjdk@17), else a newer one, else `java`. An app
/// opened from Finder has no JAVA_HOME, so the lookup must not depend on the shell.
fn java() -> PathBuf {
    if let Some(j) = std::env::var_os("DROID_LITE_JAVA") {
        return j.into();
    }
    if let Some(h) = std::env::var_os("JAVA_HOME") {
        return Path::new(&h).join("bin/java");
    }
    let java_home = |v: &str| {
        Command::new("/usr/libexec/java_home")
            .args(["-v", v])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| Path::new(String::from_utf8_lossy(&o.stdout).trim()).join("bin/java"))
    };
    java_home("17")
        .or_else(|| {
            ["/opt/homebrew/opt/openjdk@17/bin/java", "/usr/local/opt/openjdk@17/bin/java"]
                .iter()
                .map(PathBuf::from)
                .find(|p| p.exists())
        })
        .or_else(|| java_home("17+"))
        .unwrap_or_else(|| "java".into())
}

/// Libraries the build scripts name that never run in an app, so never in a preview: test libraries,
/// and build tools (KSP, Lint, detekt, annotation processors, Gradle plugins). Several of them bundle a
/// whole Kotlin compiler, which the worker's compiler then loads as a plugin and crashes on.
fn build_or_test_only(f: &Path) -> bool {
    const PARTS: [&str; 20] = [
        "/kotlin-test", "/junit", "/mockk", "/robolectric", "/espresso", "/truth/", "/turbine/", "/kotest",
        "/com.google.devtools.ksp/", "/com.android.tools.lint/", "/com.android.tools.build/",
        "/io.gitlab.arturbosch.detekt/", "/com.pinterest", "gradle-plugin", "/org.jetbrains.kotlin/kotlin-scripting",
        "/org.jetbrains.kotlin/kotlin-compiler", "/org.jetbrains.kotlin/kotlin-annotation-processing",
        "/com.squareup/kotlinpoet", "/com.google.auto.service/auto-service/", "/com.google.auto.value/auto-value/",
    ];
    let s = f.to_string_lossy();
    let artifact = f.parent().and_then(Path::parent).and_then(Path::parent).and_then(|d| d.file_name()).map_or(String::new(), |n| n.to_string_lossy().into_owned());
    PARTS.iter().any(|p| s.contains(p))
        || artifact.ends_with("-compiler")
        || artifact.ends_with("-processor")
        || artifact.ends_with("-compiler-embeddable")
}

/// The project's dependencies as class jars: jars as they are, an aar's classes.jar copied once into
/// the preview cache (the Gradle cache is never written).
/// The libraries' class jars, their R sources, and their assets folders.
#[derive(Clone, Default)]
struct LibraryCode {
    jars: Vec<String>,
    rjava: Vec<String>,
    assets: Vec<String>,
    /// Unpacked aars that have resources (res/ + AndroidManifest.xml), for the renderer.
    exploded: Vec<String>,
}

/// What a project's worker runs with besides the renderer: the project's own libraries (its Compose,
/// AndroidX, …) and their R classes, compiled once. Empty when the project has no Compose in the
/// Gradle cache: then the base pack's Compose is used.
#[derive(Clone, Debug, Default)]
struct ProjectRuntime {
    jars: Vec<String>,
    assets: Vec<String>,
    /// Every R package (modules and libraries) the renderer maps ids for, its first one the "app".
    res_packages: Vec<String>,
    module_res: Vec<String>,
    exploded: Vec<String>,
}

impl ProjectRuntime {
    fn key(&self) -> u32 {
        if self.jars.is_empty() { 0 } else { fnv(&format!("{:?}{:?}{:?}", self.jars, self.module_res, self.res_packages)) as u32 }
    }

    /// Every file of the resource and assets folders the renderer reads (path, time, size), so an
    /// edited string or drawable draws the pictures again. Folders are read again only when their
    /// time changed ([`Walks`]).
    fn files_key(&self, walks: &mut Walks) -> u64 {
        fn walk(d: &Path, h: &mut u64, w: &mut Walks) {
            let Some(time) = std::fs::metadata(d).ok().and_then(|m| m.modified().ok()) else { return };
            let listing = match w.dirs.remove(d) {
                Some(l) if l.0 == time => l,
                _ => {
                    let Ok(rd) = std::fs::read_dir(d) else { return };
                    w.reads += 1;
                    let mut entries: Vec<_> = rd.flatten().collect();
                    entries.sort_by_key(|e| e.file_name());
                    let (mut subs, mut files) = (Vec::new(), Vec::new());
                    for e in entries {
                        match e.file_type() {
                            Ok(t) if t.is_dir() => subs.push(e.path()),
                            Ok(_) => files.push(e.path()),
                            Err(_) => {}
                        }
                    }
                    (time, subs, files)
                }
            };
            for f in &listing.2 {
                let Ok(m) = std::fs::symlink_metadata(f) else { continue };
                let t = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
                *h = (*h ^ fnv(&format!("{}|{t:?}|{}", f.display(), m.len()))).wrapping_mul(0x100000001b3);
            }
            for s in &listing.1 {
                walk(s, h, w);
            }
            w.dirs.insert(d.to_path_buf(), listing);
        }
        let mut h = fnv("files");
        for d in self.module_res.iter().chain(&self.assets) {
            walk(Path::new(d), &mut h, walks);
        }
        h
    }
}

/// The R classes of every module of the project (made from its res folders, ids unique across all) and
/// of every library (from their R.txt), compiled once with javac into one folder: the renderer loads
/// them from the worker's main class loader to map ids back to resources. Also the R package names.
fn project_r(pk: &crate::preview_src::Packages, lib_r: &[String]) -> Result<(PathBuf, Vec<String>, Vec<String>), String> {
    let mut modules: Vec<(&String, &Vec<PathBuf>)> = pk.modules().collect();
    modules.sort();
    let mut next = 0x7f00_0000u32;
    let mut sources = Vec::new();
    let mut packages = Vec::new();
    let mut res = Vec::new();
    for (ns, dir) in modules {
        let java = crate::preview_src::r_java(ns, dir, &mut next);
        let path = self::dir().join("r").join(ns).join("R.java");
        if std::fs::read_to_string(&path).ok().as_deref() != Some(java.as_str()) {
            std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
            std::fs::write(&path, &java).map_err(|e| e.to_string())?;
        }
        sources.push(path.to_string_lossy().into_owned());
        packages.push(ns.clone());
        res.extend(dir.iter().map(|d| d.to_string_lossy().into_owned()));
    }
    for r in lib_r {
        if let Some(pkg) = std::fs::read_to_string(r).ok().and_then(|s| s.lines().next().map(|l| l.trim_start_matches("package ").trim_end_matches(';').to_string())) {
            packages.push(pkg);
        }
        sources.push(r.clone());
    }
    let out = library_r_classes(&sources)?.ok_or("no R classes")?;
    Ok((out, packages, res))
}

/// The libraries' R classes as one folder of classes, compiled with javac once per set of libraries
/// (they must sit next to the libraries' own code, in the worker's base class loader).
fn library_r_classes(rjava: &[String]) -> Result<Option<PathBuf>, String> {
    if rjava.is_empty() {
        return Ok(None);
    }
    // keyed on the contents (ids change when the R sources are made again), not only the paths
    let all: String = rjava.iter().map(|f| std::fs::read_to_string(f).unwrap_or_default()).collect();
    let out = dir().join(format!("librs-{:016x}", fnv(&all)));
    if out.join(".ok").exists() {
        return Ok(Some(out));
    }
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let args = out.join("sources.txt");
    std::fs::write(&args, rjava.join("\n")).map_err(|e| e.to_string())?;
    let javac = java().with_file_name("javac");
    let o = Command::new(&javac)
        .args(["-nowarn", "-proc:none", "--release", "17", "-d"])
        .arg(&out)
        .arg(format!("@{}", args.display()))
        .output()
        .map_err(|e| format!("Could not run {}: {e}", javac.display()))?;
    if !o.status.success() {
        return Err(format!("The libraries' R classes did not compile:\n{}", String::from_utf8_lossy(&o.stderr)));
    }
    let _ = std::fs::write(out.join(".ok"), "");
    Ok(Some(out))
}

fn library_classpath(root: &Path, scripts: &[PathBuf], progress: &dyn Fn(usize, usize)) -> LibraryCode {
    let aars = dir().join("aar");
    let _ = std::fs::create_dir_all(&aars);
    let mut rjava = Vec::new();
    let mut assets = Vec::new();
    let mut exploded = Vec::new();
    let mut r_packages = std::collections::HashSet::new();
    // ids unique across all libraries (a library's own R.txt has placeholders); kept apart from the
    // ids of the project's modules (0x7f000001 up) by starting high
    let mut next_id = 0x7f80_0000u32;
    let archives: Vec<PathBuf> = crate::libs::class_archives(root, scripts).into_iter().filter(|f| !build_or_test_only(f)).collect();
    let total = archives.len();
    let cp = archives
        .into_iter()
        .enumerate()
        .filter_map(|(i, f)| {
            if i % 10 == 0 {
                progress(i, total);
            }
            if f.extension().is_some_and(|e| e == "jar") {
                return Some(f);
            }
            let key = format!("{:016x}", fnv(&f.to_string_lossy()));
            let out = aars.join(format!("{key}.jar"));
            let r = aars.join(&key).join("R.java");
            if !out.exists() {
                let file = std::fs::File::open(&f).ok()?;
                let mut zip = zip::ZipArchive::new(file).ok()?;
                // the library's R class: AGP makes it from R.txt in the app's build; we do the same
                let text = |zip: &mut zip::ZipArchive<std::fs::File>, name: &str| {
                    let mut s = String::new();
                    zip.by_name(name).ok().map(|mut e| std::io::Read::read_to_string(&mut e, &mut s)).and(Some(s))
                };
                if let (Some(rtxt), Some(manifest)) = (text(&mut zip, "R.txt"), text(&mut zip, "AndroidManifest.xml"))
                    && !rtxt.trim().is_empty()
                    && let Some(pkg) = manifest.split("package=\"").nth(1).and_then(|s| s.split('"').next())
                {
                    let _ = std::fs::create_dir_all(r.parent().unwrap());
                    let _ = std::fs::write(&r, crate::preview_src::r_txt_java(pkg, &rtxt, &mut next_id));
                }
                // its assets (Compose resources and fonts load from there) and resources (strings,
                // drawables the renderer resolves), once, next to its classes
                let assets = aars.join(&key).join("assets");
                for i in 0..zip.len() {
                    let Ok(mut e) = zip.by_index(i) else { continue };
                    let Some(rel) = e.enclosed_name().filter(|n| {
                        (n.starts_with("assets/") || n.starts_with("res/") || n.as_os_str() == "AndroidManifest.xml") && !e.is_dir()
                    }) else { continue };
                    let to = aars.join(&key).join(rel);
                    let _ = std::fs::create_dir_all(to.parent().unwrap());
                    if let Ok(mut w) = std::fs::File::create(&to) {
                        let _ = std::io::copy(&mut e, &mut w);
                    }
                }
                let _ = assets;
                let mut entry = zip.by_name("classes.jar").ok()?;
                let tmp = out.with_extension("part");
                let mut w = std::fs::File::create(&tmp).ok()?;
                std::io::copy(&mut entry, &mut w).ok()?;
                std::fs::rename(&tmp, &out).ok()?;
            }
            let a = aars.join(&key).join("assets");
            if a.is_dir() {
                assets.push(a.to_string_lossy().into_owned());
            }
            if aars.join(&key).join("res").is_dir() {
                exploded.push(aars.join(&key).to_string_lossy().into_owned());
            }
            // one R per package: two versions of one library (both cached) would declare it twice
            if let Some(first) = std::fs::read_to_string(&r).ok().and_then(|s| s.lines().next().map(String::from))
                && r_packages.insert(first)
            {
                rjava.push(r.to_string_lossy().into_owned());
            }
            Some(out)
        })
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    LibraryCode { jars: cp, rjava, assets, exploded }
}

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// How long the worker may be silent before it counts as stopped.
const REPLY_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 120 });

/// Why [`ask`] got no answer.
#[derive(Debug, PartialEq)]
enum NoAnswer {
    /// Another client's request took the worker's one waiting place: ask again, the worker is fine.
    Replaced,
    /// The worker was silent past the reply timeout (it sends a line at least every 10 s, also while
    /// our request waits behind another client's draw): it hung.
    Silent(String),
    /// The connection ended or broke (the worker exited when idle, crashed, or sent garbage): connect
    /// again; another client may have started a fresh worker on the same socket, never stop that one.
    Lost(String),
}

/// Sends one request and waits for its answer, with the imports the compiler could not resolve
/// when it failed (so the caller can tell a missing library from an error in the user's code).
#[allow(clippy::too_many_arguments)]
fn ask(
    w: &mut UnixStream,
    rd: &mut BufReader<UnixStream>,
    id: u64,
    r: &Request,
    specs: &[crate::preview_spec::Spec],
    cp: &[String],
    own: Option<&Own>,
    send: &dyn Fn(Event),
) -> Result<(Event, Vec<String>), NoAnswer> {
    let lost = |e: std::io::Error| NoAnswer::Lost(e.to_string());
    let line = serde_json::json!({
        "id": id, "file": r.file, "class": r.class, "cp": cp,
        "previews": specs.iter().map(crate::preview_spec::Spec::json).collect::<Vec<_>>(),
        "sources": own.map_or(&[][..], |o| &o.sources[..]),
        "rjava": own.map_or(&[][..], |o| &o.rjava[..]),
        "key": own.map_or("", |o| o.key.as_str()),
    });
    // a worker that hangs is given up (a cold compile takes about 25 s between lines)
    rd.get_ref().set_read_timeout(Some(REPLY_TIMEOUT)).map_err(lost)?;
    writeln!(w, "{line}").map_err(lost)?;
    loop {
        let mut reply = String::new();
        match rd.read_line(&mut reply) {
            Ok(0) => return Err(NoAnswer::Lost("the connection closed".into())),
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                return Err(NoAnswer::Silent(format!("no answer in {} s", REPLY_TIMEOUT.as_secs())));
            }
            Err(e) => return Err(lost(e)),
        }
        let v: serde_json::Value = serde_json::from_str(&reply).map_err(|e| NoAnswer::Lost(e.to_string()))?;
        if v["id"].as_u64() != Some(id) {
            continue; // an older answer
        }
        if v["skipped"].as_bool() == Some(true) {
            return Err(NoAnswer::Replaced);
        }
        if let Some(step) = v["status"].as_str() {
            // the worker's real step, while it works
            let n = (v["done"].as_u64().unwrap_or(0), v["total"].as_u64().unwrap_or(0));
            send(Event::Status(r.file.clone(), step.to_string(), (n.1 > 0).then_some(n)));
            continue;
        }
        if v["ok"].as_bool() != Some(true) {
            let raw = v["error"].as_str().unwrap_or("The preview failed.");
            let error = short_error(raw, &r.file);
            return Ok((Event::Failed { file: r.file.clone(), seq: r.seq, error }, unresolved_imports(raw)));
        }
        let cards = v["images"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|img| {
                let name = img["name"].as_str().unwrap_or("").to_string();
                if let Some(e) = img["error"].as_str() {
                    return (name, Err(short_error(e, &r.file)));
                }
                let png = img["png"].as_str().unwrap_or("");
                let pic = image::open(png)
                    .map(|i| {
                        let i = i.to_rgba8();
                        egui::ColorImage::from_rgba_unmultiplied([i.width() as usize, i.height() as usize], i.as_raw())
                    })
                    .map_err(|e| format!("Could not read the picture: {e}"));
                if std::env::var_os("DROID_LITE_KEEP_PREVIEW_PNGS").is_none() {
                    let _ = std::fs::remove_file(png); // the texture holds it now
                }
                (name, pic)
            })
            .collect();
        return Ok((Event::Done { file: r.file.clone(), seq: r.seq, cards }, Vec::new()));
    }
}

/// The imports the compiler could not resolve: an "unresolved reference" error line followed by the
/// source line it points at, an `import`.
fn unresolved_imports(text: &str) -> Vec<String> {
    text.lines()
        .collect::<Vec<_>>()
        .windows(2)
        .filter(|w| w[0].contains("unresolved reference"))
        .filter_map(|w| w[1].trim().strip_prefix("import ").map(|i| i.split_whitespace().next().unwrap_or("").to_string()))
        .collect()
}

/// The compiler's text for a card, grouped: what is missing (packages of the imports that do not
/// resolve, with a count each), then the first error lines with paths cut to the file name.
/// (The compiler prints paths relative to the worker's folder, so a full path may come without its
/// leading "/": every path is cut to its file name, whatever its form.)
fn short_error(text: &str, file: &Path) -> String {
    let _ = file;
    let lines: Vec<String> = text.lines().map(file_names_only).collect();
    let mut missing: Vec<(String, usize)> = Vec::new();
    for w in text.lines().collect::<Vec<_>>().windows(2) {
        if w[0].contains("unresolved reference")
            && let Some(import) = w[1].trim().strip_prefix("import ")
        {
            // the last part is the imported name (a class or a function), never the package
            let parts: Vec<&str> = import.split('.').collect();
            let pkg: Vec<&str> = parts[..parts.len().saturating_sub(1)]
                .iter()
                .copied()
                .take_while(|s| s.starts_with(|c: char| c.is_lowercase()))
                .collect();
            let group = pkg.iter().take(4).copied().collect::<Vec<_>>().join(".");
            match missing.iter_mut().find(|(g, _)| *g == group) {
                Some((_, n)) => *n += 1,
                None => missing.push((group, 1)),
            }
        }
    }
    let errors = lines.iter().filter(|l| l.contains("error:")).count();
    let mut out = String::new();
    if !missing.is_empty() {
        out.push_str(&format!("Not found ({} packages):\n", missing.len()));
        for (g, n) in missing.iter().take(6) {
            out.push_str(&format!("  {g} ({n})\n"));
        }
        if missing.len() > 6 {
            out.push_str(&format!("  … {} more\n", missing.len() - 6));
        }
        out.push('\n');
    }
    let first: Vec<&String> = lines.iter().filter(|l| l.contains("error:")).take(3).collect();
    for l in &first {
        out.push_str(l);
        out.push('\n');
    }
    if errors > first.len() {
        out.push_str(&format!("… {} more errors", errors - first.len()));
    }
    if out.is_empty() {
        out = lines.iter().take(12).cloned().collect::<Vec<_>>().join("\n");
    }
    out.trim_end().to_string()
}

/// `a/b/Card.kt:3:5: error: …` (with or without the leading "/") -> `Card.kt:3:5: error: …`.
fn file_names_only(line: &str) -> String {
    match line.find(".kt:").or_else(|| line.find(".java:")) {
        Some(end) => {
            let start = line[..end].rfind(['/', ' ']).map_or(0, |i| i + 1);
            if line[..start].contains(' ') && !line[..start].trim_end().ends_with(':') {
                return line.to_string(); // a path in the middle of a sentence: leave it
            }
            line[start..].to_string()
        }
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_tools_and_tests_stay_out_of_previews() {
        let c = "/Users/n/.gradle/caches/modules-2/files-2.1";
        for no in ["com.google.devtools.ksp/symbol-processing-aa-embeddable/2.3.0/h/x.jar", "com.google.dagger/hilt-compiler/2.60/h/x.jar",
            "androidx.room/room-compiler/2.7/h/x.jar", "io.mockk/mockk/1.13/h/mockk-1.13.jar", "junit/junit/4.13/h/junit-4.13.jar",
            "org.jetbrains.kotlin/kotlin-scripting-compiler-embeddable/2.4.21/h/x.jar", "com.android.tools.lint/lint-api/31/h/x.jar"] {
            assert!(build_or_test_only(Path::new(&format!("{c}/{no}"))), "{no}");
        }
        for yes in ["androidx.compose.runtime/runtime-android/1.9/h/x.aar", "com.google.dagger/dagger/2.60.1/h/dagger-2.60.1.jar",
            "org.jetbrains.kotlinx/kotlinx-serialization-core-jvm/1.8/h/x.jar",
            "com.google.auto.service/auto-service-annotations/1.1/h/x.jar"] {
            assert!(!build_or_test_only(Path::new(&format!("{c}/{yes}"))), "{yes}");
        }
    }

    #[test]
    fn compiler_errors_are_grouped_by_what_is_missing_and_paths_cut_to_the_file() {
        let f = Path::new("/Users/n/a/Screen.kt");
        let text = "Users/n/a/Screen.kt:5:30: error: unresolved reference 'compose'.\n\
import androidx.activity.compose.rememberLauncherForActivityResult\n\
                             ^^^^^^^\n\
Users/n/a/Screen.kt:6:30: error: unresolved reference 'result'.\n\
import androidx.activity.result.IntentSenderRequest\n\
/Users/n/a/Screen.kt:17:22: error: unresolved reference 'designsystem'.\n\
import co.branch.designsystem.neem.component.scaffold.Scaffold\n\
/Users/n/a/Screen.kt:18:22: error: unresolved reference 'designsystem'.\n\
import co.branch.designsystem.neem.component.toast.ToastVariant\n";
        let s = short_error(text, f);
        assert!(s.starts_with("Not found (3 packages):\n  androidx.activity.compose (1)\n  androidx.activity.result (1)\n  co.branch.designsystem.neem (2)"), "{s}");
        assert!(s.contains("\nScreen.kt:5:30: error: unresolved reference 'compose'."), "{s}");
        assert!(!s.contains("Users/n"), "paths cut to the file name, with or without the leading /: {s}");
        assert!(s.ends_with("… 1 more errors"), "{s}");
    }

    fn request(seq: u64) -> Request {
        Request { file: PathBuf::from(format!("/p/F{seq}.kt")), class: "FKt".into(), previews: vec!["P".into()], seq, project: None }
    }

    /// The events `next_job` sent, as (kind, seq, text).
    type Log = std::rc::Rc<std::cell::RefCell<Vec<(String, u64, String)>>>;

    fn record() -> (Log, impl Fn(Event)) {
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let l = log.clone();
        (log, move |e: Event| {
            let row = match e {
                Event::Skipped { seq, reason, .. } => ("skipped".into(), seq, reason),
                Event::Status(_, t, _) => ("status".into(), 0, t),
                Event::Done { seq, .. } => ("done".into(), seq, String::new()),
                Event::Failed { seq, error, .. } => ("failed".into(), seq, error),
            };
            l.borrow_mut().push(row);
        })
    }

    #[test]
    fn every_request_the_loop_drops_or_skips_is_answered() {
        let (tx, jobs) = channel();
        for seq in 1..=3 {
            tx.send(request(seq)).unwrap();
        }
        let (log, send) = record();
        let go = || String::new();
        assert_eq!(next_job(&jobs, &send, &go).map(|r| r.seq), Some(3), "the newest wins");
        let replaced = |seq| ("skipped".to_string(), seq, "replaced by a newer request".to_string());
        assert_eq!(*log.borrow(), vec![replaced(1), replaced(2)]);
        // no room now: the request is answered with the reason, then the loop waits for the next
        log.borrow_mut().clear();
        tx.send(request(4)).unwrap();
        drop(tx);
        let busy = || "Skipped: mac-guard says memory is critical.".to_string();
        assert!(next_job(&jobs, &send, &busy).is_none());
        let why = busy();
        assert_eq!(*log.borrow(), vec![("status".to_string(), 0, why.clone()), ("skipped".to_string(), 4, why)]);
    }

    #[test]
    fn a_skipped_reply_for_this_request_is_an_error_not_an_endless_wait() {
        let (mut w, peer) = UnixStream::pair().unwrap();
        let mut rd = BufReader::new(w.try_clone().unwrap());
        let worker = std::thread::spawn(move || {
            let mut line = String::new();
            let mut r = BufReader::new(peer.try_clone().unwrap());
            r.read_line(&mut line).unwrap();
            let mut p = peer;
            writeln!(p, r#"{{"id":7,"ok":false,"skipped":true}}"#).unwrap();
            p // kept open: the answer must not depend on the socket closing
        });
        let (done, got) = channel();
        std::thread::spawn(move || {
            let _ = done.send(ask(&mut w, &mut rd, 7, &request(1), &[], &[], None, &|_| {}).err());
        });
        let err = got.recv_timeout(Duration::from_secs(10)).expect("an answer, not a hang");
        assert_eq!(err, Some(NoAnswer::Replaced));
        drop(worker.join());
    }

    #[test]
    fn a_silent_worker_is_given_up_after_the_reply_timeout() {
        let (mut w, peer) = UnixStream::pair().unwrap();
        let mut rd = BufReader::new(w.try_clone().unwrap());
        let (done, got) = channel();
        std::thread::spawn(move || {
            let _ = done.send(ask(&mut w, &mut rd, 1, &request(1), &[], &[], None, &|_| {}).is_err());
        });
        assert_eq!(got.recv_timeout(Duration::from_secs(10)), Ok(true), "no endless wait");
        drop(peer);
    }

    #[test]
    fn a_closed_connection_or_garbage_is_lost_not_silent() {
        // the worker reads the request, then closes the connection (or sends garbage first)
        for peer_says in [None, Some("not json\n")] {
            let (mut w, peer) = UnixStream::pair().unwrap();
            let mut rd = BufReader::new(w.try_clone().unwrap());
            let worker = std::thread::spawn(move || {
                let mut line = String::new();
                BufReader::new(&peer).read_line(&mut line).unwrap();
                if let Some(said) = peer_says {
                    (&peer).write_all(said.as_bytes()).unwrap();
                }
            });
            let err = ask(&mut w, &mut rd, 1, &request(1), &[], &[], None, &|_| {}).err();
            assert!(matches!(err, Some(NoAnswer::Lost(_))), "{peer_says:?}: {err:?}");
            worker.join().unwrap();
        }
        // a connection the worker closed before the request: writing fails
        let (mut w, peer) = UnixStream::pair().unwrap();
        let mut rd = BufReader::new(w.try_clone().unwrap());
        drop(peer);
        let err = ask(&mut w, &mut rd, 1, &request(1), &[], &[], None, &|_| {}).err();
        assert!(matches!(err, Some(NoAnswer::Lost(_))), "{err:?}");
        // a silent one, still connected
        let (mut w, peer) = UnixStream::pair().unwrap();
        let mut rd = BufReader::new(w.try_clone().unwrap());
        let err = ask(&mut w, &mut rd, 1, &request(1), &[], &[], None, &|_| {}).err();
        assert!(matches!(err, Some(NoAnswer::Silent(_))), "{err:?}");
        drop(peer);
    }

    #[test]
    fn a_request_waiting_behind_another_draw_is_kept_alive_by_status_lines() {
        let (mut w, peer) = UnixStream::pair().unwrap();
        let mut rd = BufReader::new(w.try_clone().unwrap());
        let worker = std::thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(peer.try_clone().unwrap()).read_line(&mut line).unwrap();
            let mut p = peer;
            // silent for longer than the reply timeout (1 s in tests) in all, never between lines
            for _ in 0..6 {
                writeln!(p, r#"{{"id":3,"status":"Waiting for another preview to finish","done":0,"total":0}}"#).unwrap();
                std::thread::sleep(Duration::from_millis(400));
            }
            writeln!(p, r#"{{"id":3,"ok":false,"error":"x"}}"#).unwrap();
            p
        });
        let (log, send) = record();
        let r = ask(&mut w, &mut rd, 3, &request(1), &[], &[], None, &send);
        assert!(matches!(r, Ok((Event::Failed { .. }, _))), "{:?}", r.err());
        assert_eq!(log.borrow()[0].2, "Waiting for another preview to finish");
        drop(worker.join());
        // the real worker sends those lines from its idle thread while a request waits
        assert!(WORKER_SOURCE.contains("if (waiting != null) status(waiting, \"Waiting for another preview to finish\""));
    }

    #[test]
    fn a_worker_that_closed_the_connection_is_connected_again_not_stopped() {
        let _lock = crate::app::test_lock();
        let d = tmp("lost");
        // serves one request per connection, then closes it (like a worker that exited when idle
        // and was started again by another window); counts its starts
        let fake = "import json,os,shutil,signal,socket,sys\nsignal.alarm(30)\nopen(os.environ['STARTS'],'a').write('x')\n\
                    s=socket.socket(socket.AF_UNIX)\ns.bind(sys.argv[1])\ns.listen(5)\nwhile True:\n\
                    \tc,_=s.accept()\n\tf=c.makefile('rw')\n\tr=json.loads(f.readline())\n\
                    \tpng=os.path.join(sys.argv[2],'%d.png'%r['id'])\n\tos.makedirs(sys.argv[2],exist_ok=True)\n\
                    \tshutil.copy(os.environ['FAKE_PREVIEW_PNG'],png)\n\
                    \tf.write(json.dumps({'id':r['id'],'ok':True,'images':[{'name':'P','png':png}]})+'\\n')\n\
                    \tf.flush()\n\tf.close()\n\tc.close()\n";
        std::fs::write(d.join("w.py"), fake).unwrap();
        let vars = ["DROID_LITE_PREVIEW_DIR", "DROID_LITE_PREVIEW_WORKER", "FAKE_PREVIEW_PNG", "STARTS"];
        let old: Vec<_> = vars.iter().map(std::env::var_os).collect();
        // SAFETY: the test lock is held, so no other test reads the environment meanwhile.
        unsafe {
            std::env::set_var(vars[0], d.join("pv"));
            std::env::set_var(vars[1], format!("python3 {}", d.join("w.py").display()));
            std::env::set_var(vars[2], Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/icon-256.png"));
            std::env::set_var(vars[3], d.join("starts"));
        }
        let (log, send) = record();
        let mut state = State::default();
        state.draw(&request(1), &send);
        state.draw(&request(2), &send);
        let kinds: Vec<(String, u64)> = log.borrow().iter().filter(|e| e.0 != "status").map(|e| (e.0.clone(), e.1)).collect();
        assert_eq!(kinds, [("done".to_string(), 1), ("done".to_string(), 2)], "{:?}", log.borrow());
        assert_eq!(std::fs::read_to_string(d.join("starts")).unwrap(), "x", "the same worker serves both");
        for e in std::fs::read_dir(d.join("pv")).unwrap().flatten() {
            if e.path().extension().is_some_and(|x| x == "sock") {
                stop_worker(&e.path(), Duration::from_secs(1));
            }
        }
        for (k, v) in vars.iter().zip(old) {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_worker_that_never_listens_is_killed_and_reaped() {
        let _lock = crate::app::test_lock();
        let d = tmp("nolisten");
        let vars = ["DROID_LITE_PREVIEW_DIR", "DROID_LITE_PREVIEW_WORKER"];
        let old: Vec<_> = vars.iter().map(std::env::var_os).collect();
        // SAFETY: the test lock is held, so no other test reads the environment meanwhile.
        unsafe {
            std::env::set_var(vars[0], d.join("pv"));
            std::env::set_var(vars[1], "perl -e sleep(30)"); // takes the socket and work dir, never listens
        }
        let sock = d.join("pv/w-test.sock");
        let r = connect_or_start(&|_| {}, Path::new("/p/A.kt"), &sock, &Kotlin::default(), &ProjectRuntime::default(), &mut None);
        let e = r.err().unwrap_or_default();
        assert!(e.contains("did not start in 2 s"), "{e}");
        let pid: i32 = std::fs::read_to_string(d.join("pv/worker.pid")).unwrap().split(' ').next().unwrap().parse().unwrap();
        // a zombie still answers signal 0; a reaped process is gone
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "pid {pid} was not reaped");
        for (k, v) in vars.iter().zip(old) {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn after_a_failed_request_libraries_packages_and_compiler_errors_are_read_again() {
        let mut reads = Reads { libs: Some((PathBuf::from("/p"), LibraryCode::default())), ..Default::default() };
        reads.kotlins.insert("2.2.20".into(), Ok(Kotlin::default()));
        reads.kotlins.insert("2.3.0".into(), Err("not on this Mac yet".into()));
        reads.forget_errors();
        assert!(reads.libs.is_none() && reads.packages.is_none());
        assert_eq!(reads.kotlins.keys().collect::<Vec<_>>(), ["2.2.20"], "the found compiler stays");
    }

    #[test]
    fn the_cache_key_covers_the_worker_the_renderer_and_resource_files() {
        let d = std::env::temp_dir().join(format!("dl-preview-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("res/values")).unwrap();
        std::fs::create_dir_all(d.join("assets")).unwrap();
        std::fs::write(d.join("res/values/strings.xml"), "<a/>").unwrap();
        let rt = ProjectRuntime {
            jars: vec!["/x.jar".into()],
            module_res: vec![d.join("res").to_string_lossy().into_owned()],
            assets: vec![d.join("assets").to_string_lossy().into_owned()],
            ..Default::default()
        };
        let mut walks = Walks::default();
        let mut k = |w: &str, r: &str| cache_key(&["/p/A.kt", "text"], &rt, w, r, &mut walks);
        let first = k("worker v1", "renderer v1");
        assert_eq!(first, k("worker v1", "renderer v1"), "nothing changed");
        assert_ne!(first, k("worker v2", "renderer v1"));
        assert_ne!(first, k("worker v1", "renderer v2"));
        std::fs::write(d.join("res/values/strings.xml"), "<a>b</a>").unwrap();
        let second = k("worker v1", "renderer v1");
        assert_ne!(first, second, "an edited string (its folder's time stays)");
        std::fs::write(d.join("assets/font.ttf"), "f").unwrap();
        let third = k("worker v1", "renderer v1");
        assert_ne!(second, third, "a new asset");
        std::fs::create_dir_all(d.join("res/drawable")).unwrap();
        std::fs::write(d.join("res/drawable/a.xml"), "<v/>").unwrap();
        assert_ne!(third, k("worker v1", "renderer v1"), "a new folder with a file");
        // unchanged folders are not read again: res, res/values, res/drawable, assets read once
        let before = walks.reads;
        let again = cache_key(&["/p/A.kt", "text"], &rt, "worker v1", "renderer v1", &mut walks);
        assert_eq!(walks.reads, before, "a cache hit reads no folder");
        assert_eq!(again, cache_key(&["/p/A.kt", "text"], &rt, "worker v1", "renderer v1", &mut Walks::default()), "the same key as a full walk");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn at_most_two_workers_run_the_youngest_other_one_stays() {
        assert_eq!(workers_to_stop(vec![]), Vec::<i32>::new());
        assert_eq!(workers_to_stop(vec![(10, 900)]), Vec::<i32>::new(), "another window's worker stays");
        assert_eq!(workers_to_stop(vec![(10, 900), (11, 30), (12, 4000)]), vec![10, 12], "oldest go");
        assert_eq!(etime_secs("05:07"), Some(307));
        assert_eq!(etime_secs("01:00:01"), Some(3601));
        assert_eq!(etime_secs("2-00:00:10"), Some(172_810));
        assert_eq!(etime_secs("x"), None);
    }

    #[test]
    fn mac_guard_status_decides_whether_previews_run() {
        let ok = "level Ok\nMemory 70% free · swap 1.6 GB free · disk 15 GB free\n";
        assert_eq!(room_from_status(ok), "");
        let warn = "level Warn\nMemory 30% free · swap 1.0 GB free · disk 12 GB free\n";
        assert_eq!(room_from_status(warn), "", "Warn still renders");
        let critical = "level Critical\nMemory 10% free · swap 0.3 GB free · disk 15 GB free\n";
        assert!(room_from_status(critical).contains("memory is critical"));
        let disk = "level Warn\nMemory 50% free · swap 1.0 GB free · disk 7 GB free\n";
        assert!(room_from_status(disk).contains("7 GB free"));
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dl-pv-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_hung_worker_is_stopped_and_the_retry_starts_a_fresh_one() {
        let _lock = crate::app::test_lock();
        let d = tmp("hung");
        let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_preview_worker.py");
        // the first worker takes the connection and never answers; the next one draws
        let hang = "import socket,sys,time,signal; signal.alarm(30); s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen(5); c=s.accept(); time.sleep(30)";
        let script = d.join("w.sh");
        std::fs::write(&script, format!("if [ ! -e \"$0.hung\" ]; then touch \"$0.hung\"; exec python3 -c '{hang}' \"$@\"; fi\nexec python3 {} \"$@\"\n", fake.display())).unwrap();
        let vars = ["DROID_LITE_PREVIEW_DIR", "DROID_LITE_PREVIEW_WORKER", "FAKE_PREVIEW_PNG"];
        let old: Vec<_> = vars.iter().map(std::env::var_os).collect();
        // SAFETY: the test lock is held, so no other test reads the environment meanwhile.
        unsafe {
            std::env::set_var(vars[0], d.join("pv"));
            std::env::set_var(vars[1], format!("sh {}", script.display()));
            std::env::set_var(vars[2], Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/icon-256.png"));
        }
        let (log, send) = record();
        let mut state = State::default();
        state.draw(&request(1), &send);
        let kinds: Vec<(String, u64)> = log.borrow().iter().filter(|e| e.0 != "status").map(|e| (e.0.clone(), e.1)).collect();
        assert_eq!(kinds, [("done".to_string(), 1)], "{:?}", log.borrow());
        // an ordinary compile error in the user's code reads nothing again
        let broken = Request { class: "BrokenKt".into(), ..request(2) };
        state.draw(&broken, &send);
        assert_eq!(log.borrow().last().map(|e| e.0.as_str()), Some("failed"));
        assert!(!state.forget, "an error in the user's code is not a missing library");
        for e in std::fs::read_dir(d.join("pv")).unwrap().flatten() {
            if e.path().extension().is_some_and(|x| x == "sock") {
                stop_worker(&e.path(), Duration::from_secs(1));
            }
        }
        for (k, v) in vars.iter().zip(old) {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn stopping_a_worker_ends_it_even_when_it_ignores_sigterm_and_removes_its_socket() {
        let d = tmp("stop");
        let sock = d.join("w-test.sock");
        std::fs::write(&sock, "").unwrap();
        let mut child = Command::new("perl").args(["-e", "$SIG{TERM} = 'IGNORE'; sleep 60"]).arg(&sock).spawn().unwrap();
        let pid = child.id() as i32;
        let reaper = std::thread::spawn(move || child.wait());
        let until = Instant::now() + Duration::from_secs(5);
        while worker_pids(&sock).is_empty() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(worker_pids(&sock), [pid], "found by its socket argument");
        let t = Instant::now();
        stop_worker(&sock, Duration::from_millis(300));
        assert!(reaper.join().unwrap().is_ok());
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        assert!(!sock.exists(), "the next try must not connect to it");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_panic_while_drawing_answers_that_request_and_the_loop_goes_on() {
        let (tx, jobs) = channel();
        tx.send(request(1)).unwrap();
        let tx = std::cell::RefCell::new(Some(tx));
        let (log, send) = record();
        let mut draw = |r: &Request| {
            if r.seq == 1 {
                tx.borrow().as_ref().unwrap().send(request(2)).unwrap();
                panic!("boom");
            }
            tx.borrow_mut().take(); // the last request: the loop ends after it
            send(Event::Done { file: r.file.clone(), seq: r.seq, cards: vec![] });
        };
        serve_jobs(&jobs, &send, &String::new, &mut draw);
        let failed = ("failed".to_string(), 1, "internal error: boom".to_string());
        assert_eq!(*log.borrow(), vec![failed, ("done".to_string(), 2, String::new())]);
    }

    #[test]
    fn a_mac_guard_that_hangs_says_go_after_the_limit() {
        let d = tmp("guard");
        let (hang, critical) = (d.join("hang"), d.join("critical"));
        std::fs::write(&hang, "#!/bin/sh\nexec sleep 10\n").unwrap();
        std::fs::write(&critical, "#!/bin/sh\necho 'level Critical'\n").unwrap();
        for f in [&hang, &critical] {
            std::fs::set_permissions(f, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        }
        let t = Instant::now();
        assert_eq!(room_from(&hang, Duration::from_millis(300)), "");
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        assert!(room_from(&critical, Duration::from_secs(5)).contains("memory is critical"));
        assert_eq!(room_from(&d.join("none"), Duration::from_secs(1)), "", "no mac-guard");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn pictures_of_a_file_saved_while_drawing_are_not_kept_under_the_old_texts_key() {
        let d = tmp("race");
        let (f, cache) = (d.join("Card.kt"), d.join("cache"));
        std::fs::write(&f, "B").unwrap(); // saved while the worker drew text "A"
        let cards: crate::preview_cache::Cards = vec![("P".into(), Ok(egui::ColorImage::new([1, 1], vec![egui::Color32::WHITE])))];
        store_if_current(&cache, "k-a", &f, "A", &cards);
        let now = crate::preview_cache::now_secs();
        assert!(crate::preview_cache::lookup(&cache, "k-a", now).is_none());
        store_if_current(&cache, "k-b", &f, "B", &cards);
        assert!(crate::preview_cache::lookup(&cache, "k-b", now).is_some(), "unchanged: kept");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn only_library_or_compiler_failures_read_the_project_again() {
        let d = tmp("trouble");
        let f = d.join("app/src/main/java/demo/Card.kt");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, "package demo\nclass Thing\n").unwrap();
        let pk = crate::preview_src::Packages::read(std::slice::from_ref(&f));
        let trouble = |raw: &str| library_trouble(&short_error(raw, &f), &unresolved_imports(raw), Some(&pk));
        assert!(!trouble("Card.kt:3:5: error: unresolved reference 'Oops'.\n    Oops()\n"), "the user's code");
        assert!(!trouble("Card.kt:1:8: error: unresolved reference 'Thing'.\nimport demo.Thing\n"), "a project package");
        assert!(trouble("Card.kt:1:8: error: unresolved reference 'compose'.\nimport androidx.activity.compose.X\n"), "a library");
        assert!(library_trouble(NO_COMPOSE, &[], Some(&pk)));
        assert!(library_trouble("The Kotlin 2.3.0 compiler is not on this Mac yet (it comes …).", &[], Some(&pk)));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn files_added_since_are_read_into_the_packages_and_the_libraries_are_kept() {
        let d = tmp("refresh");
        let src = d.join("app/src/main/java");
        std::fs::create_dir_all(src.join("demo/extra")).unwrap();
        std::fs::write(src.join("demo/Card.kt"), "package demo\nclass Card\n").unwrap();
        std::fs::write(src.join("demo/extra/New.kt"), "package demo.extra\nclass New\n").unwrap();
        let p = |files: Vec<PathBuf>| ProjectRef { root: d.clone(), scripts: vec![], files: std::sync::Arc::new(files) };
        let lib = LibraryCode { jars: vec!["/lib.jar".into()], ..Default::default() };
        let mut reads = Reads { libs: Some((d.clone(), lib)), ..Default::default() };
        let no_status = |_: &str, _| panic!("the libraries are read once per project");
        reads.refresh(&p(vec![src.join("demo/Card.kt")]), &no_status);
        let pk = |r: &Reads| r.packages.as_ref().unwrap().1.package_for("demo.extra.New").map(String::from);
        assert_eq!(pk(&reads).as_deref(), Some("demo"), "not read yet: only the outer package");
        reads.refresh(&p(vec![src.join("demo/Card.kt"), src.join("demo/extra/New.kt")]), &no_status);
        assert_eq!(pk(&reads).as_deref(), Some("demo.extra"), "the new folder's package");
        assert_eq!(reads.libs.as_ref().unwrap().1.jars, ["/lib.jar"]);
        let _ = std::fs::remove_dir_all(d);
    }
}
