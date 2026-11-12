//! Run on a device: the toolbar's device picker and ▶ App button, and ▶ next to each `@Preview`.
//! Gradle installs the debug build on the picked device, then adb starts the app or the preview
//! and the Logcat tab shows the app's lines.

use super::{App, Bottom};
use crate::device::{self, Launch};
use crate::icons as ic;
use egui::{Color32, RichText, Ui};
use std::sync::mpsc::{Receiver, channel};

/// An install running in Gradle, and what to start when it succeeds.
pub(super) struct Pending {
    serial: String,
    app_id: String,
    what: Launch,
}

#[derive(Default)]
pub(super) struct RunState {
    pub(super) pending: Option<Pending>,
    pub(super) launching: Option<Receiver<Result<String, String>>>,
}

impl App {
    /// Installs the debug app on the picked device, then starts the app or one preview.
    pub(super) fn run_on_device(&mut self, what: Launch, ctx: &egui::Context) {
        let Some(p) = &self.project else { return };
        let apps = device::app_modules(p);
        let Some((module, app_id)) = apps.into_iter().next() else {
            self.status = "No application module (com.android.application) in this project".into();
            return;
        };
        let Some(app_id) = app_id else {
            self.status = format!("No applicationId in {}'s build file", module.name);
            return;
        };
        if self.logcat.adb_path().is_none() {
            self.status =
                "adb not found: install Android SDK platform-tools or set DROID_LITE_ADB".into();
            return;
        }
        self.logcat.refresh_devices_now();
        let online = |s: &String| {
            self.logcat
                .devices()
                .iter()
                .any(|d| &d.serial == s && d.online)
        };
        let Some(serial) = self.logcat.device.clone().filter(online) else {
            self.status = "No device: plug one in or run adb connect".into();
            return;
        };
        let task = device::install_task(&module);
        self.run_gradle_on(task, Some(&serial), ctx);
        self.status = format!("Installing on {serial}…");
        self.run.pending = Some(Pending {
            serial,
            app_id,
            what,
        });
    }

    /// Called when a Gradle run ends: after a good install, starts the app in the background.
    pub(super) fn after_install(&mut self, ok: bool, ctx: &egui::Context) {
        let Some(p) = self.run.pending.take() else {
            return;
        };
        if !ok {
            self.status = "Install failed: see the Build tab".into();
            return;
        }
        let Some(adb) = self.logcat.adb_path() else {
            return;
        };
        let (tx, rx) = channel();
        let c = ctx.clone();
        self.status = format!("Starting on {}…", p.serial);
        std::thread::spawn(move || {
            let _ = tx.send(device::launch(&adb, &p.serial, &p.app_id, &p.what));
            c.request_repaint();
        });
        self.run.launching = Some(rx);
    }

    pub(super) fn poll_launch(&mut self) {
        let Some(res) = self
            .run
            .launching
            .as_ref()
            .and_then(|rx| rx.try_recv().ok())
        else {
            return;
        };
        self.run.launching = None;
        match res {
            Ok(msg) => {
                self.console.push(msg.clone());
                self.status = msg;
                self.logcat.show_mine();
                self.bottom = Bottom::Logcat;
            }
            Err(e) => {
                self.console.push(format!("Could not start: {e}"));
                self.status = e;
                self.bottom = Bottom::Build;
            }
        }
    }

    /// The device picker and ▶ App, in the toolbar (right to left).
    pub(super) fn run_controls(&mut self, ui: &mut Ui) {
        let busy = self.gradle.is_some() || self.run.launching.is_some();
        if ui
            .add_enabled(
                self.project.is_some() && !busy,
                egui::Button::new(
                    RichText::new(format!("{}  App", ic::PLAY))
                        .color(Color32::from_rgb(0x3D, 0xDC, 0x84)),
                ),
            )
            .on_hover_text("Install the debug app on the device and start it")
            .clicked()
        {
            self.run_on_device(Launch::App, &ui.ctx().clone());
        }
        let current = self
            .logcat
            .device
            .as_ref()
            .and_then(|s| self.logcat.devices().iter().find(|d| &d.serial == s))
            .map_or_else(|| "No device".to_string(), |d| d.label.clone());
        let r = egui::ComboBox::from_id_salt("run_device")
            .selected_text(current)
            .width(170.0)
            .show_ui(ui, |ui| {
                let devices = self.logcat.devices().to_vec();
                if devices.is_empty() {
                    ui.label("No devices. Plug one in or run adb connect.");
                }
                for d in devices {
                    let label = RichText::new(&d.label);
                    let label = if d.online { label } else { label.weak() };
                    if ui
                        .selectable_label(self.logcat.device.as_ref() == Some(&d.serial), label)
                        .clicked()
                    {
                        self.logcat.select_device(d.serial);
                    }
                }
            });
        name_combo(&r.response, "Run device");
        // the list is read when the picker opens (adb devices is a quick local call)
        if r.response.clicked() {
            self.logcat.refresh_devices_now();
        }
        self.capture_controls(ui);
    }
}

/// Gives a label-less ComboBox an accessible name (screen readers, UI tests).
pub(super) fn name_combo(r: &egui::Response, name: &str) {
    r.widget_info(|| {
        let mut info = egui::WidgetInfo::labeled(egui::Role::ComboBox, true, name);
        info.current_text_value = None;
        info
    });
}
