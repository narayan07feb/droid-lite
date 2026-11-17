//! Runs Gradle tasks in the background, and finds the preview screenshot tool a module uses.

use crate::project::Project;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

pub enum Line {
    Out(String),
    Done(Option<i32>),
}

/// The longest output line kept, in bytes: the rest of a longer one is dropped (memory stays
/// bounded whatever a tool prints).
pub const MAX_LINE: usize = 64 << 10;

/// Calls `f` with each line of `r` (without its `\n` or `\r\n`; bytes that are not UTF-8 become
/// U+FFFD) until the end or until `f` says false. A line longer than [`MAX_LINE`] is cut there and
/// marked; the rest of it is dropped unread into memory.
pub fn lines_capped(r: impl Read, mut f: impl FnMut(String) -> bool) {
    let mut r = BufReader::new(r);
    let mut line: Vec<u8> = Vec::new();
    let mut cut = false;
    let emit = |line: &mut Vec<u8>, cut: bool, f: &mut dyn FnMut(String) -> bool| {
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        let mut s = String::from_utf8_lossy(line).into_owned();
        if cut {
            s.push_str(" … [line cut]");
        }
        line.clear();
        f(s)
    };
    loop {
        let buf = match r.fill_buf() {
            Ok([]) => break,
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let (part, used, end) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (&buf[..i], i + 1, true),
            None => (buf, buf.len(), false),
        };
        if !cut {
            let room = MAX_LINE - line.len();
            line.extend_from_slice(&part[..part.len().min(room)]);
            if part.len() > room {
                cut = true;
                if !emit(&mut line, true, &mut f) {
                    return;
                }
            }
        }
        r.consume(used);
        if end {
            if !cut && !emit(&mut line, false, &mut f) {
                return;
            }
            cut = false;
        }
    }
    if !line.is_empty() && !cut {
        emit(&mut line, false, &mut f);
    }
}

pub struct Run {
    child: Child,
    rx: Receiver<Line>,
    /// It leads its own process group ([`Run::spawn_group`]): signals reach the tools it started.
    group: bool,
    /// `poll` saw it exit and reaped it. Its pid may be another process's by now, so it alone is
    /// never signalled again; its group only while the group still has members (a group id is
    /// not given to a new process while its group lives).
    reaped: bool,
    /// When SIGKILL follows the SIGTERM of [`Run::terminate`], and how long after.
    kill_at: Option<Instant>,
    kill_after: Duration,
    /// Asked to draw again when the SIGKILL is due ([`Run::poll`] sends it).
    ctx: egui::Context,
}

impl Run {
    /// Starts `./gradlew <args> --console=plain` in the project root; output arrives through
    /// `poll`. `serial` makes install tasks target that device (`ANDROID_SERIAL`).
    pub fn start_on(
        project: &Project,
        args: &str,
        serial: Option<&str>,
        ctx: egui::Context,
    ) -> std::io::Result<Run> {
        let gradlew = project.gradlew();
        let mut cmd = if gradlew.exists() {
            Command::new(gradlew)
        } else {
            Command::new("gradle")
        };
        cmd.args(args.split_whitespace())
            .arg("--console=plain")
            .current_dir(&project.root)
            .envs(serial.map(|s| ("ANDROID_SERIAL", s)));
        Run::spawn(cmd, ctx)
    }

    /// Starts any command; its stdout and stderr lines arrive through `poll`.
    pub fn spawn(cmd: Command, ctx: egui::Context) -> std::io::Result<Run> {
        Run::start(cmd, ctx, false)
    }

    /// [`Run::spawn`] in a process group of its own: [`Run::terminate`] and the kill when it is
    /// dropped reach every tool it started too.
    pub fn spawn_group(mut cmd: Command, ctx: egui::Context) -> std::io::Result<Run> {
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        Run::start(cmd, ctx, cfg!(unix))
    }

