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
    /// The real step in words ("Compiling the project code (42 files)", "Drawing 2 of 3 previews"),
    /// with (done, total) when the count is known; or a reason it cannot run ("Skipped: …").
    Status(String, Option<(u64, u64)>),
    /// One picture or one error text per preview, in request order.
    Done { file: PathBuf, cards: Vec<(String, Result<egui::ColorImage, String>)> },
    /// The whole request failed (compile error, missing class): the text for the cards.
    Failed { file: PathBuf, error: String },
}

pub struct Client {
    tx: Sender<Request>,
    rx: Receiver<Event>,
}

impl Client {
    pub fn new(ctx: &egui::Context) -> Client {
        let (tx, jobs) = channel::<Request>();
        let (events, rx) = channel();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("preview-client".into())
            .spawn(move || run(jobs, events, ctx))
            .expect("preview thread");
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
    let Ok(out) = Command::new(&guard).arg("status").output() else {
        return String::new(); // no mac-guard: nothing says no
    };
    room_from_status(&String::from_utf8_lossy(&out.stdout))
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

fn run(jobs: Receiver<Request>, events: Sender<Event>, ctx: egui::Context) {
    let send = |e: Event| {
        let _ = events.send(e);
        ctx.request_repaint();
    };
    let mut conn: Option<(UnixStream, BufReader<UnixStream>)> = None;
    let mut id = 0u64;
    // the project's libraries, read once per project root (the Gradle cache does not change under us)
    let mut libs: Option<(PathBuf, LibraryCode)> = None;
    let mut packages: Option<(PathBuf, crate::preview_src::Packages)> = None;
    let mut assets: Vec<String> = Vec::new();
    let (mut res_packages, mut module_res, mut exploded): (Vec<String>, Vec<String>, Vec<String>) = Default::default();
    let mut kotlins: std::collections::HashMap<String, Result<Kotlin, String>> = std::collections::HashMap::new();
    let mut conn_version = String::new();
    while let Ok(mut r) = jobs.recv() {
        while let Ok(newer) = jobs.try_recv() {
            r = newer; // the newest request wins
        }
        let why = room();
        if !why.is_empty() {
            send(Event::Status(why, None));
            continue;
        }
        let (cp, own) = match &r.project {
            Some(p) => {
                if libs.as_ref().is_none_or(|(r0, _)| *r0 != p.root) {
                    send(Event::Status("Reading the project's libraries".into(), None));
                    let progress = |i: usize, n: usize| send(Event::Status("Unpacking the project's libraries".into(), Some((i as u64, n as u64))));
                    libs = Some((p.root.clone(), library_classpath(&p.root, &p.scripts, &progress)));
                    packages = Some((p.root.clone(), crate::preview_src::Packages::read(&p.files)));
                }
                let lc = libs.as_ref().map(|(_, l)| l.clone()).unwrap_or_default();
                let (mut cp, lib_r) = (lc.jars, lc.rjava);
                assets = lc.assets;
                // the project's own Compose runs the preview when the Gradle cache has it
                if cp.iter().any(|j| j.contains("/androidx.compose.runtime/")) {
                    match project_r(&packages.as_ref().unwrap().1, &lib_r) {
                        Ok((rs, pkgs, res)) => {
                            cp.push(rs.to_string_lossy().into_owned());
                            res_packages = pkgs;
                            module_res = res;
                            exploded = lc.exploded.clone();
                        }
                        Err(e) => {
                            send(Event::Failed { file: r.file.clone(), error: e });
                            continue;
                        }
                    }
                } else {
                    cp.clear(); // not usable on its own: the base pack's Compose instead
                }
                match own_code(&packages.as_ref().unwrap().1, &r.file) {
                    Ok(o) => (cp, Some(o)),
                    Err(e) => {
                        send(Event::Failed { file: r.file.clone(), error: e });
                        continue;
                    }
                }
            }
            None => (Vec::new(), None),
        };
        let version = kotlin_version(r.project.as_ref().map(|p| p.root.as_path()));
        let stand_in = std::env::var_os("DROID_LITE_PREVIEW_WORKER").is_some(); // tests: no compiler needed
        if cp.is_empty() && !stand_in && base_jars("compose").is_empty() {
            send(Event::Failed { file: r.file.clone(), error: NO_COMPOSE.into() });
            continue;
        }
        let kt = match kotlins.entry(version.clone()).or_insert_with(|| {
            if stand_in { Ok(Kotlin { version: version.clone(), ..Default::default() }) } else { kotlin(&version) }
        }) {
            Ok(k) => k.clone(),
            Err(e) => {
                send(Event::Failed { file: r.file.clone(), error: e.clone() });
                continue;
            }
        };
        // never Kotlin's own compiler-side jars on the worker's classpath: the worker's compiler would
        // load them as plugins (a scripting plugin of another version crashed it); it brings its stdlib
        let text = std::fs::read_to_string(&r.file).unwrap_or_default();
        let specs = {
            let empty = std::collections::HashMap::new();
            let classes = packages.as_ref().map_or(&empty, |(_, p)| &p.annotation_classes);
            specs_for(&text, &r.previews, classes)
        };
        let rt = ProjectRuntime {
            jars: cp.iter().filter(|j| !j.contains("/org.jetbrains.kotlin/")).cloned().collect(),
            assets: if cp.is_empty() { Vec::new() } else { assets.clone() },
            res_packages: if cp.is_empty() { Vec::new() } else { res_packages.clone() },
            module_res: if cp.is_empty() { Vec::new() } else { module_res.clone() },
            exploded: if cp.is_empty() { Vec::new() } else { exploded.clone() },
        };
        // the same file, project code, previews and libraries as a recent draw: its pictures at once
        let cache_dir = dir().join("cache");
        let cache_key = crate::preview_cache::key(&[
            &r.file.to_string_lossy(),
            &text,
            own.as_ref().map_or("", |o| o.key.as_str()),
            &format!("{specs:?}"),
            &version,
            &format!("{:08x}", rt.key()),
        ]);
        if !stand_in && let Some(cards) = crate::preview_cache::lookup(&cache_dir, &cache_key, crate::preview_cache::now_secs()) {
            send(Event::Done { file: r.file.clone(), cards });
            continue;
        }
        let worker_id = format!("{version}-{:08x}", rt.key());
        if conn_version != worker_id {
            conn = None; // another project's Kotlin or libraries: its own worker
            conn_version = worker_id;
        }
        // the worker may have ended (idle exit, a crash) or never started: start it and connect by
        // itself, up to 3 tries with a short back-off, then the error goes on the card
        let mut last_error = String::new();
        let mut base = None;
        for attempt in 0..3u32 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(500 << attempt));
            }
            if conn.is_none() {
                match connect_or_start(&send, &kt, &rt, &mut base) {
                    Ok(c) => conn = Some(c),
                    Err(e) => {
                        last_error = e;
                        if base.is_none() && !stand_in {
                            break; // the renderer's files are missing: the card says which, no retry
                        }
                        continue;
                    }
                }
            }
            send(Event::Status(format!("Sending {} preview(s) to the worker", r.previews.len()), None));
            id += 1;
            let (w, rd) = conn.as_mut().unwrap();
            match ask(w, rd, id, &r, &specs, &cp, own.as_ref(), &send) {
                Ok(e) => {
                    if let Event::Done { file, cards } = &e
                        && !stand_in
                    {
                        crate::preview_cache::store(&cache_dir, &cache_key, file, cards, crate::preview_cache::now_secs());
                    }
                    send(e);
                    last_error.clear();
                    break;
                }
                Err(e) => {
                    conn = None;
                    last_error = format!("The preview worker stopped: {e}");
                }
            }
        }
        if !last_error.is_empty() {
            send(Event::Failed { file: r.file.clone(), error: last_error });
        }
    }
}

