//! The Logcat tab: device picker, live log with Android Studio colours, filter bar, pause / follow /
//! clear, and clickable stack frames. `adb` runs only while this tab is open.

use super::{App, Bottom};
use crate::logcat::{self, Device, Entry, Filter};
use egui::{Color32, RichText, Ui};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

/// Lines kept in memory; the oldest go first.
const KEEP: usize = 50_000;
/// Lines of history fetched when a device starts streaming.
const HISTORY: usize = 2_000;

#[derive(Default)]
pub(super) struct LogcatView {
    /// `None` until looked up; `Some(None)` when there is no adb.
    adb: Option<Option<PathBuf>>,
    devices: Vec<Device>,
    devices_rx: Option<Receiver<Vec<Device>>>,
    devices_at: Option<Instant>,
    pub(super) device: Option<String>,
    pub(super) stream: Option<logcat::Stream>,
    /// Lines from a restarted stream at or before this time were shown already.
    resume_after: Option<String>,
    pub(super) entries: VecDeque<Entry>,
    /// Sequence number of `entries[0]`.
    first: u64,
    /// Sequence numbers of the lines that pass the filter.
    pub(super) view: VecDeque<u64>,
    procs: HashMap<u32, Arc<str>>,
    procs_rx: Option<Receiver<HashMap<u32, Arc<str>>>>,
    procs_at: Option<Instant>,
    pub(super) query: String,
    filter: Filter,
    mine: Vec<String>,
    pub(super) paused: bool,
    scroll_lock: bool,
}

impl LogcatView {
    fn rebuild_view(&mut self) {
        let first = self.first;
        self.view = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| self.filter.matches(e))
            .map(|(i, _)| first + i as u64)
            .collect();
    }

    fn push(&mut self, mut e: Entry) {
        // after a restart, skip what was shown already; then stop checking, so a clock
        // change on the phone can never hide new lines
        match &self.resume_after {
            Some(t) if e.time <= *t => return,
            Some(_) => self.resume_after = None,
            None => {}
        }
        e.process = self.procs.get(&e.pid).cloned();
        if !self.paused && self.filter.matches(&e) {
            self.view.push_back(self.first + self.entries.len() as u64);
        }
        self.entries.push_back(e);
        while self.entries.len() > KEEP {
            self.entries.pop_front();
            self.first += 1;
        }
        while self.view.front().is_some_and(|&s| s < self.first) {
            self.view.pop_front();
        }
    }

    fn entry(&self, seq: u64) -> Option<&Entry> {
        self.entries.get(seq.checked_sub(self.first)? as usize)
    }

    /// Stops streaming (tab hidden, device changed); the lines stay.
    fn stop(&mut self) {
        if self.stream.take().is_some() {
            self.resume_after = self.entries.back().map(|e| e.time.clone());
        }
    }
}

/// Android Studio's Logcat colours per level: (letter background, message text).
fn level_colors(level: char) -> (Color32, Color32) {
    let t = crate::theme::t();
    match level {
        'V' => (Color32::from_rgb(0x5C, 0x5F, 0x66), t.weak),
        'D' => (
            Color32::from_rgb(0x30, 0x5D, 0x78),
            Color32::from_rgb(0x7E, 0xB6, 0xD9),
        ),
        'I' => (
            Color32::from_rgb(0x6A, 0x87, 0x59),
            Color32::from_rgb(0x9F, 0xC8, 0x86),
        ),
        'W' => (
            Color32::from_rgb(0xBB, 0xB5, 0x29),
            Color32::from_rgb(0xE5, 0xC0, 0x7B),
        ),
        'E' | 'A' | 'F' => (
            Color32::from_rgb(0xCF, 0x5B, 0x56),
            Color32::from_rgb(0xF0, 0x7E, 0x78),
        ),
        _ => (t.weak, t.text),
    }
}

