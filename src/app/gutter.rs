//! Editor annotations: a row of small links above each declaration (implementations, overrides,
//! expect / actual, ▶ run test or preview, manifest entry, Hilt providers and injection points)
//! in place of gutter icons, inline Compose stability badges and colour swatches after their
//! lines, and the right-click menu.

use super::{App, Search, SearchKind};
use crate::android_kit::TestTarget;
use crate::compose::{self, Stability, Verdict};
use crate::highlight::{ERROR, WARN};
use crate::icons as ic;
use crate::index::Hit;
use crate::keymap::{self, Action};
use crate::kotlin::Kind;
use egui::{Align2, Color32, FontId, Galley, Pos2, Rect, RichText, Sense, Ui};
use std::collections::{BTreeMap, HashMap, HashSet};

/// A link to related code: implementations, overrides, Kotlin Multiplatform expect / actual.
struct Mark {
    line: usize,
    /// Icon and a count or name, as the link shows it.
    short: String,
    tip: String,
    targets: Vec<Hit>,
}

/// Inline stability after a `@Composable` declaration.
pub(super) struct Badge {
    pub(super) line: usize,
    pub(super) text: String,
    color: Color32,
    pub(super) stability: Stability,
    guessed: bool,
    /// One hint per parameter, like the stability analyzer plugin's inline parameter hints.
    pub(super) params: Vec<ParamHint>,
}

/// Where a parameter's stability shows: a chip at the end of its line when it has the line to
/// itself, else a coloured underline under its name. `param` indexes `Badge::stability.params`.
pub(super) struct ParamHint {
    pub(super) line: usize,
    /// Char range of the name in the text (the galley counts chars).
    pub(super) chars: std::ops::Range<usize>,
    pub(super) own_line: bool,
    pub(super) param: usize,
}

/// Annotations of one tab, rebuilt only when its text, the index or the reports change.
pub(super) struct Annotations {
    key: (u64, u64, u64, u64),
    pub(super) badges: Vec<Badge>,
    /// Colour squares for lines that write a colour (`Color(0xFF…)`, `#RRGGBB`, `R.color.x`).
    pub(super) swatches: Vec<(usize, Color32)>,
    /// The link rows, by the line they sit above.
    pub(super) lenses: Vec<Lens>,
}

/// Where the open file's declarations are used, counted in the background.
pub(super) struct DeclUsages {
    /// File, text version and index generation it was counted for.
    key: (std::path::PathBuf, u64, u64),
    /// By container and name, so the counts stay on screen while a newer count runs; shared
    /// with the worker's result, not copied.
    pub(super) by_decl: std::sync::Arc<super::xref::Usages>,
}

/// Whether a declaration gets a usages link: not overrides (their callers go through the
/// base), previews or tests (nothing calls them).
fn wants_usages(d: &crate::kotlin::Decl) -> bool {
    (matches!(d.kind, Kind::Fun | Kind::Property) || d.kind.is_type() && d.name != "Companion")
        && !d.has_modifier("override")
        && !d.is_preview()
        && !d.has_annotation("Test")
}

fn usage_key(d: &crate::kotlin::Decl) -> (Option<String>, String) {
    (d.container.as_deref().map(String::from), d.name.clone())
}

/// One link above a line.
pub(super) struct LensItem {
    /// Icon and a count or name, as drawn.
    pub(super) text: String,
    /// The full words, shown on hover and read by screen readers.
    pub(super) tip: String,
    request: Request,
}

/// The links above one line: the first line of a declaration, its annotations included.
pub(super) struct Lens {
    pub(super) line: usize,
    /// Leading whitespace of that line, in chars: the links start where its code starts.
    indent: usize,
    pub(super) items: Vec<LensItem>,
}

impl Lens {
    pub(super) fn new(line: usize, line_text: &str, items: Vec<(String, String, Request)>) -> Lens {
        Lens {
            line,
            indent: line_text.chars().take_while(|c| c.is_whitespace()).count(),
            items: items
                .into_iter()
                .map(|(text, tip, request)| LensItem { text, tip, request })
                .collect(),
        }
    }
}

/// The first line of the declaration whose name is on `line`: annotation lines above it count.
fn head_line(lines: &[&str], line: usize) -> usize {
    let mut l = line.min(lines.len().saturating_sub(1));
    while l > 0 && lines[l - 1].trim_start().starts_with('@') {
        l -= 1;
    }
    l
}

