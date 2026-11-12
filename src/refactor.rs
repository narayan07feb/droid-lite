//! Refactorings that work on text: find a name's code occurrences, apply edits,
//! optimize imports, extract a variable.

use crate::lexer::{Tok, is_keyword, lex};
use std::collections::HashSet;
use std::ops::Range;

/// Byte ranges where `name` is used as an identifier (also as `` `name` ``). Strings and comments are skipped.
pub fn occurrences(text: &str, name: &str) -> Vec<Range<usize>> {
    if !text.contains(name) {
        return Vec::new();
    }
    lex(text)
        .into_iter()
        .filter(|t| t.kind == Tok::Ident && text[t.range.clone()].trim_matches('`') == name)
        .map(|t| t.range)
        .collect()
}

/// Applies non-overlapping edits. Order does not matter.
pub fn apply(text: &str, edits: &[(Range<usize>, String)]) -> String {
    let mut sorted: Vec<&(Range<usize>, String)> = edits.iter().collect();
    sorted.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    let mut out = text.to_string();
    let mut limit = usize::MAX;
    for (r, new) in sorted {
        if r.end <= limit && r.end <= out.len() {
            out.replace_range(r.clone(), new);
            limit = r.start;
        }
    }
    out
}

pub fn valid_identifier(name: &str) -> bool {
    let mut c = name.chars();
    c.next().is_some_and(|f| f.is_alphabetic() || f == '_')
        && c.all(|x| x.is_alphanumeric() || x == '_')
        && !is_keyword(name)
}

/// Imports Kotlin uses without naming them in code: delegates and operator functions.
const IMPLICIT: &[&str] = &[
    "getValue",
    "setValue",
    "provideDelegate",
    "plus",
    "minus",
    "times",
    "div",
    "rem",
    "contains",
    "get",
    "set",
    "invoke",
    "compareTo",
    "iterator",
    "next",
    "hasNext",
    "rangeTo",
    "rangeUntil",
    "unaryMinus",
    "unaryPlus",
    "not",
    "inc",
    "dec",
    "plusAssign",
    "minusAssign",
    "timesAssign",
    "divAssign",
    "remAssign",
    "component1",
    "component2",
    "component3",
    "component4",
    "component5",
];

/// Removes unused and duplicate imports; keeps the order. Returns `None` when nothing changes.
pub fn optimize_imports(text: &str) -> Option<String> {
    let mut import_lines: Vec<(Range<usize>, String, String)> = Vec::new(); // line range, path, simple name
    let mut pos = 0;
    let mut in_header = true;
    for line in text.split_inclusive('\n') {
        let t = line.trim();
        if in_header {
            if let Some(rest) = t.strip_prefix("import ") {
                let rest = rest.trim_end_matches(';').trim();
                let (path, alias) = match rest.split_once(" as ") {
                    Some((p, a)) => (p.trim().to_string(), Some(a.trim().to_string())),
                    None => (rest.to_string(), None),
                };
                let simple =
                    alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or("").to_string());
                import_lines.push((pos..pos + line.len(), format!("{path} as {simple}"), simple));
            } else if !(t.is_empty()
                || t.starts_with("package ")
                || t.starts_with("//")
                || t.starts_with("/*")
                || t.starts_with('*')
                || t.starts_with('@'))
            {
                in_header = false;
            }
        }
        pos += line.len();
    }
    if import_lines.is_empty() {
        return None;
    }
    let header_end = import_lines.last().map_or(0, |(r, _, _)| r.end);
    let mut used: HashSet<&str> = HashSet::new();
    for t in lex(text).iter().filter(|t| t.range.start >= header_end) {
        let w = &text[t.range.clone()];
        match t.kind {
            Tok::Ident => {
                used.insert(w.trim_matches('`'));
            }
            Tok::Annotation => {
                used.insert(w.trim_start_matches('@').split('.').next().unwrap_or(""));
            }
            Tok::Comment => {
                // KDoc links: [Name] or [Name.member]
                for part in w.split('[').skip(1) {
                    if let Some(link) = part.split(']').next() {
                        used.insert(link.split('.').next().unwrap_or(""));
                    }
                }
            }
            _ => {}
        }
    }
    let mut seen = HashSet::new();
    let remove: Vec<(Range<usize>, String)> = import_lines
        .iter()
        .filter(|(_, path, simple)| {
            let keep = simple == "*"
                || used.contains(simple.as_str())
                || IMPLICIT.contains(&simple.as_str());
            !(keep && seen.insert(path.clone()))
        })
        .map(|(r, _, _)| (r.clone(), String::new()))
        .collect();
    (!remove.is_empty()).then(|| apply(text, &remove))
}

