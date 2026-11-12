//! Syntax colours (Darcula-like) as an egui LayoutJob, with underlines for problems.

use crate::lexer::{Tok, lex};
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, Stroke};
use std::ops::Range;

pub const TEXT: Color32 = Color32::from_rgb(0xA9, 0xB7, 0xC6);
const KEYWORD: Color32 = Color32::from_rgb(0xCC, 0x78, 0x32);
const STRING: Color32 = Color32::from_rgb(0x6A, 0x87, 0x59);
const NUMBER: Color32 = Color32::from_rgb(0x68, 0x97, 0xBB);
const COMMENT: Color32 = Color32::from_rgb(0x80, 0x80, 0x80);
const ANNOTATION: Color32 = Color32::from_rgb(0xBB, 0xB5, 0x29);
const CALL: Color32 = Color32::from_rgb(0xFF, 0xC6, 0x6D);
const TYPE: Color32 = Color32::from_rgb(0xC9, 0xD1, 0xDB);
pub const WARN: Color32 = Color32::from_rgb(0xE0, 0xA0, 0x30);
pub const ERROR: Color32 = Color32::from_rgb(0xE0, 0x50, 0x50);
pub const INFO: Color32 = Color32::from_rgb(0x70, 0x90, 0xB0);

/// `marks` are byte ranges to underline with a colour (lints, diagnostics).
pub fn job(text: &str, font: FontId, marks: &[(Range<usize>, Color32)]) -> LayoutJob {
    let mut job = LayoutJob::default();
    let plain = TextFormat {
        font_id: font.clone(),
        color: TEXT,
        ..Default::default()
    };
    let toks = lex(text);
    let mut at = 0;
    for (i, t) in toks.iter().enumerate() {
        if t.range.start > at {
            job.append(&text[at..t.range.start], 0.0, plain.clone());
        }
        let w = &text[t.range.clone()];
        let next_is_call = toks
            .get(i + 1)
            .is_some_and(|n| &text[n.range.clone()] == "(" || &text[n.range.clone()] == "{");
        let color = match t.kind {
            Tok::Keyword => KEYWORD,
            Tok::Str => STRING,
            Tok::Num => NUMBER,
            Tok::Comment => COMMENT,
            Tok::Annotation => ANNOTATION,
            Tok::Ident if next_is_call => CALL,
            Tok::Ident if w.starts_with(|c: char| c.is_ascii_uppercase()) => TYPE,
            _ => TEXT,
        };
        let mut f = TextFormat {
            font_id: font.clone(),
            color,
            italics: t.kind == Tok::Comment,
            ..Default::default()
        };
        if let Some((_, c)) = marks
            .iter()
            .find(|(r, _)| r.start < t.range.end && t.range.start < r.end)
        {
            f.underline = Stroke::new(1.5, *c);
        }
        job.append(w, 0.0, f);
        at = t.range.end;
    }
    if at < text.len() {
        job.append(&text[at..], 0.0, plain);
    }
    job.wrap.max_width = f32::INFINITY;
    job
}

