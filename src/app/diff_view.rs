//! The Diff checkbox of an editor tab: the tab splits into the older version (left, read-only)
//! and the editor (right, still editable). Added lines are green, removed red, changed blue with
//! the changed words marked; blank spacer rows keep matching lines side by side.
//!
//! The lines are paired by `git diff` and the side-by-side pairing of the docked diff panel
//! ([`git_ui::side_rows`]), off the UI thread, after a short typing pause. A frame only paints
//! the rows on screen.

use super::{App, git_ui};
use crate::git;
use crate::highlight::{self, ERROR};
use egui::{Color32, FontId, Galley, Pos2, Rect, RichText, Shape, Ui};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Wait this long after the last keystroke before comparing again.
const DEBOUNCE: Duration = Duration::from_millis(150);
/// Files longer than this (either version) show a note instead of the diff.
pub(super) const MAX_LINES: usize = 20_000;
/// Files bigger than this (either version, in bytes) show a note instead of the diff.
pub(super) const MAX_BYTES: usize = 2 * 1024 * 1024;
/// How many older versions stay cached (by file and commit).
const OLD_CACHE: usize = 16;
/// Word highlights are matched exactly only while (old words × new words) stays under this.
const MAX_WORD_PAIRS: usize = 10_000;

const ADDED: Color32 = Color32::from_rgb(0x5F, 0xB8, 0x65);
const CHANGED: Color32 = Color32::from_rgb(0x58, 0x9D, 0xF6);
const SPACER: Color32 = Color32::from_gray(110);

/// What the older side is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Against {
    /// The file at git HEAD (an untracked file: every line is added).
    #[default]
    Head,
    /// The file where this branch left its PR base (the Current changes tab's base).
    Base,
}

impl Against {
    pub fn label(self) -> &'static str {
        match self {
            Against::Head => "Last commit",
            Against::Base => "PR base",
        }
    }

    fn tip(self) -> &'static str {
        match self {
            Against::Head => "Compare with the file at the last commit (HEAD)",
            Against::Base => "Compare with the file where this branch left its PR base",
        }
    }

    /// The session file's name for it.
    pub fn key(self) -> &'static str {
        match self {
            Against::Head => "head",
            Against::Base => "base",
        }
    }

    pub fn from_key(k: &str) -> Option<Against> {
        match k {
            "head" => Some(Against::Head),
            "base" => Some(Against::Base),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Same,
    Added,
    Removed,
    Changed,
}

/// One row of the two columns.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Row {
    /// 0-based line in the older version.
    pub old: Option<usize>,
    /// 0-based line in the editor text.
    pub new: Option<usize>,
    pub kind: Kind,
    /// The editor line this row sits at, or (a spacer row) the line it sits above.
    pub anchor: usize,
    /// 0 for a row with an editor line; for a spacer row, how many rows above `anchor` it is.
    pub back: usize,
    /// Changed words of a changed line, as char ranges of the old and the new line.
    pub old_words: Vec<Range<usize>>,
    pub new_words: Vec<Range<usize>>,
}

/// Both versions paired line by line.
#[derive(Debug, Default)]
pub(super) struct Aligned {
    pub rows: Vec<Row>,
    pub old_lines: Vec<String>,
    /// The row of each editor line.
    pub row_of_new: Vec<usize>,
    /// `(line, n)`: n spacer rows above editor line `line` (sorted; `line` may be the line count).
    pub room: Vec<(usize, usize)>,
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
    /// The older version's path when the file was renamed since (git's rename detection).
    pub renamed: Option<String>,
}

impl Aligned {
    pub fn summary(&self) -> String {
        let from = self
            .renamed
            .as_ref()
            .map_or(String::new(), |o| format!("Renamed from {o} · "));
        if self.added + self.removed + self.changed == 0 {
            return format!("{from}No changes");
        }
        format!(
            "{from}{} added · {} removed · {} changed",
            self.added, self.removed, self.changed
        )
    }
}

/// Pairs `old` and `new` through `git diff`, or says why not (a file too long or too big).
pub(super) fn compute(old: &str, new: &str) -> Result<Aligned, String> {
    let bytes = old.len().max(new.len());
    if bytes > MAX_BYTES {
        return Err(format!(
            "Diff is off for files over {} KB ({} KB)",
            MAX_BYTES / 1024,
            bytes / 1024
        ));
    }
    let lines = old.lines().count().max(new.lines().count());
    if lines > MAX_LINES {
        return Err(format!(
            "Diff is off for files over {MAX_LINES} lines ({lines} lines)"
        ));
    }
    Ok(align(old, new, &git::line_diff(old, new)))
}

