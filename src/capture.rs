//! Device screenshots (`screencap`) and screen recordings (`screenrecord`) with adb.
//! A recording is stopped by its own pid, and only the one file it wrote is deleted from the device.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

/// Android stops `screenrecord` after 3 minutes.
pub const MAX_RECORDING: Duration = Duration::from_secs(180);

/// How long one adb call may take before it is stopped (a device that hangs, a stuck adb server);
/// copying a finished recording may take longer (see [`pull_limit`]).
pub const ADB_TIMEOUT: Duration = Duration::from_secs(20);
/// The longest copy of a recording: 3 minutes of video (up to about 450 MB) over a slow Wi-Fi adb.
pub const MAX_PULL: Duration = Duration::from_secs(15 * 60);

/// How long copying a recording of `size` bytes may take: 30 s plus 1 MB/s (Wi-Fi adb can be that
/// slow), at most [`MAX_PULL`]; [`MAX_PULL`] when the size is not known.
pub fn pull_limit(size: Option<u64>) -> Duration {
    size.map_or(MAX_PULL, |b| {
        (Duration::from_secs(30) + Duration::from_secs(b / 1_000_000)).min(MAX_PULL)
    })
}

fn read_all(r: Option<impl std::io::Read + Send + 'static>) -> Receiver<Vec<u8>> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(mut r) = r {
            let _ = r.read_to_end(&mut b);
        }
        let _ = tx.send(b);
    });
    rx
}

/// `cmd.output()`, but the process is killed when it runs longer than `limit`.
pub fn output_within(cmd: &mut Command, limit: Duration) -> Result<std::process::Output, String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let (out, err) = (read_all(child.stdout.take()), read_all(child.stderr.take()));
    let until = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            // a helper it started (the adb server) may hold the pipes open: wait only a moment
            let left = until
                .saturating_duration_since(Instant::now())
                .max(Duration::from_secs(1));
            return Ok(std::process::Output {
                status,
                stdout: out.recv_timeout(left).unwrap_or_default(),
                stderr: err.recv_timeout(left).unwrap_or_default(),
            });
        }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("no answer in {} s", limit.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Where captures go: `DROID_LITE_CAPTURE_DIR`, else `~/Desktop`, else the home folder.
pub fn out_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("DROID_LITE_CAPTURE_DIR") {
        return d.into();
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let desk = home.join("Desktop");
    if desk.is_dir() { desk } else { home }
}

/// Local time as `2026-11-12 at 10.04.05`, like macOS screenshots.
fn stamp() -> String {
    Command::new("date")
        .arg("+%Y-%m-%d at %H.%M.%S")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            secs.to_string()
        })
}

/// `Screenshot emulator-5554 2026-11-12 at 10.04.05.png` (`:` in a Wi-Fi serial becomes `-`).
pub fn file_name(kind: &str, serial: &str, stamp: &str, ext: &str) -> String {
    let serial: String = serial
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("{kind} {serial} {stamp}.{ext}")
}

/// Saves the screen of `serial` as a PNG in `dir`.
pub fn screenshot(adb: &Path, serial: &str, dir: &Path) -> Result<PathBuf, String> {
    let out = output_within(
        Command::new(adb).args(["-s", serial, "exec-out", "screencap", "-p"]),
        ADB_TIMEOUT,
    )
    .map_err(|e| format!("adb: {e}"))?;
    if !out.stdout.starts_with(b"\x89PNG") {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            "the device sent no image".into()
        } else {
            err
        });
    }
    let path = dir.join(file_name("Screenshot", serial, &stamp(), "png"));
    std::fs::write(&path, &out.stdout).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// A `screenrecord` running on a device.
pub struct Recording {
    adb: PathBuf,
    serial: String,
    remote: String,
    child: Child,
    /// The device-side pid, sent as the first output line.
    pid: Receiver<String>,
    pub started: Instant,
    /// Set by `finish`, so `Drop` leaves the device alone.
    done: bool,
}