/// Height of a link row: a bit less than a code line.
pub(super) fn lens_height(row_h: f32) -> f32 {
    (row_h * 0.85).round()
}

/// The same galley with `h` of room above line `l` for each `(l, h)` in `room` (sorted by line;
/// one row per line: the editor never wraps). A line past the last row adds room at the end.
/// Painting, clicks and the cursor all follow the moved rows.
pub(super) fn with_room(g: &Galley, room: &[(usize, f32)]) -> Galley {
    let mut g = g.clone();
    let mut next = room.iter().peekable();
    let mut dy = 0.0;
    for (i, row) in g.rows.iter_mut().enumerate() {
        while let Some((_, h)) = next.next_if(|(l, _)| *l <= i) {
            dy += h;
        }
        row.pos.y += dy;
    }
    dy += next.map(|(_, h)| h).sum::<f32>();
    g.rect.max.y += dy;
    g.mesh_bounds.max.y += dy;
    g
}

/// What a click on a link or in the context menu asks for, run after the editor is drawn.
#[derive(Clone)]
pub(super) enum Request {
    Pick(String, Vec<Hit>),
    Run(Action),
    /// Show this `@Preview` on the device.
    Preview(String),
    /// Plugin test cases on one line (indexes into `Tests::marks`).
    Tests(Vec<usize>),
    /// A JUnit class or test function, run with Gradle.
    JUnit(TestTarget),
}