/// Pairs the lines of `old` and `new` with `diff` (their `-U0` diff): unchanged lines between the
/// hunks are filled in, so every line of both versions has a row.
pub(super) fn align(old: &str, new: &str, diff: &[String]) -> Aligned {
    let old_lines: Vec<String> = old.lines().map(String::from).collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let (on, nn) = (old_lines.len(), new_lines.len());
    let mut rows: Vec<Row> = Vec::new();
    let (mut o, mut n) = (0usize, 0usize);
    let row = |old, new, kind| Row {
        old,
        new,
        kind,
        anchor: 0,
        back: 0,
        old_words: Vec::new(),
        new_words: Vec::new(),
    };
    for r in git_ui::side_rows(diff) {
        let git_ui::SideRow::Pair {
            old: po,
            new: pn,
            changed,
        } = r
        else {
            continue;
        };
        // side_rows numbers lines from 1
        let (po, pn) = (
            po.map(|(l, _)| l.saturating_sub(1)),
            pn.map(|(l, _)| l.saturating_sub(1)),
        );
        // the unchanged lines before this one
        while o < on && n < nn && (po.is_some_and(|p| o < p) || pn.is_some_and(|p| n < p)) {
            rows.push(row(Some(o), Some(n), Kind::Same));
            (o, n) = (o + 1, n + 1);
        }
        let po = po.filter(|_| o < on);
        let pn = pn.filter(|_| n < nn);
        let kind = match (po, pn) {
            _ if !changed => Kind::Same,
            (Some(_), Some(_)) => Kind::Changed,
            (Some(_), None) => Kind::Removed,
            (None, Some(_)) => Kind::Added,
            (None, None) => continue,
        };
        rows.push(row(po.map(|_| o), pn.map(|_| n), kind));
        o += usize::from(po.is_some());
        n += usize::from(pn.is_some());
    }
    while o < on || n < nn {
        let (a, b) = ((o < on).then_some(o), (n < nn).then_some(n));
        let kind = match (a, b) {
            (Some(_), Some(_)) => Kind::Same,
            (Some(_), None) => Kind::Removed,
            _ => Kind::Added,
        };
        rows.push(row(a, b, kind));
        o += usize::from(a.is_some());
        n += usize::from(b.is_some());
    }
    let mut a = Aligned {
        row_of_new: Vec::with_capacity(nn),
        ..Default::default()
    };
    // spacer rows: where each sits, counted back from the editor line below it
    let mut run = 0;
    for i in 0..rows.len() {
        match rows[i].new {
            Some(l) => {
                if run > 0 {
                    a.room.push((l, run));
                }
                for (k, r) in rows[i - run..i].iter_mut().enumerate() {
                    (r.anchor, r.back) = (l, run - k);
                }
                rows[i].anchor = l;
                a.row_of_new.push(i);
                run = 0;
            }
            None => run += 1,
        }
    }
    if run > 0 {
        a.room.push((nn, run));
        let end = rows.len();
        for (k, r) in rows[end - run..].iter_mut().enumerate() {
            (r.anchor, r.back) = (nn, run - k);
        }
    }
    for r in &mut rows {
        match r.kind {
            Kind::Added => a.added += 1,
            Kind::Removed => a.removed += 1,
            Kind::Changed => {
                a.changed += 1;
                if let (Some(x), Some(y)) = (r.old, r.new) {
                    (r.old_words, r.new_words) = word_diff(&old_lines[x], new_lines[y]);
                }
            }
            Kind::Same => {}
        }
    }
    a.rows = rows;
    a.old_lines = old_lines;
    a
}

/// A line cut into words: runs of letters, digits and `_`, runs of spaces, or one other char.
/// Each token is its char range and its text.
fn tokens(s: &str) -> Vec<(Range<usize>, &str)> {
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            0
        } else if c.is_whitespace() {
            1
        } else {
            2
        }
    };
    let mut v: Vec<(Range<usize>, &str)> = Vec::new();
    let mut last: Option<(u8, usize, usize)> = None; // class, start char, start byte
    for (ci, (bi, c)) in s.char_indices().enumerate() {
        let k = class(c);
        if let Some((lk, cs, bs)) = last
            && (lk != k || k == 2)
        {
            v.push((cs..ci, &s[bs..bi]));
            last = None;
        }
        if last.is_none() {
            last = Some((k, ci, bi));
        }
    }
    if let Some((_, cs, bs)) = last {
        v.push((cs..s.chars().count(), &s[bs..]));
    }
    v
}

