//! Which declaration a name means, read from the code around it like a compiler front end does,
//! without a compiler: locals and parameters first, then members of the implicit receivers (the
//! enclosing classes and `apply` / `with` / `run` lambdas, with their supertypes and companions),
//! then imports (explicit, aliases, `*`), the file's package and nested classes. A `receiver.name`
//! is looked up in the receiver's type, which is read from declarations (`val x: T`, `val x = T()`,
//! `by viewModels<T>()`, a function's return type, `it` of `let` / `forEach` / `update`, casts),
//! then in the extension functions visible in the file. Overloads are told apart by their number
//! of arguments and named arguments. A call through a subclass that overrides counts as a use of
//! the overridden declaration too.
//!
//! The work is per file and on demand: a [`Resolver`] reads the files it needs (open tabs first)
//! and caches what it learned for one job. The app runs it on its one background worker.

use crate::index::Index;
use crate::kotlin::{Decl, FileSyms, Kind, scan_file};
use crate::lexer::{Tok, Token, lex};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

/// A declaration: its file and the offset of its name.
pub type DeclRef = (PathBuf, usize);

/// What a name at a place means.
#[derive(Clone, Debug, PartialEq)]
pub enum Res {
    /// Project declarations (more than one when the code cannot tell overloads apart, or an
    /// import names every overload).
    Decls(Vec<DeclRef>),
    /// A local variable, parameter or lambda parameter declared at this offset of the same file.
    Local(usize),
    /// Not a project declaration (a library), or not found.
    None,
}

#[derive(Clone, Debug, PartialEq)]
enum Ty {
    Decl(DeclRef),
    /// A type that is not in the project (`String`, `List<User>`), by simple name.
    Lib(String, Vec<Ty>),
    /// A type parameter or anything: matches every receiver.
    Any,
}

impl Ty {
    fn arg(&self, i: usize) -> Option<Ty> {
        match self {
            Ty::Lib(_, a) => a.get(i).cloned(),
            _ => None,
        }
    }
    fn lib(&self) -> Option<&str> {
        match self {
            Ty::Lib(n, _) => Some(n),
            _ => None,
        }
    }
}

/// A local declaration: `val x`, a parameter, a lambda parameter, a `for` or `catch` variable.
#[derive(Debug)]
struct Local {
    name: String,
    at: usize,
    scope: Range<usize>,
    ty: Option<String>,
    /// Token index where its initializer starts (`= expr`), or the iterable of a `for`.
    init: Option<usize>,
    for_in: bool,
    /// Token index of the `{` of the lambda it is a parameter of, and its position.
    lambda: Option<(usize, usize)>,
}

struct Imports {
    /// Simple name (or alias) -> qualified names.
    named: HashMap<String, Vec<String>>,
    /// `pkg.*` prefixes, without the `*`.
    stars: Vec<String>,
}

/// One file, read once per job.
pub struct FileInfo {
    pub path: PathBuf,
    pub text: Rc<str>,
    pub syms: FileSyms,
    toks: Vec<Token>,
    /// For each `(`, `[`, `{` its closing token, and back.
    pairs: Vec<u32>,
    /// Body (`{ … }`) of each class, by declaration index.
    bodies: HashMap<usize, Range<usize>>,
    locals: Vec<Local>,
    imports: Imports,
    java: bool,
}

const NONE: u32 = u32::MAX;

impl FileInfo {
    pub fn new(path: &Path, text: Rc<str>) -> FileInfo {
        let syms = scan_file(path, &text);
        let toks: Vec<Token> = lex(&text)
            .into_iter()
            .filter(|t| t.kind != Tok::Comment)
            .collect();
        let mut pairs = vec![NONE; toks.len()];
        let mut stack = Vec::new();
        for (i, t) in toks.iter().enumerate() {
            match &text[t.range.clone()] {
                "(" | "[" | "{" => stack.push(i),
                ")" | "]" | "}" => {
                    if let Some(o) = stack.pop() {
                        pairs[o] = i as u32;
                        pairs[i] = o as u32;
                    }
                }
                _ => {}
            }
        }
        let java = path.extension().is_some_and(|e| e == "java");
        let mut f = FileInfo {
            path: path.to_path_buf(),
            text,
            syms,
            toks,
            pairs,
            bodies: HashMap::new(),
            locals: Vec::new(),
            imports: Imports {
                named: HashMap::new(),
                stars: Vec::new(),
            },
            java,
        };
        f.bodies = f.class_bodies();
        f.imports = f.read_imports();
        f.locals = f.read_locals();
        f
    }

    fn t(&self, i: usize) -> &str {
        self.toks.get(i).map_or("", |t| &self.text[t.range.clone()])
    }
    fn kind(&self, i: usize) -> Option<Tok> {
        self.toks.get(i).map(|t| t.kind)
    }
    fn start(&self, i: usize) -> usize {
        self.toks.get(i).map_or(self.text.len(), |t| t.range.start)
    }
    fn end(&self, i: usize) -> usize {
        self.toks.get(i).map_or(self.text.len(), |t| t.range.end)
    }
    fn pair(&self, i: usize) -> Option<usize> {
        self.pairs
            .get(i)
            .filter(|&&p| p != NONE)
            .map(|&p| p as usize)
    }
    /// The token that starts at byte `at`.
    pub fn tok_at(&self, at: usize) -> Option<usize> {
        let i = self.toks.partition_point(|t| t.range.start < at);
        (self.start(i) == at).then_some(i)
    }
    fn newline_before(&self, i: usize) -> bool {
        i == 0 || self.text[self.end(i - 1)..self.start(i)].contains('\n')
    }
    fn ident(&self, i: usize) -> bool {
        matches!(self.kind(i), Some(Tok::Ident))
            || self.kind(i) == Some(Tok::Keyword)
                && matches!(
                    self.t(i),
                    "value" | "data" | "get" | "set" | "field" | "by" | "out" | "in"
                )
    }
    fn name(&self, i: usize) -> &str {
        self.t(i).trim_matches('`')
    }

    /// Each class from its name to the end of its body (or of its header when it has none):
    /// the constructor's parameters and nested classes are in it.
    fn class_bodies(&self) -> HashMap<usize, Range<usize>> {
        let mut out = HashMap::new();
        for (di, d) in self.syms.decls.iter().enumerate() {
            if !d.kind.is_type() || d.kind == Kind::TypeAlias {
                continue;
            }
            let Some(mut i) = self.tok_at(d.offset) else {
                continue;
            };
            i += 1;
            let mut depth = 0i32;
            let mut end = d.offset + d.name.len();
            while i < self.toks.len() {
                match self.t(i) {
                    "(" | "[" => {
                        if let Some(e) = self.pair(i) {
                            end = self.end(e);
                            i = e + 1;
                            continue;
                        }
                    }
                    "<" => depth += 1,
                    ">" => depth -= 1,
                    "{" if depth <= 0 => {
                        if let Some(e) = self.pair(i) {
                            end = self.end(e);
                        }
                        break;
                    }
                    "}" | ";" | "=" if depth <= 0 => break,
                    _ if depth <= 0
                        && self.newline_before(i)
                        && !matches!(self.t(i), ":" | "," | "where")
                        && !matches!(self.t(i - 1), ":" | ",") =>
                    {
                        break;
                    }
                    _ => {}
                }
                end = self.end(i);
                i += 1;
            }
            out.insert(di, d.offset..end);
        }
        out
    }

    fn read_imports(&self) -> Imports {
        let mut im = Imports {
            named: HashMap::new(),
            stars: Vec::new(),
        };
        let mut i = 0;
        while i < self.toks.len() {
            if self.t(i) != "import" || !self.newline_before(i) {
                i += 1;
                continue;
            }
            i += 1;
            if self.t(i) == "static" {
                i += 1;
            }
            let mut parts = Vec::new();
            while i < self.toks.len() && !self.newline_before(i)
                || parts.is_empty() && i < self.toks.len()
            {
                match self.t(i) {
                    "." | ";" => {}
                    "*" => parts.push("*".to_string()),
                    "as" => break,
                    _ if self.ident(i) || self.kind(i) == Some(Tok::Keyword) => {
                        parts.push(self.name(i).to_string())
                    }
                    _ => break,
                }
                i += 1;
                if i < self.toks.len() && self.newline_before(i) {
                    break;
                }
            }
            let alias = (self.t(i) == "as").then(|| self.name(i + 1).to_string());
            if parts.last().is_some_and(|p| p == "*") {
                parts.pop();
                im.stars.push(parts.join("."));
            } else if let Some(last) = parts.last() {
                let key = alias.unwrap_or_else(|| last.clone());
                im.named.entry(key).or_default().push(parts.join("."));
            }
        }
        im
    }

    /// The token range a statement or expression that starts at `i` runs over: to the end of the
    /// line unless the next line goes on with an operator, at bracket depth 0.
    fn expr_end(&self, i: usize, limit: usize) -> usize {
        let mut j = i;
        while j < limit.min(self.toks.len()) {
            match self.t(j) {
                "(" | "[" | "{" => match self.pair(j) {
                    Some(p) => {
                        j = p + 1;
                        continue;
                    }
                    None => return j,
                },
                ")" | "]" | "}" | ";" => return j,
                "," if !self.java => return j,
                _ => {}
            }
            if j > i
                && self.newline_before(j)
                && !matches!(
                    self.t(j),
                    "." | "?" | ":" | "+" | "-" | "*" | "/" | "&" | "|" | "=" | "as"
                )
                && !matches!(
                    self.t(j - 1),
                    "." | "=" | "(" | "," | "+" | "-" | "&" | "|" | "->"
                )
            {
                return j;
            }
            j += 1;
        }
        j
    }