/// Applies one contiguous edit to a coloured job without re-colouring the file: sections before
/// the edit stay, sections after it shift, and the edited span takes the colour of the section it
/// starts in. A full re-colour follows once typing pauses.
pub fn patch(job: &mut LayoutJob, new: &str) {
    use egui::text::ByteIndex as B;
    let old = job.text.as_str();
    let mut start = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    let max_suffix = old.len().min(new.len()) - start;
    let mut suffix = old
        .bytes()
        .rev()
        .zip(new.bytes().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(start) || !new.is_char_boundary(start) {
        start -= 1;
    }
    while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix) {
        suffix -= 1;
    }
    let (old_end, new_end) = (old.len() - suffix, new.len() - suffix);
    let mut out: Vec<egui::text::LayoutSection> = Vec::with_capacity(job.sections.len());
    let mut merged: Option<usize> = None; // index in `out` of the section that took the edit
    for mut sec in std::mem::take(&mut job.sections) {
        let (a, b) = (sec.byte_range.start.0, sec.byte_range.end.0);
        if b <= start {
            out.push(sec);
        } else if a >= old_end && a > start {
            sec.byte_range = B(a - old_end + new_end)..B(b - old_end + new_end);
            out.push(sec);
        } else if let Some(i) = merged {
            out[i].byte_range.end = B(out[i].byte_range.end.0.max(if b >= old_end {
                b - old_end + new_end
            } else {
                new_end
            }));
        } else {
            sec.byte_range = B(a.min(start))..B(if b >= old_end {
                b - old_end + new_end
            } else {
                new_end
            });
            out.push(sec);
            merged = Some(out.len() - 1);
        }
    }
    // pure insertion at a section boundary: no section overlapped, so grow the one before it
    if merged.is_none()
        && new_end > start
        && let Some(prev) = out.iter_mut().rev().find(|s| s.byte_range.end.0 == start)
    {
        prev.byte_range.end = B(new_end);
    }
    // make the sections contiguous and cover exactly the new text
    let mut at = 0;
    out.retain_mut(|s| {
        s.byte_range.start = B(at);
        at = at.max(s.byte_range.end.0);
        s.byte_range.start.0 < s.byte_range.end.0
    });
    match out.last_mut() {
        Some(last) => last.byte_range.end = B(new.len()),
        None if !new.is_empty() => out.push(egui::text::LayoutSection {
            leading_space: 0.0,
            byte_range: B(0)..B(new.len()),
            format: job
                .sections
                .first()
                .map(|s| s.format.clone())
                .unwrap_or_default(),
        }),
        None => {}
    }
    job.sections = out;
    job.text = new.to_string();
}

#[cfg(test)]
mod patch_tests {
    fn check(old: &str, new: &str) {
        let font = egui::FontId::monospace(13.0);
        let mut job = super::job(old, font, &[]);
        super::patch(&mut job, new);
        assert_eq!(job.text, new);
        let mut at = 0;
        for s in &job.sections {
            assert_eq!(
                s.byte_range.start.0,
                at,
                "gap or overlap in {:?} for {old:?} -> {new:?}",
                job.sections
                    .iter()
                    .map(|s| s.byte_range.start.0..s.byte_range.end.0)
                    .collect::<Vec<_>>()
            );
            assert!(s.byte_range.end.0 > s.byte_range.start.0);
            at = s.byte_range.end.0;
        }
        assert_eq!(at, new.len(), "{old:?} -> {new:?}");
    }

    #[test]
    fn patched_sections_stay_contiguous() {
        let base = "fun a() {\n    val x = \"hé\" // c\n}\n";
        check(base, &base.replace("val", "vall"));
        check(base, &base.replace("val ", ""));
        check(base, &format!("{base}x"));
        check(base, &format!("x{base}"));
        check(base, &base.replace("hé", "hé😀"));
        check(base, "");
        check("", "abc");
        check(base, &base.replace("// c", ""));
    }
}

#[cfg(test)]
mod perf {
    /// Times one keystroke in a 10 000-line file: full colouring vs the typing patch.
    /// `cargo test --release large_file -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn large_file_keystroke_time() {
        let line =
            "    val state = remember { mutableStateOf(listOf(1, 2, 3)) } // comment \"x\"\n";
        let text: String = (0..10_000)
            .map(|i| {
                if i % 50 == 0 {
                    format!("fun f{i}() {{\n")
                } else {
                    line.to_string()
                }
            })
            .collect();
        let font = egui::FontId::monospace(13.0);
        let mut job = super::job(&text, font.clone(), &[]);
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(Default::default(), |ui| {
            let mut t = text.clone();
            for round in 0..3 {
                t.insert(t.len() / 2, 'x');
                let a = std::time::Instant::now();
                let _full = super::job(&t, font.clone(), &[]);
                let b = std::time::Instant::now();
                super::patch(&mut job, &t);
                let c = std::time::Instant::now();
                let g = ui.fonts_mut(|f| f.layout_job(job.clone()));
                let d = std::time::Instant::now();
                println!("round {round}: full colour {:.1} ms, patch {:.2} ms, layout {:.1} ms ({} rows)", (b - a).as_secs_f64() * 1e3, (c - b).as_secs_f64() * 1e3, (d - c).as_secs_f64() * 1e3, g.rows.len());
            }
        });
    }
}
