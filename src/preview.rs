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
        if conn.is_none() {
            match connect_or_start(&send) {
                Ok(c) => conn = Some(c),
                Err(e) => {
                    send(Event::Failed { file: r.file.clone(), error: e });
                    continue;
                }
            }
        }
        send(Event::Status(format!("Rendering {} preview(s)…", r.previews.len())));
        id += 1;
        let (w, rd) = conn.as_mut().unwrap();
        match ask(w, rd, id, &r) {
            Ok(e) => send(e),
            Err(e) => {
                conn = None; // the worker ended (idle exit or a crash): start it again next time
                send(Event::Failed { file: r.file.clone(), error: format!("The preview worker stopped: {e}") });
            }
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

/// `DROID_LITE_JAVA`, else `$JAVA_HOME/bin/java`, else macOS's java_home for 17, else `java`.
fn java() -> PathBuf {
    if let Some(j) = std::env::var_os("DROID_LITE_JAVA") {
        return j.into();
    }
    if let Some(h) = std::env::var_os("JAVA_HOME") {
        return Path::new(&h).join("bin/java");
    }
    Command::new("/usr/libexec/java_home")
        .args(["-v", "17+"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| Path::new(String::from_utf8_lossy(&o.stdout).trim()).join("bin/java"))
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
            let error = v["error"].as_str().unwrap_or("The preview failed.").to_string();
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

#[cfg(test)]
mod tests {
    use super::*;

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