    fn start(mut cmd: Command, ctx: egui::Context, group: bool) -> std::io::Result<Run> {
        let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let (tx, rx) = channel();
        for pipe in [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let (tx, ctx) = (tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                lines_capped(pipe, |line| {
                    let sent = tx.send(Line::Out(line)).is_ok();
                    ctx.request_repaint();
                    sent
                });
            });
        }
        Ok(Run {
            child,
            rx,
            group,
            reaped: false,
            kill_at: None,
            kill_after: Duration::from_secs(5),
            ctx,
        })
    }

    /// New output lines, then `Done` (once) when the process has exited. A run that
    /// [`Run::terminate`] asked to stop and is still there (it, or in a group of its own any tool
    /// it started) gets SIGKILL once its time is up; until then the next frame is asked for then.
    pub fn poll(&mut self) -> Vec<Line> {
        let mut out: Vec<Line> = self.rx.try_iter().collect();
        if !self.reaped
            && let Ok(Some(status)) = self.child.try_wait()
        {
            self.reaped = true;
            out.extend(self.rx.try_iter());
            out.push(Line::Done(status.code()));
        }
        if let Some(t) = self.kill_at {
            let now = Instant::now();
            if !self.lives() {
                self.kill_at = None;
            } else if now >= t {
                self.kill_at = None;
                self.signal(Signal::Kill);
            } else {
                self.ctx.request_repaint_after(t - now);
            }
        }
        out
    }

    /// `poll` saw it exit (tools it started may still run: [`Run::lives`]).
    pub fn ended(&self) -> bool {
        self.reaped
    }

    /// It, or (in a group of its own) a tool it started, may still run: not reaped yet, or its
    /// group still has members.
    pub fn lives(&self) -> bool {
        !self.reaped || (self.group && group_lives(self.child.id()))
    }

    pub fn stop(&mut self) {
        let _ = self.child.kill();
    }

    /// SIGTERM to its process group (to it alone when it has none), then SIGKILL after 5 s if it
    /// is still there ([`Run::poll`], a frame is asked for then). A group whose leader ended
    /// still gets them while tools it started run. False, and no signal, once nothing of it is
    /// left ([`Run::lives`]).
    pub fn terminate(&mut self) -> bool {
        if !self.lives() {
            return false;
        }
        self.signal(Signal::Term);
        if self.kill_at.is_none() {
            self.kill_at = Some(Instant::now() + self.kill_after);
            self.ctx.request_repaint_after(self.kill_after);
        }
        true
    }

    /// Only while it lives ([`Run::lives`]): until it is reaped its pid (and group id) cannot be
    /// another's; after, its group id cannot be while the group has members.
    fn signal(&mut self, s: Signal) {
        if !self.lives() {
            return;
        }
        #[cfg(unix)]
        {
            let pid = self.child.id() as libc::pid_t;
            let sig = match s {
                Signal::Term => libc::SIGTERM,
                Signal::Kill => libc::SIGKILL,
            };
            unsafe { libc::kill(if self.group { -pid } else { pid }, sig) };
        }
        #[cfg(not(unix))]
        {
            let _ = s;
            let _ = self.child.kill();
        }
    }
}

