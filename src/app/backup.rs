//! Crash backup: edits that are not saved go to the `unsaved/` folder every 30 s (only when they
//! changed), on SIGTERM and SIGINT (a signal thread) and from a panic hook, and when a quit cannot
//! keep them in local history. The next start puts them back in their tabs.
//!
//! Each backup names the process that wrote it (pid and start time) and the hash of the file on
//! disk when it was written. Recovery takes only backups whose process is gone (another running
//! window keeps its own), and puts one back in its tab only while the file on disk is still the
//! one it was written against; a file changed on disk since gets the backup in local history
//! and a diff instead, never written over the newer file.

use super::{App, hash};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How often unsaved edits that changed are written.
const EVERY: Duration = Duration::from_secs(30);
/// How often the snapshot the signal thread and the panic hook write follows the tabs.
const SNAP_EVERY: Duration = Duration::from_secs(1);

/// The unsaved buffers (path, text), at most [`SNAP_EVERY`] old.
static SNAPSHOT: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());
/// Files [`write_snapshot`] wrote a backup of: the window tracks them like its own (a panic on
/// another thread does not end the window, so it must remove them once saved).
static SNAP_WRITTEN: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// What the window last copied and wrote.
#[derive(Default)]
pub(super) struct Backup {
    /// (path, text version) of the unsaved tabs in [`SNAPSHOT`].
    pub snap: Vec<(PathBuf, u64)>,
    pub snap_at: Option<Instant>,
    /// The same as last written to the folder.
    pub written: Vec<(PathBuf, u64)>,
    pub written_at: Option<Instant>,
    /// Files whose backup this window wrote or recovered: removed once their tab has no edits.
    pub ours: HashSet<PathBuf>,
}

/// `$DROID_LITE_UNSAVED_DIR`, else `~/Library/Application Support/droid-lite/unsaved`. Tests
/// never touch the user's folder: without the variable they get none.
pub(super) fn unsaved_dir() -> Option<PathBuf> {
    match std::env::var_os("DROID_LITE_UNSAVED_DIR") {
        Some(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ if cfg!(test) => None,
        _ => std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library/Application Support/droid-lite/unsaved")),
    }
}

/// This process's backup of `path` (each window has its own).
fn backup_file(dir: &Path, path: &Path) -> PathBuf {
    backup_file_of(dir, path, std::process::id())
}

fn backup_file_of(dir: &Path, path: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{:016x}-{pid}.json", hash(&path.to_string_lossy())))
}

/// FNV-1a of `bytes`: the same in every build, unlike std's hasher.
fn disk_hash(bytes: &[u8]) -> String {
    let h = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{h:016x}")
}

/// When process `pid` started (µs since 1970), if the system says.
#[cfg(target_os = "macos")]
pub(super) fn started(pid: u32) -> Option<u64> {
    // SAFETY: proc_pidinfo fills at most `size` bytes of `info`
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    (n == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn started(_pid: u32) -> Option<u64> {
    None
}

/// This process: pid and start time (0 when unknown).
fn me() -> (u32, u64) {
    static START: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let pid = std::process::id();
    (pid, *START.get_or_init(|| started(pid).unwrap_or(0)))
}

/// Whether the process that wrote a backup still runs and is another one (another window).
/// A pid that now belongs to a process started at another time is gone.
fn owner_alive(pid: u32, start: u64) -> bool {
    if pid == 0 || pid == me().0 {
        return false;
    }
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only checks that the process exists
        let exists = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        exists && (start == 0 || started(pid).is_none_or(|s| s == start))
    }
    #[cfg(not(unix))]
    false
}

/// Writes the unsaved `text` of `path` to `dir` (a temp file, then a rename), with this process
/// and the hash of the file on disk now. The folder (0700) and the files (0600) are this user's
/// only, as local history: they hold unsaved source. Each write has its own new temp file, so the
/// 30 s backup and the signal thread never write into one file at once.
pub(super) fn write_one(dir: &Path, path: &Path, text: &str) -> Result<PathBuf, String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    super::comfort::private_dir(dir).map_err(|e| e.to_string())?;
    let file = backup_file(dir, path);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = file.with_extension(format!("tmp{}-{n}", std::process::id()));
    let (pid, start) = me();
    let disk = std::fs::read(path).ok().map(|b| disk_hash(&b));
    let body = json!({ "path": path.to_string_lossy(), "text": text, "disk": disk, "pid": pid, "start": start })
        .to_string();
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(body.as_bytes()))
        .and_then(|()| std::fs::rename(&tmp, &file))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            e.to_string()
        })?;
    Ok(file)
}