    /// Innermost `{ … }` (token indexes) around token `i`.
    fn block_around(&self, i: usize) -> Option<(usize, usize)> {
        let mut j = i;
        let mut depth = 0;
        while j > 0 {
            j -= 1;
            match self.t(j) {
                "}" | ")" | "]" => {
                    if let Some(p) = self.pair(j) {
                        j = p;
                        continue;
                    }
                    depth += 1;
                }
                "{" => {
                    if depth == 0 {
                        return Some((j, self.pair(j)?));
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
        None
    }

    fn scope_of(&self, i: usize) -> Range<usize> {
        match self.block_around(i) {
            Some((_, e)) => self.start(i)..self.end(e),
            None => self.start(i)..self.text.len(),
        }
    }

    fn read_locals(&self) -> Vec<Local> {
        let members: HashSet<usize> = self.syms.decls.iter().map(|d| d.offset).collect();
        let mut out = Vec::new();
        let type_until = |f: &FileInfo, mut j: usize, stop: &[&str]| -> (String, usize) {
            let a = j;
            let mut depth = 0;
            while j < f.toks.len() {
                let w = f.t(j);
                if depth == 0 && (stop.contains(&w) || f.newline_before(j) && j > a) {
                    break;
                }
                match w {
                    "(" | "<" => depth += 1,
                    ")" | ">" => {
                        if depth == 0 {
                            break;
                        }
                        depth -= 1
                    }
                    _ => {}
                }
                j += 1;
            }
            let s = if j > a {
                f.text[f.start(a)..f.end(j - 1)].to_string()
            } else {
                String::new()
            };
            (s, j)
        };
        // parameters of every function, with the function's body as their scope
        for d in &self.syms.decls {
            if let Some(b) = &d.body {
                for p in &d.params {
                    out.push(Local {
                        name: p.name.clone(),
                        at: p.offset as usize,
                        scope: p.offset as usize..b.end,
                        ty: Some(p.ty.clone()),
                        init: None,
                        for_in: false,
                        lambda: None,
                    });
                }
            }
            // plain constructor parameters: seen by initializers in the class body
            if d.kind.is_type() {
                let end = self
                    .bodies
                    .get(
                        &self
                            .syms
                            .decls
                            .iter()
                            .position(|x| std::ptr::eq(x, d))
                            .unwrap_or(0),
                    )
                    .map_or(d.offset, |r| r.end);
                for p in d
                    .params
                    .iter()
                    .filter(|p| p.mutable.is_none() && !self.java)
                {
                    out.push(Local {
                        name: p.name.clone(),
                        at: p.offset as usize,
                        scope: p.offset as usize..end,
                        ty: Some(p.ty.clone()),
                        init: None,
                        for_in: false,
                        lambda: None,
                    });
                }
            }
        }
        let n = self.toks.len();
        let mut i = 0;
        while i < n {
            let w = self.t(i);
            if self.java {
                // `Type name =`, `Type name;`, `Type name :` (for-each), inside a method body
                if self.ident(i)
                    && self.ident(i + 1)
                    && matches!(self.t(i + 2), "=" | ";" | ":" | ",")
                    && !members.contains(&self.start(i + 1))
                    && self.block_around(i).is_some()
                    && (i == 0
                        || matches!(self.t(i - 1), "{" | ";" | "(" | "}" | "final" | ">" | "]"))
                    && !matches!(self.t(i), "return" | "throw" | "new" | "else")
                {
                    out.push(Local {
                        name: self.name(i + 1).to_string(),
                        at: self.start(i + 1),
                        scope: self.scope_of(i + 1),
                        ty: Some(self.t(i).to_string()),
                        init: (self.t(i + 2) == "=").then_some(i + 3),
                        for_in: self.t(i + 2) == ":",
                        lambda: None,
                    });
                }
                i += 1;
                continue;
            }
            match w {
                "val" | "var" => {
                    let j = i + 1;
                    if self.t(j) == "(" {
                        // destructuring
                        let e = self.pair(j).unwrap_or(j);
                        for k in j + 1..e {
                            if self.ident(k) && matches!(self.t(k + 1), "," | ")" | ":") {
                                out.push(Local {
                                    name: self.name(k).to_string(),
                                    at: self.start(k),
                                    scope: self.scope_of(k),
                                    ty: None,
                                    init: None,
                                    for_in: false,
                                    lambda: None,
                                });
                            }
                        }
                    } else if self.ident(j)
                        && !members.contains(&self.start(j))
                        && self.block_around(j).is_some()
                    {
                        let (ty, after) = if self.t(j + 1) == ":" {
                            let (t, a) = type_until(self, j + 2, &["=", "by", "{", "}", ";"]);
                            (Some(t), a)
                        } else {
                            (None, j + 1)
                        };
                        let init = match self.t(after) {
                            "=" => Some(after + 1),
                            "by" => Some(after),
                            _ => None,
                        };
                        // a `val` in the parameter list of a `for`
                        out.push(Local {
                            name: self.name(j).to_string(),
                            at: self.start(j),
                            scope: self.scope_of(j),
                            ty,
                            init,
                            for_in: false,
                            lambda: None,
                        });
                    }
                }
                "for" if self.t(i + 1) == "(" => {
                    let close = self.pair(i + 1).unwrap_or(i + 1);
                    let in_at = (i + 2..close).find(|&k| self.t(k) == "in");
                    if let Some(in_at) = in_at {
                        let body_end = if self.t(close + 1) == "{" {
                            self.pair(close + 1)
                                .map_or(self.text.len(), |e| self.end(e))
                        } else {
                            self.scope_of(i).end
                        };
                        for k in i + 2..in_at {
                            if self.ident(k) && matches!(self.t(k + 1), "," | ")" | "in" | ":") {
                                out.push(Local {
                                    name: self.name(k).to_string(),
                                    at: self.start(k),
                                    scope: self.start(k)..body_end,
                                    ty: (self.t(k + 1) == ":").then(|| self.t(k + 2).to_string()),
                                    init: Some(in_at + 1),
                                    for_in: in_at == k + 1,
                                    lambda: None,
                                });
                            }
                        }
                    }
                }
                "catch" if self.t(i + 1) == "(" && self.ident(i + 2) && self.t(i + 3) == ":" => {
                    let close = self.pair(i + 1).unwrap_or(i + 4);
                    let end = self
                        .pair(close + 1)
                        .map_or(self.text.len(), |e| self.end(e));
                    out.push(Local {
                        name: self.name(i + 2).to_string(),
                        at: self.start(i + 2),
                        scope: self.start(i + 2)..end,
                        ty: Some(self.t(i + 4).to_string()),
                        init: None,
                        for_in: false,
                        lambda: None,
                    });
                }
                "fun" => {
                    // a local function's parameters (members' are in `syms`)
                    let mut j = i + 1;
                    while j < n && self.t(j) != "(" && self.t(j) != "{" && j < i + 12 {
                        j += 1;
                    }
                    if self.t(j) == "(" && !members.contains(&self.start(j - 1)) {
                        let close = self.pair(j).unwrap_or(j);
                        let mut k = close + 1;
                        while k < n && !matches!(self.t(k), "{" | "=") && !self.newline_before(k) {
                            k += 1;
                        }
                        let end = match self.t(k) {
                            "{" => self.pair(k).map_or(self.text.len(), |e| self.end(e)),
                            _ => self.scope_of(i).end,
                        };
                        let mut depth = 0;
                        for p in j + 1..close {
                            match self.t(p) {
                                "(" | "<" | "[" => depth += 1,
                                ")" | ">" | "]" => depth -= 1,
                                _ => {}
                            }
                            if depth == 0
                                && self.ident(p)
                                && self.t(p + 1) == ":"
                                && matches!(self.t(p - 1), "(" | ",")
                            {
                                let (ty, _) = type_until(self, p + 2, &[",", "=", ")"]);
                                out.push(Local {
                                    name: self.name(p).to_string(),
                                    at: self.start(p),
                                    scope: self.start(p)..end,
                                    ty: Some(ty),
                                    init: None,
                                    for_in: false,
                                    lambda: None,
                                });
                            }
                        }
                    }
                }
                "{" => {
                    // lambda parameters: `{ a, (b, c), d: T -> … }`
                    if let Some(close) = self.pair(i)
                        && !self.is_when_block(i)
                    {
                        let mut k = i + 1;
                        let mut names = Vec::new();
                        let mut pos = 0;
                        let mut ok = false;
                        while k < close {
                            match self.t(k) {
                                "-" if self.t(k + 1) == ">" => {
                                    ok = true;
                                    break;
                                }
                                "," => pos += 1,
                                "(" | ")" | "." | "<" | ">" | "?" => {}
                                ":" => {
                                    // a typed parameter: skip its type
                                    k += 1;
                                    while k < close
                                        && self.t(k) != ","
                                        && !(self.t(k) == "-" && self.t(k + 1) == ">")
                                    {
                                        k += 1;
                                    }
                                    continue;
                                }
                                _ if self.ident(k) || self.t(k) == "_" => names.push((k, pos)),
                                _ => break,
                            }
                            k += 1;
                        }
                        if ok && !names.is_empty() {
                            for (k, pos) in names {
                                let typed = self.t(k + 1) == ":";
                                out.push(Local {
                                    name: self.name(k).to_string(),
                                    at: self.start(k),
                                    scope: self.start(k)..self.end(close),
                                    ty: typed.then(|| self.t(k + 2).to_string()),
                                    init: None,
                                    for_in: false,
                                    lambda: Some((i, pos)),
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
            i += 1;
        }
        out
    }

    /// `when (x) {` or `when {`: its `{ A -> … }` are branches, not lambda parameters.
    fn is_when_block(&self, open: usize) -> bool {
        if open == 0 {
            return false;
        }
        let p = open - 1;
        self.t(p) == "when"
            || self.t(p) == ")"
                && self
                    .pair(p)
                    .is_some_and(|o| o > 0 && self.t(o - 1) == "when")
    }

    /// The local declaration of `name` visible at byte `at`, innermost first.
    fn local(&self, name: &str, at: usize) -> Option<&Local> {
        self.locals
            .iter()
            .filter(|l| l.name == name && l.scope.contains(&at) && l.at <= at)
            .max_by_key(|l| l.at)
    }

    /// Classes whose body holds `at`, innermost first.
    fn classes_at(&self, at: usize) -> Vec<usize> {
        let mut v: Vec<(usize, usize)> = self
            .bodies
            .iter()
            .filter(|(_, r)| r.start < at && at < r.end)
            .map(|(&i, r)| (i, r.start))
            .collect();
        v.sort_by_key(|&(_, s)| std::cmp::Reverse(s));
        v.into_iter().map(|(i, _)| i).collect()
    }

    /// The function whose body holds `at`, innermost.
    fn fun_at(&self, at: usize) -> Option<usize> {
        self.syms
            .decls
            .iter()
            .enumerate()
            .filter(|(_, d)| d.body.as_ref().is_some_and(|b| b.contains(&at)))
            .max_by_key(|(_, d)| d.offset)
            .map(|(i, _)| i)
    }

    /// `fun Recv.name`: the receiver type text of a declaration, if it is an extension.
    fn receiver_text(&self, d: &Decl) -> Option<String> {
        let i = self.tok_at(d.offset)?;
        if i == 0 || self.t(i - 1) != "." {
            return None;
        }
        let mut j = i - 1;
        while j > 0 && !matches!(self.t(j - 1), "fun" | "val" | "var") {
            j -= 1;
            if self.t(j) == ">" || self.t(j) == ")" {
                // `fun <T> Recv<T>.x` / `(A) -> B`: walk over the brackets
                let mut depth = 0;
                while j > 0 {
                    match self.t(j) {
                        ">" | ")" => depth += 1,
                        "<" | "(" => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    j -= 1;
                }
            }
        }
        // skip the fun's own type parameters
        if self.t(j) == "<" {
            let mut depth = 0;
            while j < i {
                match self.t(j) {
                    "<" => depth += 1,
                    ">" => {
                        depth -= 1;
                        if depth == 0 {
                            j += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
        }
        (j < i - 1).then(|| self.text[self.start(j)..self.end(i - 2)].to_string())
    }

    /// Type parameters of the declaration (`fun <T, R> …`).
    fn type_params(&self, d: &Decl) -> Vec<String> {
        let Some(i) = self.tok_at(d.offset) else {
            return Vec::new();
        };
        let mut j = i;
        while j > 0 && !matches!(self.t(j), "fun" | "val" | "var" | "class" | "interface") {
            j -= 1;
        }
        let mut out = Vec::new();
        if self.t(j + 1) == "<" {
            let mut k = j + 2;
            while k < i && self.t(k) != ">" {
                if self.ident(k) && matches!(self.t(k + 1), "," | ">" | ":") {
                    out.push(self.t(k).to_string());
                }
                k += 1;
            }
        }
        out
    }
}

/// The arguments of a call, as far as they tell overloads apart.
#[derive(Clone, Debug, Default)]
struct Args {
    count: usize,
    named: Vec<String>,
    lambda: bool,
}

fn fits(d: &Decl, args: &Args) -> bool {
    if d.kind != Kind::Fun {
        return true;
    }
    let ps = &d.params;
    let required = ps.iter().filter(|p| !p.has_default).count();
    let java_vararg = ps.last().is_some_and(|p| p.ty.ends_with("[]"));
    (args.count <= ps.len() || java_vararg)
        && args.count >= required
        && args.named.iter().all(|a| ps.iter().any(|p| p.name == *a))
}

/// Reads files for a resolver: open tabs first, then the disk.
pub struct Texts {
    pub open: HashMap<PathBuf, Arc<str>>,
}

impl Texts {
    pub fn read(&self, p: &Path) -> Option<Rc<str>> {
        match self.open.get(p) {
            Some(t) => Some(Rc::from(&**t)),
            None => std::fs::read_to_string(p).ok().map(Rc::from),
        }
    }
}

/// What a resolver learned (files read, types found): kept across short index locks, so the
/// worker can let the editor update the index between chunks of a long job.
#[derive(Default)]
pub struct Cache {
    files: RefCell<HashMap<PathBuf, Option<Rc<FileInfo>>>>,
    types: RefCell<HashMap<(PathBuf, usize), Option<Ty>>>,
    packages: RefCell<Option<HashSet<String>>>,
    depth: RefCell<usize>,
}

enum CacheRef<'a> {
    #[cfg(test)]
    Own(Box<Cache>),
    Shared(&'a Cache),
}

impl std::ops::Deref for CacheRef<'_> {
    type Target = Cache;
    fn deref(&self) -> &Cache {
        match self {
            #[cfg(test)]
            CacheRef::Own(c) => c,
            CacheRef::Shared(c) => c,
        }
    }
}

/// Answers "what does this name mean" for one job (one opened file, one search).
pub struct Resolver<'a> {
    pub index: &'a Index,
    texts: &'a Texts,
    c: CacheRef<'a>,
}

impl<'a> Resolver<'a> {
    #[cfg(test)]
    pub fn new(index: &'a Index, texts: &'a Texts) -> Resolver<'a> {
        Resolver {
            index,
            texts,
            c: CacheRef::Own(Box::default()),
        }
    }

    /// A resolver that keeps what it learns in `cache`.
    pub fn with(index: &'a Index, texts: &'a Texts, cache: &'a Cache) -> Resolver<'a> {
        Resolver {
            index,
            texts,
            c: CacheRef::Shared(cache),
        }
    }

    /// Drops what was read for other files (memory of a long search), keeping `keep`.
    pub fn forget(&self, keep: usize) {
        let mut f = self.c.files.borrow_mut();
        if f.len() > keep {
            f.clear();
        }
    }

    pub fn file(&self, p: &Path) -> Option<Rc<FileInfo>> {
        if let Some(f) = self.c.files.borrow().get(p) {
            return f.clone();
        }
        let f = self.texts.read(p).map(|t| Rc::new(FileInfo::new(p, t)));
        self.c.files.borrow_mut().insert(p.to_path_buf(), f.clone());
        f
    }

    fn decl(&self, r: &DeclRef) -> Option<(Rc<FileInfo>, usize)> {
        let f = self.file(&r.0)?;
        let i = f.syms.decls.iter().position(|d| d.offset == r.1)?;
        Some((f, i))
    }

    fn guard(&self) -> Option<DepthGuard<'_>> {
        let mut d = self.c.depth.borrow_mut();
        if *d > 24 {
            return None;
        }
        *d += 1;
        Some(DepthGuard(&self.c.depth))
    }

    // ---------------------------------------------------------------- types by name

    fn is_package(&self, p: &str) -> bool {
        let mut cache = self.c.packages.borrow_mut();
        let set = cache.get_or_insert_with(|| {
            let mut s = HashSet::new();
            for f in self.index.files.values() {
                let mut p = f.package.as_str();
                s.insert(p.to_string());
                while let Some((a, _)) = p.rsplit_once('.') {
                    s.insert(a.to_string());
                    p = a;
                }
            }
            s
        });
        set.contains(p)
    }

    /// Declarations whose qualified name is `fqn` (`pkg.Outer.Inner`, `pkg.Outer.Companion.X`).
    fn by_fqn(&self, fqn: &str) -> Vec<DeclRef> {
        let (head, name) = fqn.rsplit_once('.').unwrap_or(("", fqn));
        let mut out = Vec::new();
        for (p, f, d) in self.index.named(name) {
            if self.qualified_parent(f, d) == head {
                out.push((p.clone(), d.offset));
            }
        }
        out
    }

    /// `pkg.Outer` of a member of `Outer` (its container chain), `pkg` of a top-level one.
    fn qualified_parent(&self, f: &FileSyms, d: &Decl) -> String {
        let mut chain = Vec::new();
        let mut c = d.container.clone();
        let mut before = d.offset;
        while let Some(name) = c {
            chain.push(name.to_string());
            let outer = f
                .decls
                .iter()
                .filter(|x| x.kind.is_type() && *x.name == *name && x.offset < before)
                .max_by_key(|x| x.offset);
            match outer {
                Some(o) => {
                    before = o.offset;
                    c = o.container.clone();
                }
                None => break,
            }
            if chain.len() > 8 {
                break;
            }
        }
        let mut parts: Vec<&str> = Vec::new();
        if !f.package.is_empty() {
            parts.push(&f.package);
        }
        parts.extend(chain.iter().rev().map(String::as_str));
        parts.join(".")
    }

    /// Types named `name` that the code at `at` of `f` sees, best first.
    fn visible_types(&self, f: &FileInfo, name: &str, at: usize) -> Vec<DeclRef> {
        // nested in the enclosing classes or their supertypes
        for c in f.classes_at(at) {
            let r = (f.path.clone(), f.syms.decls[c].offset);
            for t in self.hierarchy_up(&Ty::Decl(r)) {
                if let Ty::Decl(t) = t {
                    let found: Vec<DeclRef> = self
                        .members(&t)
                        .into_iter()
                        .filter(|(m, d)| d.kind.is_type() && d.name == name && m.0 == t.0)
                        .map(|(m, _)| m)
                        .collect();
                    if !found.is_empty() {
                        return found;
                    }
                }
            }
        }
        self.top_level(f, name, true)
    }

    /// Top-level declarations named `name` visible in `f`: imports, the file, its package, `*`.
    fn top_level(&self, f: &FileInfo, name: &str, types: bool) -> Vec<DeclRef> {
        let want = |d: &Decl| d.kind.is_type() == types || !types && d.kind == Kind::Fun;
        if let Some(fqns) = f.imports.named.get(name) {
            let found: Vec<DeclRef> = fqns
                .iter()
                .flat_map(|q| self.by_fqn(q))
                .filter(|r| {
                    self.decl(r)
                        .is_some_and(|(g, i)| want(&g.syms.decls[i]) || types)
                })
                .collect();
            if !found.is_empty() {
                return found;
            }
            if fqns.iter().any(|q| !q.ends_with(&format!(".{name}"))) {
                return Vec::new(); // an alias of something outside the project
            }
        }
        let mut here = Vec::new();
        let mut star = Vec::new();
        for (p, g, d) in self.index.named(name) {
            if !want(d) || d.container.is_some() && !(types && f.java) {
                continue;
            }
            if d.container.is_some() {
                continue;
            }
            if g.package == f.syms.package || *p == f.path {
                here.push((p.clone(), d.offset));
            } else if f.imports.stars.contains(&g.package) {
                star.push((p.clone(), d.offset));
            }
        }
        // the open file's own text may differ from the index
        if f.syms
            .decls
            .iter()
            .any(|d| d.name == name && d.container.is_none() && want(d))
        {
            here.retain(|r| r.0 != f.path);
            here.extend(
                f.syms
                    .decls
                    .iter()
                    .filter(|d| d.name == name && d.container.is_none() && want(d))
                    .map(|d| (f.path.clone(), d.offset)),
            );
        }
        if !here.is_empty() { here } else { star }
    }

    /// A type written as text in file `f` at `at`: `List<User>?`, `Outer.Inner`, `pkg.Type`.
    fn type_text(&self, f: &FileInfo, text: &str, at: usize, tparams: &[String]) -> Option<Ty> {
        let _g = self.guard()?;
        let t = text.trim().trim_end_matches('?').trim();
        let t = t.strip_prefix("vararg ").unwrap_or(t).trim();
        if t.is_empty() {
            return None;
        }
        if t.starts_with('(') || t.contains("->") {
            return Some(Ty::Lib("Function".into(), Vec::new()));
        }
        let (base, args) = match t.find('<') {
            Some(i) if t.ends_with('>') => (&t[..i], split_top(&t[i + 1..t.len() - 1])),
            _ => (t, Vec::new()),
        };
        let base = base.trim().trim_end_matches("[]");
        let args: Vec<Ty> = args
            .iter()
            .map(|a| {
                let a = a
                    .trim()
                    .trim_start_matches("out ")
                    .trim_start_matches("in ");
                if a == "*" {
                    Ty::Any
                } else {
                    self.type_text(f, a, at, tparams).unwrap_or(Ty::Any)
                }
            })
            .collect();
        if tparams.iter().any(|p| p == base) {
            return Some(Ty::Any);
        }
        let segs: Vec<&str> = base.split('.').map(str::trim).collect();
        // `pkg.a.Type`
        let mut start = 0;
        let mut found: Vec<DeclRef> = Vec::new();
        for k in (1..segs.len()).rev() {
            let pkg = segs[..k].join(".");
            if self.is_package(&pkg) {
                found = self.by_fqn(&segs[..=k].join("."));
                start = k;
                break;
            }
        }
        if found.is_empty() {
            start = 0;
            found = self.visible_types(f, segs[0], at);
        }
        let mut cur = found.into_iter().next();
        for s in &segs[start + 1..] {
            let Some(c) = cur.clone() else { break };
            cur = self
                .members(&c)
                .into_iter()
                .find(|(_, d)| d.kind.is_type() && d.name == *s)
                .map(|(r, _)| r);
        }
        match cur {
            Some(r) => {
                // a typealias is its aliased type
                if let Some((g, i)) = self.decl(&r)
                    && g.syms.decls[i].kind == Kind::TypeAlias
                {
                    let ti = g.tok_at(r.1)?;
                    let e = g.expr_end(ti + 2, g.toks.len());
                    let aliased = g.text[g.start(ti + 2)..g.end(e.saturating_sub(1))].to_string();
                    return self.type_text(&g, &aliased, r.1, &[]);
                }
                Some(Ty::Decl(r))
            }
            None => Some(Ty::Lib(segs.last()?.to_string(), args)),
        }
    }

    // ---------------------------------------------------------------- members and hierarchy

    /// Declarations directly inside a class (constructor properties included).
    fn members(&self, class: &DeclRef) -> Vec<(DeclRef, Decl)> {
        let Some((f, ci)) = self.decl(class) else {
            return Vec::new();
        };
        let c = &f.syms.decls[ci];
        let end = f.bodies.get(&ci).map_or(c.offset, |b| b.end);
        f.syms
            .decls
            .iter()
            .filter(|d| {
                d.container.as_deref() == Some(c.name.as_str())
                    && d.offset >= c.offset
                    && d.offset <= end
            })
            .map(|d| ((f.path.clone(), d.offset), d.clone()))
            .collect()
    }

    fn supertypes(&self, class: &DeclRef) -> Vec<Ty> {
        let Some((f, ci)) = self.decl(class) else {
            return Vec::new();
        };
        let d = &f.syms.decls[ci];
        let mut out = Vec::new();
        for s in &d.supers {
            // resolved where the class is declared, outside its own body
            if let Some(t) = self.type_text(&f, s, d.offset, &[]) {
                out.push(t);
            }
        }
        out
    }

    /// The type and every type above it, nearest first.
    fn hierarchy_up(&self, t: &Ty) -> Vec<Ty> {
        let mut out = vec![t.clone()];
        let mut i = 0;
        while i < out.len() && out.len() < 64 {
            if let Ty::Decl(r) = out[i].clone() {
                for s in self.supertypes(&r) {
                    if !out.contains(&s) {
                        out.push(s);
                    }
                }
            }
            i += 1;
        }
        out
    }

    fn companion(&self, class: &DeclRef) -> Option<DeclRef> {
        self.members(class)
            .into_iter()
            .find(|(_, d)| d.kind == Kind::Object && d.has_modifier("companion"))
            .map(|(r, _)| r)
    }

    fn is_object(&self, r: &DeclRef) -> bool {
        self.decl(r)
            .is_some_and(|(f, i)| f.syms.decls[i].kind == Kind::Object)
    }

    fn is_java(r: &DeclRef) -> bool {
        r.0.extension().is_some_and(|e| e == "java")
    }

    /// Members named `name` of a value of type `t`: its class and the classes above it, the
    /// nearest that has one.
    fn member_of(&self, t: &Ty, name: &str, args: Option<&Args>) -> Vec<DeclRef> {
        for c in self.hierarchy_up(t) {
            let Ty::Decl(c) = c else { continue };
            let found: Vec<(DeclRef, Decl)> = self
                .members(&c)
                .into_iter()
                // a Java `static` belongs to the class, not to its values
                .filter(|(_, d)| {
                    d.name == name
                        && !d.kind.is_type()
                        && !(Self::is_java(&c) && d.has_modifier("static"))
                })
                .collect();
            if let Some(v) = pick(found, args) {
                return v;
            }
        }
        Vec::new()
    }

    /// `Type.name`: nested types, companion and object members, enum entries, Java statics.
    fn static_of(&self, class: &DeclRef, name: &str, args: Option<&Args>) -> Vec<DeclRef> {
        let mut scopes = vec![class.clone()];
        if let Some(c) = self.companion(class) {
            scopes.push(c);
        }
        for s in &scopes {
            let all = self.members(s);
            let found: Vec<(DeclRef, Decl)> = all
                .into_iter()
                .filter(|(_, d)| {
                    d.name == name
                        && (d.kind.is_type()
                            || s != class
                            || self.is_object(class)
                            || Self::is_java(class)
                            || d.container.is_some() && self.is_enum(class))
                })
                .collect();
            if let Some(v) = pick(found, args) {
                return v;
            }
        }
        if name == "Companion"
            && let Some(c) = self.companion(class)
        {
            return vec![c];
        }
        // Java statics and nested types of supertypes
        for t in self
            .hierarchy_up(&Ty::Decl(class.clone()))
            .into_iter()
            .skip(1)
        {
            if let Ty::Decl(r) = t {
                let found: Vec<(DeclRef, Decl)> = self
                    .members(&r)
                    .into_iter()
                    .filter(|(_, d)| d.name == name && (d.kind.is_type() || Self::is_java(&r)))
                    .collect();
                if let Some(v) = pick(found, args) {
                    return v;
                }
            }
        }
        Vec::new()
    }

    fn is_enum(&self, r: &DeclRef) -> bool {
        self.decl(r)
            .is_some_and(|(f, i)| f.syms.decls[i].kind == Kind::Enum)
    }

    /// Extension functions and properties named `name` visible in `f` whose receiver fits `t`
    /// (any receiver when `t` is not known).
    fn extensions(
        &self,
        f: &FileInfo,
        name: &str,
        t: Option<&Ty>,
        args: Option<&Args>,
        at: usize,
    ) -> Vec<DeclRef> {
        let enclosing: Vec<String> = f
            .classes_at(at)
            .into_iter()
            .map(|c| f.syms.decls[c].name.clone())
            .collect();
        let mut found = Vec::new();
        for (p, g, d) in self.index.named(name) {
            let visible = *p == f.path
                || d.container.is_none()
                    && (g.package == f.syms.package
                        || f.imports.stars.contains(&g.package)
                        || f.imports.named.get(name).is_some_and(|q| {
                            q.iter().any(|q| *q == format!("{}.{name}", g.package))
                        }))
                || d.container
                    .as_ref()
                    .is_some_and(|c| enclosing.iter().any(|e| **e == **c));
            if !visible || !matches!(d.kind, Kind::Fun | Kind::Property) {
                continue;
            }
            let r = (p.clone(), d.offset);
            let Some((gf, gi)) = self.decl(&r) else {
                continue;
            };
            let gd = &gf.syms.decls[gi];
            let Some(recv) = gf.receiver_text(gd) else {
                continue;
            };
            if let Some(t) = t {
                let tp = gf.type_params(gd);
                let rt = self.type_text(&gf, &recv, gd.offset, &tp);
                if !rt.as_ref().is_some_and(|rt| self.conforms(t, rt)) {
                    continue;
                }
            }
            found.push((r, gd.clone()));
        }
        // the open file's own declarations may be newer than the index
        pick(found, args).unwrap_or_default()
    }

    /// Whether a value of type `t` can be passed where `want` is expected.
    fn conforms(&self, t: &Ty, want: &Ty) -> bool {
        match (t, want) {
            (_, Ty::Any) | (Ty::Any, _) => true,
            (Ty::Lib(a, _), Ty::Lib(b, _)) => {
                a == b
                    || matches!(b.as_str(), "Any" | "Iterable" | "Collection")
                    || a.trim_start_matches("Mutable") == b.trim_start_matches("Mutable")
            }
            (Ty::Decl(_), Ty::Lib(b, _)) => b == "Any",
            (Ty::Decl(_), Ty::Decl(_)) => self.hierarchy_up(t).contains(want),
            _ => false,
        }
    }

    // ---------------------------------------------------------------- types of expressions

    /// Type of a declaration used as a value: a property's or a function's (its return type),
    /// a class used as a constructor, an object.
    fn type_of_decl(&self, r: &DeclRef) -> Option<Ty> {
        if let Some(t) = self.c.types.borrow().get(r) {
            return t.clone();
        }
        self.c.types.borrow_mut().insert(r.clone(), None);
        let _g = self.guard()?;
        let (f, i) = self.decl(r)?;
        let d = &f.syms.decls[i];
        let tp = f.type_params(d);
        let t = if d.kind.is_type() {
            Some(Ty::Decl(r.clone()))
        } else if !d.ty.is_empty() {
            self.type_text(&f, &d.ty, d.offset, &tp)
        } else {
            // `val x = expr`, `fun f() = expr`, `val vm by viewModels<T>()`
            let ti = f.tok_at(d.offset)?;
            let mut j = ti + 1;
            if f.t(j) == "(" {
                j = f.pair(j)? + 1;
            }
            match f.t(j) {
                "=" => {
                    let e = f.expr_end(j + 1, f.toks.len());
                    self.type_of_expr(&f, j + 1, e)
                }
                "by" => self.delegate_type(&f, j + 1),
                _ => None,
            }
        };
        self.c.types.borrow_mut().insert(r.clone(), t.clone());
        t
    }

    /// `by viewModels<T>()`, `by lazy { T() }`, `by inject<T>()`.
    fn delegate_type(&self, f: &FileInfo, j: usize) -> Option<Ty> {
        if f.t(j + 1) == "<" {
            let mut k = j + 2;
            let a = k;
            while k < f.toks.len() && f.t(k) != ">" && f.t(k) != "(" {
                k += 1;
            }
            let text = f.text[f.start(a)..f.end(k - 1)].to_string();
            return self.type_text(f, &text, f.start(j), &[]);
        }
        if f.t(j) == "lazy" && f.t(j + 1) == "{" {
            let close = f.pair(j + 1)?;
            return self.type_of_expr(f, j + 2, close);
        }
        None
    }

    fn local_type(&self, f: &FileInfo, l: &Local) -> Option<Ty> {
        let _g = self.guard()?;
        if let Some(t) = &l.ty
            && !t.is_empty()
        {
            let tp = f
                .fun_at(l.at)
                .map(|i| f.type_params(&f.syms.decls[i]))
                .unwrap_or_default();
            return self.type_text(f, t, l.at, &tp);
        }
        if let Some((open, pos)) = l.lambda {
            return self.lambda_param(f, open, pos);
        }
        let init = l.init?;
        if f.t(init) == "by" {
            return self.delegate_type(f, init + 1);
        }
        let e = f.expr_end(init, f.toks.len());
        let t = self.type_of_expr(f, init, e)?;
        if l.for_in { elem(&t) } else { Some(t) }
    }

    /// The type of the `pos`-th parameter (or `it`) of the lambda whose `{` is token `open`.
    fn lambda_param(&self, f: &FileInfo, open: usize, pos: usize) -> Option<Ty> {
        if open < 2 {
            return None;
        }
        let mut callee = open - 1;
        // `forEach loop@{`
        if f.t(callee) == "@" && callee > 1 {
            callee -= 2;
        }
        // an argument: `f(update = { … })`, `f(x, { … })`
        if matches!(f.t(callee), "=" | "(" | ",") {
            let mut name = None;
            if f.t(callee) == "=" && callee > 0 && f.ident(callee - 1) {
                name = Some(f.name(callee - 1).to_string());
            }
            let call_open = (0..open)
                .rev()
                .find(|&k| f.t(k) == "(" && f.pair(k).is_some_and(|c| c > open))?;
            let index = split_args_tokens(f, call_open)
                .iter()
                .position(|&(s, e)| s <= open && open < e)?;
            return self.fun_lambda_param(
                f,
                call_open.checked_sub(1)?,
                name.as_deref(),
                Some(index),
                pos,
            );
        }
        if f.t(callee) == ")" {
            callee = f.pair(callee)?.checked_sub(1)?;
        }
        let name = f.t(callee);
        let recv = || {
            if callee > 0 && matches!(f.t(callee - 1), ".") {
                let mut e = callee - 1;
                if e > 0 && f.t(e - 1) == "?" {
                    e -= 1;
                }
                self.type_of_expr(f, f.expr_start(e), e)
            } else {
                None
            }
        };
        let t = match name {
            "let" | "also" | "takeIf" | "takeUnless" | "run" | "apply" => recv(),
            "forEachIndexed" | "mapIndexed" | "filterIndexed" if pos == 1 => {
                recv().as_ref().and_then(elem)
            }
            "update" | "updateAndGet" | "getAndUpdate" | "collect" | "collectLatest" | "onEach"
            | "forEach" | "map" | "mapNotNull" | "filter" | "filterNot" | "first"
            | "firstOrNull" | "last" | "lastOrNull" | "single" | "singleOrNull" | "any" | "all"
            | "none" | "find" | "count" | "sortedBy" | "sortedByDescending" | "flatMap"
            | "associateBy" | "associateWith" | "groupBy" | "sumOf" | "maxByOrNull"
            | "minByOrNull" | "distinctBy" | "partition" | "indexOfFirst" | "takeWhile"
            | "dropWhile" | "mapTo" | "onSuccess" => recv().as_ref().and_then(elem),
            _ => None,
        };
        // a trailing lambda of a project function: its last parameter's type
        t.or_else(|| self.fun_lambda_param(f, callee, None, None, pos))
    }

    /// The `pos`-th input type of a function-typed parameter (`update: (ChatData) -> ChatData`)
    /// of the function called at token `callee`: by name, by position, or the last one.
    fn fun_lambda_param(
        &self,
        f: &FileInfo,
        callee: usize,
        name: Option<&str>,
        index: Option<usize>,
        pos: usize,
    ) -> Option<Ty> {
        if !f.ident(callee) {
            return None;
        }
        let Res::Decls(v) = self.resolve_tok(f, callee) else {
            return None;
        };
        let (g, gi) = self.decl(v.first()?)?;
        let d = &g.syms.decls[gi];
        let p = match (name, index) {
            (Some(n), _) => d.params.iter().find(|p| p.name == n)?,
            (None, Some(i)) => d.params.get(i)?,
            (None, None) => d.params.last()?,
        };
        let ty = p.ty.trim();
        let ty = ty.strip_prefix("suspend ").unwrap_or(ty).trim();
        let (inputs, _) = ty.split_once("->")?;
        let inputs = inputs.trim();
        let tp = g.type_params(d);
        // `T.() -> R`: no parameters, a receiver
        let inner = inputs.strip_prefix('(')?.strip_suffix(')')?;
        let part = split_top(inner).into_iter().nth(pos)?;
        let part = part
            .rsplit_once(':')
            .map_or(part.as_str(), |(_, t)| t)
            .trim()
            .to_string();
        self.type_text(&g, &part, p.offset as usize, &tp)
    }

    /// Implicit `this` receivers at `at` from `apply` / `run` / `with(x)` / `buildX` lambdas,
    /// innermost first.
    fn lambda_receivers(&self, f: &FileInfo, ti: usize) -> Vec<Ty> {
        let mut out = Vec::new();
        let mut i = ti;
        while let Some((open, _)) = f.block_around(i) {
            i = open;
            if open == 0 {
                break;
            }
            let p = open - 1;
            let t = match f.t(p) {
                "apply" | "run" if p > 0 && f.t(p - 1) == "." => {
                    let mut e = p - 1;
                    if e > 0 && f.t(e - 1) == "?" {
                        e -= 1;
                    }
                    self.type_of_expr(f, f.expr_start(e), e)
                }
                ")" => {
                    let o = f.pair(p).unwrap_or(p);
                    if o > 0 && f.t(o - 1) == "with" {
                        let a = split_args_tokens(f, o);
                        a.first().and_then(|&(s, e)| self.type_of_expr(f, s, e))
                    } else {
                        None
                    }
                }
                "test" if p > 0 && f.t(p - 1) == "." => {
                    // Turbine: `flow.test { awaitItem() }`
                    let e = p - 1;
                    let flow = self.type_of_expr(f, f.expr_start(e), e);
                    flow.map(|t| Ty::Lib("Turbine".into(), vec![elem(&t).unwrap_or(Ty::Any)]))
                }
                _ => None,
            };
            if let Some(t) = t {
                out.push(t);
            }
        }
        out
    }

    /// `this` at a place: the innermost lambda receiver, else the enclosing class.
    fn this_type(&self, f: &FileInfo, ti: usize) -> Option<Ty> {
        if let Some(t) = self.lambda_receivers(f, ti).into_iter().next() {
            return Some(t);
        }
        let at = f.start(ti);
        if let Some(fi) = f.fun_at(at)
            && let Some(r) = f.receiver_text(&f.syms.decls[fi])
        {
            return self.type_text(f, &r, at, &f.type_params(&f.syms.decls[fi]));
        }
        f.classes_at(at)
            .first()
            .map(|&c| Ty::Decl((f.path.clone(), f.syms.decls[c].offset)))
    }

    /// Type of the expression over tokens `a..b`, from its last part.
    fn type_of_expr(&self, f: &FileInfo, a: usize, b: usize) -> Option<Ty> {
        let _g = self.guard()?;
        if a >= b {
            return None;
        }
        // `x as T`
        let mut depth = 0;
        for k in (a..b).rev() {
            match f.t(k) {
                ")" | "]" | "}" => depth += 1,
                "(" | "[" | "{" => depth -= 1,
                "as" if depth == 0 => {
                    let mut s = k + 1;
                    if f.t(s) == "?" {
                        s += 1;
                    }
                    let text = f.text[f.start(s)..f.end(b - 1)].to_string();
                    return self.type_text(f, &text, f.start(s), &[]);
                }
                ":" if depth == 0 && k > a && f.t(k - 1) == "?" => {
                    return self
                        .type_of_expr(f, a, k - 1)
                        .or_else(|| self.type_of_expr(f, k + 1, b));
                }
                _ => {}
            }
        }
        let mut e = b;
        while e > a && matches!(f.t(e - 1), "!" | "?") {
            e -= 1;
        }
        let last = e.checked_sub(1)?;
        if last < a {
            return None;
        }
        match f.kind(last) {
            Some(Tok::Str) => return Some(Ty::Lib("String".into(), vec![])),
            Some(Tok::Num) => return Some(Ty::Lib("Int".into(), vec![])),
            _ => {}
        }
        match f.t(last) {
            ")" => {
                let o = f.pair(last)?;
                if o > a && (f.ident(o - 1) || f.t(o - 1) == ">") {
                    let mut callee = o - 1;
                    let mut targs = None;
                    if f.t(callee) == ">" {
                        // `foo<T>()`
                        let mut k = callee;
                        while k > a && f.t(k) != "<" {
                            k -= 1;
                        }
                        targs = Some(f.text[f.start(k + 1)..f.end(callee - 1)].to_string());
                        callee = k.checked_sub(1)?;
                    }
                    return self.call_type(f, callee, o, targs);
                }
                if o >= a {
                    return self.type_of_expr(f, o + 1, last);
                }
                None
            }
            "}" => {
                let o = f.pair(last)?;
                if o > a && f.ident(o - 1) {
                    return self.call_type(f, o - 1, o, None);
                }
                if o > a && f.t(o - 1) == ")" {
                    let po = f.pair(o - 1)?;
                    if po > a && f.ident(po - 1) {
                        return self.call_type(f, po - 1, po, None);
                    }
                }
                None
            }
            "]" => {
                let o = f.pair(last)?;
                let base = self.type_of_expr(f, a, o)?;
                match base.lib() {
                    Some("Map" | "MutableMap" | "HashMap") => base.arg(1),
                    _ => elem(&base),
                }
            }
            "this" => self.this_type(f, last),
            "super" => {
                let c = f.classes_at(f.start(last)).first().copied()?;
                self.supertypes(&(f.path.clone(), f.syms.decls[c].offset))
                    .into_iter()
                    .next()
            }
            _ if f.ident(last) => match self.resolve_tok(f, last) {
                Res::Decls(v) => {
                    let r = v.first()?;
                    let (g, i) = self.decl(r)?;
                    let d = &g.syms.decls[i];
                    if d.kind.is_type() && d.kind != Kind::Object {
                        // `Type` used as a value: its companion
                        return self.companion(r).map(Ty::Decl);
                    }
                    self.type_of_decl(r)
                }
                Res::Local(at) => {
                    let l = f.locals.iter().find(|l| l.at == at)?;
                    self.local_type(f, l)
                }
                Res::None => {
                    if f.t(last) == "it" {
                        return self.it_type(f, last);
                    }
                    if f.t(last) == "entries"
                        && last > a + 1
                        && f.t(last - 1) == "."
                        && let Res::Decls(v) = self.resolve_tok(f, last - 2)
                        && let Some(e) = v.into_iter().find(|r| self.is_enum(r))
                    {
                        return Some(Ty::Lib("List".into(), vec![Ty::Decl(e)]));
                    }
                    // a library member of a known type
                    if last > a && f.t(last - 1) == "." {
                        let mut r = last - 1;
                        if r > 0 && f.t(r - 1) == "?" {
                            r -= 1;
                        }
                        let rt = self.type_of_expr(f, a, r)?;
                        return lib_member(&rt, f.t(last));
                    }
                    None
                }
            },
            _ => None,
        }
    }

    /// The type a call `callee(…)` returns (`callee` token, its `(` or `{` at `open`).
    fn call_type(
        &self,
        f: &FileInfo,
        callee: usize,
        open: usize,
        targs: Option<String>,
    ) -> Option<Ty> {
        let name = f.name(callee).to_string();
        match self.resolve_tok(f, callee) {
            Res::Decls(v) => {
                let r = v.first()?;
                let (g, i) = self.decl(r)?;
                let d = &g.syms.decls[i];
                if d.kind.is_type() {
                    return Some(Ty::Decl(r.clone()));
                }
                if d.kind == Kind::Fun {
                    return self.type_of_decl(r);
                }
                // `useCase()`: a property whose class has an `operator fun invoke`
                let t = self.type_of_decl(r)?;
                let inv = self.member_of(&t, "invoke", None).into_iter().next()?;
                self.type_of_decl(&inv)
            }
            _ => {
                if let Some(t) = &targs
                    && matches!(
                        name.as_str(),
                        "viewModels"
                            | "activityViewModels"
                            | "hiltViewModel"
                            | "viewModel"
                            | "inject"
                            | "get"
                            | "mockk"
                            | "mock"
                            | "spyk"
                            | "assertIs"
                            | "requireNotNull"
                    )
                {
                    return self.type_text(f, t, f.start(callee), &[]);
                }
                // `listOf<T>()`, `MutableStateFlow(x)`, `mutableStateOf(x)`: a holder of T
                let holder = match name.as_str() {
                    "listOf" | "emptyList" | "listOfNotNull" | "buildList" => Some("List"),
                    "mutableListOf" | "arrayListOf" => Some("MutableList"),
                    "setOf" | "emptySet" => Some("Set"),
                    "mutableSetOf" | "hashSetOf" => Some("MutableSet"),
                    "MutableStateFlow"
                    | "MutableSharedFlow"
                    | "StateFlow"
                    | "flowOf"
                    | "mutableStateOf"
                    | "MutableLiveData"
                    | "lazy"
                    | "Resettable"
                    | "CompletableDeferred" => Some(name.as_str()),
                    "assertNotNull" | "checkNotNull" | "requireNotNull" => Some(""),
                    _ => None,
                };
                if let Some(h) = holder {
                    let inner = match &targs {
                        Some(t) => self.type_text(f, t, f.start(callee), &[]),
                        None if f.t(open) == "(" => split_args_tokens(f, open)
                            .first()
                            .and_then(|&(s, e)| self.type_of_expr(f, s, e)),
                        None => None,
                    };
                    return if h.is_empty() {
                        inner
                    } else {
                        Some(Ty::Lib(h.to_string(), inner.into_iter().collect()))
                    };
                }
                // `Enum.entries`, `Enum.values()`
                if name == "values"
                    && callee > 1
                    && f.t(callee - 1) == "."
                    && f.ident(callee - 2)
                    && let Res::Decls(v) = self.resolve_tok(f, callee - 2)
                    && let Some(e) = v.into_iter().find(|r| self.is_enum(r))
                {
                    return Some(Ty::Lib("Array".into(), vec![Ty::Decl(e)]));
                }
                let recv = if callee > 0 && f.t(callee - 1) == "." {
                    let mut e = callee - 1;
                    if e > 0 && f.t(e - 1) == "?" {
                        e -= 1;
                    }
                    self.type_of_expr(f, f.expr_start(e), e)
                } else if name == "awaitItem" || name == "expectMostRecentItem" {
                    self.lambda_receivers(f, callee)
                        .into_iter()
                        .find(|t| t.lib() == Some("Turbine"))
                } else {
                    None
                };
                let recv = recv?;
                if name == "copy" && matches!(recv, Ty::Decl(_)) {
                    return Some(recv);
                }
                lib_member(&recv, &name)
            }
        }
    }

    /// `it` in a lambda without named parameters.
    fn it_type(&self, f: &FileInfo, ti: usize) -> Option<Ty> {
        let mut i = ti;
        while let Some((open, _)) = f.block_around(i) {
            i = open;
            let named = f
                .locals
                .iter()
                .any(|l| l.lambda.is_some_and(|(o, _)| o == open));
            if !named && open > 0 && !f.is_when_block(open) {
                let p = open - 1;
                let is_call = f.ident(p) || matches!(f.t(p), ")" | "@" | "=" | "(" | ",");
                if is_call {
                    return self.lambda_param(f, open, 0);
                }
            }
        }
        None
    }

    // ---------------------------------------------------------------- resolution

    /// What the name at byte `at` of `f` means.
    pub fn resolve(&self, f: &FileInfo, at: usize) -> Res {
        match f.tok_at(at) {
            Some(i) if f.ident(i) => self.resolve_tok(f, i),
            _ => Res::None,
        }
    }

    fn resolve_tok(&self, f: &FileInfo, i: usize) -> Res {
        match self.resolve_tok_inner(f, i) {
            Res::Decls(v) if v.len() > 1 => Res::Decls(self.by_arg_types(f, i, v)),
            r => r,
        }
    }

    /// Overloads with the same number of arguments: the ones whose parameter types take the
    /// arguments' types (when those are known).
    fn by_arg_types(&self, f: &FileInfo, i: usize, v: Vec<DeclRef>) -> Vec<DeclRef> {
        let mut open = i + 1;
        if f.t(open) != "(" || f.java && f.t(open) != "(" {
            return v;
        }
        if f.t(open) == "<" {
            open = (open..f.toks.len())
                .find(|&k| f.t(k) == "(")
                .unwrap_or(open);
        }
        let args: Vec<Option<Ty>> = split_args_tokens(f, open)
            .into_iter()
            .map(|(s, e)| {
                if f.t(s + 1) == "=" {
                    None
                } else {
                    self.type_of_expr(f, s, e)
                }
            })
            .collect();
        if args.iter().all(Option::is_none) {
            return v;
        }
        let ok: Vec<DeclRef> = v
            .iter()
            .filter(|r| {
                let Some((g, gi)) = self.decl(r) else {
                    return true;
                };
                let d = &g.syms.decls[gi];
                if d.kind != Kind::Fun {
                    return true;
                }
                let tp = g.type_params(d);
                args.iter().zip(&d.params).all(|(a, p)| match a {
                    None => true,
                    Some(a) => self
                        .type_text(&g, &p.ty, p.offset as usize, &tp)
                        .is_none_or(|pt| self.conforms(a, &pt)),
                })
            })
            .cloned()
            .collect();
        if ok.is_empty() { v } else { ok }
    }

    fn resolve_tok_inner(&self, f: &FileInfo, i: usize) -> Res {
        let Some(_g) = self.guard() else {
            return Res::None;
        };
        let name = f.name(i).to_string();
        let at = f.start(i);
        // its own declaration
        if f.syms.decls.iter().any(|d| d.offset == at) {
            return Res::Decls(vec![(f.path.clone(), at)]);
        }
        if f.locals.iter().any(|l| l.at == at) {
            return Res::Local(at);
        }
        // `import a.b.C` / `package a.b`
        let line_start = f.text[..at].rfind('\n').map_or(0, |n| n + 1);
        let line = f.text[line_start..].trim_start();
        if line.starts_with("package ") {
            return Res::None;
        }
        if line.starts_with("import ") {
            let mut parts = Vec::new();
            let mut k = f
                .tok_at(line_start + (f.text[line_start..].len() - line.len()))
                .unwrap_or(i)
                + 1;
            if f.t(k) == "static" {
                k += 1;
            }
            while k <= i {
                if f.t(k) != "." {
                    parts.push(f.name(k).to_string());
                }
                k += 1;
            }
            let fqn = parts.join(".");
            let v = self.by_fqn(&fqn);
            return if v.is_empty() {
                Res::None
            } else {
                Res::Decls(v)
            };
        }
        let args = self.call_args(f, i);
        // a named argument: `Foo(name = …)`, `x.copy(name = …)`
        if f.t(i + 1) == "="
            && i > 0
            && matches!(f.t(i - 1), "(" | ",")
            && let Some(r) = self.named_arg(f, i, &name)
        {
            return r;
        }
        let prev = if i > 0 { f.t(i - 1) } else { "" };
        // `Type::name`, `value::name`, `::name`
        if prev == ":" && i > 1 && f.t(i - 2) == ":" {
            let e = i - 2;
            if e == 0 || !(f.ident(e - 1) || matches!(f.t(e - 1), ")" | "]" | "this" | ">")) {
                return self.unqualified(f, i, &name, None);
            }
            return self.qualified(f, i, &name, e, None);
        }
        if prev == "." {
            let mut e = i - 1;
            if e > 0 && f.t(e - 1) == "?" {
                e -= 1;
            }
            if e > 0 && f.t(e - 1) == "." {
                return Res::None; // `..` range
            }
            return self.qualified(f, i, &name, e, args.as_ref());
        }
        self.unqualified(f, i, &name, args.as_ref())
    }

    /// The arguments when the name is called: `name(…)`, `name { }`, `name<T>(…)`.
    fn call_args(&self, f: &FileInfo, i: usize) -> Option<Args> {
        let mut j = i + 1;
        if f.t(j) == "<" && !f.java {
            let mut depth = 0;
            while j < f.toks.len() {
                match f.t(j) {
                    "<" => depth += 1,
                    ">" => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    "(" | ")" | "{" | "}" | "=" | ";" => return None,
                    _ => {}
                }
                j += 1;
            }
            j += 1;
        }
        let mut a = Args::default();
        if f.t(j) == "(" {
            let parts = split_args_tokens(f, j);
            a.count = parts.len();
            for (s, _) in &parts {
                if f.ident(*s) && f.t(s + 1) == "=" && f.t(s + 2) != "=" {
                    a.named.push(f.name(*s).to_string());
                }
            }
            j = f.pair(j)? + 1;
        } else if f.t(j) != "{" || f.newline_before(j) || f.java {
            return None;
        }
        if f.t(j) == "{" && !f.newline_before(j) && !f.java {
            a.count += 1;
            a.lambda = true;
        }
        Some(a)
    }

    fn named_arg(&self, f: &FileInfo, i: usize, name: &str) -> Option<Res> {
        // the `(` of the call this argument is in
        let mut k = i;
        let mut depth = 0;
        let open = loop {
            if k == 0 {
                return None;
            }
            k -= 1;
            match f.t(k) {
                ")" | "]" | "}" => depth += 1,
                "[" | "{" => {
                    if depth == 0 {
                        return None;
                    }
                    depth -= 1
                }
                "(" => {
                    if depth == 0 {
                        break k;
                    }
                    depth -= 1
                }
                _ => {}
            }
        };
        if open == 0 || !f.ident(open - 1) {
            return None;
        }
        let callee = open - 1;
        if f.kind(callee) == Some(Tok::Annotation) {
            return Some(Res::None);
        }
        let class = if f.t(callee) == "copy" && callee > 0 && f.t(callee - 1) == "." {
            let mut e = callee - 1;
            if e > 0 && f.t(e - 1) == "?" {
                e -= 1;
            }
            match self.type_of_expr(f, f.expr_start(e), e) {
                Some(Ty::Decl(r)) => Some(r),
                _ => None,
            }
        } else {
            match self.resolve_tok(f, callee) {
                Res::Decls(v) => v.into_iter().find(|r| {
                    self.decl(r)
                        .is_some_and(|(g, gi)| g.syms.decls[gi].kind.is_type())
                }),
                _ => None,
            }
        };
        let Some(class) = class else {
            return Some(Res::None); // a function's parameter
        };
        let found: Vec<DeclRef> = self
            .members(&class)
            .into_iter()
            .filter(|(_, d)| d.name == name && d.kind == Kind::Property)
            .map(|(r, _)| r)
            .collect();
        Some(if found.is_empty() {
            Res::None
        } else {
            Res::Decls(found)
        })
    }

    fn unqualified(&self, f: &FileInfo, i: usize, name: &str, args: Option<&Args>) -> Res {
        let at = f.start(i);
        if let Some(l) = f.local(name, at) {
            return Res::Local(l.at);
        }
        if name == "it" || name == "this" {
            return Res::None;
        }
        let looks_type = name.starts_with(char::is_uppercase);
        // members of `apply` / `with` receivers, of an extension's receiver, of the classes
        let mut receivers = self.lambda_receivers(f, i);
        if let Some(fi) = f.fun_at(at)
            && let Some(r) = f.receiver_text(&f.syms.decls[fi])
            && let Some(t) = self.type_text(f, &r, at, &f.type_params(&f.syms.decls[fi]))
        {
            receivers.push(t);
        }
        for t in &receivers {
            let v = self.member_of(t, name, args);
            if !v.is_empty() {
                return Res::Decls(v);
            }
        }
        for c in f.classes_at(at) {
            let r = (f.path.clone(), f.syms.decls[c].offset);
            let v = self.member_of(&Ty::Decl(r.clone()), name, args);
            if !v.is_empty() {
                return Res::Decls(v);
            }
            // companion members and nested types of the class and the classes above it
            for t in self.hierarchy_up(&Ty::Decl(r.clone())) {
                if let Ty::Decl(t) = t {
                    let v = self.static_of(&t, name, args);
                    if !v.is_empty() {
                        return Res::Decls(v);
                    }
                }
            }
            if self.is_object(&r) || f.syms.decls[c].has_modifier("companion") {
                continue;
            }
        }
        if looks_type || args.is_some_and(|a| a.lambda || a.count > 0) || args.is_some() {
            let types = self.visible_types(f, name, at);
            let funs = self.top_level(f, name, false);
            let funs: Vec<DeclRef> = funs
                .into_iter()
                .filter(|r| {
                    self.decl(r).is_some_and(|(g, gi)| {
                        let d = &g.syms.decls[gi];
                        d.kind == Kind::Fun
                            && g.receiver_text(d).is_none()
                            && args.is_none_or(|a| fits(d, a))
                    })
                })
                .collect();
            if args.is_some() && !funs.is_empty() && (types.is_empty() || !looks_type) {
                return Res::Decls(funs);
            }
            if !types.is_empty() {
                return Res::Decls(types);
            }
            if !funs.is_empty() {
                return Res::Decls(funs);
            }
        } else {
            // a top-level property or object
            let v: Vec<DeclRef> = self
                .top_level(f, name, false)
                .into_iter()
                .chain(self.top_level(f, name, true))
                .filter(|r| {
                    self.decl(r).is_some_and(|(g, gi)| {
                        let d = &g.syms.decls[gi];
                        g.receiver_text(d).is_none()
                            && (d.kind == Kind::Property
                                || d.kind == Kind::Object
                                || d.kind.is_type())
                    })
                })
                .collect();
            if !v.is_empty() {
                return Res::Decls(v);
            }
        }
        // an extension called on an implicit receiver
        let this: Vec<Ty> = receivers
            .into_iter()
            .chain(
                f.classes_at(at)
                    .into_iter()
                    .map(|c| Ty::Decl((f.path.clone(), f.syms.decls[c].offset))),
            )
            .collect();
        for t in &this {
            let v = self.extensions(f, name, Some(t), args, at);
            if !v.is_empty() {
                return Res::Decls(v);
            }
        }
        Res::None
    }

    fn qualified(
        &self,
        f: &FileInfo,
        i: usize,
        name: &str,
        dot: usize,
        args: Option<&Args>,
    ) -> Res {
        let start = f.expr_start(dot);
        if start >= dot {
            return Res::None;
        }
        // `pkg.a.Type` / `pkg.fn`
        let path: Option<Vec<&str>> = (start..dot)
            .map(|k| {
                if f.t(k) == "." {
                    Some(None)
                } else if f.ident(k) {
                    Some(Some(f.name(k)))
                } else {
                    None
                }
            })
            .collect::<Option<Vec<_>>>()
            .map(|v| v.into_iter().flatten().collect());
        if let Some(segs) = &path
            && segs
                .first()
                .is_some_and(|s| s.starts_with(char::is_lowercase))
            && self.is_package(&segs.join("."))
            && f.local(segs[0], f.start(start)).is_none()
        {
            let v = self.by_fqn(&format!("{}.{name}", segs.join(".")));
            return if v.is_empty() {
                Res::None
            } else {
                Res::Decls(v)
            };
        }
        // `Type.name`
        if dot == start + 1 || f.t(dot - 1) != ")" {
            let last = dot - 1;
            if f.ident(last) {
                let r = self.resolve_tok(f, last);
                if let Res::Decls(v) = &r
                    && let Some(t) = v.iter().find(|r| {
                        self.decl(r)
                            .is_some_and(|(g, gi)| g.syms.decls[gi].kind.is_type())
                    })
                {
                    let s = self.static_of(t, name, args);
                    if !s.is_empty() {
                        return Res::Decls(s);
                    }
                    if self.is_object(t) {
                        let m = self.member_of(&Ty::Decl(t.clone()), name, args);
                        if !m.is_empty() {
                            return Res::Decls(m);
                        }
                    }
                    if !self.is_object(t) {
                        return Res::None;
                    }
                }
            }
        }
        // `value.name`
        let t = self.type_of_expr(f, start, dot);
        if let Some(t) = &t {
            let v = self.member_of(t, name, args);
            if !v.is_empty() {
                return Res::Decls(v);
            }
            let v = self.extensions(f, name, Some(t), args, f.start(i));
            if !v.is_empty() {
                return Res::Decls(v);
            }
            // a smart cast to a subtype (`is Content -> state.items`)
            if let Ty::Decl(r) = t
                && let Some((g, gi)) = self.decl(r)
            {
                let mut found = Vec::new();
                for h in self.index.implementations(&g.syms.decls[gi].name) {
                    let sub = (h.path.clone(), h.offset);
                    if self.hierarchy_up(&Ty::Decl(sub.clone())).contains(t) {
                        found.extend(self.member_of(&Ty::Decl(sub), name, args));
                    }
                }
                found.sort();
                found.dedup();
                if !found.is_empty() {
                    return Res::Decls(found);
                }
            }
            if matches!(t, Ty::Decl(_)) || t.lib().is_some_and(|l| l != "Any") {
                return Res::None;
            }
        }
        // the receiver's type is not known: an extension visible here, else the one member of
        // that name in the project, else the ones whose class this file names
        let ext = self.extensions(f, name, None, args, f.start(i));
        let mut members: Vec<(DeclRef, Decl)> = self
            .index
            .named(name)
            .filter(|(_, _, d)| d.container.is_some() && !d.kind.is_type())
            .map(|(p, _, d)| ((p.clone(), d.offset), d.clone()))
            .filter(|(r, _)| !ext.contains(r))
            .collect();
        members.retain(|(r, _)| {
            self.decl(r)
                .is_some_and(|(g, gi)| g.receiver_text(&g.syms.decls[gi]).is_none())
        });
        let members = pick(members, args).unwrap_or_default();
        if members.is_empty() && !ext.is_empty() {
            return Res::Decls(ext);
        }
        if members.len() == 1 && ext.is_empty() {
            return Res::Decls(members);
        }
        let named: Vec<DeclRef> = members
            .into_iter()
            .filter(|r| {
                self.decl(r).is_some_and(|(g, gi)| {
                    g.syms.decls[gi]
                        .container
                        .as_ref()
                        .is_some_and(|c| has_word(&f.text, c))
                })
            })
            .collect();
        if named.len() == 1 {
            Res::Decls(named)
        } else {
            Res::None
        }
    }

    /// The type of the receiver before the name at `at` (for debugging).
    #[cfg(test)]
    pub fn debug_receiver(&self, f: &FileInfo, at: usize) -> String {
        let Some(i) = f.tok_at(at) else {
            return "no token".into();
        };
        if i == 0 || f.t(i - 1) != "." {
            return "unqualified".into();
        }
        let mut e = i - 1;
        if e > 0 && f.t(e - 1) == "?" {
            e -= 1;
        }
        let s = f.expr_start(e);
        format!(
            "receiver `{}` : {:?}",
            &f.text[f.start(s)..f.end(e - 1)],
            self.type_of_expr(f, s, e)
        )
    }

    /// Whether the declaration `r` is `target` or overrides it.
    pub fn means(&self, r: &DeclRef, target: &DeclRef) -> bool {
        if r == target {
            return true;
        }
        let mut cur = r.clone();
        for _ in 0..16 {
            let Some((f, i)) = self.decl(&cur) else {
                return false;
            };
            let d = &f.syms.decls[i];
            if !d.has_modifier("override") && !f.java {
                return false;
            }
            let Some(c) = f.classes_at(d.offset).first().copied().or_else(|| {
                f.syms.decls.iter().position(|x| {
                    x.kind.is_type() && Some(x.name.as_str()) == d.container.as_deref()
                })
            }) else {
                return false;
            };
            let class = (f.path.clone(), f.syms.decls[c].offset);
            let args = (d.kind == Kind::Fun).then(|| Args {
                count: d.params.len(),
                ..Default::default()
            });
            let up: Vec<DeclRef> = self
                .hierarchy_up(&Ty::Decl(class))
                .into_iter()
                .skip(1)
                .find_map(|t| match t {
                    Ty::Decl(s) => {
                        let v: Vec<DeclRef> = self
                            .members(&s)
                            .into_iter()
                            .filter(|(_, m)| {
                                m.name == d.name
                                    && !m.kind.is_type()
                                    && args.as_ref().is_none_or(|a| fits(m, a))
                            })
                            .map(|(r, _)| r)
                            .collect();
                        (!v.is_empty()).then_some(v)
                    }
                    _ => None,
                })
                .unwrap_or_default();
            if up.contains(target) {
                return true;
            }
            match up.into_iter().next() {
                Some(n) => cur = n,
                None => return false,
            }
        }
        false
    }

    /// Places in `files` where the declaration `target` is used (not the declaration itself).
    #[cfg(test)]
    pub fn usages(
        &self,
        target: &DeclRef,
        files: &[PathBuf],
        stop: &dyn Fn() -> bool,
    ) -> Vec<(PathBuf, usize)> {
        let mut out = Vec::new();
        for p in files {
            if stop() {
                break;
            }
            for (_, at) in self.usages_in(std::slice::from_ref(target), p) {
                out.push((p.clone(), at));
            }
        }
        out
    }

    /// The uses of each of `targets` in one file, as (index into `targets`, offset).
    pub fn usages_in(&self, targets: &[DeclRef], p: &Path) -> Vec<(usize, usize)> {
        let names: Vec<Option<String>> = targets
            .iter()
            .map(|t| self.decl(t).map(|(f, i)| f.syms.decls[i].name.clone()))
            .collect();
        let mut out = Vec::new();
        let Some(text) = self.texts.read(p) else {
            return out;
        };
        if !names.iter().flatten().any(|n| text.contains(n.as_str())) {
            return out;
        }
        let Some(f) = self.file(p) else { return out };
        let mut seen = HashSet::new();
        for name in names.iter().flatten() {
            if !seen.insert(name) {
                continue;
            }
            for r in crate::refactor::occurrences(&f.text, name) {
                // a declaration (an override too) is not a use
                if f.syms.decls.iter().any(|d| d.offset == r.start)
                    || f.tok_at(r.start).is_some_and(|i| {
                        i > 0
                            && matches!(
                                f.t(i - 1),
                                "fun" | "val" | "var" | "class" | "object" | "interface"
                            )
                            && f.t(i + 1) != "."
                    })
                {
                    continue;
                }
                if let Res::Decls(v) = self.resolve(&f, r.start) {
                    for (ti, t) in targets.iter().enumerate() {
                        if names[ti].as_deref() == Some(name.as_str())
                            && v.iter().any(|d| self.means(d, t))
                        {
                            out.push((ti, r.start));
                        }
                    }
                }
            }
        }
        self.forget(256);
        out
    }

    /// What every name in the file between bytes `from` and `to` means: `(start, end,
    /// meaning)` for the ones that are project declarations or locals.
    pub fn refs_between(&self, p: &Path, from: usize, to: usize) -> Vec<(usize, usize, Res)> {
        let Some(f) = self.file(p) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let first = f.toks.partition_point(|t| t.range.start < from);
        for (i, t) in f.toks.iter().enumerate().skip(first) {
            if t.range.start >= to {
                break;
            }
            if !f.ident(i) {
                continue;
            }
            match self.resolve_tok(&f, i) {
                Res::None => {}
                r => out.push((t.range.start, t.range.end, r)),
            }
        }
        out
    }

    /// Drops what was read of `p` (its text changed).
    pub fn changed(&self, p: &Path) {
        self.c.files.borrow_mut().remove(p);
        self.c.types.borrow_mut().retain(|(q, _), _| q != p);
    }

    /// The simple type name of parameter `i` of a declaration (`Lazy<Repo>` is `Repo`).
    pub fn param_type(&self, d: &DeclRef, i: usize) -> Option<String> {
        let (f, di) = self.decl(d)?;
        let t = f.syms.decls[di]
            .params
            .get(i)?
            .ty
            .trim()
            .trim_end_matches('?')
            .to_string();
        let t = ["Lazy<", "Provider<"]
            .iter()
            .find_map(|w| {
                t.strip_prefix(w)
                    .and_then(|x| x.strip_suffix('>'))
                    .map(str::to_string)
            })
            .unwrap_or(t);
        Some(crate::index::type_name(&t))
    }

    /// The name and the 1-based line of a declaration, for showing it.
    pub fn describe(&self, r: &DeclRef) -> Option<(String, String)> {
        let (f, i) = self.decl(r)?;
        Some((
            f.syms.decls[i].name.clone(),
            crate::index::describe(&f.syms, &f.syms.decls[i]),
        ))
    }
}

impl FileInfo {
    /// First token of the receiver expression that ends right before token `dot` (a `.`, `?.`
    /// or `::`): `a.b(c)[0]!!` of `a.b(c)[0]!!.name`.
    fn expr_start(&self, dot: usize) -> usize {
        let mut k = dot;
        loop {
            if k == 0 {
                return 0;
            }
            let p = k - 1;
            match self.t(p) {
                ")" | "]" | "}" => match self.pair(p) {
                    Some(o) => {
                        k = o;
                        // a call or index: take its callee too
                        if o > 0
                            && (self.ident(o - 1) || matches!(self.t(o - 1), ">" | ")" | "]"))
                            && self.t(p) != "}"
                        {
                            continue;
                        }
                        if self.t(p) == "}" && o > 0 && (self.ident(o - 1) || self.t(o - 1) == ")")
                        {
                            continue;
                        }
                        // `(expr)`
                        if o == 0 || !matches!(self.t(o - 1), "." | "?") {
                            return o;
                        }
                    }
                    None => return k,
                },
                "!" | "?" => k = p,
                "." => k = p,
                ">" => {
                    // `foo<T>(…)`
                    let mut q = p;
                    let mut depth = 0;
                    while q > 0 {
                        match self.t(q) {
                            ">" => depth += 1,
                            "<" => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            "(" | ")" | "{" | "}" | "=" | ";" => return k,
                            _ => {}
                        }
                        q -= 1;
                    }
                    k = q;
                }
                _ if self.ident(p)
                    || matches!(self.t(p), "this" | "super")
                    || matches!(self.kind(p), Some(Tok::Str | Tok::Num)) =>
                {
                    k = p;
                    if p == 0 || !matches!(self.t(p - 1), "." | "?") {
                        if p > 0 && self.t(p - 1) == "." {
                            continue;
                        }
                        // `?.` is two tokens: keep going over `x?`
                        if p > 1 && self.t(p - 1) == "?" && self.t(p - 2) != "?" {
                            return p;
                        }
                        return p;
                    }
                }
                _ => return k,
            }
        }
    }
}

struct DepthGuard<'a>(&'a RefCell<usize>);

impl Drop for DepthGuard<'_> {
    fn drop(&mut self) {
        *self.0.borrow_mut() -= 1;
    }
}

/// The candidates that fit the call: by arguments when called, else any. `None` when there are
/// none at all (so a lookup goes on to the next scope).
fn pick(found: Vec<(DeclRef, Decl)>, args: Option<&Args>) -> Option<Vec<DeclRef>> {
    if found.is_empty() {
        return None;
    }
    let fit: Vec<DeclRef> = match args {
        Some(a) => {
            let funs: Vec<&(DeclRef, Decl)> = found
                .iter()
                .filter(|(_, d)| d.kind == Kind::Fun || d.kind.is_type())
                .collect();
            let ok: Vec<DeclRef> = funs
                .iter()
                .filter(|(_, d)| fits(d, a))
                .map(|(r, _)| r.clone())
                .collect();
            if !ok.is_empty() {
                ok
            } else if !funs.is_empty() && funs.len() == found.len() {
                funs.iter().map(|(r, _)| r.clone()).collect()
            } else {
                // a property of a function type (`val onClick: () -> Unit`)
                found
                    .iter()
                    .filter(|(_, d)| d.kind == Kind::Property)
                    .map(|(r, _)| r.clone())
                    .collect()
            }
        }
        None => {
            let props: Vec<DeclRef> = found
                .iter()
                .filter(|(_, d)| d.kind != Kind::Fun)
                .map(|(r, _)| r.clone())
                .collect();
            if props.is_empty() {
                found.into_iter().map(|(r, _)| r).collect()
            } else {
                props
            }
        }
    };
    (!fit.is_empty()).then_some(fit)
}

/// Element type of a collection, flow or holder (`List<T>`, `StateFlow<T>`, `Array<T>`).
fn elem(t: &Ty) -> Option<Ty> {
    match t {
        Ty::Lib(n, a) if n.ends_with("Map") => a.get(1).cloned().or_else(|| a.first().cloned()),
        Ty::Lib(_, a) => a.first().cloned(),
        _ => None,
    }
}

/// The type a library member returns, for the members code reads through most.
fn lib_member(t: &Ty, name: &str) -> Option<Ty> {
    let Ty::Lib(n, _) = t else { return None };
    match name {
        "value"
        | "first"
        | "last"
        | "single"
        | "firstOrNull"
        | "lastOrNull"
        | "singleOrNull"
        | "get"
        | "getOrNull"
        | "getOrElse"
        | "elementAt"
        | "random"
        | "find"
        | "peek"
        | "getValue"
        | "awaitItem"
        | "expectMostRecentItem"
        | "await"
        | "getOrThrow"
        | "component1"
        | "requireValue"
        | "orThrow"
        | "unwrap" => elem(t),
        "orEmpty" | "toList" | "toMutableList" | "filter" | "filterNot" | "sortedBy" | "sorted"
        | "distinct" | "take" | "drop" | "asStateFlow" | "asFlow" | "asSharedFlow" | "reversed"
        | "filterNotNull" | "toSet" | "plus" | "minus" | "onEach" | "also" | "apply" => {
            Some(t.clone())
        }
        "data" if n == "Resource" => elem(t),
        _ => None,
    }
}

/// Splits `A, B<C, D>` at its top-level commas.
fn split_top(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut depth, mut cur) = (0, String::new());
    for c in s.chars() {
        match c {
            '<' | '(' => depth += 1,
            '>' | ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// Token ranges of the arguments of the call whose `(` is token `open`.
fn split_args_tokens(f: &FileInfo, open: usize) -> Vec<(usize, usize)> {
    let Some(close) = f.pair(open) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut s = open + 1;
    let mut k = open + 1;
    while k < close {
        match f.t(k) {
            "(" | "[" | "{" => {
                k = f.pair(k).unwrap_or(k) + 1;
                continue;
            }
            "," => {
                out.push((s, k));
                s = k + 1;
            }
            _ => {}
        }
        k += 1;
    }
    if s < close {
        out.push((s, close));
    }
    out
}

pub fn has_word(text: &str, w: &str) -> bool {
    text.match_indices(w).any(|(i, _)| {
        let ok = |c: Option<char>| !c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        ok(text[..i].chars().next_back()) && ok(text[i + w.len()..].chars().next())
    })
}

// ---------------------------------------------------------------- Hilt / Dagger

/// What a file adds to the Hilt graph: bindings it provides and places it injects into.
#[derive(Clone, Debug, Default)]
pub struct HiltFile {
    pub providers: Vec<Binding>,
    pub sites: Vec<Site>,
}

/// A `@Provides` / `@Binds` function or an `@Inject constructor` class, by the key it provides.
#[derive(Clone, Debug)]
pub struct Binding {
    pub decl: DeclRef,
    pub key: String,
}

/// A place a binding is injected: a constructor or `@Provides` / `@Binds` parameter, or an
/// `@Inject` field.
#[derive(Clone, Debug)]
pub struct Site {
    /// The class, function or field declaration.
    pub owner: DeclRef,
    /// Which parameter (`None` for a field); the accuracy test reads it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub param: Option<usize>,
    /// Offset of the parameter's or field's name.
    pub at: usize,
    pub key: String,
}

/// Test code: test source sets and test-support modules (`*-test`, `ui-test`, `testing`: the
/// folder that holds `src`, so folders above the project do not count).
pub fn is_test_path(p: &Path) -> bool {
    let parts: Vec<String> = p
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts.windows(2).any(|w| {
        let (c, next) = (&w[0], &w[1]);
        c == "src" && next.to_lowercase().contains("test")
            || next == "src"
                && (c.ends_with("-test")
                    || c.starts_with("test-")
                    || c == "testing"
                    || c.ends_with("-testing"))
    })
}

fn variant(p: &Path) -> String {
    crate::index::source_set(p)
}

impl Resolver<'_> {
    fn key_of(&self, t: &Ty) -> String {
        match t {
            Ty::Decl((p, o)) => format!("{}#{o}", p.display()),
            Ty::Lib(n, a) if a.is_empty() => n.clone(),
            Ty::Lib(n, a) => format!(
                "{n}<{}>",
                a.iter()
                    .map(|x| self.key_of(x))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Ty::Any => "*".into(),
        }
    }

    /// The binding key of a type written in `f`: `Lazy<T>` / `Provider<T>` are `T`, a map is
    /// keyed by its value type, annotations in type arguments are dropped.
    fn hilt_key(&self, f: &FileInfo, ty: &str, at: usize, qualifiers: &[String]) -> Option<String> {
        let mut clean = String::new();
        let mut skip = false;
        for w in
            ty.split_inclusive(|c: char| !(c.is_alphanumeric() || c == '_' || c == '@' || c == '.'))
        {
            if w.starts_with('@') {
                skip = true;
                continue;
            }
            if skip && w.trim().is_empty() {
                skip = false;
                continue;
            }
            skip = false;
            clean.push_str(w);
        }
        let t = self.type_text(f, &clean, at, &[])?;
        let t = unwrap_provider(t);
        let t = match t {
            Ty::Lib(n, a) if n.ends_with("Map") && a.len() == 2 => {
                Ty::Lib("Map".into(), vec![Ty::Any, unwrap_provider(a[1].clone())])
            }
            Ty::Lib(n, a) if n.ends_with("Set") => Ty::Lib("Set".into(), a),
            t => t,
        };
        let mut k = self.key_of(&t);
        for q in qualifiers {
            k.push_str(" @");
            k.push_str(q);
        }
        Some(k)
    }

    /// Qualifier annotations (`@Named("io")`, an annotation class marked `@Qualifier`) of the
    /// declaration or parameter whose name is token `i`, and the names of all its annotations.
    fn qualifiers_before(&self, f: &FileInfo, i: usize, param: bool) -> (Vec<String>, Vec<String>) {
        let (mut quals, mut all) = (Vec::new(), Vec::new());
        let mut k = i;
        if !param && !f.java {
            // back to `fun` / `val` / `var` / `class` over a receiver and type parameters
            while k > 0
                && i - k < 40
                && !matches!(
                    f.t(k),
                    "fun" | "val" | "var" | "class" | "object" | "interface"
                )
            {
                k -= 1;
            }
        }
        if f.java {
            // over the type: `Lazy<Foo> name`
            while k > 0
                && (f.kind(k - 1) == Some(Tok::Ident)
                    || matches!(f.t(k - 1), "." | "<" | ">" | "[" | "]" | "?" | ","))
                && !matches!(f.t(k - 1), "(")
            {
                if f.t(k - 1) == ","
                    && !(k > 1 && f.kind(k - 2) == Some(Tok::Ident) && f.t(k) != ">")
                {
                    break;
                }
                k -= 1;
            }
        }
        while k > 0 {
            k -= 1;
            let w = f.t(k);
            let ann = if w == ")" {
                let Some(o) = f.pair(k) else { break };
                if o > 0
                    && (f.kind(o - 1) == Some(Tok::Annotation)
                        || f.ident(o - 1)
                            && o > 2
                            && f.t(o - 2) == ":"
                            && f.kind(o - 3) == Some(Tok::Annotation))
                {
                    Some((o - 1, Some(f.text[f.start(o)..f.end(k)].to_string())))
                } else {
                    break;
                }
            } else if f.kind(k) == Some(Tok::Annotation)
                || f.ident(k)
                    && k > 1
                    && f.t(k - 1) == ":"
                    && f.kind(k - 2) == Some(Tok::Annotation)
            {
                Some((k, None))
            } else if f.kind(k) == Some(Tok::Keyword)
                && !matches!(
                    w,
                    "fun" | "class" | "object" | "interface" | "this" | "return" | "else"
                )
            {
                continue;
            } else {
                break;
            };
            if let Some((n, arg)) = ann {
                let name = f.t(n).trim_start_matches('@');
                let name = name.rsplit('.').next().unwrap_or(name).to_string();
                // `@field:Named`: the target is the annotation token before the `:`
                k = if f.kind(n) != Some(Tok::Annotation) {
                    n - 2
                } else {
                    n
                };
                if matches!(
                    name.as_str(),
                    "field" | "param" | "set" | "get" | "property" | "setparam" | "receiver"
                ) {
                    continue;
                }
                let qualifier = name == "Named"
                    || self
                        .index
                        .named(&name)
                        .any(|(_, _, d)| d.kind.is_type() && d.has_annotation("Qualifier"));
                if qualifier {
                    quals.push(format!("{name}{}", arg.unwrap_or_default()));
                }
                all.push(name);
            }
        }
        quals.sort();
        (quals, all)
    }

    /// What one file adds to the Hilt graph.
    pub fn hilt_file(&self, path: &Path) -> HiltFile {
        let mut out = HiltFile::default();
        let Some(f) = self.file(path) else { return out };
        for (di, d) in f.syms.decls.iter().enumerate() {
            let r = (f.path.clone(), d.offset);
            let Some(ti) = f.tok_at(d.offset) else {
                continue;
            };
            match d.kind {
                Kind::Fun if d.has_annotation("Provides") || d.has_annotation("Binds") => {
                    let (quals, all) = self.qualifiers_before(&f, ti, false);
                    let ty = match self.type_of_decl(&r) {
                        Some(t) => Some(t),
                        None => self.type_text(&f, &d.ty, d.offset, &f.type_params(d)),
                    };
                    let base = if d.ty.is_empty() {
                        ty.map(|t| self.key_of(&unwrap_provider(t)))
                    } else {
                        self.hilt_key(&f, &d.ty, d.offset, &[])
                    };
                    if let Some(mut key) = base {
                        if all.iter().any(|a| a == "IntoSet") {
                            key = format!("Set<{key}>");
                        } else if all.iter().any(|a| a == "IntoMap") {
                            key = format!("Map<*,{key}>");
                        }
                        for q in &quals {
                            key.push_str(" @");
                            key.push_str(q);
                        }
                        out.providers.push(Binding {
                            decl: r.clone(),
                            key,
                        });
                    }
                    for (pi, p) in d.params.iter().enumerate() {
                        self.add_site(&f, &mut out, &r, Some(pi), p.offset as usize, &p.ty);
                    }
                }
                _ if d.kind.is_type() => {
                    let header = f.bodies.get(&di).map_or("", |b| {
                        &f.text[b.start..b.end.min(b.start + 400 + d.params.len() * 200)]
                    });
                    let assisted = header.contains("@AssistedInject");
                    if d.has_annotation("Inject") || assisted {
                        if !assisted {
                            out.providers.push(Binding {
                                decl: r.clone(),
                                key: self.key_of(&Ty::Decl(r.clone())),
                            });
                        }
                        for (pi, p) in d.params.iter().enumerate() {
                            let pt = f.tok_at(p.offset as usize).unwrap_or(0);
                            let (_, all) = self.qualifiers_before(&f, pt, true);
                            if all.iter().any(|a| a == "Assisted") {
                                continue;
                            }
                            self.add_site(&f, &mut out, &r, Some(pi), p.offset as usize, &p.ty);
                        }
                    }
                }
                Kind::Property if d.has_annotation("Inject") => {
                    self.add_site(&f, &mut out, &r, None, d.offset, &d.ty);
                }
                _ => {}
            }
        }
        out
    }

    fn add_site(
        &self,
        f: &FileInfo,
        out: &mut HiltFile,
        owner: &DeclRef,
        param: Option<usize>,
        at: usize,
        ty: &str,
    ) {
        let Some(ti) = f.tok_at(at) else { return };
        let (quals, _) = self.qualifiers_before(f, ti, param.is_some());
        if let Some(key) = self.hilt_key(f, ty, at, &quals) {
            out.sites.push(Site {
                owner: owner.clone(),
                param,
                at,
                key,
            });
        }
    }
}

fn unwrap_provider(t: Ty) -> Ty {
    match t {
        Ty::Lib(n, a) if matches!(n.as_str(), "Lazy" | "Provider") && a.len() == 1 => a[0].clone(),
        t => t,
    }
}

/// The project's Hilt graph from each file's part.
#[derive(Default)]
pub struct HiltGraph {
    pub files: HashMap<PathBuf, (u64, Rc<HiltFile>)>,
    /// The index generation the parts are up to date with, and the files still to read.
    at: Option<u64>,
    pending: Vec<PathBuf>,
    round: u64,
}

fn has_hilt(f: &FileSyms) -> bool {
    f.decls.iter().any(|d| {
        d.annotations
            .iter()
            .any(|a| matches!(&**a, "Provides" | "Binds" | "Inject" | "AssistedInject"))
    })
}

impl HiltGraph {
    /// Brings the parts up to date: at first every file with Hilt annotations, later only the
    /// files changed since. Returns whether it finished (else `stop` cut it short; call again).
    pub fn refresh(&mut self, r: &Resolver, stop: &dyn Fn() -> bool) -> bool {
        if self.pending.is_empty() {
            let changed = self.at.and_then(|g| r.index.changed_since(g));
            self.round = r.index.generation;
            match changed {
                Some(c) => {
                    for p in c {
                        let p = p.to_path_buf();
                        match r.index.files.get(&p) {
                            Some(f) if has_hilt(f) => self.pending.push(p),
                            _ => {
                                self.files.remove(&p);
                            }
                        }
                    }
                }
                None => {
                    self.pending = r
                        .index
                        .files
                        .iter()
                        .filter(|(_, f)| has_hilt(f))
                        .map(|(p, _)| p.clone())
                        .collect();
                    let keep: HashSet<&PathBuf> = self.pending.iter().collect();
                    self.files.retain(|p, _| keep.contains(p));
                }
            }
            if self.pending.is_empty() {
                self.at = Some(self.round);
                return true;
            }
        }
        while let Some(p) = self.pending.pop() {
            if stop() {
                self.pending.push(p);
                return false;
            }
            let Some(text) = r.texts.read(&p) else {
                continue;
            };
            let h = {
                use std::hash::{Hash, Hasher};
                let mut s = std::collections::hash_map::DefaultHasher::new();
                text.hash(&mut s);
                s.finish()
            };
            if self.files.get(&p).is_some_and(|(old, _)| *old == h) {
                continue;
            }
            r.changed(&p);
            let part = r.hilt_file(&p);
            self.files.insert(p, (h, Rc::new(part)));
            r.forget(512);
        }
        self.at = Some(self.round);
        true
    }

    fn providers(&self) -> impl Iterator<Item = &Binding> {
        self.files.values().flat_map(|(_, f)| f.providers.iter())
    }
    fn sites(&self) -> impl Iterator<Item = &Site> {
        self.files.values().flat_map(|(_, f)| f.sites.iter())
    }

    /// The providers a site gets: test code sees test modules, production code does not; a
    /// `release` source set gives way to the others.
    pub fn providers_of(&self, site: &Site) -> Vec<&Binding> {
        let test = is_test_path(&site.owner.0);
        let mut v: Vec<&Binding> = self
            .providers()
            .filter(|b| b.key == site.key && (test || !is_test_path(&b.decl.0)))
            .collect();
        if test && v.iter().any(|b| is_test_path(&b.decl.0)) {
            v.retain(|b| is_test_path(&b.decl.0));
        }
        if v.iter().any(|b| variant(&b.decl.0) != "release") {
            v.retain(|b| variant(&b.decl.0) != "release");
        }
        v.sort_by(|a, b| a.decl.cmp(&b.decl));
        v
    }

    /// The sites that get this binding.
    pub fn injected_in(&self, b: &Binding) -> Vec<&Site> {
        let mut v: Vec<&Site> = self
            .sites()
            .filter(|s| s.key == b.key && self.providers_of(s).iter().any(|p| p.decl == b.decl))
            .collect();
        v.sort_by(|a, b| (&a.owner, a.at).cmp(&(&b.owner, b.at)));
        v
    }

    #[cfg(test)]
    pub fn binding(&self, d: &DeclRef) -> Option<&Binding> {
        self.providers().find(|b| b.decl == *d)
    }

    #[cfg(test)]
    pub fn sites_of(&self, owner: &DeclRef) -> Vec<&Site> {
        self.sites().filter(|s| s.owner == *owner).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An index of `files` (path, text) and a resolver over their texts.
    fn project(files: &[(&str, &str)]) -> (Index, Texts) {
        let mut index = Index::default();
        let mut open = HashMap::new();
        for (p, t) in files {
            index.update(Path::new(p), t);
            open.insert(PathBuf::from(p), Arc::from(*t));
        }
        (index, Texts { open })
    }

    /// What the `n`-th `name` (from 1) in file `path` resolves to, as `file:line` of each
    /// declaration ("local" for a local, "-" for none).
    fn at(r: &Resolver, path: &str, name: &str, n: usize) -> String {
        let f = r.file(Path::new(path)).unwrap();
        let o = crate::refactor::occurrences(&f.text, name)[n - 1].start;
        match r.resolve(&f, o) {
            Res::Decls(v) => v
                .iter()
                .map(|(p, off)| {
                    let t = r.texts.read(p).unwrap();
                    format!("{}:{}", p.display(), t[..*off].matches('\n').count() + 1)
                })
                .collect::<Vec<_>>()
                .join(" "),
            Res::Local(_) => "local".into(),
            Res::None => "-".into(),
        }
    }

    fn usages_of(r: &Resolver, path: &str, name: &str, line: usize, files: &[&str]) -> Vec<String> {
        let f = r.file(Path::new(path)).unwrap();
        let d = f
            .syms
            .decls
            .iter()
            .find(|d| d.name == name && f.text[..d.offset].matches('\n').count() + 1 == line)
            .unwrap();
        let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        r.usages(&(PathBuf::from(path), d.offset), &files, &|| false)
            .into_iter()
            .map(|(p, o)| {
                let t = r.texts.read(&p).unwrap();
                format!("{}:{}", p.display(), t[..o].matches('\n').count() + 1)
            })
            .collect()
    }

    #[test]
    fn a_member_is_found_through_the_declared_type_of_its_receiver() {
        let (i, t) = project(&[
            (
                "/p/Dao.kt",
                "package a\ninterface Dao {\n    fun insert(u: Int)\n}\nclass Cache {\n    fun insert(k: String) {}\n}\n",
            ),
            (
                "/p/Repo.kt",
                "package a\nclass Repo(private val dao: Dao, private val cache: Cache) {\n    fun save() {\n        dao.insert(1)\n        cache.insert(\"k\")\n        val local = Cache()\n        local.insert(\"x\")\n    }\n}\n",
            ),
        ]);
        let r = Resolver::new(&i, &t);
        assert_eq!(at(&r, "/p/Repo.kt", "insert", 1), "/p/Dao.kt:3");
        assert_eq!(at(&r, "/p/Repo.kt", "insert", 2), "/p/Dao.kt:6");
        assert_eq!(
            at(&r, "/p/Repo.kt", "insert", 3),
            "/p/Dao.kt:6",
            "`val local = Cache()`"
        );
    }

    #[test]
    fn imports_aliases_stars_and_same_package_pick_the_class() {
        let (i, t) = project(&[
            ("/p/a/User.kt", "package a\nclass User\n"),
            ("/p/b/User.kt", "package b\nclass User\n"),
            (
                "/p/c/Use.kt",
                "package c\nimport b.User\nimport a.User as AUser\nval x: User? = null\nval y: AUser? = null\n",
            ),
            (
                "/p/d/Star.kt",
                "package d\nimport a.*\nval z: User? = null\n",
            ),
            ("/p/b/Same.kt", "package b\nval w: User? = null\n"),
        ]);
        let r = Resolver::new(&i, &t);
        assert_eq!(
            at(&r, "/p/c/Use.kt", "User", 3),
            "/p/b/User.kt:2",
            "the explicit import"
        );
        assert_eq!(
            at(&r, "/p/c/Use.kt", "AUser", 2),
            "/p/a/User.kt:2",
            "the alias"
        );
        assert_eq!(
            at(&r, "/p/d/Star.kt", "User", 1),
            "/p/a/User.kt:2",
            "the star import"
        );
        assert_eq!(
            at(&r, "/p/b/Same.kt", "User", 1),
            "/p/b/User.kt:2",
            "the same package"
        );
    }

    #[test]
    fn named_arguments_and_copy_mean_the_constructor_property() {
        let (i, t) = project(&[
            (
                "/p/S.kt",
                "package a\ndata class State(\n    val failureText: String? = null,\n    val title: Item? = null,\n) {\n    data class Item(val text: String)\n}\ndata class Other(val failureText: String?)\n",
            ),
            (
                "/p/U.kt",
                "package a\nfun f(s: State) {\n    State(failureText = \"x\")\n    s.copy(failureText = null)\n    Other(failureText = \"y\")\n}\n",
            ),
        ]);
        let r = Resolver::new(&i, &t);
        assert_eq!(at(&r, "/p/U.kt", "failureText", 1), "/p/S.kt:3");
        assert_eq!(at(&r, "/p/U.kt", "failureText", 2), "/p/S.kt:3", "copy()");
        assert_eq!(at(&r, "/p/U.kt", "failureText", 3), "/p/S.kt:8");
        // a nested class is seen from the constructor's parameters
        assert_eq!(at(&r, "/p/S.kt", "Item", 1), "/p/S.kt:6");
    }

    #[test]
    fn overloads_are_told_apart_by_arguments() {
        let (i, t) = project(&[
            (
                "/p/W.java",
                "package a;\npublic class W {\n    public static boolean available() {\n        return available(false);\n    }\n    public static boolean available(final boolean strict) {\n        return true;\n    }\n}\n",
            ),
            (
                "/p/M.kt",
                "package a\nclass Config\nclass Configs(val all: List<Config>)\nobject Mapper {\n    fun map(c: Config): Int = 1\n    fun map(cs: Configs): List<Int> = cs.all.map { map(it) }\n}\n",
            ),
        ]);
        let r = Resolver::new(&i, &t);
        assert_eq!(
            at(&r, "/p/W.java", "available", 2),
            "/p/W.java:6",
            "one argument"
        );
        assert_eq!(
            at(&r, "/p/M.kt", "map", 4),
            "/p/M.kt:5",
            "`it` of a List<Config>"
        );
    }

    #[test]
    fn a_call_through_an_override_counts_for_the_overridden_member_but_the_override_does_not() {
        let (i, t) = project(&[
            (
                "/p/R.kt",
                "package a\ninterface Repo {\n    fun load(): Int\n}\nclass RealRepo : Repo {\n    override fun load() = 1\n}\n",
            ),
            (
                "/p/U.kt",
                "package a\nfun f(r: Repo, real: RealRepo) {\n    r.load()\n    real.load()\n}\n",
            ),
        ]);
        let r = Resolver::new(&i, &t);
        let all = ["/p/R.kt", "/p/U.kt"];
        assert_eq!(
            usages_of(&r, "/p/R.kt", "load", 3, &all),
            ["/p/U.kt:3", "/p/U.kt:4"]
        );
        assert_eq!(
            at(&r, "/p/U.kt", "load", 2),
            "/p/R.kt:6",
            "goes to the override"
        );
    }

    #[test]
    fn java_statics_from_kotlin_and_from_a_subclass() {
        let (i, t) = project(&[
            (
                "/p/Base.java",
                "package a;\npublic class Base {\n    protected static final String EXTRA = \"e\";\n    public static float dp(float v, Object c) { return v; }\n}\n",
            ),
            (
                "/p/Sub.kt",
                "package a\nclass Sub : Base() {\n    val e = EXTRA\n    val d = Base.dp(1f, this)\n    companion object {\n        const val EXTRA = \"mine\"\n    }\n}\nclass Far {\n    val e = Base.EXTRA\n}\n",
            ),
        ]);
        let r = Resolver::new(&i, &t);
        assert_eq!(at(&r, "/p/Sub.kt", "dp", 1), "/p/Base.java:4");
        assert_eq!(
            at(&r, "/p/Sub.kt", "EXTRA", 1),
            "/p/Sub.kt:6",
            "its own companion first"
        );
        assert_eq!(at(&r, "/p/Sub.kt", "EXTRA", 3), "/p/Base.java:3");
    }

    #[test]
    fn a_local_shadows_a_member_of_the_same_name() {
        let (i, t) = project(&[
            ("/p/D.kt", "package a\ndata class Details(val max: Int?)\n"),
            (
                "/p/U.kt",
                "package a\nclass U(val max: Int) {\n    fun f(details: Details?): Int {\n        val max = details?.max ?: 5\n        return max\n    }\n    fun g() = max\n}\n",
            ),
        ]);
        let r = Resolver::new(&i, &t);
        assert_eq!(at(&r, "/p/U.kt", "max", 3), "/p/D.kt:2", "`details?.max`");
        assert_eq!(at(&r, "/p/U.kt", "max", 4), "local");
        assert_eq!(at(&r, "/p/U.kt", "max", 5), "/p/U.kt:2", "the property");
    }

    #[test]
    fn lambda_parameters_get_their_type_from_the_function_they_are_passed_to() {
        let (i, t) = project(&[
            (
                "/p/S.kt",
                "package a\ndata class Data(val failureText: String?)\nclass Holder\nfun Holder.updateIfSuccess(update: (Data) -> Data) {}\n",
            ),
            (
                "/p/U.kt",
                "package a\nfun f(h: Holder) {\n    h.updateIfSuccess(update = { current -> current.copy(failureText = null) })\n    h.updateIfSuccess { it.copy(failureText = \"x\") }\n}\n",
            ),
        ]);
        let r = Resolver::new(&i, &t);
        assert_eq!(
            at(&r, "/p/U.kt", "failureText", 1),
            "/p/S.kt:2",
            "a named lambda argument"
        );
        assert_eq!(
            at(&r, "/p/U.kt", "failureText", 2),
            "/p/S.kt:2",
            "a trailing lambda"
        );
    }

    #[test]
    fn invoking_a_use_case_property_gives_its_invoke_return_type() {
        let (i, t) = project(&[
            (
                "/p/S.kt",
                "package a\nclass Flow(val attempts: Int?)\nclass GetFlow {\n    operator fun invoke(): Flow? = null\n}\nclass Other(val attempts: Int)\n",
            ),
            (
                "/p/U.kt",
                "package a\nclass U(private val getFlow: GetFlow) {\n    fun f() {\n        val flow = getFlow()\n        val n = flow?.attempts\n    }\n}\n",
            ),
        ]);
        let r = Resolver::new(&i, &t);
        assert_eq!(at(&r, "/p/U.kt", "attempts", 1), "/p/S.kt:2");
    }

    #[test]
    fn an_extension_receiver_type_counts_as_a_use_of_the_type() {
        let (i, t) = project(&[
            ("/p/S.kt", "package a\nclass Shortcut\n"),
            (
                "/p/U.kt",
                "package a\nprivate fun Shortcut.toDomain() = 1\n",
            ),
        ]);
        let r = Resolver::new(&i, &t);
        assert_eq!(
            usages_of(&r, "/p/S.kt", "Shortcut", 2, &["/p/U.kt"]),
            ["/p/U.kt:2"]
        );
    }

    fn graph(r: &Resolver) -> HiltGraph {
        let mut g = HiltGraph::default();
        while !g.refresh(r, &|| false) {}
        g
    }

    fn providers(
        r: &Resolver,
        g: &HiltGraph,
        path: &str,
        line: usize,
        param: Option<usize>,
    ) -> Vec<String> {
        g.sites()
            .filter(|s| s.owner.0 == Path::new(path) && s.param == param)
            .filter(|s| {
                r.texts.read(&s.owner.0).unwrap()[..s.at]
                    .matches('\n')
                    .count()
                    + 1
                    == line
            })
            .flat_map(|s| g.providers_of(s))
            .map(|b| {
                let t = r.texts.read(&b.decl.0).unwrap();
                format!(
                    "{}:{}",
                    b.decl.0.display(),
                    t[..b.decl.1].matches('\n').count() + 1
                )
            })
            .collect()
    }

    #[test]
    fn hilt_matches_type_and_qualifier_like_dagger() {
        let (i, t) = project(&[
            (
                "/p/m/src/main/java/a/Q.kt",
                "package a\n@Qualifier\nannotation class Io\n@Qualifier\nannotation class Main\ninterface Repo\nclass RealRepo @Inject constructor() : Repo\nclass Dispatcher\n",
            ),
            (
                "/p/m/src/main/java/a/Mod.kt",
                "package a\n@Module\nobject Mod {\n    @Provides\n    @Io\n    fun io(): Dispatcher = Dispatcher()\n    @Provides @Main fun main(): Dispatcher = Dispatcher()\n}\n@Module\ninterface Binds {\n    @Binds\n    fun repo(impl: RealRepo): Repo\n}\n",
            ),
            (
                "/p/m/src/main/java/a/Vm.kt",
                "package a\nclass Vm\n@Inject constructor(\n    private val repo: Lazy<Repo>,\n    @Io private val io: Dispatcher,\n    @Main val main: Dispatcher,\n)\n",
            ),
            (
                "/p/m/src/test/java/a/FakeMod.kt",
                "package a\n@Module\ninterface FakeBinds {\n    @Binds\n    fun repo(impl: RealRepo): Repo\n}\n",
            ),
            (
                "/p/m/src/main/java/a/Act.java",
                "package a;\npublic class Act {\n    @Inject\n    @Io\n    Dispatcher dispatcher;\n}\n",
            ),
        ]);
        let r = Resolver::new(&i, &t);
        let g = graph(&r);
        let vm = "/p/m/src/main/java/a/Vm.kt";
        assert_eq!(
            providers(&r, &g, vm, 4, Some(0)),
            ["/p/m/src/main/java/a/Mod.kt:12"],
            "@Binds, not the test module; Lazy<T> is T"
        );
        assert_eq!(
            providers(&r, &g, vm, 5, Some(1)),
            ["/p/m/src/main/java/a/Mod.kt:6"],
            "@Io"
        );
        assert_eq!(
            providers(&r, &g, vm, 6, Some(2)),
            ["/p/m/src/main/java/a/Mod.kt:7"],
            "@Main"
        );
        assert_eq!(
            providers(&r, &g, "/p/m/src/main/java/a/Act.java", 5, None),
            ["/p/m/src/main/java/a/Mod.kt:6"],
            "a Java field"
        );
        // the @Binds function takes the @Inject class
        assert_eq!(
            providers(&r, &g, "/p/m/src/main/java/a/Mod.kt", 12, Some(0)),
            ["/p/m/src/main/java/a/Q.kt:7"]
        );
    }

    #[test]
    fn hilt_multibindings_give_a_set_every_contribution() {
        let (i, t) = project(&[(
            "/p/src/main/java/a/M.kt",
            "package a\ninterface Nav\nclass A @Inject constructor() : Nav\nclass B @Inject constructor() : Nav\n@Module\ninterface M {\n    @Binds @IntoSet fun a(x: A): Nav\n    @Binds\n    @IntoSet\n    fun b(x: B): Nav\n}\nclass Host @Inject constructor(val navs: Set<@JvmSuppressWildcards Nav>, @Assisted val id: String)\n",
        )]);
        let r = Resolver::new(&i, &t);
        let g = graph(&r);
        assert_eq!(
            providers(&r, &g, "/p/src/main/java/a/M.kt", 12, Some(0)),
            ["/p/src/main/java/a/M.kt:7", "/p/src/main/java/a/M.kt:10"]
        );
        assert!(
            g.sites().all(|s| s.param != Some(1)),
            "an @Assisted parameter is not injected"
        );
        assert!(
            !is_test_path(Path::new("/tmp/snap-test/app/src/main/A.kt")),
            "a folder above the project"
        );
        assert!(
            is_test_path(Path::new("/p/core-test/src/main/A.kt"))
                && is_test_path(Path::new("/p/a/src/androidTest/A.kt"))
        );
    }

    #[test]
    fn enum_entries_give_lambda_parameters_their_type() {
        let (i, t) = project(&[
            (
                "/p/E.kt",
                "package a\nenum class E(val id: String) { A(\"a\") }\nenum class F { B }\nfun E.nav() = 1\nfun F.nav() = 2\n",
            ),
            (
                "/p/T.kt",
                "package a\nfun t() {\n    E.entries.forEach { x -> x.nav() }\n}\n",
            ),
        ]);
        let r = Resolver::new(&i, &t);
        assert_eq!(at(&r, "/p/T.kt", "nav", 1), "/p/E.kt:4");
    }
}
