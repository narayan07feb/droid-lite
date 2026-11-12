//! Device screenshots (`screencap`) and screen recordings (`screenrecord`) with adb.
//! A recording is stopped by its own pid, and only the one file it wrote is deleted from the device.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

/// Android stops `screenrecord` after 3 minutes.
pub const MAX_RECORDING: Duration = Duration::from_secs(180);

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
    let out = Command::new(adb)
        .args(["-s", serial, "exec-out", "screencap", "-p"])
        .output()
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
        Command::new(&self.adb)
            .args(["-s", &self.serial])
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("adb: {e}"))
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

    /// Stops the recording, copies the video into `dir` and deletes it from the device.
    /// Blocks for a few seconds: call it off the UI thread.
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
        let pulled = self.adb(&["pull", &self.remote, &local_s]);
        // delete only the file this recording wrote
        let _ = self.adb(&["shell", "rm", "-f", &self.remote]);
        let out = pulled?;
        if !out.status.success() || !local.is_file() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(if err.is_empty() {
                "the device kept no video".into()
            } else {
                err
            });
        }
        Ok(local)
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
