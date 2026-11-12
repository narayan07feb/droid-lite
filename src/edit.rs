//! Line editing commands: comment, delete, duplicate, move lines, go to line.
//! Each works on the lines the selection touches (or the cursor's line) and returns the new text
//! and the new cursor byte offset.

use std::ops::Range;

/// Byte range of the whole lines touched by `sel`, including the last line's `\n` if there is one.
pub fn lines_of(text: &str, sel: Range<usize>) -> Range<usize> {
    let start = text[..sel.start.min(text.len())]
        .rfind('\n')
        .map_or(0, |i| i + 1);
    // a selection that ends right at the start of a line does not include that line
    let end_at = if sel.end > sel.start && text[..sel.end].ends_with('\n') {
        sel.end - 1
    } else {
        sel.end
    };
    let end = text[end_at.min(text.len())..]
        .find('\n')
        .map_or(text.len(), |i| end_at + i + 1);
    start..end.max(start)
}

/// `//` on every touched line, or off when all non-blank lines already have it.
pub fn toggle_comment(text: &str, sel: Range<usize>) -> (String, usize) {
    let r = lines_of(text, sel.clone());
    let block = &text[r.clone()];
    let lines: Vec<&str> = block.split_inclusive('\n').collect();
    let indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let all = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .all(|l| l.trim_start().starts_with("//"));
    let out: String = lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                l.to_string()
            } else if all {
                let i = l.find("//").unwrap_or(0);
                let after = &l[i + 2..];
                format!("{}{}", &l[..i], after.strip_prefix(' ').unwrap_or(after))
            } else {
                format!("{}// {}", &l[..indent], &l[indent..])
            }
        })
        .collect();
    let delta = out.len() as isize - block.len() as isize;
    let cursor = (sel.start as isize
        + if all {
            -(3.min(sel.start - r.start) as isize)
        } else {
            3
        })
    .clamp(
        r.start as isize,
        (r.end as isize + delta).max(r.start as isize),
    );
    (
        format!("{}{}{}", &text[..r.start], out, &text[r.end..]),
        cursor as usize,
    )
}

pub fn delete_lines(text: &str, sel: Range<usize>) -> (String, usize) {
    let r = lines_of(text, sel);
    let mut out = format!("{}{}", &text[..r.start], &text[r.end..]);
    if r.end == text.len() && out.ends_with('\n') && r.start > 0 {
        out.pop(); // deleting the last line also removes the newline before it
    }
    let cursor = r.start.min(out.len());
    (out, cursor)
}

pub fn duplicate_lines(text: &str, sel: Range<usize>) -> (String, usize) {
    let r = lines_of(text, sel.clone());
    let mut block = text[r.clone()].to_string();
    if !block.ends_with('\n') {
        block.insert(0, '\n'); // last line without newline: copy goes after a new newline
        return (format!("{}{}", text, block), sel.start + block.len());
    }
    (
        format!("{}{}{}", &text[..r.end], block, &text[r.end..]),
        sel.start + block.len(),
    )
}

/// Swaps the touched lines with the line above (`up`) or below. Returns `None` at the edge.
pub fn move_lines(text: &str, sel: Range<usize>, up: bool) -> Option<(String, usize)> {
    let mut t = text.to_string();
    if !t.ends_with('\n') {
        t.push('\n');
    }
    let r = lines_of(&t, sel.clone());
    let block = t[r.clone()].to_string();
    let (out, cursor) = if up {
        if r.start == 0 {
            return None;
        }
        let prev = lines_of(&t, r.start - 1..r.start - 1);
        let above = &t[prev.clone()];
        (
            format!("{}{}{}{}", &t[..prev.start], block, above, &t[r.end..]),
            sel.start - above.len(),
        )
    } else {
        if r.end >= t.len() {
            return None;
        }
        let next = lines_of(&t, r.end..r.end);
        let below = &t[next.clone()];
        (
            format!("{}{}{}{}", &t[..r.start], below, block, &t[next.end..]),
            sel.start + below.len(),
        )
    };
    let out = if text.ends_with('\n') {
        out
    } else {
        out.strip_suffix('\n').unwrap_or(&out).to_string()
    };
    Some((out, cursor))
}

/// Byte offset of "line" or "line:column" (1-based), clamped to the text.
pub fn line_offset(text: &str, spec: &str) -> Option<usize> {
    let mut parts = spec.trim().split(':');
    let line: usize = parts.next()?.trim().parse().ok()?;
    let col: usize = parts
        .next()
        .and_then(|c| c.trim().parse().ok())
        .unwrap_or(1);
    let start: usize = text
        .split_inclusive('\n')
        .take(line.max(1) - 1)
        .map(str::len)
        .sum();
    let line_len = text[start.min(text.len())..]
        .find('\n')
        .unwrap_or(text.len() - start.min(text.len()));
    Some(start.min(text.len()) + (col.max(1) - 1).min(line_len))
}

