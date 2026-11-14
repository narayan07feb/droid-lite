//! Short notes in the bottom-right corner when a build or a test run ends. They slide in, stay a
//! few seconds, then fade out; a click opens the tab with the full result (or, for a crash, the
//! line it happened on).

use super::{App, Bottom};
use crate::icons as ic;
use crate::theme;
use egui::{Align2, Color32, RichText};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const SHOWN: Duration = Duration::from_secs(5);
const MAX: usize = 3;

pub(super) struct Toast {
    text: String,
    ok: bool,
    tab: Bottom,
    at: Instant,
    id: u64,
    /// A file and line to open instead of the tab.
    goto: Option<(PathBuf, usize)>,
}

impl App {
    /// Adds a note, unless the result's tab is already on screen (then the note would only cover
    /// the buttons under it). The oldest goes when more than three are up.
    pub(super) fn toast(&mut self, text: String, ok: bool, tab: Bottom) {
        if self.show_bottom && !self.focus && self.bottom == tab {
            return;
        }
        self.push_toast(text, ok, tab, None);
    }

    /// A failure note that opens `goto` (a file and line). It shows even when its tab is on
    /// screen: the line is the news.
    pub(super) fn toast_to(&mut self, text: String, goto: Option<(PathBuf, usize)>, tab: Bottom) {
        self.push_toast(text, false, tab, goto);
    }

    fn push_toast(&mut self, text: String, ok: bool, tab: Bottom, goto: Option<(PathBuf, usize)>) {
        self.toast_seq += 1;
        self.toasts.push(Toast {
            text,
            ok,
            tab,
            at: Instant::now(),
            id: self.toast_seq,
            goto,
        });
        if self.toasts.len() > MAX {
            self.toasts.remove(0);
        }
    }

    pub(super) fn toasts_ui(&mut self, ctx: &egui::Context) {
        self.toasts.retain(|t| t.at.elapsed() < SHOWN);
        let Some(next) = self.toasts.iter().map(|t| SHOWN - t.at.elapsed()).min() else {
            return;
        };
        // wake once when the next note leaves; the slide itself asks for frames only while it moves
        ctx.request_repaint_after(next);
        let t = theme::t();
        let mut open = None;
        let mut y = -40.0;
        for toast in self.toasts.iter().rev() {
            let leaving = SHOWN.saturating_sub(toast.at.elapsed()) < Duration::from_millis(400);
            let id = egui::Id::unique(("toast", toast.id));
            let shown = theme::ease(ctx, id.with("in"), (!leaving) as u8 as f32, 0.25);
            if leaving {
                ctx.request_repaint();
            }
            let (icon, tint) = if toast.ok {
                (ic::CHECK_CIRCLE, Color32::from_rgb(0x5F, 0xB8, 0x65))
            } else {
                (ic::WARNING, crate::highlight::ERROR)
            };
            let r = egui::Area::new(id)
                .anchor(Align2::RIGHT_BOTTOM, [-16.0 + (1.0 - shown) * 40.0, y])
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    ui.set_opacity(shown);
                    egui::Frame::popup(ui.style())
                        .fill(t.raised)
                        .inner_margin(egui::Margin::symmetric(14, 10))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(icon).color(tint));
                                ui.label(RichText::new(&toast.text).color(t.text).strong());
                            });
                            let hint = if toast.goto.is_some() {
                                "Click to open the line"
                            } else {
                                "Click to open"
                            };
                            ui.label(RichText::new(hint).color(t.weak).small());
                        })
                        .response
                        .interact(egui::Sense::click())
                });
            if r.inner.clicked() {
                open = Some((toast.id, toast.tab, toast.goto.clone()));
            }
            y -= r.response.rect.height() + 8.0;
        }
        if let Some((id, tab, goto)) = open {
            self.toasts.retain(|t| t.id != id);
            if let Some((path, line)) = goto {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                let at = crate::edit::line_offset(&text, &line.to_string());
                self.open_file(&path, at);
                return;
            }
            self.bottom = tab;
            self.show_bottom = true;
            self.focus = false;
        }
    }
}