impl App {
    /// Background work for the Logcat tab; runs every frame, does nothing while the tab is closed.
    pub(super) fn poll_logcat(&mut self, ctx: &egui::Context) {
        let lc = &mut self.logcat;
        if self.bottom != Bottom::Logcat {
            lc.stop();
            return;
        }
        let adb = lc.adb.get_or_insert_with(logcat::adb).clone();
        let Some(adb) = adb else { return };
        if lc.mine.is_empty()
            && let Some(p) = &self.project
        {
            lc.mine = logcat::application_ids(&p.files);
            lc.filter = Filter::parse(&lc.query, &lc.mine);
        }
        // devices every 3 s
        if let Some(rx) = &lc.devices_rx
            && let Ok(d) = rx.try_recv()
        {
            lc.devices = d;
            lc.devices_rx = None;
            if lc.device.is_none() {
                lc.device = lc
                    .devices
                    .iter()
                    .find(|d| d.online)
                    .map(|d| d.serial.clone());
            }
        }
        if lc.devices_rx.is_none()
            && lc
                .devices_at
                .is_none_or(|t| t.elapsed() > Duration::from_secs(3))
        {
            lc.devices_at = Some(Instant::now());
            let (tx, rx) = channel();
            let (a, c) = (adb.clone(), ctx.clone());
            std::thread::spawn(move || {
                let _ = tx.send(logcat::devices(&a));
                c.request_repaint();
            });
            lc.devices_rx = Some(rx);
        }
        ctx.request_repaint_after(Duration::from_secs(3));
        let Some(serial) = lc.device.clone() else {
            return;
        };
        let online = lc.devices.iter().any(|d| d.serial == serial && d.online);
        if lc.stream.as_mut().is_some_and(|s| !s.running()) {
            lc.stop(); // device unplugged or adb restarted; starts again when it is back
        }
        if lc.stream.is_none() && online {
            lc.stream =
                logcat::Stream::start(&adb, &serial, HISTORY, lc.resume_after.clone(), ctx.clone())
                    .ok();
            lc.procs_at = None;
        }
        // pid → process name every 5 s, and soon after an unknown pid shows up
        if let Some(rx) = &lc.procs_rx
            && let Ok(p) = rx.try_recv()
        {
            lc.procs = p;
            lc.procs_rx = None;
            // name the lines that arrived before their process was known
            let mut named = false;
            for e in lc.entries.iter_mut().filter(|e| e.process.is_none()) {
                if let Some(n) = lc.procs.get(&e.pid) {
                    e.process = Some(n.clone());
                    named = true;
                }
            }
            if named && !lc.filter.packages.is_empty() && !lc.paused {
                lc.rebuild_view();
            }
        }
        let mut new = Vec::new();
        if let Some(s) = &lc.stream {
            new.extend(s.rx.try_iter().take(20_000));
        }
        let mut unknown_pid = false;
        for e in new {
            unknown_pid |= !lc.procs.contains_key(&e.pid);
            lc.push(e);
        }
        let due = lc.procs_at.is_none_or(|t| {
            t.elapsed() > Duration::from_secs(5)
                || unknown_pid && t.elapsed() > Duration::from_secs(1)
        });
        if lc.stream.is_some() && lc.procs_rx.is_none() && due {
            lc.procs_at = Some(Instant::now());
            let (tx, rx) = channel();
            let (a, s, c) = (adb, serial, ctx.clone());
            std::thread::spawn(move || {
                let _ = tx.send(logcat::processes(&a, &s));
                c.request_repaint();
            });
            lc.procs_rx = Some(rx);
        }
    }

