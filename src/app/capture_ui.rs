//! Screenshot and screen recording of the picked device: two toolbar buttons and a result window.

use super::App;
use crate::capture::{self, Recording};
use crate::icons as ic;
use egui::{Color32, RichText, Ui};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};

#[derive(Default)]
pub(super) struct CaptureState {
    pub(super) recording: Option<Recording>,
    /// A screenshot or a stopped recording being saved.
    pub(super) saving: Option<Receiver<Result<PathBuf, String>>>,
    /// The last saved file, shown in a small window.
    pub(super) last: Option<PathBuf>,
}

impl App {
    /// The picked device when it is online, and adb.
    fn capture_target(&mut self) -> Option<(PathBuf, String)> {
        let serial = self.logcat.device.clone()?;
        let online = self
            .logcat
            .devices()
            .iter()
            .any(|d| d.serial == serial && d.online);
        online.then_some(())?;
        Some((self.logcat.adb_path()?, serial))
    }

    fn save_in_background(
        &mut self,
        ctx: &egui::Context,
        job: impl FnOnce() -> Result<PathBuf, String> + Send + 'static,
    ) {
        let (tx, rx) = channel();
        let c = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(job());
            c.request_repaint();
        });
        self.capture.saving = Some(rx);
    }

    pub(super) fn take_screenshot(&mut self, ctx: &egui::Context) {
        let Some((adb, serial)) = self.capture_target() else {
            self.status = "No device: plug one in or run adb connect".into();
            return;
        };
        self.status = format!("Taking a screenshot of {serial}…");
        self.save_in_background(ctx, move || {
            capture::screenshot(&adb, &serial, &capture::out_dir())
        });
    }

    pub(super) fn toggle_recording(&mut self, ctx: &egui::Context) {
        if let Some(rec) = self.capture.recording.take() {
            self.status = format!("Saving the recording of {}…", rec.serial());
            self.save_in_background(ctx, move || rec.finish(&capture::out_dir()));
            return;
        }
        let Some((adb, serial)) = self.capture_target() else {
            self.status = "No device: plug one in or run adb connect".into();
            return;
        };
        match Recording::start(&adb, &serial) {
            Ok(r) => {
                self.status = format!("Recording {serial} (3 min at most, no sound)");
                self.capture.recording = Some(r);
            }
            Err(e) => self.status = format!("Could not record: {e}"),
        }
    }

    /// Saves a recording that ended by itself, and takes finished saves.
    pub(super) fn poll_capture(&mut self, ctx: &egui::Context) {
        if let Some(r) = &mut self.capture.recording {
            if r.ended() {
                self.toggle_recording(ctx);
            } else {
                // the elapsed time in the toolbar
                ctx.request_repaint_after(std::time::Duration::from_millis(500));
            }
        }
        let Some(res) = self
            .capture
            .saving
            .as_ref()
            .and_then(|rx| rx.try_recv().ok())
        else {
            return;
        };
        self.capture.saving = None;
        match res {
            Ok(path) => {
                self.status = format!("Saved {}", path.display());
                self.console.push(self.status.clone());
                self.capture.last = Some(path);
            }
            Err(e) => {
                self.status = format!("Capture failed: {e}");
                self.console.push(self.status.clone());
            }
        }
    }

    /// 📷 and ⏺ (⏹ with the time while recording), left of the device picker.
    pub(super) fn capture_controls(&mut self, ui: &mut Ui) {
        let ready = self.capture.saving.is_none() && self.logcat.device.is_some();
        let rec = self
            .capture
            .recording
            .as_ref()
            .map(|r| r.started.elapsed().as_secs());
        let label = match rec {
            Some(s) => RichText::new(format!("{}  {}:{:02}", ic::STOP_CIRCLE, s / 60, s % 60))
                .color(crate::highlight::ERROR),
            None => RichText::new(ic::RECORD).color(Color32::from_rgb(0xE0, 0x55, 0x55)),
        };
        if ui
            .add_enabled(ready || rec.is_some(), egui::Button::new(label))
            .on_hover_text(if rec.is_some() {
                "Stop and save the video"
            } else {
                "Record the device screen (3 min at most, no sound)"
            })
            .clicked()
        {
            self.toggle_recording(&ui.ctx().clone());
        }
        if ui
            .add_enabled(ready && rec.is_none(), egui::Button::new(ic::CAMERA))
            .on_hover_text("Screenshot of the device")
            .clicked()
        {
            self.take_screenshot(&ui.ctx().clone());
        }
    }

    /// The last capture: the image, Copy and Show in Finder.
    pub(super) fn capture_window(&mut self, ctx: &egui::Context) {
        let Some(path) = self.capture.last.clone() else {
            return;
        };
        let name = path
            .file_name()
            .map_or(String::new(), |n| n.to_string_lossy().into_owned());
        let image = (path.extension().is_some_and(|e| e == "png"))
            .then(|| super::texture(ctx, &mut self.textures, &path))
            .flatten();
        let mut open = true;
        egui::Window::new(format!(
            "{}  {name}",
            if image.is_some() {
                ic::CAMERA
            } else {
                ic::VIDEO_CAMERA
            }
        ))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::RIGHT_TOP, [-12.0, 48.0])
        .show(ctx, |ui| {
            if let Some(t) = &image {
                ui.add(egui::Image::new(t).max_size(egui::vec2(320.0, 420.0)));
            }
            ui.label(RichText::new(path.display().to_string()).weak().small());
            ui.horizontal(|ui| {
                if image.is_some()
                    && ui.button(format!("{}  Copy", ic::COPY)).clicked()
                    && let Ok(img) = image::open(&path)
                {
                    let img = img.to_rgba8();
                    let size = [img.width() as usize, img.height() as usize];
                    ui.ctx()
                        .copy_image(egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()));
                    self.status = "Screenshot copied".into();
                }
                if ui
                    .button(format!("{}  Show in Finder", ic::FOLDER_OPEN))
                    .clicked()
                {
                    let _ = std::process::Command::new("open")
                        .arg("-R")
                        .arg(&path)
                        .spawn();
                }
            });
        });
        if !open {
            self.capture.last = None;
        }
    }
}
