//! Logcat: devices from `adb devices -l`, a live `adb logcat -v threadtime` stream, the pid → process
//! names from `ps`, and Android Studio style filters (`package:mine level:warn tag:Foo -tag:Bar text`).
//!
//! Read-only on the device: nothing here clears (`logcat -c`) or resizes (`-G`) the phone's buffer,
//! because other tools may read the same buffer. "Clear" in the UI clears only Droid Lite's view.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

/// One parsed logcat line.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub time: String,
    pub pid: u32,
    pub tid: u32,
    pub level: char,
    pub tag: String,
    pub msg: String,
    /// The process name when the line arrived (from `ps`), e.g. `com.demo.app`.
    pub process: Option<Arc<str>>,
}

/// Parses `-v threadtime`: `11-12 12:00:00.123  1234  1250 D Tag     : message`.
pub fn parse(line: &str) -> Option<Entry> {
    let mut it = line.split_whitespace();
    let (date, time) = (it.next()?, it.next()?);
    if !date.contains('-') || !time.contains(':') {
        return None;
    }
    let pid = it.next()?.parse().ok()?;
    let tid = it.next()?.parse().ok()?;
    let level = it.next()?;
    let level = level.chars().next().filter(|_| level.len() == 1)?;
    // the tag runs to the first ": " after the level; tags may contain spaces
    let after_level = line.find(&format!(" {level} "))? + 3;
    let rest = &line[after_level..];
    let (tag, msg) = rest
        .split_once(": ")
        .unwrap_or((rest.trim_end_matches(':'), ""));
    Some(Entry {
        time: format!("{date} {time}"),
        pid,
        tid,
        level,
        tag: tag.trim().to_string(),
        msg: msg.to_string(),
        process: None,
    })
}

/// V < D < I < W < E < A (F is the old name for A).
pub fn rank(level: char) -> u8 {
    match level {
        'V' => 0,
        'D' => 1,
        'I' => 2,
        'W' => 3,
        'E' => 4,
        'A' | 'F' => 5,
        _ => 0,
    }
}

/// A parsed filter query. Terms are ANDed; several `tag:` or `package:` terms are ORed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filter {
    pub tags: Vec<String>,
    pub not_tags: Vec<String>,
    pub packages: Vec<String>,
    pub min_level: u8,
    pub text: Vec<String>,
}

impl Filter {
    /// `mine`: the project's application ids, used for `package:mine`.
    pub fn parse(query: &str, mine: &[String]) -> Filter {
        let mut f = Filter::default();
        for term in query.split_whitespace() {
            if let Some(t) = term.strip_prefix("-tag:") {
                f.not_tags.push(t.to_string());
            } else if let Some(t) = term.strip_prefix("tag:") {
                f.tags.push(t.to_string());
            } else if let Some(p) = term.strip_prefix("package:") {
                if p == "mine" {
                    f.packages.extend(mine.iter().cloned());
                } else {
                    f.packages.push(p.to_string());
                }
            } else if let Some(l) = term.strip_prefix("level:") {
                f.min_level = l.chars().next().map_or(0, |c| rank(c.to_ascii_uppercase()));
            } else {
                f.text.push(term.to_lowercase());
            }
        }
        f
    }

    pub fn matches(&self, e: &Entry) -> bool {
        if rank(e.level) < self.min_level {
            return false;
        }
        if !self.tags.is_empty() && !self.tags.iter().any(|t| e.tag.contains(t.as_str())) {
            return false;
        }
        if self.not_tags.iter().any(|t| e.tag.contains(t.as_str())) {
            return false;
        }
        // a process name is the app id, maybe with a suffix (`.debug`) or `:service`
        if !self.packages.is_empty()
            && !e
                .process
                .as_deref()
                .is_some_and(|p| self.packages.iter().any(|pk| p.starts_with(pk.as_str())))
        {
            return false;
        }
        self.text.iter().all(|t| {
            e.msg.to_lowercase().contains(t.as_str()) || e.tag.to_lowercase().contains(t.as_str())
        })
    }
}

