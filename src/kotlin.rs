//! Finds declarations in a Kotlin (or, for classes only, Java) file from the token stream.
//! Tree-sitter grammars misread `@Preview(...) private fun`, which Compose code uses everywhere,
//! so this scanner works on tokens and brace depth and never gives up on bad code.

use crate::lexer::{Tok, Token, lex};
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Class,
    Interface,
    Object,
    Enum,
    Fun,
    Property,
    TypeAlias,
}

impl Kind {
    pub fn is_type(self) -> bool {
        matches!(
            self,
            Kind::Class | Kind::Interface | Kind::Object | Kind::Enum | Kind::TypeAlias
        )
    }
    pub fn label(self) -> &'static str {
        match self {
            Kind::Class => "class",
            Kind::Interface => "interface",
            Kind::Object => "object",
            Kind::Enum => "enum",
            Kind::Fun => "fun",
            Kind::Property => "val",
            Kind::TypeAlias => "typealias",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: String,
    pub has_default: bool,
    /// `Some(true)` for a `var` constructor parameter, `Some(false)` for `val`.
    pub mutable: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct Decl {
    pub name: String,
    pub kind: Kind,
    pub container: Option<String>,
    /// Byte offset of the name.
    pub offset: usize,
    pub annotations: Vec<String>,
    pub modifiers: Vec<String>,
    pub params: Vec<Param>,
    /// Return type of a fun, or the declared type of a property.
    pub ty: String,
    /// `var` property.
    pub mutable: bool,
    /// Byte range of a fun body, including braces or the `=` expression.
    pub body: Option<Range<usize>>,
}

impl Decl {
    pub fn has_annotation(&self, name: &str) -> bool {
        self.annotations.iter().any(|a| a == name)
    }
    pub fn is_composable(&self) -> bool {
        self.kind == Kind::Fun && self.has_annotation("Composable")
    }
    /// `@Preview` or a multipreview such as `@PreviewLightDark` / `@PreviewScreenSizes`.
    pub fn is_preview(&self) -> bool {
        self.kind == Kind::Fun
            && self.annotations.iter().any(|a| {
                a.starts_with("Preview") || a.ends_with("Preview") || a.ends_with("Previews")
            })
    }
}

#[derive(Clone, Debug, Default)]
pub struct FileSyms {
    pub package: String,
    pub imports: Vec<String>,
    pub decls: Vec<Decl>,
}

const MODIFIERS: &[&str] = &[
    "abstract",
    "annotation",
    "companion",
    "const",
    "crossinline",
    "data",
    "enum",
    "external",
    "final",
    "infix",
    "inline",
    "inner",
    "internal",
    "lateinit",
    "noinline",
    "open",
    "operator",
    "override",
    "private",
    "protected",
    "public",
    "sealed",
    "static",
    "suspend",
    "tailrec",
    "value",
];

const DECL_START: &[&str] = &[
    "class",
    "constructor",
    "fun",
    "init",
    "interface",
    "object",
    "typealias",
    "val",
    "var",
];

struct Scanner<'a> {
    src: &'a str,
    t: Vec<Token>,
    i: usize,
}

impl<'a> Scanner<'a> {
    fn text(&self, i: usize) -> &'a str {
        self.t.get(i).map_or("", |t| &self.src[t.range.clone()])
    }
    fn is(&self, i: usize, s: &str) -> bool {
        self.text(i) == s
    }
    fn kind(&self, i: usize) -> Option<Tok> {
        self.t.get(i).map(|t| t.kind)
    }
    fn newline_before(&self, i: usize) -> bool {
        i == 0
            || i >= self.t.len()
            || self.src[self.t[i - 1].range.end..self.t[i].range.start].contains('\n')
    }
    /// An identifier, or a soft keyword used as a name (`get`, `value`, `data`).
    fn is_name(&self, i: usize) -> bool {
        self.kind(i) == Some(Tok::Ident)
            || (self.kind(i) == Some(Tok::Keyword)
                && !matches!(
                    self.text(i),
                    "fun" | "val" | "var" | "class" | "this" | "super"
                ))
    }
    /// A token that can only begin a new declaration.
    fn starts_decl(&self, i: usize) -> bool {
        let w = self.text(i);
        self.kind(i) == Some(Tok::Annotation) || MODIFIERS.contains(&w) || DECL_START.contains(&w)
    }
    /// Index just past the bracket that closes the one at `i` (`(`, `[`, `{` or `<`).
    fn skip_balanced(&self, i: usize) -> usize {
        let open = self.text(i);
        let close = match open {
            "(" => ")",
            "[" => "]",
            "{" => "}",
            _ => ">",
        };
        let mut depth = 0i32;
        let mut j = i;
        while j < self.t.len() {
            let w = self.text(j);
            if w == open {
                depth += 1;
            } else if w == close {
                depth -= 1;
                if depth == 0 {
                    return j + 1;
                }
            } else if open == "<" && (w == "{" || w == "=" || w == ";") {
                return j; // a less-than sign, not a type list
            }
            j += 1;
        }
        j
    }
    /// End of an expression (property initializer or `= expr` body): the next declaration on a new line,
    /// a `;`, or the `}` that closes the container.
    fn skip_expr(&self, mut j: usize) -> usize {
        let mut depth = 0i32;
        while j < self.t.len() {
            let w = self.text(j);
            if depth == 0
                && (w == "}"
                    || w == ";"
                    || (self.newline_before(j) && self.starts_decl(j) && !self.is(j, "object")))
            {
                return j;
            }
            match w {
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" => depth -= 1,
                _ => {}
            }
            j += 1;
        }
        j
    }
    fn dotted(&mut self) -> String {
        let mut s = String::new();
        while matches!(self.kind(self.i), Some(Tok::Ident | Tok::Keyword))
            || self.is(self.i, ".")
            || self.is(self.i, "*")
        {
            if self.i > 0 && self.newline_before(self.i) && !s.is_empty() {
                break;
            }
            s.push_str(self.text(self.i));
            self.i += 1;
        }
        s
    }
    /// Text of tokens `a..b`, with single spaces dropped around punctuation.
    fn join(&self, a: usize, b: usize) -> String {
        if a >= b || a >= self.t.len() {
            return String::new();
        }
        let b = b.min(self.t.len());
        self.src[self.t[a].range.start..self.t[b - 1].range.end]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
    /// Parses the parameter list whose `(` is at `self.i`; leaves `self.i` after `)`.
    fn params(&mut self) -> Vec<Param> {
        let end = self.skip_balanced(self.i);
        let mut out = Vec::new();
        let mut j = self.i + 1;
        while j < end.saturating_sub(1) {
            // one parameter runs to the next comma at depth 0
            let mut k = j;
            let mut depth = 0;
            while k < end - 1 {
                match self.text(k) {
                    "(" | "[" | "{" | "<" => depth += 1,
                    ")" | "]" | "}" | ">" => depth -= 1,
                    "," if depth == 0 => break,
                    _ => {}
                }
                k += 1;
            }
            let mut p = Param::default();
            let mut m = j;
            while m < k
                && (self.kind(m) == Some(Tok::Annotation)
                    || (self.kind(m) == Some(Tok::Keyword) && !self.is(m + 1, ":")))
            {
                match self.text(m) {
                    "val" => p.mutable = Some(false),
                    "var" => p.mutable = Some(true),
                    _ => {}
                }
                if self.kind(m) == Some(Tok::Annotation) && self.is(m + 1, "(") {
                    m = self.skip_balanced(m + 1);
                } else {
                    m += 1;
                }
            }
            if self.is_name(m) {
                p.name = self.text(m).trim_matches('`').to_string();
                let eq = (m..k).find(|&q| self.is(q, "=") && !self.is(q + 1, ">"));
                if self.is(m + 1, ":") {
                    p.ty = self.join(m + 2, eq.unwrap_or(k));
                }
                p.has_default = eq.is_some();
                out.push(p);
            }
            j = k + 1;
        }
        self.i = end;
        out
    }
}

pub fn scan(src: &str) -> FileSyms {
    let t: Vec<Token> = lex(src)
        .into_iter()
        .filter(|t| t.kind != Tok::Comment)
        .collect();
    let mut s = Scanner { src, t, i: 0 };
    let mut out = FileSyms::default();
    // Brace frames: Some(container) for a body that holds members, None for code blocks.
    let mut frames: Vec<Option<Option<String>>> = Vec::new();
    let mut pending_body: Option<String> = None;
    let mut annotations: Vec<String> = Vec::new();
    let mut modifiers: Vec<String> = Vec::new();

    while s.i < s.t.len() {
        let recording = frames.last().is_none_or(|f| f.is_some());
        let container = frames.iter().rev().find_map(|f| f.clone()).flatten();
        let w = s.text(s.i);
        let kind = s.kind(s.i);

        if kind == Some(Tok::Annotation) {
            let name = w.trim_start_matches('@');
            let name = name.rsplit('.').next().unwrap_or(name).to_string();
            let glued =
                s.t.get(s.i + 1)
                    .is_some_and(|n| n.range.start == s.t[s.i].range.end);
            s.i += 1;
            if s.is(s.i, ":") && glued {
                s.i += 2; // @file:JvmName / @get:Rule use-site targets
            }
            if s.is(s.i, "(") && s.t[s.i].range.start == s.t[s.i - 1].range.end {
                s.i = s.skip_balanced(s.i);
            }
            annotations.push(name);
            continue;
        }
        match w {
            "{" => {
                frames.push(pending_body.take().map(Some));
                s.i += 1;
                continue;
            }
            "}" => {
                frames.pop();
                pending_body = None;
                s.i += 1;
                continue;
            }
            _ => {}
        }
        if !recording {
            s.i += 1;
            continue;
        }
        if MODIFIERS.contains(&w) && kind == Some(Tok::Keyword) {
            modifiers.push(w.to_string());
            s.i += 1;
            continue;
        }
        let prev = if s.i > 0 { s.text(s.i - 1) } else { "" };
        match w {
            "package" if frames.is_empty() => {
                s.i += 1;
                out.package = s.dotted();
            }
            "import" if frames.is_empty() => {
                s.i += 1;
                out.imports.push(s.dotted());
            }
            "class" | "interface" | "object" if prev != ":" && prev != "." => {
                let decl_kw = w;
                s.i += 1;
                let name = if s.kind(s.i) == Some(Tok::Ident) {
                    let n = s.text(s.i).to_string();
                    s.i += 1;
                    Some(n)
                } else if decl_kw == "object" && modifiers.iter().any(|m| m == "companion") {
                    Some("Companion".to_string())
                } else {
                    None // object expression
                };
                let Some(name) = name else {
                    annotations.clear();
                    modifiers.clear();
                    continue;
                };
                let offset = s.t[s.i - 1].range.start;
                if s.is(s.i, "<") {
                    s.i = s.skip_balanced(s.i);
                }
                // `private constructor(` / `@Inject constructor(` on the header line
                while !s.newline_before(s.i)
                    && (s.kind(s.i) == Some(Tok::Annotation)
                        || MODIFIERS.contains(&s.text(s.i))
                        || s.is(s.i, "constructor"))
                {
                    s.i += 1; // private constructor / @Inject constructor
                }
                let params = if s.is(s.i, "(") {
                    s.params()
                } else {
                    Vec::new()
                };
                let kind = match decl_kw {
                    "interface" => Kind::Interface,
                    "object" => Kind::Object,
                    _ if modifiers.iter().any(|m| m == "enum") => Kind::Enum,
                    _ => Kind::Class,
                };
                for p in params.iter().filter(|p| p.mutable.is_some()) {
                    out.decls.push(Decl {
                        name: p.name.clone(),
                        kind: Kind::Property,
                        container: Some(name.clone()),
                        offset,
                        annotations: Vec::new(),
                        modifiers: Vec::new(),
                        params: Vec::new(),
                        ty: p.ty.clone(),
                        mutable: p.mutable == Some(true),
                        body: None,
                    });
                }
                out.decls.push(Decl {
                    name: name.clone(),
                    kind,
                    container: container.clone(),
                    offset,
                    annotations: std::mem::take(&mut annotations),
                    modifiers: std::mem::take(&mut modifiers),
                    params,
                    ty: String::new(),
                    mutable: false,
                    body: None,
                });
                // the header (supertypes, where clause) runs until the body brace or the next declaration
                let mut depth = 0;
                while s.i < s.t.len() {
                    let x = s.text(s.i);
                    if depth == 0
                        && (x == "{"
                            || x == "}"
                            || x == ";"
                            || (s.newline_before(s.i) && s.starts_decl(s.i)))
                    {
                        break;
                    }
                    match x {
                        "(" | "<" => depth += 1,
                        ")" | ">" => depth -= 1,
                        _ => {}
                    }
                    s.i += 1;
                }
                if s.is(s.i, "{") {
                    pending_body = Some(name);
                }
            }
            "fun" => {
                s.i += 1;
                if s.is(s.i, "<") {
                    s.i = s.skip_balanced(s.i);
                }
                let mut name_i = None;
                while s.i < s.t.len() && !s.is(s.i, "(") && !s.is(s.i, "{") {
                    if s.is(s.i, "<") {
                        s.i = s.skip_balanced(s.i);
                        continue;
                    }
                    if s.is_name(s.i) {
                        name_i = Some(s.i);
                    }
                    s.i += 1;
                }
                let Some(ni) = name_i.filter(|_| s.is(s.i, "(")) else {
                    annotations.clear();
                    modifiers.clear();
                    continue; // `fun interface` or broken code
                };
                let params = s.params();
                let mut ty = String::new();
                if s.is(s.i, ":") {
                    let a = s.i + 1;
                    let mut depth = 0;
                    while s.i < s.t.len() {
                        let x = s.text(s.i);
                        if depth == 0
                            && (x == "{"
                                || x == "="
                                || x == "where"
                                || x == "}"
                                || (s.newline_before(s.i) && s.starts_decl(s.i)))
                        {
                            break;
                        }
                        match x {
                            "(" | "<" => depth += 1,
                            ")" | ">" => depth -= 1,
                            _ => {}
                        }
                        s.i += 1;
                    }
                    ty = s.join(a, s.i);
                }
                if s.is(s.i, "where") {
                    while s.i < s.t.len()
                        && !s.is(s.i, "{")
                        && !s.is(s.i, "=")
                        && !(s.newline_before(s.i) && s.starts_decl(s.i))
                    {
                        s.i += 1;
                    }
                }
                let body = if s.is(s.i, "{") {
                    let a = s.t[s.i].range.start;
                    s.i = s.skip_balanced(s.i);
                    Some(a..s.t[s.i - 1].range.end)
                } else if s.is(s.i, "=") {
                    let a = s.t[s.i].range.start;
                    let end = s.skip_expr(s.i + 1);
                    s.i = end;
                    Some(a..s.t.get(end - 1).map_or(src.len(), |t| t.range.end))
                } else {
                    None
                };
                out.decls.push(Decl {
                    name: s.text(ni).trim_matches('`').to_string(),
                    kind: Kind::Fun,
                    container: container.clone(),
                    offset: s.t[ni].range.start,
                    annotations: std::mem::take(&mut annotations),
                    modifiers: std::mem::take(&mut modifiers),
                    params,
                    ty,
                    mutable: false,
                    body,
                });
            }
            "val" | "var" => {
                let mutable = w == "var";
                s.i += 1;
                if s.is(s.i, "<") {
                    s.i = s.skip_balanced(s.i);
                }
                let mut name_i = None;
                while s.i < s.t.len()
                    && !matches!(
                        s.text(s.i),
                        ":" | "=" | "by" | "{" | "}" | ";" | "(" | "get" | "set"
                    )
                    && !(name_i.is_some() && s.newline_before(s.i))
                {
                    if s.is_name(s.i) {
                        name_i = Some(s.i);
                    }
                    s.i += 1;
                }
                let mut ty = String::new();
                if s.is(s.i, ":") {
                    let a = s.i + 1;
                    let mut depth = 0;
                    while s.i < s.t.len() {
                        let x = s.text(s.i);
                        if depth == 0
                            && (matches!(x, "=" | "by" | "{" | "}" | ";")
                                || (s.i > a && s.newline_before(s.i)))
                        {
                            break;
                        }
                        match x {
                            "(" | "<" => depth += 1,
                            ")" | ">" => depth -= 1,
                            _ => {}
                        }
                        s.i += 1;
                    }
                    ty = s.join(a, s.i);
                }
                if s.is(s.i, "=") || s.is(s.i, "by") {
                    s.i = s.skip_expr(s.i + 1);
                }
                if let Some(ni) = name_i {
                    out.decls.push(Decl {
                        name: s.text(ni).trim_matches('`').to_string(),
                        kind: Kind::Property,
                        container: container.clone(),
                        offset: s.t[ni].range.start,
                        annotations: std::mem::take(&mut annotations),
                        modifiers: std::mem::take(&mut modifiers),
                        params: Vec::new(),
                        ty,
                        mutable,
                        body: None,
                    });
                }
                annotations.clear();
                modifiers.clear();
            }
            "typealias" => {
                s.i += 1;
                if s.kind(s.i) == Some(Tok::Ident) {
                    out.decls.push(Decl {
                        name: s.text(s.i).to_string(),
                        kind: Kind::TypeAlias,
                        container: container.clone(),
                        offset: s.t[s.i].range.start,
                        annotations: std::mem::take(&mut annotations),
                        modifiers: std::mem::take(&mut modifiers),
                        params: Vec::new(),
                        ty: String::new(),
                        mutable: false,
                        body: None,
                    });
                }
            }
            _ => {
                // enum entries: `A, B, C;` directly inside an enum body
                if kind == Some(Tok::Ident)
                    && let Some(c) = &container
                    && out
                        .decls
                        .iter()
                        .any(|d| &d.name == c && d.kind == Kind::Enum)
                    && matches!(prev, "{" | ",")
                {
                    out.decls.push(Decl {
                        name: w.to_string(),
                        kind: Kind::Property,
                        container: Some(c.clone()),
                        offset: s.t[s.i].range.start,
                        annotations: Vec::new(),
                        modifiers: Vec::new(),
                        params: Vec::new(),
                        ty: c.clone(),
                        mutable: false,
                        body: None,
                    });
                }
                annotations.clear();
                modifiers.clear();
                s.i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
package com.example.ui

import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier

data class User(val name: String, var age: Int)

object Repo { fun load(): List<User> = emptyList() }

interface Marker
@Composable fun AfterMarker() {}

interface Api { suspend fun get(id: Long): User
    fun set(value: String, data: Int) }

@Composable
fun UserCard(user: User, modifier: Modifier = Modifier, onClick: () -> Unit) {
    val x = remember { mutableStateOf(0) }
    fun local() {}
    Column(modifier) { Text(user.name) }
}

@Preview(name = "Card", showBackground = true)
@Composable
private fun UserCardPreview() { UserCard(User("a", 1), onClick = {}) }

class Vm @Inject constructor(private val api: Api) : ViewModel() {
    val state = MutableStateFlow(0)
    private var count: Int = 0
    enum class Kind { A, B }
    companion object { const val K = 1 }
    val Int.double get() = this * 2
}

fun <T> List<T>.second(): T = this[1]
typealias Users = List<User>
"#;

    fn find<'a>(f: &'a FileSyms, name: &str) -> &'a Decl {
        f.decls.iter().find(|d| d.name == name).unwrap_or_else(|| {
            panic!(
                "{name} missing: {:#?}",
                f.decls.iter().map(|d| &d.name).collect::<Vec<_>>()
            )
        })
    }

    #[test]
    fn scans_package_imports_and_declarations() {
        let f = scan(SAMPLE);
        assert_eq!(f.package, "com.example.ui");
        assert_eq!(
            f.imports,
            vec![
                "androidx.compose.runtime.Composable",
                "androidx.compose.ui.Modifier"
            ]
        );
        assert_eq!(find(&f, "User").kind, Kind::Class);
        assert_eq!(find(&f, "Repo").kind, Kind::Object);
        assert_eq!(find(&f, "load").container.as_deref(), Some("Repo"));
        assert_eq!(find(&f, "get").container.as_deref(), Some("Api"));
        assert_eq!(
            find(&f, "set")
                .params
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            vec!["value", "data"]
        );
        assert_eq!(find(&f, "Kind").kind, Kind::Enum);
        assert_eq!(find(&f, "A").container.as_deref(), Some("Kind"));
        assert_eq!(find(&f, "Companion").container.as_deref(), Some("Vm"));
        assert_eq!(find(&f, "K").container.as_deref(), Some("Companion"));
        assert_eq!(find(&f, "second").ty, "T");
        assert_eq!(find(&f, "Users").kind, Kind::TypeAlias);
        assert!(find(&f, "AfterMarker").is_composable());
        assert!(find(&f, "count").mutable);
        assert_eq!(find(&f, "count").ty, "Int");
        assert!(find(&f, "age").mutable);
        assert_eq!(find(&f, "double").container.as_deref(), Some("Vm"));
        assert!(
            !f.decls.iter().any(|d| d.name == "local" || d.name == "x"),
            "locals are not indexed"
        );
    }

    #[test]
    fn reads_preview_with_args_then_private_fun() {
        let f = scan(SAMPLE);
        let p = find(&f, "UserCardPreview");
        assert!(p.is_preview() && p.is_composable());
        assert_eq!(p.modifiers, vec!["private"]);
        let card = find(&f, "UserCard");
        assert!(card.is_composable() && !card.is_preview());
        assert_eq!(card.params.len(), 3);
        assert_eq!(
            card.params[1],
            Param {
                name: "modifier".into(),
                ty: "Modifier".into(),
                has_default: true,
                mutable: None
            }
        );
        assert_eq!(card.params[2].ty, "() -> Unit");
        let body = card.body.clone().unwrap();
        assert!(SAMPLE[body].contains("Column(modifier)"));
    }

    #[test]
    fn constructor_params_and_injected_constructor() {
        let f = scan(SAMPLE);
        let vm = find(&f, "Vm");
        assert_eq!(vm.params[0].name, "api");
        assert_eq!(find(&f, "state").container.as_deref(), Some("Vm"));
    }

    #[test]
    fn java_classes_are_found() {
        let f = scan(
            "package a.b;\nimport x.Y;\npublic final class Foo extends Bar implements Baz {\n  void run() {}\n  static class Inner {}\n}\n",
        );
        assert_eq!(f.package, "a.b");
        assert_eq!(find(&f, "Foo").kind, Kind::Class);
        assert_eq!(find(&f, "Inner").container.as_deref(), Some("Foo"));
    }

    #[test]
    fn class_literals_and_object_expressions_are_not_declarations() {
        let f = scan(
            "val k = Foo::class.java\nval l = object : Runnable { override fun run() {} }\nfun g() = 1\n",
        );
        let names: Vec<_> = f.decls.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["k", "l", "g"]);
    }
}
