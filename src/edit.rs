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
}