/// The process group `pgid` still has members (`kill(-pgid, 0)`).
pub fn group_lives(pgid: u32) -> bool {
    #[cfg(unix)]
    return unsafe { libc::kill(-(pgid as libc::pid_t), 0) } == 0;
    #[cfg(not(unix))]
    return {
        let _ = pgid;
        false
    };
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

impl Drop for Run {
    /// A dropped run (new project, app quit) must not leave Gradle running or a zombie behind: it
    /// and, in a group of its own, every tool it started get SIGKILL (also after it was reaped,
    /// while tools it started still run).
    fn drop(&mut self) {
        self.signal(Signal::Kill);
        if !self.reaped {
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alive(pid: &str) -> bool {
        unsafe { libc::kill(pid.trim().parse().unwrap(), 0) == 0 }
    }

    /// Waits up to 5 s for `pid_file` to name a process.
    fn pid_in(pid_file: &std::path::Path) -> String {
        for _ in 0..250 {
            if let Ok(p) = std::fs::read_to_string(pid_file)
                && !p.trim().is_empty()
            {
                return p;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("no pid in {}", pid_file.display());
    }

    fn sh(script: &str, dir: &std::path::Path) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]).current_dir(dir);
        c
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("droid-lite-run-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_dropped_group_run_takes_the_tools_it_started_along() {
        let d = temp("drop");
        let run = Run::spawn_group(
            sh("sleep 30 & echo $! > child.pid; wait", &d),
            egui::Context::default(),
        )
        .unwrap();
        let child = pid_in(&d.join("child.pid"));
        assert!(alive(&child));
        drop(run);
        std::thread::sleep(Duration::from_millis(200));
        assert!(!alive(&child), "the grandchild ended with the run");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn terminate_kills_a_group_that_ignores_sigterm_after_its_time() {
        let d = temp("escalate");
        let mut run = Run::spawn_group(
            sh("trap '' TERM; sleep 30 & echo $! > child.pid; wait", &d),
            egui::Context::default(),
        )
        .unwrap();
        run.kill_after = Duration::from_millis(400);
        let child = pid_in(&d.join("child.pid"));
        assert!(run.terminate());
        std::thread::sleep(Duration::from_millis(200));
        let _ = run.poll();
        assert!(alive(&child), "TERM is ignored");
        let t0 = Instant::now();
        let mut done = false;
        while !done && t0.elapsed() < Duration::from_secs(5) {
            done = run.poll().iter().any(|l| matches!(l, Line::Done(_)));
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(done, "SIGKILL after its time");
        std::thread::sleep(Duration::from_millis(100));
        assert!(!alive(&child));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Waits up to 3 s for `pid` to end.
    fn ends(pid: &str) -> bool {
        let t0 = Instant::now();
        while alive(pid) && t0.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        !alive(pid)
    }

    /// Polls until `Done`.
    fn done(run: &mut Run) {
        let t0 = Instant::now();
        while !run.poll().iter().any(|l| matches!(l, Line::Done(_))) {
            assert!(t0.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_tool_left_running_by_an_ended_run_is_stopped_by_terminate_and_by_drop() {
        let d = temp("left");
        for how in ["terminate", "drop"] {
            let _ = std::fs::remove_file(d.join("child.pid"));
            let mut run = Run::spawn_group(
                sh("sleep 30 >/dev/null 2>&1 & echo $! > child.pid", &d),
                egui::Context::default(),
            )
            .unwrap();
            done(&mut run);
            let child = pid_in(&d.join("child.pid"));
            assert!(
                alive(&child) && run.lives(),
                "{how}: the tool outlives the run"
            );
            assert!(run.poll().is_empty(), "Done comes once");
            if how == "terminate" {
                assert!(run.terminate(), "its group still has a member");
                assert!(ends(&child), "SIGTERM reached it");
                let _ = run.poll();
                assert!(!run.lives() && !run.terminate());
            } else {
                drop(run);
                assert!(ends(&child), "SIGKILL reached it");
            }
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_terminated_run_asks_for_a_frame_when_its_sigkill_is_due() {
        let d = temp("repaint");
        let ctx = egui::Context::default();
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = asked.clone();
        ctx.set_repaint_observer(move |i| seen.lock().unwrap().push(i.delay));
        let mut run =
            Run::spawn_group(sh("trap '' TERM; sleep 30 >/dev/null 2>&1", &d), ctx).unwrap();
        run.kill_after = Duration::from_secs(4);
        std::thread::sleep(Duration::from_millis(100));
        asked.lock().unwrap().clear();
        assert!(run.terminate());
        let _ = run.poll();
        let delays = asked.lock().unwrap().clone();
        assert!(
            delays.len() >= 2 && delays.iter().all(|d| *d > Duration::from_secs(3)),
            "{delays:?}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_huge_line_is_cut_and_the_next_one_still_comes() {
        let mut text = vec![b'x'; 200_000];
        text.extend_from_slice(b"\r\nafter\n\xff ok\r\nlast");
        let mut got = Vec::new();
        lines_capped(&text[..], |l| {
            got.push(l);
            true
        });
        assert_eq!(
            got.len(),
            4,
            "{:?}",
            got.iter().map(String::len).collect::<Vec<_>>()
        );
        assert_eq!(got[0].len(), MAX_LINE + " … [line cut]".len());
        assert!(got[0].ends_with("x … [line cut]"));
        assert_eq!(got[1..], ["after", "\u{fffd} ok", "last"]);
        // a reader that says stop gets no more lines
        let mut n = 0;
        lines_capped(&b"a\nb\nc\n"[..], |_| {
            n += 1;
            false
        });
        assert_eq!(n, 1);
    }

    #[test]
    fn a_reaped_run_is_never_signalled() {
        let d = temp("reaped");
        let mut run = Run::spawn_group(sh("exit 0", &d), egui::Context::default()).unwrap();
        let t0 = Instant::now();
        while !run.poll().iter().any(|l| matches!(l, Line::Done(_))) {
            assert!(t0.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!run.terminate(), "its pid may be another process's now");
        assert!(run.kill_at.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }
}