/// Every match of `query`, optionally case-sensitive and whole-word only.
pub fn find_all(text: &str, query: &str, case: bool, whole: bool) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let (hay, needle) = if case {
        (text.to_string(), query.to_string())
    } else {
        (text.to_lowercase(), query.to_lowercase())
    };
    // lowercasing can change byte lengths for some scripts; fall back to exact search then
    let (hay, needle) = if hay.len() == text.len() && needle.len() == query.len() {
        (hay, needle)
    } else {
        (text.to_string(), query.to_string())
    };
    hay.match_indices(&needle)
        .map(|(i, m)| i..i + m.len())
        .filter(|r| {
            !whole || (!word(text[..r.start].chars().last()) && !word(text[r.end..].chars().next()))
        })
        .collect()
}

/// Enter: a new line with the current line's indent, one level more after `{` `(` `[` or `->`,
/// and the closing bracket moved to its own line when the cursor sits between a pair.
pub fn newline(text: &str, sel: Range<usize>) -> (String, usize) {
    let line_start = text[..sel.start].rfind('\n').map_or(0, |i| i + 1);
    let indent: String = text[line_start..]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    let before = text[line_start..sel.start].trim_end();
    let opens = before.ends_with(['{', '(', '[']) || before.ends_with("->");
    let inner = if opens {
        format!("{indent}    ")
    } else {
        indent.clone()
    };
    let next = text[sel.end..]
        .trim_start_matches([' ', '\t'])
        .chars()
        .next();
    let closes = opens
        && matches!(
            (before.chars().last(), next),
            (Some('{'), Some('}')) | (Some('('), Some(')')) | (Some('['), Some(']'))
        );
    let insert = if closes {
        format!("\n{inner}\n{indent}")
    } else {
        format!("\n{inner}")
    };
    let cursor = sel.start + 1 + inner.len();
    let rest = if closes {
        text[sel.end..].trim_start_matches([' ', '\t'])
    } else {
        &text[sel.end..]
    };
    (format!("{}{}{}", &text[..sel.start], insert, rest), cursor)
}

const PAIRS: [(char, char); 5] = [('(', ')'), ('[', ']'), ('{', '}'), ('"', '"'), ('\'', '\'')];

/// Typing one character with auto-closing pairs. `None` means: let the editor insert it normally.
/// - an opening bracket or quote also inserts its closer (quotes only before space, a closer or the end)
/// - typing a closer that is already next to the cursor just steps over it
/// - with a selection, brackets and quotes wrap it
pub fn type_char(text: &str, sel: Range<usize>, ch: char) -> Option<(String, usize)> {
    let next = text[sel.end..].chars().next();
    if sel.start == sel.end && next == Some(ch) && PAIRS.iter().any(|(_, c)| *c == ch) {
        return Some((text.to_string(), sel.end + ch.len_utf8()));
    }
    let close = PAIRS.iter().find(|(o, _)| *o == ch).map(|(_, c)| *c)?;
    if sel.start != sel.end {
        let wrapped = format!("{ch}{}{close}", &text[sel.clone()]);
        return Some((
            format!("{}{}{}", &text[..sel.start], wrapped, &text[sel.end..]),
            sel.start + wrapped.len(),
        ));
    }
    let free_after = next.is_none_or(|c| c.is_whitespace() || ")]},;".contains(c));
    let quote = ch == '"' || ch == '\'';
    let prev = text[..sel.start].chars().last();
    if !free_after || (quote && prev.is_some_and(|c| c.is_alphanumeric())) {
        return None;
    }
    Some((
        format!("{}{ch}{close}{}", &text[..sel.start], &text[sel.end..]),
        sel.start + ch.len_utf8(),
    ))
}

/// Backspace between an empty pair `(|)` removes both. `None` means: normal backspace.
pub fn backspace_pair(text: &str, cursor: usize) -> Option<(String, usize)> {
    let prev = text[..cursor].chars().last()?;
    let next = text[cursor..].chars().next()?;
    PAIRS.contains(&(prev, next)).then(|| {
        (
            format!(
                "{}{}",
                &text[..cursor - prev.len_utf8()],
                &text[cursor + next.len_utf8()..]
            ),
            cursor - prev.len_utf8(),
        )
    })
}

