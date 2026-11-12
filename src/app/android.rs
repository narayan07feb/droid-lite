//! Android navigation: `R.string.x` / `@string/x` to the resource, `libs.x.y` to the version catalog,
//! and resource values in the hover card.

use super::App;
use crate::catalog;
use crate::index::Hit;
use crate::resources::{self, Resources};
use crate::theme;
use egui::{RichText, Ui};
use std::sync::mpsc::channel;

impl App {
    /// Handles ⌘-click / Go to declaration on Android references. Returns false when the word is
    /// not one, so the normal lookup runs.
    pub(super) fn goto_android_reference(&mut self, word: &str, at: usize) -> bool {
        let Some(t) = self.tabs.get(self.active) else {
            return false;
        };
        let script = t
            .path
            .extension()
            .is_some_and(|e| e == "kts" || e == "gradle");
        if script && let Some(acc) = catalog::accessor_at(&t.text, at) {
            let Some(toml) = self
                .project
                .as_ref()
                .map(|p| p.root.join("gradle/libs.versions.toml"))
            else {
                return false;
            };
            match std::fs::read_to_string(&toml)
                .ok()
                .and_then(|txt| catalog::find(&txt, &acc))
            {
                Some(o) => self.open_file(&toml, Some(o)),
                None => self.status = format!("`{acc}` is not in gradle/libs.versions.toml"),
            }
            return true;
        }
        let Some((kind, name)) = resources::reference_at(&t.text, at, at + word.len()) else {
            return false;
        };
        let hits: Vec<Hit> = self
            .res
            .get(&kind, &name)
            .into_iter()
            .map(|e| Hit {
                name: format!("{kind}/{name}"),
                detail: format!(
                    "{}  {}",
                    if e.qualifier.is_empty() {
                        "default"
                    } else {
                        &e.qualifier
                    },
                    e.value.clone().unwrap_or_default()
                ),
                path: e.path.clone(),
                offset: e.offset,
                score: 0,
            })
            .collect();
        match hits.len() {
            0 => self.status = format!("No resource {kind}/{name} in the project"),
            1 => self.open_file(&hits[0].path.clone(), Some(hits[0].offset)),
            n => self.pick(format!("{kind}/{name}: {n} versions"), hits),
        }
        true
    }

    /// The ⌘-hover card of a resource: its value in each folder (default, night, other languages).
    pub(super) fn resource_card(&mut self, ui: &mut Ui, kind: &str, name: &str) {
        let entries = self.res.get(kind, name);
        ui.label(
            RichText::new(format!("R.{kind}.{name}"))
                .monospace()
                .strong(),
        );
        if entries.is_empty() {
            ui.label(
                RichText::new("Not found in the project's res folders").color(theme::t().weak),
            );
        }
        for e in entries.iter().take(10) {
            let folder = if e.qualifier.is_empty() {
                "default".to_string()
            } else {
                e.qualifier.clone()
            };
            ui.horizontal(|ui| {
                if kind == "color"
                    && let Some(c) = e
                        .value
                        .as_deref()
                        .and_then(resources::parse_hex)
                        .or_else(|| self.res.color(name))
                {
                    let (r, _) =
                        ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                    ui.painter().rect_filled(r, 2.0, c);
                }
                let what = e.value.clone().unwrap_or_else(|| {
                    e.path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                });
                ui.label(RichText::new(format!("{folder}:  {what}")).small());
            });
        }
        ui.label(
            RichText::new("⌘-click: open")
                .color(theme::t().weak)
                .small(),
        );
    }

    /// Rescans resources in the background when files under `res/` changed.
    pub(super) fn rescan_resources(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.res_rx
            && let Ok(r) = rx.try_recv()
        {
            self.res = r;
            self.res_rx = None;
            for t in self.tabs.iter_mut() {
                t.annotations = None; // swatches may change
            }
        }
        if self.res_stale && self.res_rx.is_none() {
            self.res_stale = false;
            let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                return;
            };
            let (tx, rx) = channel();
            let c = ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(Resources::scan(&root));
                c.request_repaint();
            });
            self.res_rx = Some(rx);
        }
    }
}
