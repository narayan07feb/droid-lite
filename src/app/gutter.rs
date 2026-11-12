//! Editor annotations, like Android Studio's gutter: "implemented by" / "overrides" icons next to
//! the line numbers, inline Compose stability badges after `@Composable` declarations, and the
//! right-click menu.

use super::{App, Search, SearchKind};
use crate::compose::{self, Stability, Verdict};
use crate::highlight::{ERROR, WARN};
use crate::icons as ic;
use crate::index::Hit;
use crate::keymap::{self, Action};
use crate::kotlin::Kind;
use egui::{Align2, Color32, FontId, Galley, Pos2, Rect, RichText, Sense, Ui};

pub(super) const ICON_WIDTH: f32 = 18.0;
const UP: Color32 = Color32::from_rgb(0x58, 0x9D, 0xF6);
const DOWN: Color32 = Color32::from_rgb(0x5F, 0xB8, 0x65);

/// One gutter icon: `up` points to what this overrides or implements, else to what implements it.
pub(super) struct Mark {
    pub(super) line: usize,
    pub(super) up: bool,
    tip: String,
    pub(super) targets: Vec<Hit>,
}

/// Inline stability after a `@Composable` declaration.
pub(super) struct Badge {
    pub(super) line: usize,
    pub(super) text: String,
    color: Color32,
    pub(super) stability: Stability,
    guessed: bool,
}

/// Annotations of one tab, rebuilt only when its text, the index or the reports change.
pub(super) struct Annotations {
    key: (u64, u64, u64),
    pub(super) marks: Vec<Mark>,
    pub(super) badges: Vec<Badge>,
    /// Colour squares for lines that write a colour (`Color(0xFF…)`, `#RRGGBB`, `R.color.x`).
    pub(super) swatches: Vec<(usize, Color32)>,
}

/// What a click in the gutter or the context menu asks for, run after the editor is drawn.
pub(super) enum Request {
    Pick(String, Vec<Hit>),
    Run(Action),
}

impl App {
    /// Builds the active tab's annotations when they are stale.
    pub(super) fn refresh_annotations(&mut self) {
        let Some(t) = self.tabs.get(self.active) else {
            return;
        };
        let key = (t.rev, self.index.generation, self.outputs_gen);
        if t.annotations.as_ref().is_some_and(|a| a.key == key) {
            return;
        }
        let line_of = |o: usize| t.text[..o.min(t.text.len())].matches('\n').count();
        let mut marks = Vec::new();
        let mut badges = Vec::new();
        for d in &t.syms.decls {
            let line = line_of(d.offset);
            if d.kind.is_type() {
                let supers: Vec<Hit> = d
                    .supers
                    .iter()
                    .flat_map(|s| {
                        self.index
                            .named(s)
                            .filter(|(_, _, x)| x.kind.is_type())
                            .map(|(p, f, x)| hit_of(p, f, x))
                    })
                    .collect();
                if !supers.is_empty() {
                    let verb = if d.kind == Kind::Interface {
                        "Extends"
                    } else {
                        "Implements / extends"
                    };
                    marks.push(Mark {
                        line,
                        up: true,
                        tip: format!("{verb} {}", d.supers.join(", ")),
                        targets: supers,
                    });
                }
                let impls = self.index.implementations(&d.name);
                if !impls.is_empty() {
                    let verb = if d.kind == Kind::Interface {
                        "Implemented in"
                    } else {
                        "Subclassed by"
                    };
                    marks.push(Mark {
                        line,
                        up: false,
                        tip: format!(
                            "{verb} {} {}",
                            impls.len(),
                            if impls.len() == 1 { "class" } else { "classes" }
                        ),
                        targets: impls,
                    });
                }
            } else if let Some(c) = &d.container
                && matches!(d.kind, Kind::Fun | Kind::Property)
            {
                if d.modifiers.iter().any(|m| m == "override") {
                    if let Some(h) = self.index.overridden(c, &d.name) {
                        marks.push(Mark {
                            line,
                            up: true,
                            tip: format!("Overrides {}", h.name),
                            targets: vec![h],
                        });
                    }
                } else {
                    let impls = self.index.member_implementations(c, &d.name);
                    if !impls.is_empty() {
                        marks.push(Mark {
                            line,
                            up: false,
                            tip: format!(
                                "Implemented in {} {}",
                                impls.len(),
                                if impls.len() == 1 { "class" } else { "classes" }
                            ),
                            targets: impls,
                        });
                    }
                }
            }
            if d.is_composable() && !d.is_preview() && !d.params.is_empty() {
                let (stability, guessed) = match self.stability.get(&d.name) {
                    Some(s) => (s.clone(), false),
                    None => (compose::guess(d, &self.index), true),
                };
                let count = |v: Verdict| stability.params.iter().filter(|p| p.verdict == v).count();
                let (text, color) = match (
                    count(Verdict::Unstable),
                    count(Verdict::Unknown) + count(Verdict::Runtime),
                ) {
                    (0, 0) if stability.skippable == Some(true) => (
                        format!("{}  skippable", ic::CHECK),
                        Color32::from_rgb(0x5F, 0xB8, 0x65),
                    ),
                    (0, 0) => (
                        format!("{}  stable parameters", ic::CHECK),
                        Color32::from_rgb(0x5F, 0xB8, 0x65),
                    ),
                    (0, n) => (format!("?  {n} unknown"), crate::theme::t().weak),
                    (n, _) => (format!("{}  {n} unstable", ic::WARNING), WARN),
                };
                badges.push(Badge {
                    line,
                    text,
                    color,
                    stability,
                    guessed,
                });
            }
        }
        let mut swatches = Vec::new();
        for (line, text) in t.text.lines().enumerate() {
            let mut colors = crate::resources::literal_colors(text);
            for pat in ["R.color.", "@color/"] {
                for (i, _) in text.match_indices(pat) {
                    let name: String = text[i + pat.len()..]
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    colors.extend(self.res.color(&name));
                }
            }
            if let Some(c) = colors.first() {
                swatches.push((line, *c));
            }
        }
        if let Some(t) = self.tabs.get_mut(self.active) {
            t.annotations = Some(Annotations {
                key,
                marks,
                badges,
                swatches,
            });
        }
    }

