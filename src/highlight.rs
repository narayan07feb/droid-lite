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