impl App {
    /// Builds the active tab's annotations when they are stale.
    pub(super) fn refresh_annotations(&mut self) {
        self.count_decl_usages();
        let Some(t) = self.tabs.get(self.active) else {
            return;
        };
        let key = (
            t.rev,
            self.index.read().generation,
            self.outputs_gen,
            self.decl_usages_gen,
        );
        if t.annotations.as_ref().is_some_and(|a| a.key == key) {
            return;
        }
        // line and char index of a byte offset, by binary search instead of a scan from the top
        let newlines: Vec<usize> = t.text.match_indices('\n').map(|(i, _)| i).collect();
        let line_of = |o: usize| newlines.partition_point(|&n| n < o);
        let ascii = t.text.is_ascii();
        let char_of = |o: usize| {
            if ascii {
                o
            } else {
                t.text[..o].chars().count()
            }
        };
        let mut marks = Vec::new();
        let mut badges = Vec::new();
        for d in &t.syms.decls {
            let line = line_of(d.offset);
            let kmp = self.index.read().counterparts(&t.path, &t.syms, d);
            if !kmp.is_empty() {
                let expect = crate::index::kmp_role(&t.syms, d) == Some("expect");
                let mut sets: Vec<&str> = kmp
                    .iter()
                    .filter_map(|h| h.detail.split(" · ").next())
                    .collect();
                sets.dedup();
                marks.push(Mark {
                    line,
                    short: format!("{}  {}", ic::DEVICES, sets.join(", ")),
                    tip: format!(
                        "{} in {}",
                        if expect { "Actual" } else { "Expected" },
                        sets.join(", ")
                    ),
                    targets: kmp,
                });
            }
            if d.kind.is_type() {
                let supers: Vec<Hit> = d
                    .supers
                    .iter()
                    .flat_map(|s| {
                        self.index
                            .read()
                            .named(s)
                            .filter(|(_, _, x)| x.kind.is_type())
                            .map(|(p, f, x)| hit_of(p, f, x))
                            .collect::<Vec<_>>()
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
                        short: format!("{}  {}", ic::ARROW_UP, d.supers.join(", ")),
                        tip: format!("{verb} {}", d.supers.join(", ")),
                        targets: supers,
                    });
                }
                let impls = self.index.read().implementations(&d.name);
                if !impls.is_empty() {
                    let verb = if d.kind == Kind::Interface {
                        "Implemented in"
                    } else {
                        "Subclassed by"
                    };
                    // a `fun interface` is also implemented by `Name { … }` lambdas
                    let what = match (d.has_modifier("fun"), impls.len()) {
                        (true, 1) => "place",
                        (true, _) => "places",
                        (false, 1) => "class",
                        (false, _) => "classes",
                    };
                    marks.push(Mark {
                        line,
                        short: format!("{}  {}", ic::ARROW_DOWN, impls.len()),
                        tip: format!("{verb} {} {what}", impls.len()),
                        targets: impls,
                    });
                }
            } else if let Some(c) = &d.container
                && matches!(d.kind, Kind::Fun | Kind::Property)
            {
                if d.has_modifier("override") {
                    if let Some(h) = self.index.read().overridden(c, &d.name) {
                        marks.push(Mark {
                            line,
                            short: format!("{}  {}", ic::ARROW_UP, h.name),
                            tip: format!("Overrides {}", h.name),
                            targets: vec![h],
                        });
                    }
                } else {
                    let impls = self.index.read().member_implementations(c, &d.name);
                    if !impls.is_empty() {
                        marks.push(Mark {
                            line,
                            short: format!("{}  {}", ic::ARROW_DOWN, impls.len()),
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
                    None => (compose::guess(d, &self.index.read()), true),
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
                let spans = compose::param_names(&t.text, d.offset);
                let lines: Vec<usize> = spans.iter().map(|(_, r)| line_of(r.start)).collect();
                let params = spans
                    .iter()
                    .zip(&lines)
                    .filter_map(|((name, r), &l)| {
                        Some(ParamHint {
                            line: l,
                            chars: char_of(r.start)..char_of(r.end),
                            own_line: l != line && lines.iter().filter(|&&x| x == l).count() == 1,
                            param: stability.params.iter().position(|p| &p.name == name)?,
                        })
                    })
                    .collect();
                badges.push(Badge {
                    line,
                    text,
                    color,
                    stability,
                    guessed,
                    params,
                });
            }
        }
        let previews: Vec<(usize, String)> = t
            .syms
            .decls
            .iter()
            .filter(|d| d.is_preview())
            .map(|d| {
                let name = crate::device::preview_name(
                    &t.syms.package,
                    &t.path,
                    d.container.as_deref(),
                    &d.name,
                );
                (line_of(d.offset), name)
            })
            .collect();
        // the links above each declaration, in this order: run, manifest, navigation, Hilt
        let lines: Vec<&str> = t.text.split('\n').collect();
        let mut rows: BTreeMap<usize, Vec<(String, String, Request)>> = BTreeMap::new();
        let mut add = |line: usize, text: String, tip: String, request: Request| {
            rows.entry(head_line(&lines, line))
                .or_default()
                .push((text, tip, request));
        };
        // usages first, like the code vision of Android Studio; the bare icon holds the row
        // until the count arrives, so the code does not jump down then
        let counted = self.decl_usages.as_ref().filter(|u| u.key.0 == t.path);
        let mut counted_lines = HashSet::new();
        for d in t.syms.decls.iter().filter(|d| wants_usages(d)) {
            // one count per line: a class, not also each `val` of its constructor
            if !counted_lines.insert(line_of(d.offset)) {
                continue;
            }
            let (text, tip, hits) = match counted.and_then(|u| u.by_decl.get(&usage_key(d))) {
                Some(hits) => (
                    format!("{}  {}", ic::LINK_SIMPLE, hits.len()),
                    match hits.len() {
                        0 => "No usages".to_string(),
                        1 => "1 usage".to_string(),
                        n => format!("{n} usages"),
                    },
                    hits.clone(),
                ),
                None => (ic::LINK_SIMPLE.into(), "Counting usages".into(), Vec::new()),
            };
            add(
                line_of(d.offset),
                text,
                tip,
                Request::Pick(format!("Usages of {}", d.name), hits),
            );
        }
        for (o, x) in crate::android_kit::test_targets(&t.path, &t.syms) {
            let what = match &x.method {
                Some(_) => "Run test",
                None => "Run all tests",
            };
            let on = if x.device { " on the device" } else { "" };
            let short = if x.method.is_some() { "test" } else { "tests" };
            add(
                line_of(o),
                format!("{}  {short}", ic::PLAY),
                format!("{what}{on}"),
                Request::JUnit(x),
            );
        }
        for (line, name) in &previews {
            add(
                *line,
                format!("{}  preview", ic::PLAY),
                "Run preview on the device".into(),
                Request::Preview(name.clone()),
            );
        }
        for d in t
            .syms
            .decls
            .iter()
            .filter(|d| d.kind.is_type() && d.container.is_none())
        {
            if let Some((p, c)) = self.res.component(&crate::index::qualified(&t.syms, d)) {
                let text = if c.launcher {
                    "Launcher activity".to_string()
                } else {
                    format!("In AndroidManifest ({})", c.tag)
                };
                let hit = Hit {
                    name: "AndroidManifest.xml".into(),
                    detail: String::new(),
                    path: p.clone(),
                    offset: c.offset,
                    score: 0,
                };
                let short = if c.launcher { "launcher" } else { c.tag };
                add(
                    line_of(d.offset),
                    format!("{}  {short}", ic::ANDROID_LOGO),
                    text,
                    Request::Pick(String::new(), vec![hit]),
                );
            }
        }
        for m in &marks {
            add(
                m.line,
                m.short.clone(),
                m.tip.clone(),
                Request::Pick(m.tip.clone(), m.targets.clone()),
            );
        }
        for (line, short, tip, r) in self.hilt_links(&t.path, t.rev, &line_of) {
            add(line, short, tip, r);
        }
        let lenses = rows
            .into_iter()
            .map(|(line, items)| Lens::new(line, lines[line], items))
            .collect();
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
                badges,
                swatches,
                lenses,
            });
        }
    }

