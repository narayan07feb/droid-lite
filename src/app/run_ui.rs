//! Run on a device: the toolbar's device picker and ▶ App button, and ▶ next to each `@Preview`.
//! Gradle installs the debug build on the picked device, then adb starts the app or the preview
//! and the Logcat tab shows the app's lines.

use super::{App, Bottom};
use crate::device::{self, Launch};
use crate::icons as ic;
use egui::{Color32, RichText, Ui};
use std::path::PathBuf;
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
    /// The app module, its application id, adb and the picked online device; else says what is
    /// missing in the status bar.
    fn device_target(&mut self) -> Option<(crate::project::Module, String, PathBuf, String)> {
        let p = self.project.as_ref()?;
        let Some((module, app_id)) = device::app_modules(p).into_iter().next() else {
            self.status = "No application module (com.android.application) in this project".into();
            return None;
        };
        let Some(app_id) = app_id else {
            self.status = format!("No applicationId in {}'s build file", module.name);
            return None;
        };
        let Some(adb) = self.logcat.adb_path() else {
            self.status =
                "adb not found: install Android SDK platform-tools or set DROID_LITE_ADB".into();
            return None;
        };
        self.logcat.refresh_devices_now();
        let online = |s: &String| {
            self.logcat
                .devices()
                .iter()
                .any(|d| &d.serial == s && d.online)
        };
        let Some(serial) = self.logcat.device.clone().filter(online) else {
            self.status = "No device: plug one in or run adb connect".into();
            return None;
        };
        Some((module, app_id, adb, serial))
    }

    /// Installs the debug app on the picked device, then starts the app or one preview.
    pub(super) fn run_on_device(&mut self, what: Launch, ctx: &egui::Context) {
        let Some((module, app_id, _, serial)) = self.device_target() else {
            return;
        };
        let task = crate::android_kit::install_task(&module.name, &self.variant);
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
        if crate::theme::button(
            ui,
            self.project.is_some() && !busy,
            RichText::new(format!("{}  App", ic::PLAY)).color(Color32::from_rgb(0x3D, 0xDC, 0x84)),
        )
        .on_hover_text("Install the app (the picked build variant) on the device and start it")
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
                if !self.avds.is_empty() {
                    ui.separator();
                    ui.label(RichText::new("Start an emulator").weak().small());
                }
                for avd in self.avds.clone() {
                    if ui.button(format!("{}  {avd}", ic::PLAY)).clicked() {
                        self.start_emulator(&avd);
                        ui.close();
                    }
                }
                ui.separator();
                if ui.button("Open a deep link…").clicked() {
                    self.deep_link = Some(self.res.links.first().cloned().unwrap_or_default());
                    ui.close();
                }
            });
        name_combo(&r.response, "Run device");
        // the lists are read when the picker opens (both are quick local calls)
        if r.response.clicked() {
            self.logcat.refresh_devices_now();
            self.load_avds(ui.ctx());
        }
        if let Some(a) = self.avds_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.avds = a;
            self.avds_rx = None;
        }
        self.variant_combo(ui);
        self.capture_controls(ui);
    }
}

impl App {
    /// Reads the emulator's virtual devices in the background (`emulator -list-avds` starts a
    /// process: too slow for a frame).
    pub(super) fn load_avds(&mut self, ctx: &egui::Context) {
        if self.avds_rx.is_some() {
            return;
        }
        let adb = self.logcat.adb_path();
        let (tx, rx) = channel();
        let c = ctx.clone();
        std::thread::spawn(move || {
            let list = device::emulator(adb.as_deref())
                .map(|e| device::avds(&e))
                .unwrap_or_default();
            let _ = tx.send(list);
            c.request_repaint();
        });
        self.avds_rx = Some(rx);
    }

    fn start_emulator(&mut self, avd: &str) {
        let adb = self.logcat.adb_path();
        let started = device::emulator(adb.as_deref()).map(|e| device::start_avd(&e, avd));
        self.status = match started {
            Some(Ok(())) => {
                format!("Starting the emulator {avd}… it shows in the device list when it is up")
            }
            Some(Err(e)) => format!("Could not start the emulator: {e}"),
            None => "The Android emulator is not installed (SDK Manager → Android Emulator)".into(),
        };
    }

    /// Opens a deep link in the app on the picked device, in the background.
    pub(super) fn open_deep_link(&mut self, uri: String, ctx: &egui::Context) {
        let Some((_, app_id, adb, serial)) = self.device_target() else {
            return;
        };
        let (tx, rx) = channel();
        let c = ctx.clone();
        self.status = format!("Opening {uri}…");
        std::thread::spawn(move || {
            let _ = tx.send(device::open_link(&adb, &serial, &app_id, &uri));
            c.request_repaint();
        });
        self.run.launching = Some(rx);
    }

    /// The Deep link window: the manifest's links to pick from, or any link typed.
    pub(super) fn deep_link_window(&mut self, ctx: &egui::Context) {
        let Some(mut uri) = self.deep_link.take() else {
            return;
        };
        let (mut open, mut go) = (true, false);
        egui::Window::new("Open a deep link")
            .open(&mut open)
            .collapsible(false)
            .default_width(420.0)
            .show(ctx, |ui| {
                if self.res.links.is_empty() {
                    ui.label(
                        RichText::new("No VIEW intent filters in the manifests. Type a link.")
                            .weak()
                            .small(),
                    );
                }
                for l in &self.res.links {
                    if ui.selectable_label(*l == uri, l).clicked() {
                        uri = l.clone();
                    }
                }
                ui.add(
                    egui::TextEdit::singleline(&mut uri)
                        .hint_text("myapp://screen?id=1")
                        .desired_width(f32::INFINITY),
                );
                go = crate::theme::button(ui, !uri.trim().is_empty(), "Open on the device")
                    .clicked();
            });
        if go {
            self.open_deep_link(uri.trim().to_string(), ctx);
        }
        if open && !go {
            self.deep_link = Some(uri);
        }
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