/// The words that differ between two versions of a line, as char ranges of each. Words both
/// keep (the longest common run, found by LCS) are not marked.
pub(super) fn word_diff(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let (a, b) = (tokens(old), tokens(new));
    let mut p = 0;
    while p < a.len() && p < b.len() && a[p].1 == b[p].1 {
        p += 1;
    }
    let mut s = 0;
    while s < a.len() - p && s < b.len() - p && a[a.len() - 1 - s].1 == b[b.len() - 1 - s].1 {
        s += 1;
    }
    let (am, bm) = (&a[p..a.len() - s], &b[p..b.len() - s]);
    let (mut ka, mut kb) = (vec![false; am.len()], vec![false; bm.len()]);
    if am.len() * bm.len() <= MAX_WORD_PAIRS {
        // LCS table from the end, then walk it from the start
        let w = bm.len() + 1;
        let mut t = vec![0u16; (am.len() + 1) * w];
        for i in (0..am.len()).rev() {
            for j in (0..bm.len()).rev() {
                t[i * w + j] = if am[i].1 == bm[j].1 {
                    t[(i + 1) * w + j + 1] + 1
                } else {
                    t[(i + 1) * w + j].max(t[i * w + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < am.len() && j < bm.len() {
            if am[i].1 == bm[j].1 {
                (ka[i], kb[j]) = (true, true);
                (i, j) = (i + 1, j + 1);
            } else if t[(i + 1) * w + j] >= t[i * w + j + 1] {
                i += 1;
            } else {
                j += 1;
            }
        }
    }
    let marked = |toks: &[(Range<usize>, &str)], keep: &[bool]| {
        let mut v: Vec<Range<usize>> = Vec::new();
        for ((r, _), k) in toks.iter().zip(keep) {
            if *k {
                continue;
            }
            match v.last_mut() {
                Some(l) if l.end == r.start => l.end = r.end,
                _ => v.push(r.clone()),
            }
        }
        v
    };
    (marked(am, &ka), marked(bm, &kb))
}

/// Diff state of one tab.
#[derive(Default)]
pub(super) struct TabDiff {
    /// The Diff checkbox.
    pub on: bool,
    pub against: Against,
    pub view: Option<Arc<Aligned>>,
    /// Why there is no view (a long file, no PR base).
    pub note: Option<String>,
    rx: Option<Receiver<Result<Aligned, String>>>,
    /// What the view (or the running compare) is for: text rev, side, base and HEAD.
    for_key: Option<Key>,
    seen_rev: u64,
    seen_at: Option<Instant>,
    /// The editor's scroll offset last frame (a wheel over the left column scrolls it).
    pub scroll: egui::Vec2,
    /// Where the left column was drawn last frame.
    pub left: Option<Rect>,
}

type Key = (u64, Against, Option<String>, Option<String>);

impl TabDiff {
    /// Turns the view on or off; the next frame compares afresh.
    pub fn set(&mut self, on: bool, against: Against) {
        if (on, against) != (self.on, self.against) {
            (self.on, self.against) = (on, against);
            self.view = None;
            self.note = None;
            self.rx = None;
            self.for_key = None;
            self.left = None;
        }
    }
}

/// What a click in the Diff controls asks for.
pub(super) enum Ctl {
    Toggle,
    Against(Against),
}

/// The Diff checkbox and, while it is on, the Compare with choice and a summary. Laid out right
/// to left (the checkbox at the right end). `blocked`: why the checkbox is disabled.
pub(super) fn controls(
    ui: &mut Ui,
    d: &TabDiff,
    blocked: Option<&str>,
    shortcut: &str,
) -> Option<Ctl> {
    let mut ctl = None;
    let mut on = d.on;
    let r = ui.add_enabled(blocked.is_none(), egui::Checkbox::new(&mut on, "Diff"));
    let r = match blocked {
        Some(why) => r.on_disabled_hover_text(why),
        None => r.on_hover_text(format!(
            "Show the older version beside this file, changes highlighted  {shortcut}"
        )),
    };
    if r.changed() {
        ctl = Some(Ctl::Toggle);
    }
    if !d.on {
        return ctl;
    }
    for a in [Against::Base, Against::Head] {
        if ui
            .selectable_label(d.against == a, RichText::new(a.label()).small())
            .on_hover_text(a.tip())
            .clicked()
        {
            ctl = Some(Ctl::Against(a));
        }
    }
    let weak = crate::theme::t().weak;
    ui.label(RichText::new("Compare with").small().color(weak));
    ui.add_space(12.0);
    let summary = match (&d.note, &d.view) {
        (Some(n), _) => RichText::new(n).small().color(ERROR),
        (None, Some(v)) => RichText::new(v.summary()).small().color(weak),
        (None, None) => RichText::new("Comparing…").small().color(weak),
    };
    ui.label(summary);
    ctl
}

impl App {
    /// Why the active tab cannot show a diff (the checkbox is disabled), or `None`.
    pub(super) fn diff_blocked(&self) -> Option<&'static str> {
        let Some(t) = self.tabs.get(self.active) else {
            return Some("No file is open");
        };
        if t.readonly {
            return Some("A library source has no git history to compare with");
        }
        match self.git.changes.as_ref().map(|c| c.repo) {
            None => Some("Reading git status…"),
            Some(false) => Some("Not a git project: there is no older version to compare with"),
            Some(true) => None,
        }
    }

    /// The Diff checkbox, from the tab header or the "Toggle diff" action.
    pub(super) fn toggle_diff(&mut self) {
        if let Some(why) = self.diff_blocked() {
            self.status = why.into();
            return;
        }
        let t = &mut self.tabs[self.active];
        t.diff.set(!t.diff.on, t.diff.against);
        self.save_session(true);
    }

    /// A click in the Diff controls; the editor keeps the keyboard afterwards.
    pub(super) fn diff_ctl(&mut self, c: Ctl) {
        match c {
            Ctl::Toggle => self.toggle_diff(),
            Ctl::Against(a) => {
                if let Some(t) = self.tabs.get_mut(self.active) {
                    t.diff.set(t.diff.on, a);
                    self.save_session(true);
                }
            }
        }
        if let Some(t) = self.tabs.get_mut(self.active) {
            t.caret = Some(t.cursor);
        }
    }

    /// Compares the active tab again when its text, side, base or HEAD changed (after a typing
    /// pause), off the UI thread; takes a finished compare. A view of another base or HEAD is
    /// dropped at once: it never shows as the older version of the new one.
    pub(super) fn poll_diff(&mut self, ctx: &egui::Context) {
        let base = self.git.changes.as_ref().and_then(|c| c.base.clone());
        let root = self.project.as_ref().map(|p| p.root.clone());
        let head = self.prs.head.clone();
        let Some(t) = self.tabs.get_mut(self.active).filter(|t| t.diff.on) else {
            return;
        };
        let d = &mut t.diff;
        let base = (d.against == Against::Base).then_some(base).flatten();
        let key: Key = (t.rev, d.against, base.clone(), head);
        let same_base = |k: &Key| (k.1, &k.2, &k.3) == (key.1, &key.2, &key.3);
        if d.for_key.as_ref().is_some_and(|k| !same_base(k)) {
            (d.view, d.note, d.rx, d.for_key) = (None, None, None, None);
        }
        if let Some(rx) = &d.rx
            && let Ok(r) = rx.try_recv()
        {
            d.rx = None;
            match r {
                Ok(a) => (d.view, d.note) = (Some(Arc::new(a)), None),
                Err(e) => (d.view, d.note) = (None, Some(e)),
            }
        }
        if t.rev != d.seen_rev {
            d.seen_rev = t.rev;
            // the first compare starts now; after a keystroke, wait for a pause
            d.seen_at = Some(if d.for_key.is_none() {
                Instant::now() - DEBOUNCE
            } else {
                Instant::now()
            });
        }
        if d.for_key.as_ref() == Some(&key) || d.rx.is_some() {
            return;
        }
        let wait = DEBOUNCE.saturating_sub(d.seen_at.map_or(DEBOUNCE, |at| at.elapsed()));
        if !wait.is_zero() {
            ctx.request_repaint_after(wait);
            return;
        }
        if d.against == Against::Base && base.is_none() {
            d.note = Some("No base: no PR base found (pick one in the Current changes tab)".into());
            d.view = None;
            d.for_key = Some(key);
            return;
        }
        let (tx, rx) = channel();
        let (path, text, against, c) = (t.path.clone(), t.text.clone(), d.against, ctx.clone());
        std::thread::spawn(move || {
            let rev = match against {
                Against::Head => Ok("HEAD".to_string()),
                Against::Base => base
                    .zip(root)
                    .and_then(|(b, r)| git::merge_base(&r, &b))
                    .ok_or_else(|| "No base: the PR base is not in this repository".to_string()),
            };
            let _ = tx.send(rev.and_then(|rev| compare(&path, &rev, against, &text)));
            c.request_repaint();
        });
        d.rx = Some(rx);
        d.for_key = Some(key);
    }
}

/// An older version's file and commit sha.
type OldKey = (PathBuf, String);

/// Cached older versions by (file, commit sha): a commit's file never changes.
static OLD: Mutex<Vec<(OldKey, Arc<String>)>> = Mutex::new(Vec::new());

/// `rel` at commit `f.sha`, from the cache or git (then cached); `None` when it is not there.
fn old_at(f: &git::RevFile, rel: &str) -> Result<Option<Arc<String>>, String> {
    let key = (f.top.join(rel), f.sha.clone());
    let cache = || OLD.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, t)) = cache().iter().find(|(k, _)| *k == key) {
        return Ok(Some(t.clone()));
    }
    let Some(text) = git::blob_at(&f.top, &f.sha, rel, MAX_BYTES)? else {
        return Ok(None);
    };
    let text = Arc::new(text);
    let mut c = cache();
    if c.len() >= OLD_CACHE {
        c.remove(0);
    }
    c.push((key, text.clone()));
    Ok(Some(text))
}

/// The older version of `path` at commit `rev` (followed through a rename), with its path then
/// when it was renamed; else the note that says why there is none: "New file: …" or
/// "No base: <reason>". An empty older side is never made up.
pub(super) fn old_side(
    path: &Path,
    rev: &str,
    against: Against,
) -> Result<(Arc<String>, Option<String>), String> {
    let no_base = |e: String| format!("No base: {e}");
    let f = git::rev_file(path, rev).map_err(no_base)?;
    if let Some(t) = old_at(&f, &f.rel).map_err(no_base)? {
        return Ok((t, None));
    }
    if let Some(from) = git::renamed_from(&f.top, &f.sha, &f.rel)
        && let Some(t) = old_at(&f, &from).map_err(no_base)?
    {
        return Ok((t, Some(from)));
    }
    Err(format!(
        "New file: it is not in the {}",
        against.label().to_lowercase()
    ))
}

/// The editor text `text` of `path` beside its older version at commit `rev`.
pub(super) fn compare(
    path: &Path,
    rev: &str,
    against: Against,
    text: &str,
) -> Result<Aligned, String> {
    let (old, renamed) = old_side(path, rev, against)?;
    let mut a = compute(&old, text)?;
    a.renamed = renamed;
    Ok(a)
}

/// The editor's scroll area, moved by `wheel` from `prev` (a wheel over the left column).
pub(super) fn follow_wheel(
    s: egui::ScrollArea,
    wheel: Option<egui::Vec2>,
    prev: egui::Vec2,
) -> egui::ScrollArea {
    match wheel {
        Some(d) => s.scroll_offset(prev - d),
        None => s,
    }
}

/// Where the editor drew its text: rows (after the spacer and link room) and their screen origin.
pub(super) struct Placed<'a> {
    pub galley: &'a Galley,
    pub pos: Pos2,
    pub row_h: f32,
    /// Lines with a link row above them (sorted), and its height.
    pub lens: &'a [usize],
    pub lens_h: f32,
}

impl Placed<'_> {
    fn top(&self, line: usize) -> Option<f32> {
        self.galley.rows.get(line).map(|r| self.pos.y + r.pos.y)
    }

    /// The bottom of the spacer rows above editor line `anchor` (the line count: after the end).
    fn spacer_bottom(&self, v: &Aligned, anchor: usize) -> f32 {
        match self.top(anchor) {
            Some(y) if self.lens.binary_search(&anchor).is_ok() => y - self.lens_h,
            Some(y) => y,
            None => {
                let trailing = v
                    .room
                    .last()
                    .filter(|(l, _)| *l >= self.galley.rows.len())
                    .map_or(0, |(_, n)| *n);
                let end = self
                    .galley
                    .rows
                    .last()
                    .map_or(self.pos.y, |r| self.pos.y + r.pos.y + self.row_h);
                end + trailing as f32 * self.row_h
            }
        }
    }

    /// The top of one row of the two columns.
    fn row_y(&self, v: &Aligned, r: &Row) -> f32 {
        match r.new.and_then(|n| self.top(n)) {
            Some(y) => y,
            None => self.spacer_bottom(v, r.anchor) - r.back as f32 * self.row_h,
        }
    }

    /// The first editor line at or below `y`.
    fn line_at(&self, y: f32) -> usize {
        self.galley
            .rows
            .partition_point(|r| self.pos.y + r.pos.y + self.row_h < y)
    }
}