    pub(super) fn logcat_panel(&mut self, ui: &mut Ui) {
        let lc = &mut self.logcat;
        if matches!(lc.adb, Some(None)) {
            ui.label(
                "adb not found. Install Android SDK platform-tools (Android Studio's SDK Manager), \
                 or set DROID_LITE_ADB to the adb path.",
            );
            return;
        }
        ui.horizontal(|ui| {
            let current = lc
                .devices
                .iter()
                .find(|d| Some(&d.serial) == lc.device.as_ref())
                .map_or_else(
                    || lc.device.clone().unwrap_or_else(|| "No device".into()),
                    |d| d.label.clone(),
                );
            egui::ComboBox::from_id_salt("logcat_device")
                .selected_text(current)
                .width(220.0)
                .show_ui(ui, |ui| {
                    if lc.devices.is_empty() {
                        ui.label("No devices. Plug one in or run adb connect.");
                    }
                    for d in lc.devices.clone() {
                        if ui
                            .selectable_label(lc.device.as_ref() == Some(&d.serial), &d.label)
                            .clicked()
                            && lc.device.as_ref() != Some(&d.serial)
                        {
                            lc.stop();
                            lc.resume_after = None;
                            lc.entries.clear();
                            lc.view.clear();
                            lc.procs.clear();
                            lc.device = Some(d.serial);
                        }
                    }
                });
            let r = ui.add(
                egui::TextEdit::singleline(&mut lc.query)
                    .desired_width(ui.available_width() - 230.0)
                    .hint_text("package:mine  level:warn  tag:OkHttp  -tag:chatty  text"),
            );
            if r.changed() {
                lc.filter = Filter::parse(&lc.query, &lc.mine);
                lc.rebuild_view();
            }
            let pause = if lc.paused { "Resume" } else { "Pause" };
            if ui.button(pause).clicked() {
                lc.paused = !lc.paused;
                if !lc.paused {
                    lc.rebuild_view();
                }
            }
            ui.toggle_value(&mut lc.scroll_lock, "Scroll lock")
                .on_hover_text("Stop following new lines");
            if ui
                .button("Clear")
                .on_hover_text("Clears this view. The phone's log is not touched.")
                .clicked()
            {
                lc.first += lc.entries.len() as u64;
                lc.entries.clear();
                lc.view.clear();
            }
            ui.label(
                RichText::new(format!("{} lines", lc.view.len())).color(crate::theme::t().weak),
            );
        });
        ui.separator();
        if lc.device.is_none() {
            ui.label(
                RichText::new("Connect a device (USB or adb connect) to see its log.")
                    .color(crate::theme::t().weak),
            );
            return;
        }
        let mono = egui::FontId::monospace(12.0);
        let row_h = ui.fonts_mut(|f| f.row_height(&mono)) + 2.0;
        let mut open: Option<(String, String, usize)> = None;
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .stick_to_bottom(!lc.scroll_lock)
            .show_rows(ui, row_h, lc.view.len(), |ui, range| {
                for i in range {
                    let Some(e) = lc.view.get(i).and_then(|&s| lc.entry(s)) else {
                        continue;
                    };
                    let (bg, fg) = level_colors(e.level);
                    let weak = crate::theme::t().weak;
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        let col = |ui: &mut Ui, text: String, w: f32, color| {
                            ui.add_sized(
                                [w, row_h],
                                egui::Label::new(
                                    RichText::new(text).font(mono.clone()).color(color),
                                )
                                .truncate(),
                            );
                        };
                        col(
                            ui,
                            e.time
                                .split_once(' ')
                                .map_or(&e.time[..], |t| t.1)
                                .to_string(),
                            92.0,
                            weak,
                        );
                        col(ui, format!("{}-{}", e.pid, e.tid), 80.0, weak);
                        col(ui, e.tag.clone(), 150.0, fg);
                        let process = e.process.as_deref().unwrap_or("");
                        col(
                            ui,
                            process.rsplit('.').next().unwrap_or(process).to_string(),
                            110.0,
                            weak,
                        );
                        ui.label(
                            RichText::new(format!(" {} ", e.level))
                                .font(mono.clone())
                                .color(Color32::WHITE)
                                .background_color(bg),
                        );
                        if let Some(frame) = logcat::stack_frame(&e.msg) {
                            if ui
                                .link(RichText::new(&e.msg).font(mono.clone()).color(fg))
                                .clicked()
                            {
                                open = Some(frame);
                            }
                        } else {
                            ui.label(RichText::new(&e.msg).font(mono.clone()).color(fg));
                        }
                    });
                }
            });
        if let Some((pkg, file, line)) = open {
            self.open_stack_frame(&pkg, &file, line);
        }
    }

    /// Opens `file` at `line`, picking the project file whose folders match the frame's package.
    fn open_stack_frame(&mut self, pkg: &str, file: &str, line: usize) {
        let Some(p) = &self.project else { return };
        let mut found: Vec<&PathBuf> = p
            .files
            .iter()
            .filter(|f| f.file_name().is_some_and(|n| n == file))
            .collect();
        found.sort_by_key(|f| !f.to_string_lossy().contains(pkg));
        let Some(f) = found.first().map(|f| (*f).clone()) else {
            self.status = format!("{file} is not in this project (a library or the platform)");
            return;
        };
        let text = std::fs::read_to_string(&f).unwrap_or_default();
        let at = crate::edit::line_offset(&text, &line.to_string());
        self.open_file(&f, at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_from_a_known_pid_is_named_and_filtered_as_it_arrives() {
        let mine = vec!["com.demo.app".to_string()];
        let mut lc = LogcatView {
            filter: Filter::parse("package:mine", &mine),
            mine,
            ..Default::default()
        };
        lc.procs.insert(42, Arc::from("com.demo.app"));
        let line =
            |pid| logcat::parse(&format!("11-12 10:00:00.001  {pid}  {pid} I Tag : hi")).unwrap();
        lc.push(line(42));
        lc.push(line(7));
        assert_eq!(lc.entries[0].process.as_deref(), Some("com.demo.app"));
        assert_eq!(lc.view.len(), 1, "only the app's line passes package:mine");
    }

    #[test]
    fn a_restarted_stream_skips_shown_lines_once() {
        let mut lc = LogcatView {
            resume_after: Some("11-12 10:00:00.005".into()),
            ..Default::default()
        };
        let at = |t: &str| logcat::parse(&format!("11-12 {t}  1  1 I Tag : hi")).unwrap();
        lc.push(at("10:00:00.004"));
        lc.push(at("10:00:00.005"));
        assert!(lc.entries.is_empty(), "already shown before the restart");
        lc.push(at("10:00:00.006"));
        // the phone clock moved back: the line still shows
        lc.push(at("09:00:00.000"));
        assert_eq!(lc.entries.len(), 2);
    }

    #[test]
    fn keeps_at_most_keep_lines() {
        let mut lc = LogcatView::default();
        let e = logcat::parse("11-12 10:00:00.001  1  1 I Tag : hi").unwrap();
        for _ in 0..KEEP + 10 {
            lc.push(e.clone());
        }
        assert_eq!(lc.entries.len(), KEEP);
        assert_eq!(lc.view.len(), KEEP);
        assert_eq!(lc.view.front(), Some(&lc.first));
    }
}