impl Recording {
    pub fn start(adb: &Path, serial: &str) -> Result<Self, String> {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let remote = format!("/sdcard/droid-lite-{ms}.mp4");
        // `exec` keeps the shell's pid, so `$$` is the pid of screenrecord itself
        let cmd = format!(
            "echo $$; exec screenrecord --time-limit {} {remote}",
            MAX_RECORDING.as_secs()
        );
        let mut child = Command::new(adb)
            .args(["-s", serial, "shell", &cmd])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("adb: {e}"))?;
        let stdout = child.stdout.take().ok_or("adb: no output")?;
        let (tx, pid) = channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            if BufReader::new(stdout).read_line(&mut line).is_ok() {
                let _ = tx.send(line.trim().to_string());
            }
        });
        Ok(Self {
            adb: adb.into(),
            serial: serial.into(),
            remote,
            child,
            pid,
            started: Instant::now(),
            done: false,
        })
    }

    fn adb(&self, args: &[&str]) -> Result<std::process::Output, String> {
        self.adb_within(args, ADB_TIMEOUT)
    }

    fn adb_within(&self, args: &[&str], limit: Duration) -> Result<std::process::Output, String> {
        output_within(
            Command::new(&self.adb)
                .args(["-s", &self.serial])
                .args(args),
            limit,
        )
        .map_err(|e| format!("adb: {e}"))
    }

    /// The size in bytes of the recording on the device, when it can be read.
    fn remote_size(&self) -> Option<u64> {
        let out = self
            .adb(&["shell", "stat", "-c", "%s", &self.remote])
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }

    /// Stops screenrecord with SIGINT (so it finishes the mp4) by its own pid, never by name.
    fn interrupt(&self, wait: Duration) -> bool {
        let pid = self.pid.recv_timeout(wait).unwrap_or_default();
        let ok = !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit());
        ok && self.adb(&["shell", "kill", "-INT", &pid]).is_ok()
    }

    pub fn serial(&self) -> &str {
        &self.serial
    }

    /// True when screenrecord ended by itself (time limit, device gone).
    pub fn ended(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }

    /// Stops the recording, copies the video into `dir` and deletes it from the device, only once
    /// the whole file arrived; when the copy fails the video stays on the device and the error names
    /// it. Blocks for seconds, minutes for a long recording over Wi-Fi: call it off the UI thread.
    /// At most about 16 minutes: 2 + 20 (stop) + 10 (finish) + 20 (size) + [`MAX_PULL`] + 20 (delete)
    /// seconds, plus up to 1 s per adb call for its output.
    pub fn finish(mut self, dir: &Path) -> Result<PathBuf, String> {
        if matches!(self.child.try_wait(), Ok(None)) {
            if !self.interrupt(Duration::from_secs(2)) {
                let _ = self.child.kill();
            }
            let until = Instant::now() + Duration::from_secs(10);
            while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        self.done = true;
        let local = dir.join(file_name("Recording", &self.serial, &stamp(), "mp4"));
        let local_s = local.to_string_lossy().into_owned();
        let size = self.remote_size();
        let pulled = self.adb_within(&["pull", &self.remote, &local_s], pull_limit(size));
        let got = std::fs::metadata(&local)
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.len());
        let copied = pulled.as_ref().is_ok_and(|o| o.status.success()) && got.is_some();
        if copied && size.is_none_or(|s| got == Some(s)) {
            // delete only the file this recording wrote, and only once all of it is here (a size
            // the device would not say: keep it there)
            if size.is_some() {
                let _ = self.adb(&["shell", "rm", "-f", &self.remote]);
            }
            return Ok(local);
        }
        let _ = std::fs::remove_file(&local); // a part of a video
        let why = match pulled {
            Err(e) => e,
            Ok(out) => {
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                match (size, got) {
                    (None, _) if err.is_empty() => "the device kept no video".into(),
                    (Some(s), Some(g)) if err.is_empty() && g != s => {
                        format!("only {g} of {s} bytes arrived")
                    }
                    _ if err.is_empty() => "the copy failed".into(),
                    _ => err,
                }
            }
        };
        Err(match size {
            Some(_) => format!("{why}. The video is still on the device: {}", self.remote),
            None => why,
        })
    }
}