/// x of char `i` in a galley row (its end past the last char).
fn char_x(row: &egui::epaint::text::PlacedRow, i: usize) -> f32 {
    let g = &row.row.glyphs;
    let x = match g.get(i) {
        Some(c) => c.pos.x,
        None => g.last().map_or(0.0, |c| c.pos.x + c.advance_width),
    };
    row.pos.x + x
}

fn tint(k: Kind) -> Option<Color32> {
    match k {
        Kind::Same => None,
        Kind::Added => Some(ADDED.gamma_multiply(0.16)),
        Kind::Removed => Some(ERROR.gamma_multiply(0.16)),
        Kind::Changed => Some(CHANGED.gamma_multiply(0.14)),
    }
}

fn word_rects(
    out: &mut Vec<Shape>,
    row: &egui::epaint::text::PlacedRow,
    words: &[Range<usize>],
    x0: f32,
    y: f32,
    row_h: f32,
) {
    for w in words {
        let r = Rect::from_min_max(
            Pos2::new(x0 + char_x(row, w.start), y),
            Pos2::new(x0 + char_x(row, w.end), y + row_h),
        );
        out.push(Shape::rect_filled(r, 2.0, CHANGED.gamma_multiply(0.38)));
    }
}

/// The editor side's highlights for the rows on screen (painted under the text): tinted lines,
/// changed words and grey spacer rows. `x` is the left edge of the tint.
pub(super) fn new_side(v: &Aligned, at: &Placed, x: f32, clip: Rect) -> Vec<Shape> {
    let mut out = Vec::new();
    let first = at.line_at(clip.top());
    for (n, row) in at.galley.rows.iter().enumerate().skip(first) {
        let y = at.pos.y + row.pos.y;
        if y > clip.bottom() {
            break;
        }
        let Some(r) = v.row_of_new.get(n).map(|&i| &v.rows[i]) else {
            break;
        };
        if let Some(c) = tint(r.kind) {
            let rect = Rect::from_min_max(Pos2::new(x, y), Pos2::new(clip.right(), y + at.row_h));
            out.push(Shape::rect_filled(rect, 0.0, c));
        }
        word_rects(&mut out, row, &r.new_words, at.pos.x, y, at.row_h);
    }
    let from = v.room.partition_point(|(l, _)| *l < first);
    for &(anchor, n) in &v.room[from..] {
        let bottom = at.spacer_bottom(v, anchor);
        let top = bottom - n as f32 * at.row_h;
        if top > clip.bottom() {
            break;
        }
        let rect = Rect::from_min_max(Pos2::new(x, top), Pos2::new(clip.right(), bottom));
        out.push(Shape::rect_filled(rect, 0.0, SPACER.gamma_multiply(0.12)));
    }
    out
}