/// `val name = <selection>` on a new line above the statement; the selection becomes `name`.
/// Returns the new text and the byte offset of the new name in the declaration.
pub fn extract_variable(text: &str, sel: Range<usize>, name: &str) -> Option<(String, usize)> {
    let expr = text.get(sel.clone())?.trim();
    if expr.is_empty() || !valid_identifier(name) {
        return None;
    }
    let line_start = text[..sel.start].rfind('\n').map_or(0, |i| i + 1);
    let indent: String = text[line_start..]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    let decl = format!("{indent}val {name} = {expr}\n");
    let out = apply(
        text,
        &[
            (line_start..line_start, decl.clone()),
            (sel, name.to_string()),
        ],
    );
    Some((out, line_start + indent.len() + 4))
}

/// A name for an extracted expression: `user.profile.displayName` → `displayName`, `getTitle()` → `title`.
pub fn suggest_name(expr: &str) -> String {
    let words: Vec<&str> = expr
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty() && !w.starts_with(|c: char| c.is_ascii_digit()))
        .collect();
    let last = words
        .iter()
        .rev()
        .find(|w| !is_keyword(w))
        .copied()
        .unwrap_or("value");
    let last = last
        .strip_prefix("get")
        .filter(|r| r.starts_with(|c: char| c.is_uppercase()))
        .unwrap_or(last);
    let mut c = last.chars();
    let name: String = c
        .next()
        .map(|f| f.to_lowercase().chain(c).collect())
        .unwrap_or_default();
    if valid_identifier(&name) {
        name
    } else {
        "value".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occurrences_skip_strings_comments_and_longer_names() {
        let t = "val user = User(\"user\") // user\nfun f(`user`: Users) = user.name\n";
        let r = occurrences(t, "user");
        assert_eq!(r.len(), 3, "{r:?}");
        assert_eq!(&t[r[1].clone()], "`user`");
        assert_eq!(occurrences(t, "User").len(), 1);
    }

    #[test]
    fn apply_edits_in_any_order() {
        assert_eq!(
            apply("a b a", &[(0..1, "xx".into()), (4..5, "yy".into())]),
            "xx b yy"
        );
        assert_eq!(apply("abc", &[(1..1, "-".into())]), "a-bc");
    }

    #[test]
    fn identifiers() {
        assert!(valid_identifier("userName") && valid_identifier("_x"));
        assert!(
            !valid_identifier("1a")
                && !valid_identifier("val")
                && !valid_identifier("a-b")
                && !valid_identifier("")
        );
    }

    #[test]
    fn optimize_imports_keeps_used_operators_annotations_and_kdoc() {
        let src = "package a\n\nimport x.Used\nimport x.Unused\nimport x.Used\nimport androidx.compose.runtime.getValue\nimport y.*\nimport z.Thing as Alias\nimport z.Gone as G2\nimport k.Linked\nimport k.Ann\n\n/** See [Linked.foo]. */\n@Ann\nfun f(u: Used) { val v by remember { 1 }; Alias() }\n";
        let out = optimize_imports(src).unwrap();
        assert_eq!(
            out,
            "package a\n\nimport x.Used\nimport androidx.compose.runtime.getValue\nimport y.*\nimport z.Thing as Alias\nimport k.Linked\nimport k.Ann\n\n/** See [Linked.foo]. */\n@Ann\nfun f(u: Used) { val v by remember { 1 }; Alias() }\n"
        );
        assert!(optimize_imports(&out).is_none());
    }

    #[test]
    fn extract_variable_above_the_statement() {
        let src = "fun f() {\n    show(user.profile.displayName)\n}\n";
        let sel = src.find("user").unwrap()..src.rfind(")").unwrap();
        let name = suggest_name(&src[sel.clone()]);
        assert_eq!(name, "displayName");
        let (out, at) = extract_variable(src, sel, &name).unwrap();
        assert_eq!(
            out,
            "fun f() {\n    val displayName = user.profile.displayName\n    show(displayName)\n}\n"
        );
        assert_eq!(&out[at..at + name.len()], "displayName");
        assert_eq!(suggest_name("repo.getTitle()"), "title");
        assert_eq!(suggest_name("42"), "value");
    }
}