    /// Takes the worker's finished pass of the active file, and asks for a new one when the
    /// file's text or the index changed. The worker cancels a pass that a newer one replaces.
    fn count_decl_usages(&mut self) {
        if self.xref.poll()
            && let Some(f) = &self.xref.file
        {
            self.decl_usages = Some(DeclUsages {
                key: (f.path.clone(), f.rev, 0),
                by_decl: f.usages.clone(),
            });
            self.decl_usages_gen += 1;
        }
        let (Some(t), Some(p)) = (self.tabs.get(self.active), &self.project) else {
            return;
        };
        let generation = self.index.read().generation;
        if t.readonly
            || !crate::project::is_code(&t.path)
            || self.xref.asked_for(&t.path, t.rev, generation)
        {
            return;
        }
        let targets: Vec<((Option<String>, String), usize)> = t
            .syms
            .decls
            .iter()
            .filter(|d| wants_usages(d))
            .map(|d| (usage_key(d), d.offset))
            .collect();
        // the file list is worked out on the worker; open texts are copied only when changed
        let (path, rev, text, root) = (t.path.clone(), t.rev, t.text.clone(), p.root.clone());
        let open = self.open_texts(false);
        self.xref
            .want_file(&path, rev, generation, &text, open, root, targets);
    }

    /// The open tabs' texts as the worker reads them (a deleted file's only `with_deleted`),
    /// shared with the last copy while a tab's text is the same.
    pub(super) fn open_texts(
        &mut self,
        with_deleted: bool,
    ) -> HashMap<std::path::PathBuf, std::sync::Arc<str>> {
        let mut out = HashMap::new();
        for t in self.tabs.iter().filter(|t| with_deleted || !t.deleted) {
            let shared = match self.open_shared.get(&t.path) {
                Some(s) if **s == *t.text => s.clone(),
                _ => std::sync::Arc::from(t.text.as_str()),
            };
            out.insert(t.path.clone(), shared);
        }
        self.open_shared = out.clone();
        out
    }

    pub(super) fn handle(&mut self, r: Request, ctx: &egui::Context) {
        match r {
            Request::Pick(_, hits) if hits.len() == 1 => {
                self.open_file(&hits[0].path, Some(hits[0].offset))
            }
            Request::Pick(title, hits) => self.pick(title, hits),
            Request::Run(a) => self.run(a, ctx),
            Request::Preview(name) => self.run_on_device(crate::device::Launch::Preview(name), ctx),
            Request::Tests(marks) => self.gutter_tests(marks, ctx),
            Request::JUnit(t) => self.run_junit(&t, ctx),
        }
    }