/// Droid Lite quit while recording: stop screenrecord and delete its file from the device.
impl Drop for Recording {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        self.interrupt(Duration::ZERO);
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = self.adb(&["shell", "rm", "-f", &self.remote]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_that_hangs_is_stopped_at_its_limit() {
        let t = Instant::now();
        let r = output_within(Command::new("sleep").arg("10"), Duration::from_millis(300));
        assert!(r.is_err(), "{r:?}");
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        let ok = output_within(
            Command::new("sh").args(["-c", "echo out; echo err >&2"]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(
            (&ok.stdout[..], &ok.stderr[..]),
            (&b"out\n"[..], &b"err\n"[..])
        );
    }

    #[test]
    fn a_long_recording_gets_time_to_copy_in_proportion_to_its_size() {
        assert_eq!(pull_limit(Some(1_000)), Duration::from_secs(30));
        // 3 minutes at 20 Mbit/s: about 450 MB, 8 minutes at 1 MB/s
        assert_eq!(pull_limit(Some(450_000_000)), Duration::from_secs(480));
        assert_eq!(pull_limit(Some(10_000_000_000)), MAX_PULL);
        assert_eq!(pull_limit(None), MAX_PULL);
    }

    /// A fake adb: screenrecord ends by itself, `stat` says 1000 bytes, `pull` does what the file
    /// `pull` next to it says (`ok`, `short` or `fail`). Every call goes to `adb.log`.
    fn fake_adb(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let adb = dir.join("adb");
        let d = dir.display();
        let script = format!(
            "#!/bin/sh\necho \"$@\" >> {d}/adb.log\ncase \"$*\" in\n\
             *screenrecord*) echo 4242; sleep 0.2;;\n\
             *stat*) echo 1000;;\n\
             *pull*) case $(cat {d}/pull) in\n\
               ok) head -c 1000 /dev/zero > \"$5\";;\n\
               short) head -c 600 /dev/zero > \"$5\";;\n\
               *) echo 'adb: error: failed to copy' >&2; exit 1;;\n\
             esac;;\nesac\n"
        );
        std::fs::write(&adb, script).unwrap();
        std::fs::set_permissions(&adb, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        adb
    }

    #[test]
    fn the_video_leaves_the_device_only_after_all_of_it_was_copied() {
        let dir = std::env::temp_dir().join(format!("dl-capture-pull-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let adb = fake_adb(&dir);
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let log = || std::fs::read_to_string(dir.join("adb.log")).unwrap_or_default();
        for mode in ["fail", "short"] {
            std::fs::write(dir.join("pull"), mode).unwrap();
            let _ = std::fs::remove_file(dir.join("adb.log"));
            let r = Recording::start(&adb, "emu").unwrap().finish(&out);
            let e = r.expect_err(mode);
            assert!(
                e.contains("still on the device: /sdcard/droid-lite-"),
                "{mode}: {e}"
            );
            assert!(!log().contains("rm -f"), "{mode}: {}", log());
            assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0, "no part video");
        }
        std::fs::write(dir.join("pull"), "ok").unwrap();
        let _ = std::fs::remove_file(dir.join("adb.log"));
        let video = Recording::start(&adb, "emu").unwrap().finish(&out).unwrap();
        assert_eq!(std::fs::metadata(&video).unwrap().len(), 1000);
        let calls = log();
        let (pull, rm) = (calls.find(" pull ").unwrap(), calls.find("rm -f").unwrap());
        assert!(pull < rm, "deleted after the copy: {calls}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn file_names_are_safe_for_wifi_serials() {
        assert_eq!(
            file_name(
                "Screenshot",
                "192.168.1.5:5555",
                "2026-11-12 at 10.04.05",
                "png"
            ),
            "Screenshot 192.168.1.5-5555 2026-11-12 at 10.04.05.png"
        );
        assert_eq!(
            file_name("Recording", "adb-R5C/x._tcp", "s", "mp4"),
            "Recording adb-R5C-x.-tcp s.mp4"
        );
    }
}