/// `adb`: `$DROID_LITE_ADB`, then `$ANDROID_HOME`, the PATH, then the default Mac SDK folder.
pub fn adb() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("DROID_LITE_ADB") {
        return Some(PathBuf::from(p)).filter(|p| p.is_file());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let sdk = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
        .iter()
        .filter_map(|v| std::env::var_os(v).map(PathBuf::from))
        .chain(home.map(|h| h.join("Library/Android/sdk")))
        .map(|s| s.join("platform-tools/adb"));
    let path = std::env::var("PATH").unwrap_or_default();
    let on_path = path.split(':').map(|d| Path::new(d).join("adb"));
    sdk.chain(on_path)
        .chain(["/opt/homebrew/bin/adb".into()])
        .find(|p| p.is_file())
}

/// A device from `adb devices -l`.
#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub serial: String,
    /// "Pixel 8 (serial)", with the state when it is not ready ("unauthorized", "offline").
    pub label: String,
    pub online: bool,
}

/// Connected devices from `adb devices -l`.
pub fn devices(adb: &Path) -> Vec<Device> {
    let Ok(out) = Command::new(adb).args(["devices", "-l"]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let (serial, state) = (it.next()?, it.next()?);
            let model = it
                .find_map(|w| w.strip_prefix("model:"))
                .map_or_else(|| serial.to_string(), |m| m.replace('_', " "));
            let online = state == "device";
            let label = if online {
                format!("{model} ({serial})")
            } else {
                format!("{model} ({serial}, {state})")
            };
            Some(Device {
                serial: serial.to_string(),
                label,
                online,
            })
        })
        .collect()
}

/// pid → process name, from `ps -A -o PID,NAME` (falls back to plain `ps` on old Android).
pub fn processes(adb: &Path, serial: &str) -> HashMap<u32, Arc<str>> {
    let run = |args: &[&str]| {
        Command::new(adb)
            .args(["-s", serial, "shell"])
            .args(args)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let text = run(&["ps", "-A", "-o", "PID,NAME"])
        .filter(|t| t.lines().count() > 2)
        .or_else(|| run(&["ps"]))
        .unwrap_or_default();
    let mut header = text.lines().next().unwrap_or("").split_whitespace();
    let pid_col = header.position(|h| h == "PID").unwrap_or(0);
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let cols: Vec<&str> = l.split_whitespace().collect();
            let pid = cols.get(pid_col)?.parse().ok()?;
            Some((pid, Arc::from(*cols.last()?)))
        })
        .collect()
}

/// A running `adb logcat`; dropping it stops the process.
pub struct Stream {
    child: Child,
    pub rx: Receiver<Entry>,
}

impl Stream {
    /// Streams from `serial`: the lines since `since` (a logcat time, when resuming), else the
    /// last `history` lines, then new ones.
    pub fn start(
        adb: &Path,
        serial: &str,
        history: usize,
        since: Option<String>,
        ctx: egui::Context,
    ) -> std::io::Result<Stream> {
        let mut child = Command::new(adb)
            .args(["-s", serial, "logcat", "-v", "threadtime", "-T"])
            .arg(since.unwrap_or_else(|| history.to_string()))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let out = child.stdout.take().expect("piped stdout");
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(out);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                // logcat lines are not always UTF-8; never stop the stream on one bad line
                match r.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let line = String::from_utf8_lossy(&buf);
                if let Some(e) = parse(line.trim_end())
                    && tx.send(e).is_err()
                {
                    break;
                }
                ctx.request_repaint();
            }
        });
        Ok(Stream { child, rx })
    }

    /// Whether `adb logcat` is still running (it ends when the device goes away).
    pub fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `at com.demo.ui.UserCardKt.UserCard(UserCard.kt:12)` → ("com/demo/ui", "UserCard.kt", 12).