    /// Hilt / Dagger links of the active file from the worker's pass: "Injected in N places"
    /// on a binding (`@Provides` / `@Binds` / `@Inject constructor`), and on an `@Inject` class,
    /// field or `@Provides` function the bindings its dependencies get, matched by type and
    /// qualifier like Dagger does.
    fn hilt_links(
        &self,
        path: &std::path::Path,
        rev: u64,
        line_of: &dyn Fn(usize) -> usize,
    ) -> Vec<(usize, String, String, Request)> {
        let Some(f) = self.xref.for_file(path, rev) else {
            return Vec::new();
        };
        f.hilt
            .iter()
            .filter(|l| !l.hits.is_empty() || l.injected)
            .map(|l| {
                let short = if l.injected {
                    format!("{}  {}", ic::SYRINGE, l.hits.len())
                } else {
                    match &l.hits[..] {
                        [one] => format!("{}  {}", ic::PLUGS_CONNECTED, one.name),
                        hits => format!("{}  {}", ic::PLUGS_CONNECTED, hits.len()),
                    }
                };
                (
                    line_of(l.at),
                    short,
                    l.title.clone(),
                    Request::Pick(l.title.clone(), l.hits.clone()),
                )
            })
            .collect()
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
        let mut hits = self.index.read().implementations(&word);
        let containers: Vec<String> = self
            .index
            .read()
            .named(&word)
            .filter_map(|(_, _, d)| d.container.as_deref().map(String::from))
            .collect();
        for c in containers {
            hits.extend(self.index.read().member_implementations(&c, &word));
        }
        // Kotlin Multiplatform: from an expect to its actuals, from an actual to its expect
        if let Some(t) = self.tabs.get(self.active) {
            for d in t.syms.decls.iter().filter(|d| *d.name == *word) {
                hits.extend(self.index.read().counterparts(&t.path, &t.syms, d));
            }
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
    // the row of `line` when it is on screen
    let visible = |line: usize| {
        galley.rows.get(line).filter(|row| {
            let y = galley_pos.y + row.pos.y;
            y <= clip.bottom() && y + row.rect().height() >= clip.top()
        })
    };
    for b in &a.badges {
        if let Some(row) = visible(b.line) {
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
        for (i, h) in b.params.iter().enumerate() {
            let Some(row) = visible(h.line) else {
                continue;
            };
            let p = &b.stability.params[h.param];
            let (word, color) = verdict_look(p.verdict);
            let rect = if h.own_line {
                let pos = galley_pos + row.rect().right_top().to_vec2() + egui::vec2(16.0, 0.0);
                let font = FontId::proportional(crate::theme::font_size() - 2.0);
                let g = ui
                    .painter()
                    .layout_no_wrap(word.to_lowercase(), font, color);
                let rect = Rect::from_min_size(pos, g.size()).expand2(egui::vec2(5.0, 1.0));
                ui.painter()
                    .rect_filled(rect, 5.0, color.gamma_multiply(0.14));
                ui.painter().galley(pos, g, color);
                if h.line == line {
                    right_on_line = Some(rect.right().max(right_on_line.unwrap_or(0.0)));
                }
                rect
            } else {
                let at = |c: usize| {
                    galley
                        .pos_from_cursor(egui::text::CCursor::new(c))
                        .translate(galley_pos.to_vec2())
                };
                let (a, z) = (at(h.chars.start), at(h.chars.end));
                ui.painter().hline(
                    a.left()..=z.left(),
                    a.bottom() - 1.0,
                    egui::Stroke::new(1.5, color),
                );
                Rect::from_min_max(a.left_top(), z.left_bottom())
            };
            ui.interact(
                rect,
                ui.scope_id().with(("param", b.line, i)),
                Sense::hover(),
            )
            .on_hover_ui(|ui| {
                ui.label(
                    RichText::new(format!("{}: {word}", p.name))
                        .strong()
                        .color(color),
                );
                if !p.reason.is_empty() {
                    ui.label(&p.reason);
                }
                if b.guessed {
                    ui.label(RichText::new("local guess").weak().small());
                }
            });
        }
    }
    right_on_line
}

fn verdict_look(v: Verdict) -> (&'static str, Color32) {
    match v {
        Verdict::Stable => ("STABLE", Color32::from_rgb(0x5F, 0xB8, 0x65)),
        Verdict::Unstable => ("UNSTABLE", ERROR),
        Verdict::Runtime => ("RUNTIME", WARN),
        Verdict::Unknown => ("UNKNOWN", crate::theme::t().weak),
    }
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
                let (txt, color) = verdict_look(p.verdict);
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
    item(ui, Action::QuickFix);
    ui.separator();
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

/// Paints the link rows above their lines (`lenses` may hold two rows for one line: they continue
/// each other). Returns what a click asks for.
pub(super) fn paint_lenses<'a>(
    ui: &mut Ui,
    lenses: impl Iterator<Item = &'a Lens>,
    galley: &Galley,
    galley_pos: Pos2,
    char_w: f32,
    lens_h: f32,
) -> Option<Request> {
    let clip = ui.clip_rect();
    let font = FontId::proportional(crate::theme::font_size() - 2.0);
    let t = crate::theme::t();
    let mut ends: HashMap<usize, f32> = HashMap::new();
    let mut out = None;
    for lens in lenses {
        let Some(row) = galley.rows.get(lens.line) else {
            continue;
        };
        let top = galley_pos.y + row.pos.y - lens_h;
        if top > clip.bottom() || top + lens_h < clip.top() {
            continue;
        }
        let mut x = ends
            .get(&lens.line)
            .copied()
            .unwrap_or(galley_pos.x + lens.indent as f32 * char_w);
        let mid = top + lens_h * 0.5;
        let mut first = !ends.contains_key(&lens.line);
        for (i, item) in lens.items.iter().enumerate() {
            if !std::mem::take(&mut first) {
                let r = ui.painter().text(
                    Pos2::new(x, mid),
                    Align2::LEFT_CENTER,
                    "  ·  ",
                    font.clone(),
                    t.weak,
                );
                x = r.right();
            }
            let g = ui
                .painter()
                .layout_no_wrap(item.text.clone(), font.clone(), t.weak);
            let rect = Rect::from_min_size(Pos2::new(x, mid - g.size().y * 0.5), g.size());
            let r = ui.interact(
                rect,
                ui.scope_id().with(("lens", lens.line, i, &item.tip)),
                Sense::click(),
            );
            let color = if r.hovered() { t.accent } else { t.weak };
            ui.painter()
                .galley_with_override_text_color(rect.min, g, color);
            if r.hovered() {
                ui.painter()
                    .hline(rect.x_range(), rect.bottom(), egui::Stroke::new(1.0, color));
            }
            r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Link, true, &item.tip));
            if r.on_hover_text(&item.tip)
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                out = Some(item.request.clone());
            }
            x = rect.right();
        }
        ends.insert(lens.line, x);
    }
    out
}