/// Writes every buffer of the snapshot (signal thread, panic hook); `wait`: wait for the lock
/// (a panic hook must not: the panicking thread may hold it). How many were written.
pub(super) fn write_snapshot(wait: bool) -> usize {
    let Some(dir) = unsaved_dir() else {
        return 0;
    };
    let snap = if wait {
        SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner())
    } else {
        match SNAPSHOT.try_lock() {
            Ok(s) => s,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return 0,
        }
    };
    let mut n = 0;
    for (path, text) in snap.iter() {
        match write_one(&dir, path, text) {
            Ok(_) => {
                n += 1;
                if let Ok(mut w) = SNAP_WRITTEN.try_lock() {
                    w.push(path.clone());
                }
            }
            Err(e) => eprintln!(
                "droid-lite: could not keep the unsaved edits of {}: {e}",
                path.display()
            ),
        }
    }
    if n > 0 {
        eprintln!(
            "droid-lite: kept the unsaved edits of {n} files in {}",
            dir.display()
        );
    }
    n
}

/// A panic writes the unsaved edits first, then reports as before.
pub fn install_panic_hook() {
    let before = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write_snapshot(false);
        before(info);
    }));
}

/// SIGTERM and SIGINT write the unsaved edits (on a thread: a signal handler may only write to a
/// pipe), then end the app as the signal would have.
#[cfg(unix)]
pub fn install_signal_handler() {
    use std::sync::atomic::{AtomicI32, Ordering};
    static PIPE: AtomicI32 = AtomicI32::new(-1);
    extern "C" fn on_signal(sig: libc::c_int) {
        let b = sig as u8;
        // SAFETY: write(2) is async-signal-safe; the pipe stays open for the whole run
        unsafe { libc::write(PIPE.load(Ordering::Relaxed), (&raw const b).cast(), 1) };
    }
    let Some(fds) = cloexec_pipe() else {
        return;
    };
    PIPE.store(fds[1], Ordering::Relaxed);
    std::thread::spawn(move || {
        loop {
            let mut b = 0u8;
            // SAFETY: reads one byte into `b`
            let n = unsafe { libc::read(fds[0], (&raw mut b).cast(), 1) };
            if n == 1 {
                write_snapshot(true);
                // SAFETY: the default action of the same signal ends the process as it would have
                unsafe {
                    libc::signal(b as libc::c_int, libc::SIG_DFL);
                    libc::raise(b as libc::c_int);
                }
            } else if n < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            } else {
                return;
            }
        }
    });
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: the handler only writes to the pipe
    unsafe {
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
}

/// A pipe whose ends a started process (Gradle, adb, git) does not inherit.
#[cfg(unix)]
fn cloexec_pipe() -> Option<[libc::c_int; 2]> {
    let mut fds = [0; 2];
    // SAFETY: pipe(2) fills the two descriptors; fcntl(2) only sets their close-on-exec flag
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return None;
        }
        for fd in fds {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    Some(fds)
}

impl App {
    /// Backups the signal thread or a panic hook wrote count as this window's own. Whether
    /// there were any.
    fn adopt_snapshot_backups(&mut self) -> bool {
        let written = std::mem::take(&mut *SNAP_WRITTEN.lock().unwrap_or_else(|e| e.into_inner()));
        let any = !written.is_empty();
        self.backup.ours.extend(written);
        any
    }

    /// (path, text version) of the tabs with unsaved edits.
    fn unsaved_sig(&self) -> Vec<(PathBuf, u64)> {
        self.unsaved_tabs()
            .into_iter()
            .map(|i| (self.tabs[i].path.clone(), self.tabs[i].rev))
            .collect()
    }