    pub(super) fn handle(&mut self, r: Request, ctx: &egui::Context) {
        match r {
            Request::Pick(_, hits) if hits.len() == 1 => {
                self.open_file(&hits[0].path, Some(hits[0].offset))
            }
            Request::Pick(title, hits) => self.pick(title, hits),
            Request::Run(a) => self.run(a, ctx),
        }
    }

    /// A popup to choose one of several targets (implementations, declarations).
    pub(super) fn pick(&mut self, title: String, hits: Vec<Hit>) {
        self.search = Some(Search {
            kind: SearchKind::Pick,
            query: String::new(),
            hits: Vec::new(),
            selected: 0,
            stale: true,
            pinned: hits,
            title: Some(title),
        });
    }

    /// "Go to implementations" for the name under the cursor: a type's subclasses, or the
    /// overriding members of an interface / abstract member.
    pub(super) fn goto_implementations(&mut self) {
        let Some((word, _)) = self.word_at_cursor() else {
            return;
        };
        let mut hits = self.index.implementations(&word);
        let containers: Vec<String> = self
            .index
            .named(&word)
            .filter_map(|(_, _, d)| d.container.clone())
            .collect();
        for c in containers {
            hits.extend(self.index.member_implementations(&c, &word));
        }
        match hits.len() {
            0 => self.status = format!("No implementations of `{word}` in the project"),
            1 => self.open_file(&hits[0].path.clone(), Some(hits[0].offset)),
            n => self.pick(format!("{n} implementations of `{word}`"), hits),
        }
    }
}

fn hit_of(p: &std::path::Path, f: &crate::kotlin::FileSyms, d: &crate::kotlin::Decl) -> Hit {
    Hit {
        name: d.name.clone(),
        detail: crate::index::describe(f, d),
        path: p.to_path_buf(),
        offset: d.offset,
        score: 0,
    }
}

/// Paints the icons of the visible rows into `column`; returns what a click asks for.
pub(super) fn paint_marks(
    ui: &mut Ui,
    a: &Annotations,
    column: Rect,
    galley: &Galley,
    galley_pos: Pos2,
) -> Option<Request> {
    let clip = ui.clip_rect();
    let mut out = None;
    for (line, color) in &a.swatches {
        let Some(row) = galley.rows.get(*line) else {
            continue;
        };
        let y = galley_pos.y + row.pos.y;
        if y > clip.bottom()
            || y + row.rect().height() < clip.top()
            || a.marks.iter().any(|m| m.line == *line)
        {
            continue;
        }
        let h = row.rect().height();
        let r = Rect::from_center_size(
            Pos2::new(column.left() + ICON_WIDTH * 0.5, y + h * 0.5),
            egui::vec2(10.0, 10.0),
        );
        ui.painter().rect_filled(r, 2.0, *color);
        ui.painter().rect_stroke(
            r,
            2.0,
            egui::Stroke::new(1.0, crate::theme::t().border),
            egui::StrokeKind::Outside,
        );
    }
    for m in &a.marks {
        let Some(row) = galley.rows.get(m.line) else {
            continue;
        };
        let y = galley_pos.y + row.pos.y;
        if y > clip.bottom() || y + row.rect().height() < clip.top() {
            continue;
        }
        // a class can both implement (up) and be implemented (down): then the down icon sits a bit right
        let shared = !m.up && a.marks.iter().any(|o| o.line == m.line && o.up);
        let pos = Pos2::new(
            column.left() + 1.0 + if shared { ICON_WIDTH * 0.45 } else { 0.0 },
            y,
        );
        let (icon, color) = if m.up {
            (ic::ARROW_CIRCLE_UP, UP)
        } else {
            (ic::ARROW_CIRCLE_DOWN, DOWN)
        };
        let rect = ui.painter().text(
            pos,
            Align2::LEFT_TOP,
            icon,
            FontId::proportional(crate::theme::font_size()),
            color,
        );
        let r = ui.interact(
            rect.expand(2.0),
            ui.scope_id().with(("mark", m.line, m.up)),
            Sense::click(),
        );
        if r.clicked() {
            out = Some(Request::Pick(m.tip.clone(), m.targets.clone()));
        }
        r.on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_ui(|ui| {
                ui.label(RichText::new(&m.tip).strong());
                for t in m.targets.iter().take(12) {
                    ui.label(RichText::new(&t.name).monospace().small());
                }
                if m.targets.len() > 12 {
                    ui.label(
                        RichText::new(format!("… and {} more", m.targets.len() - 12))
                            .weak()
                            .small(),
                    );
                }
                ui.label(RichText::new("Click to go there").weak().small());
            });
    }
    out
}

