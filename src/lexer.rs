//! A small, error-tolerant Kotlin/Java lexer. Used for syntax colours and the symbol scanner.

use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tok {
    Ident,
    Keyword,
    Str,
    Num,
    Comment,
    Annotation,
    Punct,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub kind: Tok,
    pub range: Range<usize>,
}

pub const KEYWORDS: &[&str] = &[
    "abstract",
    "annotation",
    "as",
    "break",
    "by",
    "catch",
    "class",
    "companion",
    "const",
    "constructor",
    "continue",
    "crossinline",
    "data",
    "do",
    "else",
    "enum",
    "extends",
    "external",
    "false",
    "final",
    "finally",
    "for",
    "fun",
    "get",
    "if",
    "implements",
    "import",
    "in",
    "infix",
    "init",
    "inline",
    "inner",
    "interface",
    "internal",
    "is",
    "lateinit",
    "new",
    "noinline",
    "null",
    "object",
    "open",
    "operator",
    "out",
    "override",
    "package",
    "private",
    "protected",
    "public",
    "reified",
    "return",
    "sealed",
    "set",
    "static",
    "super",
    "suspend",
    "tailrec",
    "this",
    "throw",
    "true",
    "try",
    "typealias",
    "val",
    "value",
    "var",
    "vararg",
    "void",
    "when",
    "where",
    "while",
];

pub fn is_keyword(word: &str) -> bool {
    KEYWORDS.binary_search(&word).is_ok()
}

fn ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

/// Splits `src` into tokens. Whitespace is dropped. Never fails: unknown bytes become `Punct`.
pub fn lex(src: &str) -> Vec<Token> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let start = i;
        let kind = if c.is_ascii_whitespace() {
            i += 1;
            continue;
        } else if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            Tok::Comment
        } else if b[i..].starts_with(b"/*") {
            i = src[i + 2..].find("*/").map_or(b.len(), |p| i + 2 + p + 2);
            Tok::Comment
        } else if b[i..].starts_with(b"\"\"\"") {
            i = src[i + 3..]
                .find("\"\"\"")
                .map_or(b.len(), |p| i + 3 + p + 3);
            Tok::Str
        } else if c == b'"' || c == b'\'' {
            i = skip_quoted(b, i);
            Tok::Str
        } else if c == b'@' && i + 1 < b.len() && ident_char(b[i + 1]) {
            i += 1;
            while i < b.len()
                && (ident_char(b[i]) || (b[i] == b'.' && i + 1 < b.len() && ident_char(b[i + 1])))
            {
                i += 1;
            }
            Tok::Annotation
        } else if c == b'`' {
            i = src[i + 1..].find('`').map_or(b.len(), |p| i + 1 + p + 1);
            Tok::Ident
        } else if c.is_ascii_digit() {
            while i < b.len()
                && (ident_char(b[i])
                    || (b[i] == b'.' && i + 1 < b.len() && b[i + 1].is_ascii_digit()))
            {
                i += 1;
            }
            Tok::Num
        } else if ident_char(c) {
            while i < b.len() && ident_char(b[i]) {
                i += 1;
            }
            if is_keyword(&src[start..i]) {
                Tok::Keyword
            } else {
                Tok::Ident
            }
        } else {
            i += src[i..].chars().next().map_or(1, char::len_utf8);
            Tok::Punct
        };
        out.push(Token {
            kind,
            range: start..i,
        });
    }
    out
}

/// Skips a single-line string or char literal, including `${ ... }` templates with nested braces.
fn skip_quoted(b: &[u8], start: usize) -> usize {
    let quote = b[start];
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'\n' => return i,
            c if c == quote => return i + 1,
            b'$' if quote == b'"' && b.get(i + 1) == Some(&b'{') => {
                let mut depth = 0;
                while i < b.len() {
                    match b[i] {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        b'"' => i = skip_quoted(b, i) - 1,
                        _ => {}
                    }
                    i += 1;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    b.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<(Tok, &str)> {
        lex(src)
            .into_iter()
            .map(|t| (t.kind, &src[t.range]))
            .collect()
    }

    #[test]
    fn keywords_are_sorted_for_binary_search() {
        assert!(KEYWORDS.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn lexes_annotations_strings_and_templates() {
        let t = kinds(r#"@Preview(name = "a ${x + "}"} b") fun `odd name`() // c"#);
        assert_eq!(t[0], (Tok::Annotation, "@Preview"));
        assert_eq!(t[4], (Tok::Str, r#""a ${x + "}"} b""#));
        assert_eq!(t[6], (Tok::Keyword, "fun"));
        assert_eq!(t[7], (Tok::Ident, "`odd name`"));
        assert_eq!(t.last().unwrap(), &(Tok::Comment, "// c"));
    }

    #[test]
    fn unterminated_block_comment_runs_to_end() {
        assert_eq!(
            kinds("a /* b"),
            vec![(Tok::Ident, "a"), (Tok::Comment, "/* b")]
        );
    }
}