    /// Every frame: the snapshot follows the unsaved tabs (at most once a second), and the
    /// folder gets them when they changed, at most every 30 s. Tabs without edits cost nothing.
    pub(super) fn backup_tick(&mut self, ctx: &egui::Context) {
        // a panic on another thread wrote backups: they go once their tabs have no edits
        if self.adopt_snapshot_backups() {
            self.write_backups();
        }
        let sig = self.unsaved_sig();
        if sig != self.backup.snap {
            if self
                .backup
                .snap_at
                .is_none_or(|t| t.elapsed() >= SNAP_EVERY)
            {
                let texts = self
                    .unsaved_tabs()
                    .into_iter()
                    .map(|i| (self.tabs[i].path.clone(), self.tabs[i].text.clone()))
                    .collect();
                *SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner()) = texts;
                self.backup.snap = sig.clone();
                self.backup.snap_at = Some(Instant::now());
            } else {
                ctx.request_repaint_after(SNAP_EVERY);
            }
        }
        if sig != self.backup.written {
            match self.backup.written_at.map(|t| t.elapsed()) {
                Some(e) if e < EVERY => ctx.request_repaint_after(EVERY - e),
                _ => self.write_backups(),
            }
        }
    }

    /// Writes the unsaved tabs to the folder now, and removes this window's backups of tabs that
    /// have no edits any more.
    pub(super) fn write_backups(&mut self) {
        self.backup.written = self.unsaved_sig();
        self.backup.written_at = Some(Instant::now());
        self.adopt_snapshot_backups();
        let Some(dir) = unsaved_dir() else {
            return;
        };
        let unsaved = self.unsaved_tabs();
        for &i in &unsaved {
            let t = &self.tabs[i];
            match write_one(&dir, &t.path, &t.text) {
                Ok(_) => {
                    self.backup.ours.insert(t.path.clone());
                }
                Err(e) => self.status = super::comfort::copy_failed(&t.path, &e),
            }
        }
        let keep: HashSet<&PathBuf> = unsaved.iter().map(|&i| &self.tabs[i].path).collect();
        self.backup.ours.retain(|p| {
            keep.contains(p)
                || std::fs::remove_file(backup_file(&dir, p))
                    .is_err_and(|e| e.kind() != std::io::ErrorKind::NotFound)
        });
    }

    /// On quit or a project switch, once the unsaved edits went to local history: the edits of
    /// tabs `failed` (their copy failed) go to the folder, or at least to stderr's log; this
    /// window's other backups are not needed any more.
    pub(super) fn backups_kept(&mut self, failed: &[(usize, String)]) {
        self.adopt_snapshot_backups();
        let dir = unsaved_dir();
        let mut kept = HashSet::new();
        for (i, why) in failed {
            let t = &self.tabs[*i];
            eprintln!("droid-lite: {why}");
            match dir.as_deref().map(|d| write_one(d, &t.path, &t.text)) {
                Some(Ok(f)) => {
                    eprintln!("droid-lite: its unsaved edits are in {}", f.display());
                    kept.insert(t.path.clone());
                }
                Some(Err(e)) => {
                    eprintln!("droid-lite: nor could they be written to the unsaved folder: {e}")
                }
                None => eprintln!("droid-lite: there is no unsaved folder (HOME is not set)"),
            }
        }
        if let Some(dir) = dir {
            for p in self.backup.ours.iter().filter(|p| !kept.contains(*p)) {
                let _ = std::fs::remove_file(backup_file(&dir, p));
            }
        }
        self.backup.ours = kept;
    }

    /// After a project opens: edits a crash or a failed quit left in the folder for its files
    /// are put back in their tabs ("Recovered unsaved edits for …"). Only backups of processes
    /// that are gone count (another window keeps its own). A backup that matches the file on
    /// disk is dropped; one whose file is gone stays in the folder; one whose file changed on
    /// disk since it was written goes to local history with a diff, never into the tab.
    pub(super) fn recover_unsaved(&mut self) {
        let (Some(dir), Some(root)) =
            (unsaved_dir(), self.project.as_ref().map(|p| p.root.clone()))
        else {
            return;
        };
        let mut found: Vec<(PathBuf, PathBuf, String, Option<String>)> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|f| f.extension().is_some_and(|x| x == "json"))
            .filter_map(|f| {
                let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).ok()?).ok()?;
                let path = PathBuf::from(v["path"].as_str()?);
                let text = v["text"].as_str()?.to_string();
                let pid = v["pid"].as_u64().unwrap_or(0) as u32;
                if !path.starts_with(&root) || owner_alive(pid, v["start"].as_u64().unwrap_or(0)) {
                    return None;
                }
                let disk = v["disk"].as_str().map(String::from);
                Some((f, path, text, disk))
            })
            .collect();
        found.sort_by(|a, b| a.1.cmp(&b.1));
        let (mut names, mut changed) = (Vec::new(), Vec::new());
        let first = found.first().map(|f| f.1.clone());
        for (file, path, text, disk) in found {
            let now = std::fs::read(&path);
            if now.as_ref().is_ok_and(|b| *b == text.as_bytes()) {
                let _ = std::fs::remove_file(&file); // saved since
                continue;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !path.is_file() {
                names.push(format!(
                    "{name} (its file is gone: the copy stays in {})",
                    file.display()
                ));
                continue;
            }
            if now.ok().map(|b| disk_hash(&b)) != disk {
                // the file changed on disk after the backup: the newer file stays
                match super::comfort::record_text(&path, text.as_bytes()) {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&file);
                        changed.push(format!("{name} (in Local history)"));
                    }
                    Err(e) => changed.push(format!(
                        "{name} (could not keep it in Local history: {e}; the copy stays in {})",
                        file.display()
                    )),
                }
                self.show_diff_text(
                    format!("Unsaved backup of {name} (file on disk → backup)"),
                    super::comfort::diff_texts(&path, &text),
                );
                continue;
            }
            self.show_file(&path, None);
            let Some(t) = self.tabs.iter_mut().find(|t| t.path == path && !t.readonly) else {
                continue;
            };
            if t.text != text {
                t.text = text;
                t.cursor = t.cursor.min(t.text.len());
                t.mark_edited();
            }
            // this window's own backup replaces the old process's
            if file != backup_file(&dir, &path) && write_one(&dir, &path, &t.text).is_ok() {
                let _ = std::fs::remove_file(&file);
            }
            self.backup.ours.insert(path);
            names.push(name);
        }
        let mut notes = Vec::new();
        if !names.is_empty() {
            notes.push(format!("Recovered unsaved edits for {}", names.join(", ")));
        }
        if !changed.is_empty() {
            notes.push(format!(
                "Changed on disk since their unsaved backup, so not put back: {}",
                changed.join(", ")
            ));
        }
        if !notes.is_empty() {
            // a note that stays a while (later status messages replace the status bar's)
            let text = notes.join(". ");
            self.status = text.clone();
            self.toast_to(text, first.map(|p| (p, 0)), super::Bottom::Problems);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The child half of the signal and panic tests: runs only in the process they start.
    #[test]
    fn crash_backup_child() {
        let (Some(dir), Some(mode)) = (
            std::env::var_os("DL_CRASH_CHILD_DIR"),
            std::env::var_os("DL_CRASH_CHILD_MODE"),
        ) else {
            return;
        };
        let dir = PathBuf::from(dir);
        // SAFETY: this process runs only this test
        unsafe { std::env::set_var("DROID_LITE_UNSAVED_DIR", dir.join("unsaved")) };
        *SNAPSHOT.lock().unwrap() = vec![(PathBuf::from("/p/A.kt"), "typed, not saved".into())];
        if mode == "panic" {
            install_panic_hook();
            panic!("a crash");
        }
        install_signal_handler();
        std::fs::write(dir.join("ready"), "").unwrap();
        std::thread::sleep(Duration::from_secs(20));
    }

    fn run_child(mode: &str) -> (std::process::ExitStatus, Option<String>) {
        let dir =
            std::env::temp_dir().join(format!("droid-lite-crash-{mode}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "app::backup::tests::crash_backup_child",
                "--test-threads=1",
            ])
            .env("DL_CRASH_CHILD_DIR", &dir)
            .env("DL_CRASH_CHILD_MODE", mode)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        if mode == "signal" {
            let ready = dir.join("ready");
            let until = Instant::now() + Duration::from_secs(20);
            while !ready.exists() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
            // SAFETY: the child this test started
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        }
        let status = child.wait().unwrap();
        let file = backup_file_of(&dir.join("unsaved"), Path::new("/p/A.kt"), child.id());
        let text = std::fs::read_to_string(file).ok().and_then(|b| {
            Some(
                serde_json::from_str::<Value>(&b).ok()?["text"]
                    .as_str()?
                    .to_string(),
            )
        });
        let _ = std::fs::remove_dir_all(&dir);
        (status, text)
    }

    #[test]
    fn the_signal_pipe_is_not_inherited_by_started_processes() {
        let fds = cloexec_pipe().unwrap();
        for fd in fds {
            // SAFETY: reads the flags of a descriptor this test owns, then closes it
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            unsafe { libc::close(fd) };
            assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC, "fd {fd}");
        }
    }

    #[test]
    fn a_backup_names_its_process_and_the_disk_file_it_was_written_against() {
        let dir = std::env::temp_dir().join(format!("dl-backup-owner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("A.kt");
        std::fs::write(&src, "on disk").unwrap();
        let f = write_one(&dir.join("unsaved"), &src, "typed").unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(v["disk"].as_str(), Some(disk_hash(b"on disk").as_str()));
        assert_eq!(v["pid"].as_u64(), Some(u64::from(std::process::id())));
        assert_eq!(v["start"].as_u64(), Some(me().1));
        assert!(me().1 > 0, "the start time is known on macOS");
        assert_eq!(
            f,
            backup_file(&dir.join("unsaved"), &src),
            "one file per process"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backups_are_private_and_two_writers_never_mix_one_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("dl-backup-private-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("A.kt");
        let unsaved = dir.join("unsaved");
        std::fs::create_dir_all(&unsaved).unwrap();
        std::fs::set_permissions(&unsaved, std::fs::Permissions::from_mode(0o755)).unwrap();
        let f = write_one(&unsaved, &src, "secret").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&unsaved), 0o700, "an open folder is closed");
        assert_eq!(mode(&f), 0o600);
        // the 30 s backup and the signal thread at once: the file is always one whole backup
        let texts: Vec<String> = (0..4).map(|i| format!("{i}").repeat(200_000)).collect();
        std::thread::scope(|s| {
            for t in &texts {
                let (unsaved, src) = (&unsaved, &src);
                s.spawn(move || {
                    for _ in 0..20 {
                        write_one(unsaved, src, t).unwrap();
                    }
                });
            }
        });
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert!(texts.iter().any(|t| v["text"].as_str() == Some(t)));
        assert_eq!(
            std::fs::read_dir(&unsaved).unwrap().count(),
            1,
            "no temp file is left"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_live_other_process_owns_its_backups() {
        assert!(
            !owner_alive(std::process::id(), me().1),
            "this process's own"
        );
        assert!(!owner_alive(0, 0), "no owner written");
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let start = started(pid).unwrap();
        assert!(owner_alive(pid, start), "another window still runs");
        assert!(
            owner_alive(pid, 0),
            "an unknown start time: the pid decides"
        );
        assert!(
            !owner_alive(pid, start + 1),
            "the pid now belongs to another process"
        );
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!owner_alive(pid, start), "the window is gone");
    }

    #[test]
    fn sigterm_keeps_the_unsaved_edits_and_still_ends_the_app() {
        use std::os::unix::process::ExitStatusExt;
        let (status, text) = run_child("signal");
        assert_eq!(status.signal(), Some(libc::SIGTERM), "{status:?}");
        assert_eq!(text.as_deref(), Some("typed, not saved"));
    }

    #[test]
    fn a_panic_keeps_the_unsaved_edits() {
        let (status, text) = run_child("panic");
        assert!(!status.success());
        assert_eq!(text.as_deref(), Some("typed, not saved"));
    }
}
