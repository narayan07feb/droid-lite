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
}

/// What comes back, by message only.
pub enum Event {
    /// Work in progress or a reason it cannot run ("Starting the preview worker…", "Skipped: …").
    Status(String),
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

/// The base pack: what every preview needs (Compose, layoutlib, the Kotlin compiler, the renderer).
///   runtime/*.jar     the worker's classpath (renderer, compiler, Compose, android.jar last)
///   compile/*.jar     what the open file compiles against (Compose + android.jar)
///   compose-plugin.jar, layoutlib/runtime, layoutlib/resources
pub fn base_dir() -> PathBuf {
    dir().join("base")
}

pub fn base_ready(base: &Path) -> bool {
    ["runtime", "compile", "layoutlib/runtime", "layoutlib/resources", "compose-plugin.jar"]
        .iter()
        .all(|p| base.join(p).exists())
}

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
    while let Ok(mut r) = jobs.recv() {
        while let Ok(newer) = jobs.try_recv() {
            r = newer; // the newest request wins
        }
        let why = room();
        if !why.is_empty() {
            send(Event::Status(why));
            continue;
        }
        // the worker may have ended (idle exit, a crash) or never started: start it and connect by
        // itself, up to 3 tries with a short back-off, then the error goes on the card
        let mut last_error = String::new();
        for attempt in 0..3u32 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(500 << attempt));
            }
            if conn.is_none() {
                match connect_or_start(&send) {
                    Ok(c) => conn = Some(c),
                    Err(e) => {
                        last_error = e;
                        continue;
                    }
                }
            }
            send(Event::Status(format!("Rendering {} preview(s)…", r.previews.len())));
            id += 1;
            let (w, rd) = conn.as_mut().unwrap();
            match ask(w, rd, id, &r) {
                Ok(e) => {
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
) -> Result<(UnixStream, BufReader<UnixStream>), String> {
    let sock = dir().join("w.sock");
    if let Ok(s) = UnixStream::connect(&sock) {
        let rd = BufReader::new(s.try_clone().map_err(|e| e.to_string())?);
        return Ok((s, rd));
    }
    let base = base_dir();
    if std::env::var_os("DROID_LITE_PREVIEW_WORKER").is_none() && !base_ready(&base) {
        return Err("The preview pack is not downloaded yet.".into());
    }
    send(Event::Status("Starting the preview worker…".into()));
    start_worker(&base, &sock)?;
    let until = Instant::now() + Duration::from_secs(60);
    while Instant::now() < until {
        if let Ok(s) = UnixStream::connect(&sock) {
            let rd = BufReader::new(s.try_clone().map_err(|e| e.to_string())?);
            return Ok((s, rd));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(format!(
        "The preview worker did not start in 60 s. Its log: {}",
        dir().join("worker.log").display()
    ))
}

/// Starts the worker detached (its own process group, output to worker.log), so it outlives nothing
/// it should not and a restart of Droid Lite finds it still warm.
fn start_worker(base: &Path, sock: &Path) -> Result<(), String> {
    let d = dir();
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
            let src = base.join("PreviewWorker.java");
            if std::fs::read_to_string(&src).ok().as_deref() != Some(WORKER_SOURCE) {
                std::fs::write(&src, WORKER_SOURCE).map_err(|e| e.to_string())?;
            }
            let jars = |sub: &str| {
                let mut v: Vec<PathBuf> = std::fs::read_dir(base.join(sub))
                    .map(|rd| rd.flatten().map(|e| e.path()).collect())
                    .unwrap_or_default();
                v.sort(); // android.jar is named zz-android.jar so it comes last, after layoutlib
                v.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join(":")
            };
            let mut cmd = Command::new(java());
            cmd.arg("-Xmx1500m")
                .arg("-XX:+UseSerialGC")
                .arg(format!("-Dpaparazzi.layoutlib.runtime.root={}", base.join("layoutlib/runtime").display()))
                .arg(format!("-Dpaparazzi.layoutlib.resources.root={}", base.join("layoutlib/resources").display()))
                .arg(format!("-Ddroidlite.compose.plugin={}", base.join("compose-plugin.jar").display()))
                .arg(format!("-Ddroidlite.compile.cp={}", jars("compile")))
                .arg("-cp")
                .arg(jars("runtime"))
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
    let mut child = cmd.spawn().map_err(|e| format!("Could not start the preview worker: {e}"))?;
    std::thread::spawn(move || child.wait()); // reap it when it ends (idle exit)
    Ok(())
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

fn ask(w: &mut UnixStream, rd: &mut BufReader<UnixStream>, id: u64, r: &Request) -> Result<Event, String> {
    let line = serde_json::json!({
        "id": id, "file": r.file, "sources": [], "class": r.class, "previews": r.previews,
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
                let png = img["png"].as_str().unwrap_or("");
                let pic = image::open(png)
                    .map(|i| {
                        let i = i.to_rgba8();
                        egui::ColorImage::from_rgba_unmultiplied([i.width() as usize, i.height() as usize], i.as_raw())
                    })
                    .map_err(|e| format!("Could not read the picture: {e}"));
                let _ = std::fs::remove_file(png); // the texture holds it now
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