pub fn stack_frame(msg: &str) -> Option<(String, String, usize)> {
    let at = msg.trim_start().strip_prefix("at ")?;
    let (method, loc) = at.split_once('(')?;
    let (file, line) = loc.trim_end_matches(')').split_once(':')?;
    let line = line.parse().ok()?;
    // drop the method and the class (with any `$Inner`): what is left is the package
    let mut parts: Vec<&str> = method.split('.').collect();
    parts.truncate(parts.len().saturating_sub(2));
    Some((parts.join("/"), file.to_string(), line))
}

/// The project's application ids (`applicationId = "…"` in any module's build file).
pub fn application_ids(files: &[PathBuf]) -> Vec<String> {
    let mut ids: Vec<String> = files
        .iter()
        .filter(|f| {
            f.file_name()
                .is_some_and(|n| n == "build.gradle.kts" || n == "build.gradle")
        })
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .flat_map(|text| {
            text.lines()
                .filter_map(|l| {
                    let rest = l.trim().strip_prefix("applicationId")?;
                    let q = rest.trim_start_matches([' ', '=']).trim();
                    let q = q.strip_prefix('"').or_else(|| q.strip_prefix('\''))?;
                    let end = q.find(['"', '\''])?;
                    Some(q[..end].to_string())
                })
                .collect::<Vec<_>>()
        })
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(level: char, tag: &str, msg: &str, process: Option<&str>) -> Entry {
        Entry {
            time: String::new(),
            pid: 1,
            tid: 1,
            level,
            tag: tag.into(),
            msg: msg.into(),
            process: process.map(Arc::from),
        }
    }

    #[test]
    fn parses_threadtime_lines() {
        let x = parse("11-12 12:00:00.123  1234  1250 D OkHttp  : --> GET https://x").unwrap();
        assert_eq!((x.pid, x.tid, x.level), (1234, 1250, 'D'));
        assert_eq!(x.tag, "OkHttp");
        assert_eq!(x.msg, "--> GET https://x");
        assert_eq!(x.time, "11-12 12:00:00.123");
        let y = parse("11-12 12:00:00.123  1 2 E Some Tag: boom: twice").unwrap();
        assert_eq!(
            (y.tag.as_str(), y.msg.as_str()),
            ("Some Tag", "boom: twice")
        );
        assert!(parse("--------- beginning of main").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn filters_like_android_studio() {
        let mine = vec!["com.demo.app".to_string()];
        let app_w = e('W', "Net", "slow call", Some("com.demo.app"));
        let app_d = e('D', "Net", "fast call", Some("com.demo.app:sync"));
        let sys_e = e('E', "ActivityManager", "ANR", Some("system_server"));
        let f = Filter::parse("package:mine", &mine);
        assert!(f.matches(&app_w) && f.matches(&app_d) && !f.matches(&sys_e));
        let f = Filter::parse("level:warn", &mine);
        assert!(f.matches(&app_w) && !f.matches(&app_d) && f.matches(&sys_e));
        let f = Filter::parse("-tag:Activity CALL", &mine);
        assert!(f.matches(&app_w) && f.matches(&app_d) && !f.matches(&sys_e));
        let f = Filter::parse("tag:Net slow", &mine);
        assert!(f.matches(&app_w) && !f.matches(&app_d));
        let f = Filter::parse("package:com.other", &mine);
        assert!(!f.matches(&app_w));
        assert!(!Filter::parse("package:mine", &mine).matches(&e('I', "x", "y", None)));
    }

    #[test]
    fn finds_stack_frames() {
        assert_eq!(
            stack_frame("\tat com.demo.ui.UserCardKt$Inner.UserCard(UserCard.kt:12)"),
            Some(("com/demo/ui".into(), "UserCard.kt".into(), 12))
        );
        assert_eq!(
            stack_frame("at android.os.Looper.loop(Native Method)"),
            None
        );
        assert_eq!(stack_frame("nothing here"), None);
    }
}
