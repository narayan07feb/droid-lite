//! The Performance tab: frame times with the slowest part of each frame, the app's memory
//! footprint and how long the project took to open. Recording a frame is a push into a fixed
//! ring; the graph and the memory read cost something only while the tab is open.

use super::App;
use egui::{Color32, RichText, Sense, Ui};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Frames kept for the graph.
const KEEP: usize = 240;
/// A frame over this is the "slow frame" the status bar reports.
pub(super) const SLOW_MS: f32 = 100.0;

/// One frame: total time and its slowest part.
#[derive(Clone, Copy)]
pub(super) struct Frame {
    pub ms: f32,
    pub stage: &'static str,
    pub stage_ms: f32,
}

#[derive(Default)]
pub(super) struct Perf {
    pub frames: VecDeque<Frame>,
    /// Every part of the last frame, in order.
    pub last: Vec<(&'static str, f32)>,
    /// When the current project started to open, until it is ready.
    pub opening: Option<Instant>,
    /// Open + index time of the current project.
    pub opened_in: Option<Duration>,
    footprint: Option<(Instant, u64)>,
}

impl Perf {
    pub fn record(&mut self, stages: Vec<(&'static str, f32)>, ms: f32) {
        let (stage, stage_ms) = stages
            .iter()
            .copied()
            .fold(("", 0.0), |a, b| if b.1 > a.1 { b } else { a });
        if self.frames.len() == KEEP {
            self.frames.pop_front();
        }
        self.frames.push_back(Frame {
            ms,
            stage,
            stage_ms,
        });
        self.last = stages;
    }

    /// Median and worst of the kept frames.
    pub fn summary(&self) -> Option<(f32, Frame)> {
        let mut ms: Vec<f32> = self.frames.iter().map(|f| f.ms).collect();
        if ms.is_empty() {
            return None;
        }
        ms.sort_by(f32::total_cmp);
        let worst = *self.frames.iter().max_by(|a, b| a.ms.total_cmp(&b.ms))?;
        Some((ms[ms.len() / 2], worst))
    }
}

/// The app's memory as Activity Monitor shows it (physical footprint), in bytes.
#[cfg(target_os = "macos")]
pub(super) fn footprint() -> Option<u64> {
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a rusage_info_v2, the struct the RUSAGE_INFO_V2 flavour fills.
    let r = unsafe {
        libc::proc_pid_rusage(
            std::process::id() as i32,
            libc::RUSAGE_INFO_V2,
            (&raw mut info).cast(),
        )
    };
    (r == 0).then_some(info.ri_phys_footprint)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn footprint() -> Option<u64> {
    None
}

impl App {
    pub(super) fn perf_panel(&mut self, ui: &mut Ui) {
        // memory: read at most once a second, and only while this tab is open
        let now = Instant::now();
        if self
            .perf
            .footprint
            .is_none_or(|(at, _)| now - at >= Duration::from_secs(1))
        {
            self.perf.footprint = footprint().map(|b| (now, b));
        }
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        let p = &self.perf;
        ui.horizontal_wrapped(|ui| {
            if let Some((median, worst)) = p.summary() {
                ui.label(format!(
                    "Frames: median {median:.1} ms, slowest {:.0} ms ({} {:.0} ms)",
                    worst.ms, worst.stage, worst.stage_ms
                ));
                ui.separator();
            }
            if let Some((_, b)) = p.footprint {
                ui.label(format!("Memory {:.0} MB", b as f64 / 1_048_576.0));
                ui.separator();
            }
            match (p.opening, p.opened_in) {
                (Some(t), _) => ui.label(format!(
                    "Opening project… {:.1} s",
                    t.elapsed().as_secs_f32()
                )),
                (None, Some(d)) => ui.label(format!("Project opened in {:.2} s", d.as_secs_f32())),
                _ => ui.label("No project"),
            };
        });
        frame_graph(ui, &p.frames);
        ui.label(RichText::new("Last frame").strong());
        ui.horizontal_wrapped(|ui| {
            for (name, ms) in &p.last {
                ui.label(RichText::new(format!("{name} {ms:.1} ms")).weak());
            }
        });
    }
}

/// Bars of the kept frame times, with lines at 16.7 ms (60 fps) and the slow-frame limit.
/// Hover a bar for its time and slowest part.
fn frame_graph(ui: &mut Ui, frames: &VecDeque<Frame>) {
    let h = 90.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), Sense::hover());
    let painter = ui.painter_at(rect);
    let t = crate::theme::t();
    painter.rect_filled(rect, 2.0, t.weak.gamma_multiply(0.08));
    // a fixed scale: one very slow frame must not flatten the rest; it is clipped, and red
    let top = SLOW_MS * 1.25;
    let y = |ms: f32| rect.bottom() - (ms / top).min(1.0) * h;
    let bar_w = rect.width() / KEEP as f32;
    let mut hovered = None;
    for (i, f) in frames.iter().enumerate() {
        let x = rect.left() + i as f32 * bar_w;
        let color = if f.ms >= SLOW_MS {
            crate::highlight::ERROR
        } else if f.ms > 1000.0 / 60.0 {
            crate::highlight::WARN
        } else {
            Color32::from_rgb(0x5F, 0xB8, 0x65)
        };
        let bar = egui::Rect::from_min_max(
            egui::pos2(x, y(f.ms)),
            egui::pos2(x + bar_w.max(1.0), rect.bottom()),
        );
        painter.rect_filled(bar, 0.0, color);
        if resp.hovered()
            && let Some(p) = resp.hover_pos()
            && p.x >= x
            && p.x < x + bar_w
        {
            hovered = Some(*f);
        }
    }
    for (ms, label) in [(1000.0 / 60.0, "16.7 ms"), (SLOW_MS, "100 ms")] {
        let at = y(ms);
        painter.hline(rect.x_range(), at, (1.0, t.weak.gamma_multiply(0.6)));
        painter.text(
            egui::pos2(rect.right() - 4.0, at - 2.0),
            egui::Align2::RIGHT_BOTTOM,
            label,
            egui::FontId::proportional(10.0),
            t.weak,
        );
    }
    if let Some(f) = hovered {
        resp.on_hover_text(format!(
            "{:.1} ms, mostly {} ({:.1} ms)",
            f.ms, f.stage, f.stage_ms
        ));
    }
}