/// The bracket next to the cursor and its partner, skipping brackets in strings and comments.
pub fn matching_bracket(text: &str, cursor: usize) -> Option<(usize, usize)> {
    use crate::lexer::{Tok, lex};
    let toks: Vec<_> = lex(text)
        .into_iter()
        .filter(|t| {
            t.kind == Tok::Punct && t.range.len() == 1 && "()[]{}".contains(&text[t.range.clone()])
        })
        .collect();
    let at = toks
        .iter()
        .position(|t| t.range.start == cursor)
        .or_else(|| toks.iter().position(|t| t.range.end == cursor))?;
    let c = text[toks[at].range.clone()].chars().next()?;
    let (open, close, forward) = match c {
        '(' => ('(', ')', true),
        '[' => ('[', ']', true),
        '{' => ('{', '}', true),
        ')' => ('(', ')', false),
        ']' => ('[', ']', false),
        _ => ('{', '}', false),
    };
    let mut depth = 0i32;
    let order: Box<dyn Iterator<Item = usize>> = if forward {
        Box::new(at..toks.len())
    } else {
        Box::new((0..=at).rev())
    };
    for i in order {
        let t = text[toks[i].range.clone()].chars().next()?;
        if t == open {
            depth += if forward { 1 } else { -1 };
        } else if t == close {
            depth += if forward { -1 } else { 1 };
        }
        if depth == 0 {
            return Some((toks[at].range.start, toks[i].range.start));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "a\n    b\n    c\nd";

    #[test]
    fn lines_touched_by_selection() {
        assert_eq!(&T[lines_of(T, 3..3)], "    b\n");
        assert_eq!(&T[lines_of(T, 3..10)], "    b\n    c\n");
        assert_eq!(&T[lines_of(T, 14..14)], "d");
        assert_eq!(
            &T[lines_of(T, 0..2)],
            "a\n",
            "selection ending at a line start stops before it"
        );
    }

    #[test]
    fn comment_and_uncomment_keep_indent() {
        let (c, _) = toggle_comment(T, 3..10);
        assert_eq!(c, "a\n    // b\n    // c\nd");
        let (u, _) = toggle_comment(&c, 3..12);
        assert_eq!(u, T);
    }

    #[test]
    fn delete_duplicate_move() {
        assert_eq!(delete_lines(T, 3..3).0, "a\n    c\nd");
        assert_eq!(delete_lines(T, 14..14).0, "a\n    b\n    c");
        assert_eq!(
            duplicate_lines(T, 3..3),
            ("a\n    b\n    b\n    c\nd".to_string(), 9)
        );
        assert_eq!(duplicate_lines(T, 14..14).0, "a\n    b\n    c\nd\nd");
        assert_eq!(
            move_lines(T, 3..3, true).unwrap(),
            ("    b\na\n    c\nd".to_string(), 1)
        );
        assert_eq!(move_lines(T, 3..3, false).unwrap().0, "a\n    c\n    b\nd");
        assert_eq!(move_lines(T, 14..14, true).unwrap().0, "a\n    b\nd\n    c");
        assert!(move_lines(T, 0..0, true).is_none());
        assert!(move_lines(T, 14..14, false).is_none());
    }

    #[test]
    fn go_to_line_and_column() {
        assert_eq!(line_offset(T, "2"), Some(2));
        assert_eq!(line_offset(T, "3:5"), Some(12));
        assert_eq!(line_offset(T, "99"), Some(T.len()));
        assert_eq!(line_offset(T, "x"), None);
    }

    #[test]
    fn find_options() {
        let t = "User user USER users";
        assert_eq!(find_all(t, "user", false, false).len(), 4);
        assert_eq!(find_all(t, "user", true, false).len(), 2);
        assert_eq!(find_all(t, "user", false, true).len(), 3);
        assert_eq!(find_all(t, "user", true, true), vec![5..9]);
        assert!(find_all(t, "", false, false).is_empty());
    }

    #[test]
    fn enter_indents() {
        let t = "fun a() {}";
        assert_eq!(newline(t, 9..9), ("fun a() {\n    \n}".to_string(), 14));
        let t = "    val x = 1";
        assert_eq!(newline(t, 13..13), ("    val x = 1\n    ".to_string(), 18));
        let t = "    list.map { it ->";
        assert_eq!(
            newline(t, t.len()..t.len()).0,
            "    list.map { it ->\n        "
        );
        let t = "f(a)";
        assert_eq!(
            newline(t, 2..2),
            ("f(\n    a)".to_string(), 7),
            "not between an empty pair: no extra line"
        );
    }

    #[test]
    fn pairs_close_step_over_wrap_and_delete() {
        assert_eq!(type_char("f", 1..1, '('), Some(("f()".to_string(), 2)));
        assert_eq!(
            type_char("f()", 2..2, ')'),
            Some(("f()".to_string(), 3)),
            "step over"
        );
        assert_eq!(
            type_char("ab", 0..1, '"'),
            Some(("\"a\"b".to_string(), 3)),
            "wrap selection"
        );
        assert_eq!(type_char("ab", 1..1, '('), None, "no pair before a letter");
        assert_eq!(
            type_char("it", 2..2, '\''),
            None,
            "no quote pair after a letter"
        );
        assert_eq!(type_char("x", 1..1, 'a'), None);
        assert_eq!(backspace_pair("f()", 2), Some(("f".to_string(), 1)));
        assert_eq!(backspace_pair("f(a)", 3), None);
    }

    #[test]
    fn brackets_match_outside_strings() {
        let t = "f(a, \"(\", g[1]) { }";
        assert_eq!(matching_bracket(t, 1), Some((1, 14)));
        assert_eq!(
            matching_bracket(t, 15),
            Some((14, 1)),
            "cursor right after the closer"
        );
        assert_eq!(matching_bracket(t, 16), Some((16, 18)));
        assert_eq!(matching_bracket(t, 3), None);
    }
}
