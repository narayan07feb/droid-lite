//! Compose analyser: the UI call tree of a composable, Compose lint checks, and parameter stability
//! (from skydoves' compose-stability-analyzer `.stability` dumps, Compose compiler reports, or a local guess).

use crate::index::Index;
use crate::kotlin::{Decl, FileSyms, Kind};
use crate::lexer::{Tok, lex};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub name: String,
    pub offset: usize,
    pub children: Vec<Node>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warning,
}

#[derive(Clone, Debug)]
pub struct Lint {
    pub offset: usize,
    pub len: usize,
    pub severity: Severity,
    pub rule: &'static str,
    pub message: String,
}

/// Common Compose UI functions from androidx, so the tree works before the project is indexed.
const UI: &[&str] = &[
    "AlertDialog",
    "AnimatedContent",
    "AnimatedVisibility",
    "AsyncImage",
    "Badge",
    "BadgedBox",
    "BasicText",
    "BasicTextField",
    "BottomAppBar",
    "BottomSheetScaffold",
    "Box",
    "BoxWithConstraints",
    "Button",
    "Canvas",
    "Card",
    "Checkbox",
    "CircularProgressIndicator",
    "Column",
    "CompositionLocalProvider",
    "Crossfade",
    "DropdownMenu",
    "DropdownMenuItem",
    "ElevatedButton",
    "ElevatedCard",
    "ExtendedFloatingActionButton",
    "FilledTonalButton",
    "FilterChip",
    "FloatingActionButton",
    "FlowColumn",
    "FlowRow",
    "HorizontalDivider",
    "HorizontalPager",
    "Icon",
    "IconButton",
    "Image",
    "LazyColumn",
    "LazyHorizontalGrid",
    "LazyRow",
    "LazyVerticalGrid",
    "LazyVerticalStaggeredGrid",
    "LinearProgressIndicator",
    "ListItem",
    "MaterialTheme",
    "ModalBottomSheet",
    "ModalDrawerSheet",
    "ModalNavigationDrawer",
    "NavHost",
    "NavigationBar",
    "NavigationBarItem",
    "NavigationRail",
    "OutlinedButton",
    "OutlinedCard",
    "OutlinedTextField",
    "RadioButton",
    "Row",
    "Scaffold",
    "SnackbarHost",
    "Spacer",
    "Surface",
    "Switch",
    "Tab",
    "TabRow",
    "Text",
    "TextButton",
    "TextField",
    "TopAppBar",
    "VerticalDivider",
    "VerticalPager",
];

/// Uppercase calls that build values, not UI.
fn is_value_type(name: &str) -> bool {
    const VALUES: &[&str] = &[
        "Alignment",
        "Arrangement",
        "Brush",
        "Color",
        "Dp",
        "IntOffset",
        "IntSize",
        "KeyboardActions",
        "KeyboardOptions",
        "Modifier",
        "Offset",
        "PaddingValues",
        "Rect",
        "RoundedCornerShape",
        "Size",
        "SolidColor",
        "TextStyle",
        "CircleShape",
        "CutCornerShape",
        "Pair",
        "Triple",
        "Exception",
        "IllegalStateException",
        "IllegalArgumentException",
        "Intent",
        "Bundle",
        "Locale",
        "File",
        "FocusRequester",
        "MutableInteractionSource",
    ];
    VALUES.contains(&name)
        || [
            "State",
            "Options",
            "Values",
            "Shape",
            "Style",
            "Spec",
            "Defaults",
            "Exception",
            "Event",
            "Action",
            "Request",
            "Response",
            "Data",
            "Model",
            "Uri",
        ]
        .iter()
        .any(|s| name.ends_with(s))
}

/// Is a call to `name` a UI node? Project composables yes, project classes no, then the built-in lists.
pub fn is_ui_call(name: &str, index: &Index, libs: &crate::libs::Libraries) -> bool {
    if UI.contains(&name)
        || index
            .find_decl(name, Kind::Fun)
            .is_some_and(|d| d.is_composable())
        || libs.named(name).any(|s| s.composable)
    {
        return true;
    }
    if index.find_type(name).is_some()
        || is_value_type(name)
        || libs.named(name).any(|s| s.kind.is_type())
    {
        return false;
    }
    true
}