fn connect_or_start(
    send: &dyn Fn(Event),
    kt: &Kotlin,
    rt: &ProjectRuntime,
    base: &mut Option<crate::preview_base::Base>,
) -> Result<(UnixStream, BufReader<UnixStream>), String> {
    // one socket per worker source and Kotlin version: after an update, or for a project on another
    // Kotlin, a new worker starts; an old one exits when idle
    let plugins = format!("{:?}{:?}{:?}", kt.compose_plugin, kt.parcelize_plugin, kt.serialization_plugin);
    let sock = dir().join(format!("w-{:08x}-{}-{:08x}.sock", (fnv(WORKER_SOURCE) ^ fnv(&plugins)) as u32, kt.version, rt.key()));
    if let Ok(s) = UnixStream::connect(&sock) {
        let rd = BufReader::new(s.try_clone().map_err(|e| e.to_string())?);
        return Ok((s, rd));
    }
    // the renderer's files, checked at every worker start: one that went away is found again or
    // downloaded; never a start with a partial set
    if base.is_none() && std::env::var_os("DROID_LITE_PREVIEW_WORKER").is_none() {
        let status = |t: String, n| send(Event::Status(t, n));
        *base = Some(crate::preview_base::ensure(&base_dir(), &dir().join("m2"), &status)?);
    }
    send(Event::Status("Starting the preview worker".into(), None));
    stop_other_workers(&sock);
    let mut child = start_worker(&sock, kt, rt, base.as_ref())?;
    let _ = std::fs::write(dir().join("worker.pid"), format!("{} {}", child.id(), sock.display()));
    let until = Instant::now() + Duration::from_secs(60);
    while Instant::now() < until {
        if let Ok(s) = UnixStream::connect(&sock) {
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
    Err(format!(
        "The preview worker did not start in 60 s. Its log: {}",
        dir().join("worker.log").display()
    ))
}

/// Only one worker at a time: before a new one starts, any other preview worker of this preview folder
/// (an older build's, another project's or Kotlin version's) is asked to stop (SIGTERM); a worker
/// holds about a gigabyte. Found by its command line (worker source and socket), so older builds
/// that wrote no pidfile are found too.
fn stop_other_workers(keep: &Path) {
    let Ok(out) = Command::new("ps").args(["-axo", "pid=,command="]).output() else { return };
    let src = base_dir().join("PreviewWorker.java");
    let (src, keep, me) = (src.to_string_lossy().into_owned(), keep.to_string_lossy().into_owned(), std::process::id());
    let mut stopped = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim_start();
        let Some((pid, cmd)) = line.split_once(' ') else { continue };
        let Ok(pid) = pid.parse::<i32>() else { continue };
        if pid as u32 != me && cmd.contains(&src) && !cmd.contains(&keep) {
            unsafe { libc::kill(pid, libc::SIGTERM) };
            stopped.push(pid);
        }
    }
    // give them a moment to go before the new one starts (memory)
    let until = Instant::now() + Duration::from_secs(5);
    while !stopped.is_empty() && Instant::now() < until {
        stopped.retain(|p| unsafe { libc::kill(*p, 0) } == 0);
        std::thread::sleep(Duration::from_millis(100));
    }
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
) -> Result<Event, String> {
    let line = serde_json::json!({
        "id": id, "file": r.file, "class": r.class, "cp": cp,
        "previews": specs.iter().map(crate::preview_spec::Spec::json).collect::<Vec<_>>(),
        "sources": own.map_or(&[][..], |o| &o.sources[..]),
        "rjava": own.map_or(&[][..], |o| &o.rjava[..]),
        "key": own.map_or("", |o| o.key.as_str()),
    });
    writeln!(w, "{line}").map_err(|e| e.to_string())?;
    loop {
        let mut reply = String::new();
        if rd.read_line(&mut reply).map_err(|e| e.to_string())? == 0 {
            return Err("no answer".into());
        }
        let v: serde_json::Value = serde_json::from_str(&reply).map_err(|e| e.to_string())?;
        if v["id"].as_u64() != Some(id) || v["skipped"].as_bool() == Some(true) {
            continue; // an older answer
        }
        if let Some(step) = v["status"].as_str() {
            // the worker's real step, while it works
            let n = (v["done"].as_u64().unwrap_or(0), v["total"].as_u64().unwrap_or(0));
            send(Event::Status(step.to_string(), (n.1 > 0).then_some(n)));
            continue;
        }
        if v["ok"].as_bool() != Some(true) {
            let error = short_error(v["error"].as_str().unwrap_or("The preview failed."), &r.file);
            return Ok(Event::Failed { file: r.file.clone(), error });
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
        return Ok(Event::Done { file: r.file.clone(), cards });
    }
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
}
