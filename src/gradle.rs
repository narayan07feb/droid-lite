//! Runs Gradle tasks in the background, and finds the preview screenshot tool a module uses.

use crate::project::Project;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};

pub enum Line {
    Out(String),
    Done(Option<i32>),
}

pub struct Run {
    child: Child,
    rx: Receiver<Line>,
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
    pub fn spawn(mut cmd: Command, ctx: egui::Context) -> std::io::Result<Run> {
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
        Ok(Run { child, rx })
    }

    /// New output lines, then `Done` once the process has exited.
    pub fn poll(&mut self) -> Vec<Line> {
        let mut out: Vec<Line> = self.rx.try_iter().collect();
        if let Ok(Some(status)) = self.child.try_wait() {
            out.extend(self.rx.try_iter());
            out.push(Line::Done(status.code()));
        }
        out
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn stop(&mut self) {
        let _ = self.child.kill();
    }
}

impl Drop for Run {
    /// A dropped run (new project, app quit) must not leave Gradle running or a zombie behind.
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}