/// The older version in `rect`, row by row beside the editor rows: line numbers, syntax colours,
/// the same highlights, scrolled `scroll_x` to the right like the editor.
pub(super) fn paint_old(
    ui: &Ui,
    rect: Rect,
    v: &Aligned,
    at: &Placed,
    scroll_x: f32,
    font: &FontId,
) {
    let theme = crate::theme::t();
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, theme.editor_bg);
    let char_w = ui.fonts_mut(|f| f.glyph_width(font, '0'));
    let num_w = char_w * v.old_lines.len().to_string().len().max(3) as f32 + 18.0;
    let text_x = rect.left() + num_w - scroll_x;
    let text_painter = ui.painter_at(rect.with_min_x(rect.left() + num_w));
    let first = at.line_at(rect.top());
    let from = v.rows.partition_point(|r| r.anchor < first);
    for r in &v.rows[from..] {
        let y = at.row_y(v, r);
        if y > rect.bottom() {
            break;
        }
        if y + at.row_h < rect.top() {
            continue;
        }
        let line = Rect::from_min_max(
            Pos2::new(rect.left(), y),
            Pos2::new(rect.right(), y + at.row_h),
        );
        let Some(o) = r.old else {
            painter.rect_filled(line, 0.0, SPACER.gamma_multiply(0.12));
            continue;
        };
        if let Some(c) = tint(r.kind) {
            painter.rect_filled(line, 0.0, c);
        }
        painter.text(
            Pos2::new(rect.left() + num_w - 12.0, y),
            egui::Align2::RIGHT_TOP,
            (o + 1).to_string(),
            font.clone(),
            theme.gutter,
        );
        let job = highlight::job(&v.old_lines[o], font.clone(), &[], &[]);
        let g = ui.fonts_mut(|f| f.layout_job(job));
        if let Some(row) = g.rows.first() {
            let mut words = Vec::new();
            word_rects(&mut words, row, &r.old_words, text_x, y, at.row_h);
            text_painter.extend(words);
        }
        text_painter.galley(Pos2::new(text_x, y), g, theme.code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(a: &Aligned) -> Vec<(Option<usize>, Option<usize>, Kind)> {
        a.rows.iter().map(|r| (r.old, r.new, r.kind)).collect()
    }

    fn diff(old: &str, new: &str) -> Aligned {
        compute(old, new).unwrap()
    }

    /// Ranges as (start, end) pairs.
    fn spans(v: &[Range<usize>]) -> Spans {
        v.iter().map(|r| (r.start, r.end)).collect()
    }

    type Spans = Vec<(usize, usize)>;

    fn words(old: &str, new: &str) -> (Spans, Spans) {
        let (a, b) = word_diff(old, new);
        (spans(&a), spans(&b))
    }

    #[test]
    fn an_added_line_gets_a_spacer_on_the_old_side() {
        let a = diff("a\nb\n", "a\nx\nb\n");
        assert_eq!(
            kinds(&a),
            [
                (Some(0), Some(0), Kind::Same),
                (None, Some(1), Kind::Added),
                (Some(1), Some(2), Kind::Same),
            ]
        );
        assert_eq!(a.row_of_new, [0, 1, 2]);
        assert!(a.room.is_empty(), "the editor side needs no room");
        assert_eq!((a.added, a.removed, a.changed), (1, 0, 0));
    }

    #[test]
    fn a_removed_line_makes_room_in_the_editor() {
        let a = diff("a\nb\nc\nd\n", "a\nd\n");
        assert_eq!(
            kinds(&a),
            [
                (Some(0), Some(0), Kind::Same),
                (Some(1), None, Kind::Removed),
                (Some(2), None, Kind::Removed),
                (Some(3), Some(1), Kind::Same),
            ]
        );
        // two spacer rows above editor line 1, the upper one two rows up
        assert_eq!(a.room, [(1, 2)]);
        assert_eq!((a.rows[1].anchor, a.rows[1].back), (1, 2));
        assert_eq!((a.rows[2].anchor, a.rows[2].back), (1, 1));
        assert_eq!(a.row_of_new, [0, 3]);
    }

    #[test]
    fn a_changed_line_marks_the_changed_words() {
        let a = diff(
            "fun a() {\n    val count = mutableStateOf(0)\n}\n",
            "fun a() {\n    val clicks = remember { mutableStateOf(0) }\n}\n",
        );
        assert_eq!(kinds(&a)[1], (Some(1), Some(1), Kind::Changed));
        assert_eq!((a.added, a.removed, a.changed), (0, 0, 1));
        let r = &a.rows[1];
        // "count" in the old line; "clicks", "remember {" and " }" in the new one
        assert_eq!(spans(&r.old_words), [(8, 13)]);
        assert_eq!(spans(&r.new_words), [(8, 14), (17, 28), (45, 47)]);
    }

    #[test]
    fn more_removed_than_added_lines_pair_then_leave_spacers() {
        let a = diff("x\nold1\nold2\nold3\ny\n", "x\nnew1\ny\n");
        assert_eq!(
            kinds(&a)[1..4],
            [
                (Some(1), Some(1), Kind::Changed),
                (Some(2), None, Kind::Removed),
                (Some(3), None, Kind::Removed),
            ]
        );
        assert_eq!(a.room, [(2, 2)]);
    }

    #[test]
    fn empty_and_new_files() {
        let empty = diff("", "");
        assert!(empty.rows.is_empty());
        assert_eq!(empty.summary(), "No changes");
        // a new (untracked) file: every line is added, nothing on the old side
        let new = diff("", "a\nb");
        assert_eq!(
            kinds(&new),
            [(None, Some(0), Kind::Added), (None, Some(1), Kind::Added),]
        );
        // everything deleted: the spacers sit above the editor's only (empty) line
        let gone = diff("a\nb\n", "");
        assert_eq!(gone.room, [(0, 2)]);
        assert_eq!((gone.removed, gone.added), (2, 0));
        // removed lines at the end of the file go after the last line
        let tail = diff("a\nb\nc\n", "a\n");
        assert_eq!(tail.room, [(1, 2)]);
        assert_eq!(tail.rows[2].back, 1);
    }

    #[test]
    fn crlf_line_ends_are_not_changes() {
        let a = diff("a\r\nb\r\nc\r\n", "a\r\nB\r\nc\r\n");
        assert_eq!(
            kinds(&a),
            [
                (Some(0), Some(0), Kind::Same),
                (Some(1), Some(1), Kind::Changed),
                (Some(2), Some(2), Kind::Same),
            ]
        );
        assert_eq!(a.old_lines[1], "b", "no \\r in the shown text");
        let lf = diff("a\r\nb\r\n", "a\nb\n");
        assert_eq!(lf.summary(), "No changes");
    }

    #[test]
    fn a_very_long_file_is_not_compared() {
        let long = "x\n".repeat(MAX_LINES + 1);
        let e = compute("", &long).unwrap_err();
        assert!(e.contains("over 20000 lines"), "{e}");
        let e = compute(&long, "").unwrap_err();
        assert!(e.contains(&format!("{} lines", MAX_LINES + 1)), "{e}");
        assert!(compute("", &"x\n".repeat(MAX_LINES)).is_ok());
    }

    /// A git repository in a new temp folder; `g` runs git in it.
    fn repo(name: &str) -> (PathBuf, impl Fn(&[&str])) {
        let dir = std::env::temp_dir().join(format!("dl-diffview-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.clone();
        let g = move |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&d)
                .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "{args:?}");
        };
        g(&["init", "-q"]);
        (dir, g)
    }

    #[test]
    fn no_base_and_new_files_say_so_instead_of_an_empty_older_side() {
        let plain = std::env::temp_dir().join(format!("dl-diffview-plain-{}", std::process::id()));
        std::fs::create_dir_all(&plain).unwrap();
        let e = compare(&plain.join("A.kt"), "HEAD", Against::Head, "a\n").unwrap_err();
        assert_eq!(e, "No base: not a git repository");
        let (dir, g) = repo("nocommit");
        std::fs::write(dir.join("A.kt"), "a\n").unwrap();
        let e = compare(&dir.join("A.kt"), "HEAD", Against::Head, "a\n").unwrap_err();
        assert_eq!(e, "No base: no commit yet");
        g(&["add", "A.kt"]);
        g(&["commit", "-q", "-m", "a"]);
        std::fs::write(dir.join("B.kt"), "b\n").unwrap();
        let e = compare(&dir.join("B.kt"), "HEAD", Against::Head, "b\n").unwrap_err();
        assert_eq!(e, "New file: it is not in the last commit");
        let e = compare(&dir.join("A.kt"), "nope", Against::Base, "a\n").unwrap_err();
        assert_eq!(e, "No base: nope is not a commit here");
        let a = compare(&dir.join("A.kt"), "HEAD", Against::Head, "a\nx\n").unwrap();
        assert_eq!((a.added, a.renamed), (1, None));
        for d in [plain, dir] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn a_renamed_file_compares_with_its_old_path_and_the_old_side_is_cached() {
        let (dir, g) = repo("rename");
        std::fs::create_dir_all(dir.join("ui")).unwrap();
        let body = "class A {\n    fun a() = 1\n    fun b() = 2\n    fun c() = 3\n}\n";
        std::fs::write(dir.join("ui/Old.kt"), body).unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "a"]);
        g(&["mv", "ui/Old.kt", "ui/New.kt"]);
        let new = dir.join("ui/New.kt");
        let edited = body.replace("= 2", "= 22");
        let a = compare(&new, "HEAD", Against::Head, &edited).unwrap();
        assert_eq!(a.renamed.as_deref(), Some("ui/Old.kt"));
        assert_eq!((a.added, a.removed, a.changed), (0, 0, 1));
        assert!(
            a.summary().starts_with("Renamed from ui/Old.kt · "),
            "{}",
            a.summary()
        );
        // the same file at the same commit comes from the cache
        let (one, _) = old_side(&new, "HEAD", Against::Head).unwrap();
        let (two, _) = old_side(&new, "HEAD", Against::Head).unwrap();
        assert!(Arc::ptr_eq(&one, &two), "read from git once");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_very_big_file_is_not_compared() {
        let big = "x".repeat(MAX_BYTES + 1);
        let e = compute("", &big).unwrap_err();
        assert!(e.contains("over 2048 KB"), "{e}");
        assert!(compute(&big, "").is_err());
        assert!(compute("", &"x".repeat(MAX_BYTES)).is_ok());
        // nor read from git
        let (dir, g) = repo("big");
        std::fs::write(dir.join("Big.kt"), &big).unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "a"]);
        let e = compare(&dir.join("Big.kt"), "HEAD", Against::Head, "").unwrap_err();
        assert!(e.contains("over 2048 KB"), "{e}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn word_diff_keeps_common_words() {
        assert_eq!(words("a b c", "a x c"), (vec![(2, 3)], vec![(2, 3)]));
        assert_eq!(words("same", "same"), (vec![], vec![]));
        assert_eq!(words("", "new words"), (vec![], vec![(0, 9)]));
        // a word inserted in the middle; the rest moves but is kept
        assert_eq!(words("f(a, b)", "f(a, x, b)"), (vec![], vec![(5, 8)]));
        // chars, not bytes
        assert_eq!(words("é a", "é b"), (vec![(2, 3)], vec![(2, 3)]));
    }
}