/// Paints the stability badges after their declaration lines. Returns the right edge of a badge
/// on `line`, so inline blame can start after it.
pub(super) fn paint_badges(
    ui: &mut Ui,
    a: &Annotations,
    galley: &Galley,
    galley_pos: Pos2,
    line: usize,
) -> Option<f32> {
    let clip = ui.clip_rect();
    let mut right_on_line = None;
    for b in &a.badges {
        let Some(row) = galley.rows.get(b.line) else {
            continue;
        };
        let y = galley_pos.y + row.pos.y;
        if y > clip.bottom() || y + row.rect().height() < clip.top() {
            continue;
        }
        let pos = galley_pos + row.rect().right_top().to_vec2() + egui::vec2(24.0, 0.0);
        let font = FontId::proportional(crate::theme::font_size() - 1.0);
        let galley_b = ui.painter().layout_no_wrap(b.text.clone(), font, b.color);
        let rect = Rect::from_min_size(pos, galley_b.size()).expand2(egui::vec2(6.0, 1.0));
        ui.painter()
            .rect_filled(rect, 6.0, b.color.gamma_multiply(0.14));
        ui.painter().galley(pos, galley_b, b.color);
        if b.line == line {
            right_on_line = Some(rect.right());
        }
        let r = ui.interact(rect, ui.scope_id().with(("badge", b.line)), Sense::hover());
        r.on_hover_ui(|ui| stability_tooltip(ui, &b.stability, b.guessed));
    }
    right_on_line
}

fn stability_tooltip(ui: &mut Ui, st: &Stability, guessed: bool) {
    let yes_no = |b: Option<bool>| b.map_or("?", |b| if b { "yes" } else { "no" });
    ui.label(
        RichText::new(format!(
            "{}   skippable: {}   restartable: {}",
            st.name,
            yes_no(st.skippable),
            yes_no(st.restartable)
        ))
        .strong(),
    );
    egui::Grid::new("stability-tip")
        .num_columns(3)
        .spacing([12.0, 2.0])
        .show(ui, |ui| {
            for p in &st.params {
                let (txt, color) = match p.verdict {
                    Verdict::Stable => ("STABLE", Color32::from_rgb(0x5F, 0xB8, 0x65)),
                    Verdict::Unstable => ("UNSTABLE", ERROR),
                    Verdict::Runtime => ("RUNTIME", WARN),
                    Verdict::Unknown => ("UNKNOWN", crate::theme::t().weak),
                };
                ui.label(RichText::new(&p.name).monospace());
                ui.label(RichText::new(txt).color(color).strong());
                ui.label(RichText::new(&p.reason).small());
                ui.end_row();
            }
        });
    let source = if guessed {
        "local guess: run the stabilityDump task (skydoves plugin) or build with Compose compiler reports for exact results".to_string()
    } else {
        format!("from {}", st.source)
    };
    ui.label(RichText::new(source).weak().small());
}

/// The right-click menu of the editor. Returns the chosen command.
pub(super) fn context_menu(
    ui: &mut Ui,
    km: keymap::Keymap,
    has_selection: bool,
) -> Option<Request> {
    let mut out = None;
    let mut item = |ui: &mut Ui, a: Action| {
        let text = keymap::text(km, a);
        if ui
            .add(egui::Button::new(a.label()).shortcut_text(text))
            .clicked()
        {
            out = Some(Request::Run(a));
            ui.close();
        }
    };
    item(ui, Action::GotoDefinition);
    item(ui, Action::Implementations);
    item(ui, Action::References);
    ui.separator();
    item(ui, Action::Rename);
    if has_selection {
        item(ui, Action::ExtractVariable);
    }
    item(ui, Action::ToggleComment);
    item(ui, Action::DuplicateLine);
    ui.separator();
    item(ui, Action::OptimizeImports);
    item(ui, Action::LocalHistory);
    item(ui, Action::LocateFile);
    out
}
