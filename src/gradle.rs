//! Runs Gradle tasks in the background, and finds the preview screenshot tool a module uses.

use crate::project::Project;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

pub enum Line {
    Out(String),
    Done(Option<i32>),
}

pub struct Run {
    child: Child,
    rx: Receiver<Line>,
    /// It leads its own process group ([`Run::spawn_group`]): signals reach the tools it started.
    group: bool,
    /// `poll` saw it exit and reaped it: its pid (and group id) may be another process's by now,
    /// so it is never signalled again.
    reaped: bool,
    /// When SIGKILL follows the SIGTERM of [`Run::terminate`], and how long after.
    kill_at: Option<Instant>,
    kill_after: Duration,
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
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    if tx.send(Line::Out(line)).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            });
        }
        Ok(Run {
            child,
            rx,
            group,
            reaped: false,
            kill_at: None,
            kill_after: Duration::from_secs(5),
        })
    }

    /// New output lines, then `Done` once the process has exited. A run that [`Run::terminate`]
    /// asked to stop and is still there gets SIGKILL once its time is up.
    pub fn poll(&mut self) -> Vec<Line> {
        let mut out: Vec<Line> = self.rx.try_iter().collect();
        if let Ok(Some(status)) = self.child.try_wait() {
            self.reaped = true;
            out.extend(self.rx.try_iter());
            out.push(Line::Done(status.code()));
        } else if self.kill_at.is_some_and(|t| Instant::now() >= t) {
            self.kill_at = None;
            self.signal(Signal::Kill);
        }
        out
    }

    pub fn stop(&mut self) {
        let _ = self.child.kill();
    }

    /// SIGTERM to its process group (to it alone when it has none), then SIGKILL after 5 s if it
    /// is still there ([`Run::poll`]). False, and no signal, once `poll` reaped it.
    pub fn terminate(&mut self) -> bool {
        if self.reaped {
            return false;
        }
        self.signal(Signal::Term);
        if self.kill_at.is_none() {
            self.kill_at = Some(Instant::now() + self.kill_after);
        }
        true
    }

    /// Only while it is not reaped: until then its pid (and group id) cannot be another's.
    fn signal(&mut self, s: Signal) {
        if self.reaped {
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

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

impl Drop for Run {
    /// A dropped run (new project, app quit) must not leave Gradle running or a zombie behind: it
    /// and, in a group of its own, every tool it started get SIGKILL (unless it was reaped).
    fn drop(&mut self) {
        self.signal(Signal::Kill);
        let _ = self.child.wait();
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