/// A small colour square after each line that writes a colour. Returns its right edge on `line`.
pub(super) fn paint_swatches(
    ui: &mut Ui,
    a: &Annotations,
    galley: &Galley,
    galley_pos: Pos2,
    line_at: usize,
) -> Option<f32> {
    let clip = ui.clip_rect();
    let mut right = None;
    for (line, color) in &a.swatches {
        let Some(row) = galley.rows.get(*line) else {
            continue;
        };
        let rect = row.rect().translate(galley_pos.to_vec2());
        if rect.top() > clip.bottom() || rect.bottom() < clip.top() {
            continue;
        }
        let r = Rect::from_center_size(
            Pos2::new(rect.right() + 14.0, rect.center().y),
            egui::vec2(10.0, 10.0),
        );
        ui.painter().rect_filled(r, 2.0, *color);
        ui.painter().rect_stroke(
            r,
            2.0,
            egui::Stroke::new(1.0, crate::theme::t().border),
            egui::StrokeKind::Outside,
        );
        if *line == line_at {
            right = Some(r.right());
        }
    }
    right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_rows_make_room_above_their_lines() {
        let lines = [
            "package a",
            "",
            "@Preview",
            "@Composable",
            "fun P() {}",
            "  @Inject lateinit var x: X",
        ];
        assert_eq!(head_line(&lines, 4), 2, "above the annotations");
        assert_eq!(head_line(&lines, 1), 1);
        assert_eq!(Lens::new(5, lines[5], Vec::new()).indent, 2);
        let ctx = egui::Context::default();
        let mut g = None;
        ctx.run_ui(Default::default(), |ui| {
            g = Some(ui.fonts_mut(|f| {
                f.layout_no_wrap("a\nb\nc\nd".into(), FontId::monospace(13.0), Color32::WHITE)
            }));
        })
        .textures_delta
        .clear(); // no renderer here to take the font texture
        let g = g.unwrap();
        let moved = with_room(&g, &[(0, 10.0), (2, 10.0), (9, 5.0)]);
        let ys: Vec<f32> = moved
            .rows
            .iter()
            .zip(&g.rows)
            .map(|(m, o)| m.pos.y - o.pos.y)
            .collect();
        assert_eq!(ys, [10.0, 10.0, 20.0, 20.0]);
        assert_eq!(
            moved.rect.height(),
            g.rect.height() + 25.0,
            "room after the end"
        );
        // a click between rows lands on the nearest one
        let row1 = moved.rows[2].pos.y;
        assert_eq!(
            moved.cursor_from_pos(egui::vec2(0.0, row1 - 4.0)).index.0,
            4
        );
    }
}