/// The tree of UI calls inside a composable body.
pub fn tree(src: &str, decl: &Decl, is_ui: &dyn Fn(&str) -> bool) -> Vec<Node> {
    let Some(body) = decl.body.clone() else {
        return Vec::new();
    };
    let toks: Vec<_> = lex(&src[body.clone()])
        .into_iter()
        .filter(|t| t.kind != Tok::Comment)
        .collect();
    let words: Vec<&str> = toks
        .iter()
        .map(|t| &src[body.start + t.range.start..body.start + t.range.end])
        .collect();
    let offsets: Vec<usize> = toks.iter().map(|t| body.start + t.range.start).collect();
    let idents: Vec<bool> = toks.iter().map(|t| t.kind == Tok::Ident).collect();
    build(&words, &offsets, &idents, 0, words.len(), is_ui)
}

fn build(
    w: &[&str],
    offsets: &[usize],
    idents: &[bool],
    from: usize,
    to: usize,
    is_ui: &dyn Fn(&str) -> bool,
) -> Vec<Node> {
    let text = |i: usize| w.get(i).copied().unwrap_or("");
    let mut out = Vec::new();
    let mut i = from;
    while i < to {
        let called = matches!(text(i + 1), "(" | "{");
        let after_dot = i > 0 && matches!(text(i - 1), "." | "::");
        if idents[i]
            && called
            && !after_dot
            && w[i].starts_with(|c: char| c.is_ascii_uppercase())
            && is_ui(w[i])
        {
            // the call runs over `(...)` and then an optional trailing `{...}`
            let mut end = i + 1;
            for open in ["(", "{"] {
                if text(end) == open {
                    end = skip(w, end, to);
                }
            }
            out.push(Node {
                name: w[i].to_string(),
                offset: offsets[i],
                children: build(w, offsets, idents, i + 1, end, is_ui),
            });
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

fn skip(w: &[&str], i: usize, to: usize) -> usize {
    let (open, close) = if w[i] == "(" { ("(", ")") } else { ("{", "}") };
    let mut depth = 0;
    for (j, x) in w.iter().enumerate().take(to).skip(i) {
        if *x == open {
            depth += 1;
        } else if *x == close {
            depth -= 1;
            if depth == 0 {
                return j + 1;
            }
        }
    }
    to
}

fn returns_unit(d: &Decl) -> bool {
    matches!(d.ty.as_str(), "" | "Unit" | "kotlin.Unit")
}

/// `Some(true)` returns Unit, `Some(false)` returns a value, `None` when an `= expr` body hides the type.
fn returns(d: &Decl, src: &str) -> Option<bool> {
    let expr_body = d
        .body
        .as_ref()
        .is_some_and(|b| src[b.clone()].starts_with('='));
    if d.ty.is_empty() && expr_body {
        None
    } else {
        Some(returns_unit(d))
    }
}

fn is_modifier(ty: &str) -> bool {
    ty.trim_end_matches('?') == "Modifier"
}

/// Compose lint checks for every composable in a file.
pub fn lint(src: &str, syms: &FileSyms) -> Vec<Lint> {
    let mut out = Vec::new();
    for d in syms.decls.iter().filter(|d| d.is_composable()) {
        let name_lint = |rule, severity, message: String| Lint {
            offset: d.offset,
            len: d.name.len(),
            severity,
            rule,
            message,
        };
        let private = d
            .modifiers
            .iter()
            .any(|m| matches!(&**m, "private" | "internal"));
        let upper = d.name.starts_with(|c: char| c.is_ascii_uppercase());
        let unit = returns(d, src);
        if unit == Some(true) && !upper {
            out.push(name_lint(
                "naming",
                Severity::Warning,
                format!(
                    "Composables that emit UI are PascalCase: `{}`",
                    capitalize(&d.name)
                ),
            ));
        }
        if unit == Some(false) && upper && d.container.is_none() {
            out.push(name_lint(
                "naming",
                Severity::Warning,
                format!(
                    "Composables that return a value are camelCase, like `remember…`: `{}`",
                    decapitalize(&d.name)
                ),
            ));
        }
        let modifier = d.params.iter().position(|p| is_modifier(&p.ty));
        match modifier {
            None if unit == Some(true)
                && upper
                && !private
                && !d.is_preview()
                && !d.params.is_empty() =>
            {
                out.push(name_lint(
                    "modifier-missing",
                    Severity::Info,
                    "Public UI composables should take `modifier: Modifier = Modifier`".into(),
                ));
            }
            Some(m) => {
                let p = &d.params[m];
                if !p.has_default {
                    out.push(name_lint(
                        "modifier-default",
                        Severity::Warning,
                        format!("`{}: Modifier` should default to `Modifier`", p.name),
                    ));
                }
                if p.name != "modifier"
                    && d.params.iter().filter(|p| is_modifier(&p.ty)).count() == 1
                {
                    out.push(name_lint(
                        "modifier-name",
                        Severity::Info,
                        format!("Name the Modifier parameter `modifier`, not `{}`", p.name),
                    ));
                }
                if let Some(first_optional) = d.params.iter().position(|p| p.has_default)
                    && first_optional < m
                {
                    out.push(name_lint(
                        "modifier-order",
                        Severity::Warning,
                        "`modifier` should be the first optional parameter".into(),
                    ));
                }
            }
            None => {}
        }
        for p in &d.params {
            if p.ty.ends_with("ViewModel") && !d.is_preview() {
                out.push(name_lint("viewmodel-param", Severity::Info, format!("`{}` takes a ViewModel; pass state and callbacks so it can be previewed and tested", d.name)));
            }
        }
        if let Some(body) = d.body.clone() {
            body_lints(src, body, &mut out);
        }
    }
    out.sort_by_key(|l| l.offset);
    out
}

/// Checks inside a body: state without `remember`, effects without keys, `collectAsState` on Android.
fn body_lints(src: &str, body: std::ops::Range<usize>, out: &mut Vec<Lint>) {
    let toks: Vec<_> = lex(&src[body.clone()])
        .into_iter()
        .filter(|t| t.kind != Tok::Comment)
        .collect();
    let text = |i: usize| {
        toks.get(i).map_or("", |t| {
            &src[body.start + t.range.start..body.start + t.range.end]
        })
    };
    // brace stack: true when the block is a `remember { }` style lambda
    let mut stack: Vec<bool> = Vec::new();
    for (i, tok) in toks.iter().enumerate() {
        let w = text(i);
        let at = body.start + tok.range.start;
        match w {
            "{" => {
                // `remember {`, `remember(key) {`, `rememberSaveable {`, `derivedStateOf {`
                let mut j = i;
                if j > 0 && text(j - 1) == ")" {
                    let mut depth = 0;
                    while j > 0 {
                        j -= 1;
                        match text(j) {
                            ")" => depth += 1,
                            "(" => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                let callee = if j > 0 { text(j - 1) } else { "" };
                stack.push(
                    callee.starts_with("remember")
                        || callee == "derivedStateOf"
                        || callee == "lazy",
                );
            }
            "}" => {
                stack.pop();
            }
            _ if w.starts_with("mutable") && w.ends_with("StateOf")
                || matches!(w, "mutableStateListOf" | "mutableStateMapOf") =>
            {
                if text(i + 1) == "(" && !stack.iter().any(|r| *r) && text(i.wrapping_sub(1)) != "."
                {
                    out.push(Lint { offset: at, len: w.len(), severity: Severity::Warning, rule: "remember-state", message: format!("`{w}` is created again on every recomposition; wrap it in `remember {{ }}`") });
                }
            }
            "LaunchedEffect" | "DisposableEffect" | "produceState"
                if text(i + 1) == "{" || (text(i + 1) == "(" && text(i + 2) == ")") =>
            {
                out.push(Lint {
                    offset: at,
                    len: w.len(),
                    severity: Severity::Warning,
                    rule: "effect-keys",
                    message: format!("`{w}` needs at least one key, e.g. `{w}(Unit)` to run once"),
                });
            }
            "collectAsState" if text(i.wrapping_sub(1)) == "." => {
                out.push(Lint { offset: at, len: w.len(), severity: Severity::Info, rule: "collect-lifecycle", message: "On Android prefer `collectAsStateWithLifecycle()`, which stops collecting in the background".into() });
            }
            _ => {}
        }
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

fn decapitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_lowercase().chain(c).collect())
        .unwrap_or_default()
}

// ---------- stability ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Stable,
    Unstable,
    Runtime,
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParamStability {
    pub name: String,
    pub verdict: Verdict,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stability {
    pub name: String,
    pub skippable: Option<bool>,
    pub restartable: Option<bool>,
    pub params: Vec<ParamStability>,
    /// Where it came from: a report file, or "guess" for the local heuristic.
    pub source: String,
}

fn verdict(word: &str) -> Verdict {
    match word.to_ascii_lowercase().as_str() {
        "stable" | "@static" => Verdict::Stable,
        "unstable" => Verdict::Unstable,
        "runtime" => Verdict::Runtime,
        _ => Verdict::Unknown,
    }
}

/// Simple function name from `public fun com.example.UserCard(user: ...)` or `fun UserCard(`.
fn fun_name(line: &str) -> Option<String> {
    let after = line.split("fun ").nth(1)?;
    let head = after.split('(').next()?;
    Some(head.rsplit('.').next()?.trim().to_string())
}

/// Parses a skydoves compose-stability-analyzer `.stability` dump.
pub fn parse_stability_dump(text: &str, source: &str) -> Vec<Stability> {
    let mut out: Vec<Stability> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if !line.starts_with(' ') && t.contains("fun ") {
            if let Some(name) = fun_name(t) {
                out.push(Stability {
                    name,
                    skippable: None,
                    restartable: None,
                    params: Vec::new(),
                    source: source.into(),
                });
            }
        } else if let Some(s) = out.last_mut() {
            if let Some(v) = t.strip_prefix("skippable:") {
                s.skippable = Some(v.trim() == "true");
            } else if let Some(v) = t.strip_prefix("restartable:") {
                s.restartable = Some(v.trim() == "true");
            } else if let Some(p) = t.strip_prefix("- ") {
                let (name, rest) = p.split_once(':').unwrap_or((p, ""));
                let rest = rest.trim();
                let (word, reason) = rest.split_once(' ').unwrap_or((rest, ""));
                s.params.push(ParamStability {
                    name: name.trim().into(),
                    verdict: verdict(word),
                    reason: reason.trim().trim_matches(|c| c == '(' || c == ')').into(),
                });
            }
        }
    }
    out
}

/// Parses a Compose compiler `*-composables.txt` report.
pub fn parse_compiler_report(text: &str, source: &str) -> Vec<Stability> {
    let mut out: Vec<Stability> = Vec::new();
    let mut open = false;
    for line in text.lines() {
        let t = line.trim();
        if t.contains("fun ") && !line.starts_with(' ') {
            let head = t.split("fun ").next().unwrap_or("");
            if let Some(name) = fun_name(t) {
                out.push(Stability {
                    name,
                    skippable: Some(head.split_whitespace().any(|w| w == "skippable")),
                    restartable: Some(head.split_whitespace().any(|w| w == "restartable")),
                    params: Vec::new(),
                    source: source.into(),
                });
                open = !t.ends_with("()") && t.ends_with('(');
            }
        } else if open && t == ")" {
            open = false;
        } else if open && let Some(s) = out.last_mut() {
            let mut words = t.split_whitespace();
            let first = words.next().unwrap_or("");
            let (v, rest) = if matches!(first, "stable" | "unstable" | "runtime" | "unused") {
                (verdict(first), words.collect::<Vec<_>>().join(" "))
            } else {
                (Verdict::Unknown, t.to_string())
            };
            let rest = rest
                .trim_start_matches("@dynamic ")
                .trim_start_matches("@static ");
            let (name, ty) = rest.split_once(':').unwrap_or((rest, ""));
            s.params.push(ParamStability {
                name: name.trim().into(),
                verdict: v,
                reason: ty.split('=').next().unwrap_or("").trim().into(),
            });
        }
    }
    out
}

/// Finds and parses every stability source under the project's modules.
pub fn load_reports(module_dirs: &[PathBuf]) -> HashMap<String, Stability> {
    let mut map = HashMap::new();
    for dir in module_dirs {
        let mut files: Vec<(PathBuf, bool)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir.join("stability")) {
            files.extend(
                rd.filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|e| e == "stability"))
                    .map(|p| (p, true)),
            );
        }
        let build = dir.join("build");
        if build.is_dir() {
            files.extend(
                ignore::WalkBuilder::new(&build)
                    .standard_filters(false)
                    .max_depth(Some(4))
                    .filter_entry(|e| {
                        !matches!(
                            e.file_name().to_str(),
                            Some("intermediates" | "tmp" | "kotlin" | "generated" | "outputs")
                        )
                    })
                    .build()
                    .filter_map(Result::ok)
                    .map(|e| e.into_path())
                    .filter(|p| p.to_string_lossy().ends_with("-composables.txt"))
                    .map(|p| (p, false)),
            );
        }
        for (path, dump) in files {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let source = path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            let parsed = if dump {
                parse_stability_dump(&text, &source)
            } else {
                parse_compiler_report(&text, &source)
            };
            for s in parsed {
                // the skydoves dump wins over a compiler report for the same function
                if dump || !map.contains_key(&s.name) {
                    map.insert(s.name.clone(), s);
                }
            }
        }
    }
    map
}

/// Byte range of each parameter name in the declaration whose name starts at `name_at`
/// (`fun UserCard(user: User, modifier: Modifier = Modifier)`), for the inline stability chips.
pub fn param_names(src: &str, name_at: usize) -> Vec<(String, std::ops::Range<usize>)> {
    let b = src.as_bytes();
    let Some(open) = src
        .get(name_at..)
        .and_then(|t| t.find('('))
        .map(|i| name_at + i)
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let (mut depth, mut i, mut seg) = (0i32, open + 1, open + 1);
    // the type part of a parameter ends at `=`: only there do `<` and `>` nest
    let mut in_type = true;
    let mut colon = None;
    while i < b.len() {
        let c = b[i];
        match c {
            b'"' | b'\'' => {
                i += 1;
                while i < b.len() && b[i] != c {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'(' | b'[' | b'{' => depth += 1,
            b'<' if in_type => depth += 1,
            b'>' if in_type && b.get(i.wrapping_sub(1)) != Some(&b'-') => depth -= 1,
            b'=' if depth == 0 => in_type = false,
            b':' if depth == 0 && colon.is_none() => colon = Some(i),
            b')' | b']' | b'}' if depth > 0 => depth -= 1,
            b',' | b')' if depth == 0 => {
                if let Some(c) = colon {
                    // the name: the last word before `:` (after annotations and `vararg`)
                    let head = &src[seg..c];
                    let t = head.trim_end();
                    let start = t
                        .rfind(|ch: char| !(ch.is_alphanumeric() || ch == '_' || ch == '`'))
                        .map_or(0, |k| k + 1);
                    if start < t.len() {
                        out.push((
                            t[start..].trim_matches('`').to_string(),
                            seg + start..seg + t.len(),
                        ));
                    }
                }
                if c == b')' {
                    break;
                }
                (seg, in_type, colon) = (i + 1, true, None);
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Local guess when no report exists: mutable collections, project classes with `var`, interfaces.
pub fn guess(d: &Decl, index: &Index) -> Stability {
    let params = d
        .params
        .iter()
        .map(|p| {
            let base = p.ty.trim_end_matches('?');
            let raw = base.split('<').next().unwrap_or(base).trim();
            let (verdict, reason) = if raw.starts_with("Immutable") || raw.starts_with("Persistent")
            {
                (Verdict::Stable, "kotlinx immutable collection".to_string())
            } else if matches!(
                raw,
                "List"
                    | "Map"
                    | "Set"
                    | "Collection"
                    | "Iterable"
                    | "MutableList"
                    | "MutableMap"
                    | "MutableSet"
                    | "ArrayList"
                    | "HashMap"
                    | "HashSet"
                    | "Array"
            ) {
                (
                    Verdict::Unstable,
                    format!("`{raw}` may be mutable; use ImmutableList or an @Immutable wrapper"),
                )
            } else if p.ty.contains("->")
                || matches!(
                    raw,
                    "String"
                        | "Int"
                        | "Long"
                        | "Float"
                        | "Double"
                        | "Boolean"
                        | "Char"
                        | "Short"
                        | "Byte"
                        | "Unit"
                        | "Modifier"
                        | "Color"
                        | "Dp"
                        | "TextUnit"
                        | "TextStyle"
                        | "Shape"
                )
            {
                (
                    Verdict::Stable,
                    "primitive, function type or known stable Compose type".to_string(),
                )
            } else if let Some(c) = index.find_type(raw) {
                let marked = c.has_annotation("Immutable") || c.has_annotation("Stable");
                let has_var = index
                    .members(raw)
                    .any(|m| m.kind == Kind::Property && m.mutable);
                if marked {
                    (Verdict::Stable, "marked @Stable or @Immutable".into())
                } else if has_var {
                    (Verdict::Unstable, format!("`{raw}` has a `var` property"))
                } else if c.kind == Kind::Interface
                    || c.modifiers
                        .iter()
                        .any(|m| matches!(&**m, "abstract" | "open" | "sealed"))
                {
                    (
                        Verdict::Unknown,
                        "interface or open class; depends on the implementation".into(),
                    )
                } else if c.kind == Kind::Enum || c.kind == Kind::Object {
                    (Verdict::Stable, "enum or object".into())
                } else {
                    let vals = index
                        .members(raw)
                        .filter(|m| m.kind == Kind::Property)
                        .map(|m| m.ty.clone())
                        .collect::<Vec<_>>();
                    if vals.iter().any(|t| {
                        matches!(
                            t.split('<').next().unwrap_or("").trim(),
                            "List" | "Map" | "Set" | "MutableList"
                        )
                    }) {
                        (Verdict::Unstable, format!("`{raw}` holds a List/Map/Set"))
                    } else {
                        (
                            Verdict::Stable,
                            format!("`{raw}` has only `val` properties (inferred)"),
                        )
                    }
                }
            } else {
                (Verdict::Unknown, "type outside the project".into())
            };
            ParamStability {
                name: p.name.clone(),
                verdict,
                reason,
            }
        })
        .collect::<Vec<_>>();
    let skippable = params.iter().all(|p| p.verdict != Verdict::Unstable);
    Stability {
        name: d.name.clone(),
        skippable: Some(skippable && returns_unit(d)),
        restartable: Some(returns_unit(d)),
        params,
        source: "guess".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_each_parameter_name() {
        let src = "@Composable\nfun UserCard(\n    user: User,\n    @StringRes title: Int,\n    items: Map<String, List<Int>> = mapOf(\"a,b\" to listOf(1, 2)),\n    onClick: (Int, String) -> Unit = { a, b -> },\n    vararg tags: String,\n) {}";
        let at = src.find("UserCard").unwrap();
        let names = param_names(src, at);
        assert_eq!(
            names.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            ["user", "title", "items", "onClick", "tags"]
        );
        for (n, r) in &names {
            assert_eq!(&src[r.clone()], n);
        }
        let one = "fun A(a: Int = if (x < y) 1 else 2, b: Boolean)";
        assert_eq!(
            param_names(one, 4)
                .into_iter()
                .map(|(n, _)| n)
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(param_names("fun B() {}", 4).is_empty());
    }
    use crate::kotlin::scan;
    use std::path::Path;

    const SRC: &str = r#"
@Composable
fun Screen(vm: HomeViewModel, title: String = "", modifier: Modifier) {
    var count = mutableStateOf(0)
    val ok by remember { mutableStateOf(1) }
    val keyed = remember(title) { mutableIntStateOf(2) }
    val s = vm.state.collectAsState()
    LaunchedEffect { load() }
    LaunchedEffect(Unit) { load() }
    Scaffold(topBar = { TopAppBar(title = { Text(title) }) }) { padding ->
        Column(Modifier.padding(padding)) {
            if (count.value > 0) Text("x") else UserRow(User("a"))
            Spacer(Modifier.height(8.dp))
        }
    }
}

@Composable
fun userRow(user: User) { Text(user.name) }

@Composable
fun RememberThing(): Int = 1

@Composable
private fun chipText() = TextStyle(fontSize = 12.sp)
"#;

    #[test]
    fn builds_ui_tree_with_lambda_slots() {
        let f = scan(SRC);
        let d = f.decls.iter().find(|d| d.name == "Screen").unwrap();
        let t = tree(SRC, d, &|n| !matches!(n, "User"));
        let names = |v: &[Node]| v.iter().map(|n| n.name.clone()).collect::<Vec<_>>();
        assert_eq!(
            names(&t),
            vec!["LaunchedEffect", "LaunchedEffect", "Scaffold"]
        );
        let scaffold = &t[2];
        assert_eq!(names(&scaffold.children), vec!["TopAppBar", "Column"]);
        assert_eq!(names(&scaffold.children[0].children), vec!["Text"]);
        assert_eq!(
            names(&scaffold.children[1].children),
            vec!["Text", "UserRow", "Spacer"]
        );
    }

    #[test]
    fn lints_common_compose_mistakes() {
        let f = scan(SRC);
        let rules: Vec<_> = lint(SRC, &f).iter().map(|l| l.rule).collect();
        assert_eq!(
            rules.iter().filter(|r| **r == "remember-state").count(),
            1,
            "{rules:?}"
        );
        assert_eq!(rules.iter().filter(|r| **r == "effect-keys").count(), 1);
        for r in [
            "modifier-default",
            "modifier-order",
            "viewmodel-param",
            "collect-lifecycle",
            "naming",
        ] {
            assert!(rules.contains(&r), "{r} missing from {rules:?}");
        }
        assert_eq!(
            rules.iter().filter(|r| **r == "naming").count(),
            2,
            "userRow and RememberThing"
        );
    }

    #[test]
    fn parses_skydoves_dump() {
        let dump = "@Composable\npublic fun com.example.UserCard(user: com.example.User): kotlin.Unit\n  skippable: false\n  restartable: true\n  params:\n    - user: UNSTABLE (has mutable properties)\n";
        let s = parse_stability_dump(dump, "app.stability");
        assert_eq!(s[0].name, "UserCard");
        assert_eq!(s[0].skippable, Some(false));
        assert_eq!(
            s[0].params[0],
            ParamStability {
                name: "user".into(),
                verdict: Verdict::Unstable,
                reason: "has mutable properties".into()
            }
        );
    }

    #[test]
    fn parses_compiler_report() {
        let r = "restartable skippable scheme(\"[androidx.compose.ui.UiComposable]\") fun UserCard(\n  stable user: User\n  unstable items: List<String>\n  stable modifier: Modifier? = @static Companion\n)\nrestartable fun Empty()\n";
        let s = parse_compiler_report(r, "app-composables.txt");
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].skippable, Some(true));
        assert_eq!(
            s[0].params[1],
            ParamStability {
                name: "items".into(),
                verdict: Verdict::Unstable,
                reason: "List<String>".into()
            }
        );
        assert_eq!(s[0].params[2].reason, "Modifier?");
        assert_eq!(s[1].skippable, Some(false));
        assert!(s[1].params.is_empty());
    }

    #[test]
    fn guesses_stability_from_project_classes() {
        let mut i = Index::default();
        i.update(Path::new("/m.kt"), "data class User(val name: String, var age: Int)\n@Immutable data class Tags(val all: List<String>)\ninterface Repo\n@Composable fun Card(user: User, tags: Tags, items: List<String>, repo: Repo, onClick: () -> Unit) {}\n");
        let d = i.find_decl("Card", Kind::Fun).unwrap().clone();
        let v: Vec<_> = guess(&d, &i).params.iter().map(|p| p.verdict).collect();
        assert_eq!(
            v,
            vec![
                Verdict::Unstable,
                Verdict::Stable,
                Verdict::Unstable,
                Verdict::Unknown,
                Verdict::Stable
            ]
        );
    }
}
