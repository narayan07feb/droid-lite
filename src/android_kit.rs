//! Kotlin and Android helpers with no UI: JUnit targets and their Gradle tasks, build variants,
//! live templates, new-file templates, quick fixes, string extraction, manifest components and
//! crash traces from Logcat.

use crate::compose::Lint;
use crate::kotlin::{FileSyms, Kind};
use crate::lexer::{Tok, lex};
use std::ops::Range;
use std::path::Path;

fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
}

fn unquote(s: &str) -> &str {
    s.trim_matches(|c| c == '"' || c == '\'')
}

/// The first quoted value after `key` on its line: `namespace = "com.app"` → `com.app`.
fn quoted_after<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|l| {
        let rest = l.trim().strip_prefix(key)?;
        let rest = rest.trim_start_matches([' ', '=', '(']).trim();
        let q = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let rest = &rest[1..];
        Some(&rest[..rest.find(q)?])
    })
}

// ---------- JUnit ----------

/// A test class, or one test function in it, that a gutter ▶ runs.
#[derive(Clone, Debug, PartialEq)]
pub struct TestTarget {
    /// `com.app.FooTest`
    pub class: String,
    /// The function name without backticks; `None` runs the whole class.
    pub method: Option<String>,
    /// From `androidTest`: runs on the device.
    pub device: bool,
}

/// `Some(true)` for an instrumented test source set, `Some(false)` for a unit test one.
fn test_source(path: &Path) -> Option<bool> {
    let comps: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let set = comps.windows(2).find(|w| w[0] == "src").map(|w| w[1])?;
    if set.starts_with("androidTest") || set == "androidInstrumentedTest" {
        Some(true)
    } else if set.starts_with("test") || set.ends_with("Test") {
        Some(false)
    } else {
        None
    }
}

/// The `@Test` functions of a test file, and each class that has one, by name offset.
pub fn test_targets(path: &Path, syms: &FileSyms) -> Vec<(usize, TestTarget)> {
    let Some(device) = test_source(path) else {
        return Vec::new();
    };
    let fq = |c: &str| {
        if syms.package.is_empty() {
            c.to_string()
        } else {
            format!("{}.{c}", syms.package)
        }
    };
    let tests: Vec<_> = syms
        .decls
        .iter()
        .filter(|d| d.kind == Kind::Fun && d.has_annotation("Test"))
        .collect();
    let mut out = Vec::new();
    for c in syms.decls.iter().filter(|d| d.kind == Kind::Class) {
        if tests
            .iter()
            .any(|t| t.container.as_deref() == Some(c.name.as_str()))
        {
            let target = TestTarget {
                class: fq(&c.name),
                method: None,
                device,
            };
            out.push((c.offset, target));
        }
    }
    for t in &tests {
        if let Some(c) = &t.container {
            let target = TestTarget {
                class: fq(c),
                method: Some(t.name.trim_matches('`').to_string()),
                device,
            };
            out.push((t.offset, target));
        }
    }
    out.sort_by_key(|(o, _)| *o);
    out
}

/// Instrumented tests run on a debug build: `release` → `debug`, `freeRelease` → `freeDebug`.
fn device_variant(v: &str) -> String {
    match v.strip_suffix("Release") {
        Some(f) => format!("{f}Debug"),
        None if v == "release" => "debug".into(),
        None => v.into(),
    }
}

/// The Gradle command line that runs `t` in `module` (`:app`), for the picked build variant.
pub fn test_task(module: &str, android: bool, variant: &str, t: &TestTarget) -> String {
    let m = if module == ":" { "" } else { module };
    if t.device {
        let class = match &t.method {
            Some(n) => format!("{}#{n}", t.class),
            None => t.class.clone(),
        };
        return format!(
            "{m}:connected{}AndroidTest -Pandroid.testInstrumentationRunnerArguments.class={class}",
            cap(&device_variant(variant))
        );
    }
    let filter = match &t.method {
        Some(n) => format!("{}.{n}", t.class),
        None => t.class.clone(),
    };
    let task = if android {
        format!("test{}UnitTest", cap(variant))
    } else {
        "test".into()
    };
    // the command line is split on spaces: a `name with spaces` becomes a pattern
    format!("{m}:{task} --tests {}", filter.replace(' ', "*"))
}

/// "3 tests failed" / "Tests passed" from Gradle's test output, when the run had tests.
pub fn test_summary(console: &[String]) -> Option<(String, bool)> {
    let failed = console
        .iter()
        .filter(|l| l.contains(" > ") && l.trim_end().ends_with(" FAILED"))
        .count();
    if failed > 0 {
        let s = if failed == 1 { "" } else { "s" };
        return Some((format!("{failed} test{s} failed"), false));
    }
    console
        .iter()
        .any(|l| l == "BUILD SUCCESSFUL" || l.starts_with("BUILD SUCCESSFUL in"))
        .then(|| ("Tests passed".to_string(), true))
}

// ---------- build variants ----------

/// The named blocks inside `block { }`: `release { }`, `create("staging") { }`, with their bodies.
fn blocks(src: &str, block: &str) -> Vec<(String, Range<usize>)> {
    let toks: Vec<_> = lex(src)
        .into_iter()
        .filter(|t| t.kind != Tok::Comment)
        .collect();
    let s = |i: usize| toks.get(i).map_or("", |t| &src[t.range.clone()]);
    let Some(start) = (0..toks.len()).find(|&i| s(i) == block && s(i + 1) == "{") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut open: Option<(Option<String>, usize)> = None;
    let mut depth = 0;
    for i in start + 1..toks.len() {
        match s(i) {
            "{" => {
                depth += 1;
                if depth == 2 {
                    let name = if s(i - 1) == ")" && toks[i - 2].kind == Tok::Str {
                        Some(unquote(s(i - 2)).to_string())
                    } else if toks[i - 1].kind == Tok::Ident && !matches!(s(i - 1), "all") {
                        Some(s(i - 1).to_string())
                    } else {
                        None
                    };
                    open = Some((name, toks[i].range.end));
                }
            }
            "}" => {
                if depth == 2
                    && let Some((Some(n), b)) = open.take()
                {
                    out.push((n, b..toks[i].range.start));
                }
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
    }
    out
}

/// The build variants of an app module's build file: flavours × build types, like
/// `debug`, `release`, `freeDebug`. Debug comes first.
pub fn variants(build: &str) -> Vec<String> {
    let mut types = vec!["debug".to_string(), "release".to_string()];
    for (n, _) in blocks(build, "buildTypes") {
        if !types.contains(&n) {
            types.push(n);
        }
    }
    // flavours grouped by dimension, in `flavorDimensions` order when it is given
    let mut groups: Vec<(String, Vec<String>)> = quoted_list(build, "flavorDimensions")
        .into_iter()
        .map(|d| (d, Vec::new()))
        .collect();
    for (n, body) in blocks(build, "productFlavors") {
        let dim = quoted_after(&build[body], "dimension")
            .unwrap_or("")
            .to_string();
        match groups.iter_mut().find(|(d, _)| *d == dim) {
            Some((_, v)) => v.push(n),
            None => groups.push((dim, vec![n])),
        }
    }
    let mut combos = vec![String::new()];
    for (_, flavours) in groups.iter().filter(|(_, v)| !v.is_empty()) {
        combos = combos
            .iter()
            .flat_map(|c| flavours.iter().map(move |f| format!("{c}{}", cap(f))))
            .collect();
    }
    combos
        .iter()
        .flat_map(|c| {
            types.iter().map(move |t| {
                let v = format!("{c}{}", cap(t));
                let mut ch = v.chars();
                ch.next()
                    .map_or(String::new(), |f| f.to_lowercase().chain(ch).collect())
            })
        })
        .collect()
}

/// The quoted strings of the statement starting with `key`: `flavorDimensions += listOf("a", "b")`.
fn quoted_list(text: &str, key: &str) -> Vec<String> {
    let Some(line) = text.lines().find(|l| l.trim().starts_with(key)) else {
        return Vec::new();
    };
    line.split(['"', '\''])
        .skip(1)
        .step_by(2)
        .map(String::from)
        .collect()
}

/// `installDebug` → `install{Variant}` for the picked variant.
pub fn install_task(module: &str, variant: &str) -> String {
    let m = if module == ":" { "" } else { module };
    let task = format!("install{}", cap(variant));
    if m.is_empty() {
        task
    } else {
        format!("{m}:{task}")
    }
}

// ---------- live templates ----------

pub struct Template {
    pub abbr: &'static str,
    pub about: &'static str,
    /// The text; `$0` is where the cursor goes. Lines after the first get the line's indent.
    pub body: &'static str,
    pub imports: &'static [&'static str],
}

const COMPOSABLE: &str = "androidx.compose.runtime.Composable";
const MODIFIER: &str = "androidx.compose.ui.Modifier";
const REMEMBER: &str = "androidx.compose.runtime.remember";

pub const TEMPLATES: &[Template] = &[
    Template {
        abbr: "comp",
        about: "@Composable function",
        body: "@Composable\nfun $0(modifier: Modifier = Modifier) {\n}",
        imports: &[COMPOSABLE, MODIFIER],
    },
    Template {
        abbr: "prev",
        about: "@Preview function",
        body: "@Preview(showBackground = true)\n@Composable\nprivate fun $0Preview() {\n}",
        imports: &["androidx.compose.ui.tooling.preview.Preview", COMPOSABLE],
    },
    Template {
        abbr: "vm",
        about: "ViewModel class",
        body: "class $0ViewModel : ViewModel() {\n}",
        imports: &["androidx.lifecycle.ViewModel"],
    },
    Template {
        abbr: "rem",
        about: "var x by remember { mutableStateOf() }",
        body: "var $0 by remember { mutableStateOf(false) }",
        imports: &[
            "androidx.compose.runtime.getValue",
            "androidx.compose.runtime.mutableStateOf",
            REMEMBER,
            "androidx.compose.runtime.setValue",
        ],
    },
    Template {
        abbr: "le",
        about: "LaunchedEffect(Unit) { }",
        body: "LaunchedEffect(Unit) {\n    $0\n}",
        imports: &["androidx.compose.runtime.LaunchedEffect"],
    },
    Template {
        abbr: "col",
        about: "Column { }",
        body: "Column(modifier = Modifier) {\n    $0\n}",
        imports: &["androidx.compose.foundation.layout.Column", MODIFIER],
    },
    Template {
        abbr: "row",
        about: "Row { }",
        body: "Row(modifier = Modifier) {\n    $0\n}",
        imports: &["androidx.compose.foundation.layout.Row", MODIFIER],
    },
    Template {
        abbr: "box",
        about: "Box { }",
        body: "Box(modifier = Modifier) {\n    $0\n}",
        imports: &["androidx.compose.foundation.layout.Box", MODIFIER],
    },
    Template {
        abbr: "lazy",
        about: "LazyColumn with items()",
        body: "LazyColumn(modifier = Modifier) {\n    items($0) { item ->\n    }\n}",
        imports: &[
            "androidx.compose.foundation.lazy.LazyColumn",
            "androidx.compose.foundation.lazy.items",
            MODIFIER,
        ],
    },
    Template {
        abbr: "logd",
        about: "Log.d(TAG, \"…\")",
        body: "Log.d(TAG, \"$0\")",
        imports: &["android.util.Log"],
    },
    Template {
        abbr: "tag",
        about: "private const val TAG",
        body: "private const val TAG = \"$0\"",
        imports: &[],
    },
    Template {
        abbr: "toast",
        about: "Toast.makeText(…).show()",
        body: "Toast.makeText(context, \"$0\", Toast.LENGTH_SHORT).show()",
        imports: &["android.widget.Toast"],
    },
];

pub fn template(abbr: &str) -> Option<&'static Template> {
    TEMPLATES.iter().find(|t| t.abbr == abbr)
}

/// The template's text for a line indented with `indent`, and the cursor offset in it.
pub fn expand(t: &Template, indent: &str) -> (String, usize) {
    let body = t.body.replace('\n', &format!("\n{indent}"));
    let caret = body.find("$0").unwrap_or(body.len());
    (body.replacen("$0", "", 1), caret)
}

// ---------- new file ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewKind {
    File,
    Class,
    DataClass,
    Interface,
    Object,
    Enum,
    Screen,
    ViewModel,
    Activity,
    Fragment,
}

impl NewKind {
    pub const ALL: [NewKind; 10] = [
        NewKind::Screen,
        NewKind::ViewModel,
        NewKind::Class,
        NewKind::DataClass,
        NewKind::Interface,
        NewKind::Object,
        NewKind::Enum,
        NewKind::Activity,
        NewKind::Fragment,
        NewKind::File,
    ];

    pub fn label(self) -> &'static str {
        match self {
            NewKind::File => "Kotlin file",
            NewKind::Class => "Class",
            NewKind::DataClass => "Data class",
            NewKind::Interface => "Interface",
            NewKind::Object => "Object",
            NewKind::Enum => "Enum class",
            NewKind::Screen => "Composable screen (with preview)",
            NewKind::ViewModel => "ViewModel (with UI state)",
            NewKind::Activity => "Activity (Compose, added to the manifest)",
            NewKind::Fragment => "Fragment",
        }
    }

    /// The class name for what was typed: `Home` → `HomeViewModel` for a ViewModel.
    pub fn class_name(self, typed: &str) -> String {
        let suffix = match self {
            NewKind::Screen => "Screen",
            NewKind::ViewModel => "ViewModel",
            NewKind::Activity => "Activity",
            NewKind::Fragment => "Fragment",
            _ => "",
        };
        let name = cap(typed.trim().trim_end_matches(".kt"));
        if name.ends_with(suffix) {
            name
        } else {
            format!("{name}{suffix}")
        }
    }

    /// The new file's text.
    pub fn text(self, package: &str, name: &str) -> String {
        let head = if package.is_empty() {
            String::new()
        } else {
            format!("package {package}\n\n")
        };
        let imports = |list: &[&str]| -> String {
            let mut s: String = list.iter().map(|i| format!("import {i}\n")).collect();
            s.push('\n');
            s
        };
        let body = match self {
            NewKind::File => String::new(),
            NewKind::Class => format!("class {name} {{\n}}\n"),
            NewKind::DataClass => format!("data class {name}(\n    val id: String,\n)\n"),
            NewKind::Interface => format!("interface {name} {{\n}}\n"),
            NewKind::Object => format!("object {name} {{\n}}\n"),
            NewKind::Enum => format!("enum class {name} {{\n}}\n"),
            NewKind::Screen => {
                imports(&[
                    "androidx.compose.foundation.layout.Box",
                    COMPOSABLE,
                    MODIFIER,
                    "androidx.compose.ui.tooling.preview.Preview",
                ]) + &format!(
                    "@Composable\nfun {name}(modifier: Modifier = Modifier) {{\n    Box(modifier = modifier) {{\n    }}\n}}\n\n\
                 @Preview(showBackground = true)\n@Composable\nprivate fun {name}Preview() {{\n    {name}()\n}}\n"
                )
            }
            NewKind::ViewModel => {
                let state = format!("{}UiState", name.trim_end_matches("ViewModel"));
                imports(&[
                    "androidx.lifecycle.ViewModel",
                    "kotlinx.coroutines.flow.MutableStateFlow",
                    "kotlinx.coroutines.flow.StateFlow",
                    "kotlinx.coroutines.flow.asStateFlow",
                ]) + &format!(
                    "class {name} : ViewModel() {{\n    private val _uiState = MutableStateFlow({state}())\n    \
                     val uiState: StateFlow<{state}> = _uiState.asStateFlow()\n}}\n\n\
                     data class {state}(\n    val loading: Boolean = false,\n)\n"
                )
            }
            NewKind::Activity => {
                imports(&[
                    "android.os.Bundle",
                    "androidx.activity.ComponentActivity",
                    "androidx.activity.compose.setContent",
                ]) + &format!(
                    "class {name} : ComponentActivity() {{\n    override fun onCreate(savedInstanceState: Bundle?) {{\n        \
                 super.onCreate(savedInstanceState)\n        setContent {{\n        }}\n    }}\n}}\n"
                )
            }
            NewKind::Fragment => {
                imports(&["androidx.fragment.app.Fragment"])
                    + &format!("class {name} : Fragment() {{\n}}\n")
            }
        };
        head + &body
    }
}

/// The Kotlin package of a source folder: `app/src/main/kotlin/com/app/ui` → `com.app.ui`.
pub fn package_of(dir: &Path) -> String {
    let comps: Vec<&str> = dir
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let root = (0..comps.len().saturating_sub(2))
        .find(|&i| comps[i] == "src" && matches!(comps[i + 2], "kotlin" | "java"));
    match root {
        Some(i) => comps[i + 3..].join("."),
        None => String::new(),
    }
}

/// A name a Kotlin class can have.
pub fn valid_name(s: &str) -> bool {
    let s = s.trim().trim_end_matches(".kt");
    s.chars().next().is_some_and(|c| c.is_alphabetic())
        && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// Adds `<activity android:name="…" />` inside `<application>`; `None` when there is none.
pub fn add_activity(manifest: &str, class: &str) -> Option<String> {
    let at = manifest.rfind("</application>")?;
    let line = manifest[..at].rfind('\n').map_or(0, |i| i + 1);
    let indent = &manifest[line..at];
    let indent = if indent.trim().is_empty() { indent } else { "" };
    Some(format!(
        "{}{indent}    <activity\n{indent}        android:name=\"{class}\"\n{indent}        android:exported=\"false\" />\n{}",
        &manifest[..line],
        &manifest[line..]
    ))
}

// ---------- manifest ----------

/// An Activity, Service, Receiver, Provider or Application declared in a manifest.
#[derive(Clone, Debug, PartialEq)]
pub struct Component {
    /// Fully qualified class name.
    pub class: String,
    pub tag: &'static str,
    /// Offset of the `<tag` in the manifest.
    pub offset: usize,
    /// Has the `MAIN` / `LAUNCHER` intent filter.
    pub launcher: bool,
}

/// The value of `android:name="…"` in a tag header.
fn android_name(header: &str) -> Option<&str> {
    let mut rest = header;
    while let Some(i) = rest.find("android:name") {
        let after = rest[i + "android:name".len()..].trim_start();
        if let Some(v) = after.strip_prefix('=') {
            let v = v.trim_start();
            let q = v.chars().next()?;
            let v = &v[1..];
            return Some(&v[..v.find(q)?]);
        }
        rest = &rest[i + 1..];
    }
    None
}

/// The package that relative names in a manifest start from: its `package` attribute, else
/// the module's `namespace`.
pub fn namespace(manifest: &str, build: &str) -> String {
    let attr = manifest.find("<manifest").and_then(|i| {
        let header = &manifest[i..i + manifest[i..].find('>')?];
        let p = header.find("package")?;
        let v = header[p + 7..].trim_start().strip_prefix('=')?.trim_start();
        let q = v.chars().next()?;
        let v = &v[1..];
        Some(v[..v.find(q)?].to_string())
    });
    attr.or_else(|| quoted_after(build, "namespace").map(String::from))
        .unwrap_or_default()
}

/// The components a manifest declares, with their full class names.
pub fn components(manifest: &str, namespace: &str) -> Vec<Component> {
    let mut out = Vec::new();
    for tag in ["application", "activity", "service", "receiver", "provider"] {
        let open = format!("<{tag}");
        let mut from = 0;
        while let Some(i) = manifest[from..].find(&open).map(|i| i + from) {
            from = i + open.len();
            // `<activity-alias` is another tag
            if !manifest[from..].starts_with(|c: char| c.is_whitespace() || c == '>') {
                continue;
            }
            let Some(end) = manifest[i..].find('>').map(|e| i + e) else {
                break;
            };
            let header = &manifest[i..end];
            let Some(name) = android_name(header) else {
                continue;
            };
            let class = if let Some(rel) = name.strip_prefix('.') {
                format!("{namespace}.{rel}")
            } else if !name.contains('.') && !namespace.is_empty() {
                format!("{namespace}.{name}")
            } else {
                name.to_string()
            };
            let body = if header.ends_with('/') || tag == "application" {
                ""
            } else {
                let close = format!("</{tag}>");
                manifest[end..]
                    .find(&close)
                    .map_or("", |c| &manifest[end..end + c])
            };
            out.push(Component {
                class,
                tag,
                offset: i,
                launcher: body.contains("android.intent.category.LAUNCHER"),
            });
        }
    }
    out.sort_by_key(|c| c.offset);
    out
}

/// Example links for each `VIEW` intent filter: every scheme × host × path its `<data>` tags
/// allow (`demo://home`, `https://demo.app/item`). A `pathPattern` loses its wildcards.
pub fn deep_links(manifest: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, _) in manifest.match_indices("<intent-filter") {
        let end = manifest[i..]
            .find("</intent-filter>")
            .map_or(manifest.len(), |e| i + e);
        let block = &manifest[i..end];
        if !block.contains("android.intent.action.VIEW") {
            continue;
        }
        let (mut schemes, mut hosts, mut paths) = (Vec::new(), Vec::new(), Vec::new());
        for (d, _) in block.match_indices("<data") {
            let tag = &block[d..d + block[d..].find('>').unwrap_or(block.len() - d)];
            let get = |k: &str| {
                let pat = format!("android:{k}=\"");
                let at = tag.find(&pat)? + pat.len();
                Some(tag[at..at + tag[at..].find('"')?].to_string())
            };
            schemes.extend(get("scheme"));
            hosts.extend(get("host"));
            for k in ["path", "pathPrefix", "pathPattern"] {
                paths.extend(get(k).map(|p| p.replace(".*", "").replace('*', "")));
            }
        }
        if hosts.is_empty() {
            hosts.push(String::new());
        }
        if paths.is_empty() {
            paths.push(String::new());
        }
        for s in &schemes {
            for h in &hosts {
                for p in &paths {
                    out.push(format!("{s}://{h}{p}"));
                }
            }
        }
    }
    out.dedup();
    out
}

// ---------- quick fixes ----------

/// One ⌥Enter fix: text edits in the file, imports to add, and a string resource to create.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Fix {
    pub label: String,
    pub edits: Vec<(Range<usize>, String)>,
    pub imports: Vec<String>,
    /// `(name, value)` for `res/values/strings.xml`.
    pub string: Option<(String, String)>,
}

/// The `)` that closes the `(` at `open`, skipping strings.
fn close_paren(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut in_str = false;
    let mut prev = ' ';
    for (i, c) in text[open..].char_indices() {
        match c {
            '"' if prev != '\\' => in_str = !in_str,
            '(' if !in_str => depth += 1,
            ')' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
        prev = c;
    }
    None
}

fn line_range(text: &str, at: usize) -> Range<usize> {
    let a = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let b = text[at..].find('\n').map_or(text.len(), |i| at + i);
    a..b
}

/// The quoted text around `at` on its line (the range includes the quotes).
fn quoted_at(text: &str, at: usize) -> Option<Range<usize>> {
    let line = line_range(text, at);
    let mut start = None;
    let mut prev = ' ';
    for (i, c) in text[line.clone()].char_indices() {
        let i = line.start + i;
        if c == '"' && prev != '\\' {
            match start {
                None => start = Some(i),
                Some(s) => {
                    if (s..=i).contains(&at) && i > s {
                        return Some(s..i + 1);
                    }
                    start = None;
                }
            }
        }
        prev = c;
    }
    None
}

/// A resource name from the text: `Sign in to continue` → `sign_in_to_continue`.
pub fn string_key(value: &str) -> String {
    let words: Vec<String> = value
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(5)
        .map(str::to_lowercase)
        .collect();
    let key = words.join("_");
    if key.is_empty() || key.starts_with(|c: char| c.is_ascii_digit()) || !key.is_ascii() {
        format!(
            "text_{}",
            key.chars()
                .filter(char::is_ascii_alphanumeric)
                .collect::<String>()
        )
        .trim_end_matches('_')
        .to_string()
    } else {
        key
    }
}

/// Plain text as a `strings.xml` value (`&`, `<` and `'` escaped; `\'` stays).
pub fn xml_value(kotlin: &str) -> String {
    let mut out = String::new();
    let mut prev = ' ';
    for c in kotlin.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '\'' if prev != '\\' => out.push_str("\\'"),
            _ => out.push(c),
        }
        prev = c;
    }
    out
}

/// A `strings.xml` value as plain text, for editing.
pub fn plain_value(xml: &str) -> String {
    xml.replace("\\'", "'")
        .replace("\\\"", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

/// Replaces the value of the `<string>` whose name is at `name_at`.
pub fn set_string(xml: &str, name_at: usize, value: &str) -> Option<String> {
    let open = name_at + xml.get(name_at..)?.find('>')? + 1;
    if xml[..open].ends_with("/>") {
        return Some(format!(
            "{}>{value}</string>{}",
            &xml[..open - 2],
            &xml[open..]
        ));
    }
    let close = open + xml[open..].find("</string>")?;
    Some(format!("{}{value}{}", &xml[..open], &xml[close..]))
}

/// Adds `<string name="key">value</string>` to a `strings.xml` (or makes a new one).
pub fn add_string(xml: Option<&str>, key: &str, value: &str) -> String {
    let entry = format!("    <string name=\"{key}\">{value}</string>\n");
    match xml.and_then(|x| x.rfind("</resources>").map(|i| (x, i))) {
        Some((x, i)) => {
            let line = x[..i].rfind('\n').map_or(0, |n| n + 1);
            // `</resources>` on a line of its own, or after other text on its line
            let inline = !x[line..i].trim().is_empty();
            let (at, pre) = if inline { (i, "\n") } else { (line, "") };
            format!("{}{pre}{entry}{}", &x[..at], &x[at..])
        }
        None => format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<resources>\n{entry}</resources>\n"
        ),
    }
}

/// The fixes for the cursor's line: Compose checks on it, expression body, extract string.
/// `taken` says whether a string resource name is in use already.
pub fn quick_fixes(
    text: &str,
    syms: &FileSyms,
    lints: &[Lint],
    cursor: usize,
    xml: bool,
    taken: &dyn Fn(&str) -> bool,
) -> Vec<Fix> {
    let cursor = cursor.min(text.len());
    let line = line_range(text, cursor);
    let mut out = Vec::new();
    for l in lints.iter().filter(|l| line.contains(&l.offset)) {
        let name_end = l.offset + l.len;
        match l.rule {
            "remember-state" => {
                if let Some(open) = text[name_end..].find('(').map(|i| name_end + i)
                    && let Some(close) = close_paren(text, open)
                {
                    out.push(Fix {
                        label: "Wrap in remember { }".into(),
                        edits: vec![
                            (l.offset..l.offset, "remember { ".into()),
                            (close + 1..close + 1, " }".into()),
                        ],
                        imports: vec![REMEMBER.into()],
                        string: None,
                    });
                }
            }
            "effect-keys" => {
                let rest = &text[name_end..];
                let edit = if rest.trim_start().starts_with("()") {
                    let i = name_end + rest.find('(').unwrap_or(0);
                    (i..i + 2, "(Unit)".to_string())
                } else {
                    (name_end..name_end, "(Unit)".to_string())
                };
                out.push(Fix {
                    label: "Add the key Unit (runs once)".into(),
                    edits: vec![edit],
                    ..Fix::default()
                });
            }
            "collect-lifecycle" => out.push(Fix {
                label: "Use collectAsStateWithLifecycle()".into(),
                edits: vec![(l.offset..name_end, "collectAsStateWithLifecycle".into())],
                imports: vec!["androidx.lifecycle.compose.collectAsStateWithLifecycle".into()],
                string: None,
            }),
            "modifier-missing" | "modifier-default" => {
                let Some(d) = syms.decls.iter().find(|d| d.offset == l.offset) else {
                    continue;
                };
                let spans = crate::compose::param_names(text, d.offset);
                let fix = if l.rule == "modifier-default" {
                    modifier_default(text, d, &spans)
                } else {
                    modifier_missing(text, d, &spans)
                };
                out.extend(fix);
            }
            _ => {}
        }
    }
    if !xml {
        out.extend(expression_body(text, syms, line.clone()));
    }
    out.extend(extract_string(text, syms, cursor, xml, taken));
    out
}

fn modifier_default(
    text: &str,
    d: &crate::kotlin::Decl,
    spans: &[(String, Range<usize>)],
) -> Option<Fix> {
    let (p, (_, r)) = d
        .params
        .iter()
        .zip(spans)
        .find(|(p, _)| !p.has_default && (p.ty == "Modifier" || p.ty.ends_with(".Modifier")))?;
    let after = &text[r.end..];
    let ty = after.find(p.ty.as_str())?;
    let at = r.end + ty + p.ty.len();
    Some(Fix {
        label: format!("Default `{}` to `Modifier`", p.name),
        edits: vec![(at..at, " = Modifier".into())],
        ..Fix::default()
    })
}

/// Adds `modifier: Modifier = Modifier` as the first optional parameter (before any trailing
/// lambda), like the Compose API guidelines ask.
fn modifier_missing(
    text: &str,
    d: &crate::kotlin::Decl,
    spans: &[(String, Range<usize>)],
) -> Option<Fix> {
    const PARAM: &str = "modifier: Modifier = Modifier";
    let open = d.offset + text[d.offset..].find('(')?;
    let close = close_paren(text, open)?;
    let before = d
        .params
        .iter()
        .zip(spans)
        .find(|(p, _)| p.has_default || p.ty.contains("->"))
        .map(|(_, (_, r))| r.start);
    let multiline = text[open..close].contains('\n');
    let edit = match before {
        Some(at) => {
            let line = text[..at].rfind('\n').map_or(0, |i| i + 1);
            let sep = if multiline {
                format!(",\n{}", &text[line..at])
            } else {
                ", ".into()
            };
            (at..at, format!("{PARAM}{sep}"))
        }
        None => {
            let inner = text[open + 1..close].trim_end();
            let at = open + 1 + inner.len();
            if inner.is_empty() {
                (at..at, PARAM.to_string())
            } else if multiline {
                let last = spans.last().map_or(at, |(_, r)| r.start);
                let line = text[..last].rfind('\n').map_or(0, |i| i + 1);
                let indent = &text[line..last];
                let comma = if inner.ends_with(',') { "" } else { "," };
                (at..at, format!("{comma}\n{indent}{PARAM},"))
            } else {
                (at..at, format!(", {PARAM}"))
            }
        }
    };
    Some(Fix {
        label: format!("Add `{PARAM}`"),
        edits: vec![edit],
        imports: vec![MODIFIER.into()],
        string: None,
    })
}

/// `fun f(): Int { return 1 }` → `fun f(): Int = 1`, for a fun declared on the cursor's line.
fn expression_body(text: &str, syms: &FileSyms, line: Range<usize>) -> Option<Fix> {
    let d = syms
        .decls
        .iter()
        .find(|d| d.kind == Kind::Fun && line.contains(&d.offset))?;
    let body = d.body.clone()?;
    let inner = text
        .get(body.clone())?
        .strip_prefix('{')?
        .strip_suffix('}')?;
    let expr = inner.trim().strip_prefix("return ")?.trim();
    // one statement only: `return a; b()` keeps its body
    if expr.is_empty() || expr.contains(';') {
        return None;
    }
    Some(Fix {
        label: "Convert to expression body".into(),
        edits: vec![(body, format!("= {expr}"))],
        ..Fix::default()
    })
}

/// A string literal in a composable becomes `stringResource(R.string.x)`; an XML attribute
/// value becomes `@string/x`.
fn extract_string(
    text: &str,
    syms: &FileSyms,
    cursor: usize,
    xml: bool,
    taken: &dyn Fn(&str) -> bool,
) -> Option<Fix> {
    let q = quoted_at(text, cursor)?;
    let value = &text[q.start + 1..q.end - 1];
    if value.trim().is_empty() || value.contains('$') {
        return None;
    }
    if xml {
        // only the value of a text attribute (`android:text="Hello"`), not an id or a reference
        let before = text[..q.start].trim_end();
        let attr = before.rsplit(char::is_whitespace).next().unwrap_or("");
        let attr = attr.trim_end_matches('=').rsplit(':').next().unwrap_or("");
        if !before.ends_with('=')
            || value.starts_with(['@', '?'])
            || !matches!(
                attr,
                "text" | "hint" | "contentDescription" | "title" | "label" | "summary"
            )
        {
            return None;
        }
    } else if !syms
        .decls
        .iter()
        .any(|d| d.is_composable() && d.body.as_ref().is_some_and(|b| b.contains(&cursor)))
    {
        return None;
    }
    let base = string_key(value);
    let key = std::iter::once(base.clone())
        .chain((2..).map(|n| format!("{base}_{n}")))
        .find(|k| !taken(k))?;
    let (replace, imports) = if xml {
        (format!("\"@string/{key}\""), Vec::new())
    } else {
        (
            format!("stringResource(R.string.{key})"),
            vec!["androidx.compose.ui.res.stringResource".to_string()],
        )
    };
    Some(Fix {
        label: format!("Extract string resource `{key}`"),
        edits: vec![(q, replace)],
        imports,
        string: Some((key, xml_value(value))),
    })
}

// ---------- crashes in Logcat ----------

/// A crash the app's process logged (`FATAL EXCEPTION`), with its stack frames.
#[derive(Clone, Debug, PartialEq)]
pub struct Crash {
    pub process: String,
    /// The deepest `Caused by:` line, else the exception line.
    pub exception: String,
    /// `(package path, file, line)` per `at …(File.kt:42)` line, top first.
    pub frames: Vec<(String, String, usize)>,
}

impl Crash {
    /// `NullPointerException: Attempt to invoke…` without the package, kept short.
    pub fn short(&self) -> String {
        let (class, msg) = self
            .exception
            .split_once(':')
            .unwrap_or((&self.exception, ""));
        let class = class.rsplit('.').next().unwrap_or(class).trim();
        let msg = msg.trim();
        if msg.is_empty() {
            return class.to_string();
        }
        let mut m: String = msg.chars().take(60).collect();
        if m.len() < msg.len() {
            m.push('…');
        }
        format!("{class}: {m}")
    }
}

/// Collects the `AndroidRuntime` lines of a crash as Logcat lines arrive.
#[derive(Default)]
pub struct CrashWatch {
    lines: Vec<String>,
    pid: u32,
    active: bool,
}

impl CrashWatch {
    /// Feeds one Logcat line; returns a crash when its trace has ended.
    pub fn feed(&mut self, tag: &str, level: char, pid: u32, msg: &str) -> Option<Crash> {
        let runtime = tag == "AndroidRuntime" && level == 'E';
        if runtime && msg.starts_with("FATAL EXCEPTION") {
            let done = self.finish();
            self.active = true;
            self.pid = pid;
            return done;
        }
        if self.active && runtime && pid == self.pid {
            self.lines.push(msg.to_string());
            return if self.lines.len() >= 200 {
                self.finish()
            } else {
                None
            };
        }
        // a line from somewhere else ends the trace
        self.finish()
    }

    /// Ends the trace being collected (no new lines came).
    pub fn finish(&mut self) -> Option<Crash> {
        if !std::mem::take(&mut self.active) {
            return None;
        }
        let lines = std::mem::take(&mut self.lines);
        let process = lines
            .iter()
            .find_map(|l| l.strip_prefix("Process: "))
            .map(|r| r.split(',').next().unwrap_or("").trim().to_string())
            .unwrap_or_default();
        let first = lines
            .iter()
            .find(|l| {
                let t = l.trim();
                !t.is_empty() && !t.starts_with("Process:") && !t.starts_with("at ")
            })
            .cloned()
            .unwrap_or_default();
        let exception = lines
            .iter()
            .rev()
            .find_map(|l| l.trim().strip_prefix("Caused by: "))
            .map_or(first, String::from);
        let frames = lines
            .iter()
            .filter_map(|l| crate::logcat::stack_frame(l))
            .collect();
        Some(Crash {
            process,
            exception,
            frames,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kotlin::scan;

    #[test]
    fn junit_targets_and_their_gradle_tasks() {
        let src = "package com.app\n\nclass MathTest {\n    @Test\n    fun `adds two numbers`() {}\n    fun helper() {}\n}\n";
        let syms = scan(src);
        let unit = test_targets(
            Path::new("/p/app/src/test/kotlin/com/app/MathTest.kt"),
            &syms,
        );
        assert_eq!(unit.len(), 2, "{unit:?}");
        assert_eq!(unit[0].1.method, None);
        assert_eq!(unit[0].0, src.find("MathTest").unwrap());
        assert_eq!(unit[1].1.method.as_deref(), Some("adds two numbers"));
        assert_eq!(
            test_task(":app", true, "freeDebug", &unit[1].1),
            ":app:testFreeDebugUnitTest --tests com.app.MathTest.adds*two*numbers"
        );
        assert_eq!(
            test_task(":lib", false, "debug", &unit[0].1),
            ":lib:test --tests com.app.MathTest"
        );
        let device = test_targets(
            Path::new("/p/app/src/androidTest/java/com/app/MathTest.kt"),
            &syms,
        );
        assert_eq!(
            test_task(":app", true, "release", &device[1].1),
            ":app:connectedDebugAndroidTest -Pandroid.testInstrumentationRunnerArguments.class=com.app.MathTest#adds two numbers"
        );
        // not a test source set: no ▶
        assert!(test_targets(Path::new("/p/app/src/main/kotlin/A.kt"), &syms).is_empty());
        let out = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            test_summary(&out(&["MathTest > adds() FAILED", "BUILD FAILED in 2s"])),
            Some(("1 test failed".into(), false))
        );
        assert_eq!(
            test_summary(&out(&["BUILD SUCCESSFUL in 3s"])),
            Some(("Tests passed".into(), true))
        );
    }

    #[test]
    fn variants_are_flavours_times_build_types() {
        assert_eq!(variants("android { }"), ["debug", "release"]);
        let build = r#"
android {
    flavorDimensions += listOf("tier", "store")
    buildTypes {
        getByName("release") { isMinifyEnabled = true }
        create("staging") { initWith(getByName("debug")) }
    }
    productFlavors {
        create("play") { dimension = "store" }
        create("free") { dimension = "tier" }
        create("paid") { dimension = "tier" }
    }
}"#;
        let v = variants(build);
        assert_eq!(v.len(), 6, "{v:?}");
        assert_eq!(
            &v[..3],
            ["freePlayDebug", "freePlayRelease", "freePlayStaging"]
        );
        assert_eq!(v[3], "paidPlayDebug");
        // Groovy
        let groovy = "android {\n  buildTypes {\n    qa {\n    }\n  }\n}";
        assert_eq!(variants(groovy), ["debug", "release", "qa"]);
        assert_eq!(install_task(":app", "freeDebug"), ":app:installFreeDebug");
        assert_eq!(install_task(":", "debug"), "installDebug");
    }

    #[test]
    fn templates_expand_with_the_line_indent_and_a_cursor() {
        let (text, caret) = expand(template("comp").unwrap(), "    ");
        assert_eq!(
            text,
            "@Composable\n    fun (modifier: Modifier = Modifier) {\n    }"
        );
        assert_eq!(&text[..caret], "@Composable\n    fun ");
        let (text, caret) = expand(template("col").unwrap(), "  ");
        assert_eq!(&text[caret..], "\n  }");
        let mut abbrs: Vec<_> = TEMPLATES.iter().map(|t| t.abbr).collect();
        abbrs.dedup();
        assert_eq!(abbrs.len(), TEMPLATES.len());
    }

    #[test]
    fn new_files_get_their_package_name_and_manifest_entry() {
        assert_eq!(
            package_of(Path::new("/p/app/src/main/kotlin/com/app/ui")),
            "com.app.ui"
        );
        assert_eq!(package_of(Path::new("/p/app/src/main/java")), "");
        assert_eq!(package_of(Path::new("/p/docs")), "");
        assert_eq!(NewKind::ViewModel.class_name("home"), "HomeViewModel");
        assert_eq!(NewKind::Screen.class_name("HomeScreen.kt"), "HomeScreen");
        assert_eq!(NewKind::Class.class_name("Foo"), "Foo");
        let vm = NewKind::ViewModel.text("com.app", "HomeViewModel");
        assert!(vm.starts_with("package com.app\n\nimport androidx.lifecycle.ViewModel\n"));
        assert!(vm.contains("MutableStateFlow(HomeUiState())"));
        assert!(
            NewKind::Screen
                .text("", "Home")
                .contains("private fun HomePreview()")
        );
        assert!(valid_name("Home2") && !valid_name("2Home") && !valid_name("a b"));
        let m = "<manifest>\n    <application>\n        <activity android:name=\".Main\" />\n    </application>\n</manifest>\n";
        let added = add_activity(m, "com.app.ui.HomeActivity").unwrap();
        assert!(added.contains(
            "        <activity\n            android:name=\"com.app.ui.HomeActivity\"\n            android:exported=\"false\" />\n    </application>"
        ));
        assert_eq!(add_activity("<manifest/>", "X"), None);
    }

    #[test]
    fn manifest_components_resolve_names_and_find_the_launcher() {
        let m = r#"<manifest xmlns:android="http://schemas.android.com/apk/res/android">
    <application android:name=".App">
        <activity android:name=".MainActivity" android:exported="true">
            <intent-filter>
                <action android:name="android.intent.action.MAIN" />
                <category android:name="android.intent.category.LAUNCHER" />
            </intent-filter>
        </activity>
        <activity-alias android:name=".Alias" android:targetActivity=".MainActivity" />
        <activity android:name="com.other.SettingsActivity" />
        <service android:name="SyncService" />
    </application>
</manifest>"#;
        let ns = namespace(m, "android {\n    namespace = \"com.app\"\n}");
        assert_eq!(ns, "com.app");
        assert_eq!(namespace("<manifest package=\"com.old\">", ""), "com.old");
        let c = components(m, &ns);
        let names: Vec<_> = c.iter().map(|c| (c.class.as_str(), c.launcher)).collect();
        assert_eq!(
            names,
            [
                ("com.app.App", false),
                ("com.app.MainActivity", true),
                ("com.other.SettingsActivity", false),
                ("com.app.SyncService", false),
            ]
        );
        assert_eq!(&m[c[1].offset..c[1].offset + 9], "<activity");
        let links = r#"<activity android:name=".Main">
            <intent-filter>
                <action android:name="android.intent.action.VIEW" />
                <data android:scheme="https" android:host="demo.app" android:pathPrefix="/item" />
                <data android:scheme="demo" />
            </intent-filter>
            <intent-filter>
                <action android:name="android.intent.action.MAIN" />
                <data android:scheme="nope" />
            </intent-filter>
        </activity>"#;
        assert_eq!(
            deep_links(links),
            ["https://demo.app/item", "demo://demo.app/item"]
        );
        assert_eq!(
            deep_links(
                "<intent-filter><action android:name=\"android.intent.action.VIEW\"/><data android:scheme=\"demo\" android:host=\"home\" android:pathPattern=\"/p/.*\"/></intent-filter>"
            ),
            ["demo://home/p/"]
        );
    }

    fn fixes(src: &str, at: &str) -> Vec<Fix> {
        let syms = scan(src);
        let lints = crate::compose::lint(src, &syms);
        quick_fixes(src, &syms, &lints, src.find(at).unwrap(), false, &|k| {
            k == "hello"
        })
    }

    fn apply(src: &str, f: &Fix) -> String {
        crate::refactor::apply(src, &f.edits)
    }

    #[test]
    fn quick_fixes_for_compose_checks() {
        let src = "@Composable\nfun Card(title: String) {\n    val s = mutableStateOf(f(1))\n    LaunchedEffect {\n    }\n}\n";
        let f = fixes(src, "mutableStateOf");
        assert_eq!(f[0].label, "Wrap in remember { }");
        assert!(apply(src, &f[0]).contains("val s = remember { mutableStateOf(f(1)) }\n"));
        assert_eq!(f[0].imports, [REMEMBER]);
        let f = fixes(src, "LaunchedEffect");
        assert!(apply(src, &f[0]).contains("LaunchedEffect(Unit) {"));
        let f = fixes(src, "Card");
        assert!(
            apply(src, &f[0]).contains("fun Card(title: String, modifier: Modifier = Modifier)"),
            "{f:?}"
        );
        // before the first optional parameter and the trailing lambda
        let src = "@Composable\nfun Card(\n    title: String,\n    onClick: () -> Unit,\n) {\n    Text(title)\n}\n";
        let f = fixes(src, "Card");
        assert!(apply(src, &f[0]).contains(
            "    title: String,\n    modifier: Modifier = Modifier,\n    onClick: () -> Unit,"
        ));
        let src =
            "@Composable\nfun Card(title: String, modifier: Modifier) {\n    Text(title)\n}\n";
        let f = fixes(src, "Card");
        assert!(
            apply(src, &f[0]).contains("modifier: Modifier = Modifier)"),
            "{f:?}"
        );
        let src = "@Composable\nfun Card(vm: Vm) {\n    val s by vm.state.collectAsState()\n}\n";
        let f = fixes(src, "collectAsState");
        assert!(apply(src, &f[0]).contains("vm.state.collectAsStateWithLifecycle()"));
    }

    #[test]
    fn expression_body_and_string_extraction() {
        let src = "fun twice(x: Int): Int {\n    return x * 2\n}\n";
        let f = fixes(src, "twice");
        assert_eq!(apply(src, &f[0]), "fun twice(x: Int): Int = x * 2\n");
        assert!(fixes("fun f() {\n    a()\n    return 1\n}\n", "f(").is_empty());
        assert!(fixes("fun f(): Int {\n    return 1; log()\n}\n", "f(").is_empty());
        // a return that spans lines is still one expression
        let src = "fun n(): Int {\n    return listOf(1)\n        .size\n}\n";
        assert_eq!(
            apply(src, &fixes(src, "n(")[0]),
            "fun n(): Int = listOf(1)\n        .size\n"
        );
        let src = "@Composable\nfun Hi() {\n    Text(\"Hello\")\n    Text(\"Don't go\")\n    Text(\"$name\")\n}\n";
        let f = fixes(src, "ello");
        // `hello` is taken, so the next free name
        assert_eq!(f[0].label, "Extract string resource `hello_2`");
        assert!(apply(src, &f[0]).contains("Text(stringResource(R.string.hello_2))"));
        let f = fixes(src, "Don't");
        assert_eq!(f[0].string, Some(("don_t_go".into(), "Don\\'t go".into())));
        assert!(fixes(src, "name").is_empty(), "a string template stays");
        assert!(
            fixes("val a = \"Hello\"\n", "ello").is_empty(),
            "outside a composable"
        );
        let xml =
            "<TextView\n    android:id=\"@+id/t\"\n    android:text=\"Sign in &amp; go\" />\n";
        let syms = FileSyms::default();
        let f = quick_fixes(xml, &syms, &[], xml.find("Sign").unwrap(), true, &|_| false);
        assert_eq!(
            apply(xml, &f[0]),
            xml.replace("\"Sign in &amp; go\"", "\"@string/sign_in_amp_go\"")
        );
        let at = xml.find("+id").unwrap();
        assert!(quick_fixes(xml, &syms, &[], at, true, &|_| false).is_empty());
        assert_eq!(
            add_string(
                Some("<resources>\n    <string name=\"a\">A</string>\n</resources>\n"),
                "b",
                "B"
            ),
            "<resources>\n    <string name=\"a\">A</string>\n    <string name=\"b\">B</string>\n</resources>\n"
        );
        assert!(
            add_string(None, "b", "B")
                .contains("<resources>\n    <string name=\"b\">B</string>\n</resources>")
        );
        assert_eq!(string_key("Sign in to continue!"), "sign_in_to_continue");
        // the translations table edits values in place
        let x = "<resources>\n    <string name=\"a\">Old</string>\n    <string name=\"b\"/>\n</resources>\n";
        let a = x.find("\"a\"").unwrap() + 1;
        assert_eq!(
            set_string(x, a, "New").unwrap(),
            x.replace(">Old<", ">New<")
        );
        let b = x.find("\"b\"").unwrap() + 1;
        assert!(
            set_string(x, b, "B")
                .unwrap()
                .contains("<string name=\"b\">B</string>\n")
        );
        assert_eq!(plain_value("Don\\'t &amp; &lt;go&gt;"), "Don't & <go>");
        assert_eq!(
            xml_value(&plain_value("Don\\'t &amp; go")),
            "Don\\'t &amp; go"
        );
        assert_eq!(string_key("42 items"), "text_42items");
    }

    #[test]
    fn a_crash_trace_is_collected_until_another_line_comes() {
        let mut w = CrashWatch::default();
        let rt = |w: &mut CrashWatch, m: &str| w.feed("AndroidRuntime", 'E', 7, m);
        assert_eq!(rt(&mut w, "FATAL EXCEPTION: main"), None);
        for l in [
            "Process: com.demo.app, PID: 7",
            "java.lang.RuntimeException: Unable to start activity",
            "\tat android.app.ActivityThread.main(ActivityThread.java:8)",
            "Caused by: java.lang.NullPointerException: name was null",
            "\tat com.demo.ui.HomeKt.Home(Home.kt:42)",
        ] {
            assert_eq!(rt(&mut w, l), None);
        }
        let c = w
            .feed("ActivityManager", 'W', 1, "Force finishing")
            .unwrap();
        assert_eq!(c.process, "com.demo.app");
        assert_eq!(c.short(), "NullPointerException: name was null");
        assert_eq!(c.frames[1], ("com/demo/ui".into(), "Home.kt".into(), 42));
        assert_eq!(w.finish(), None, "reported once");
        // a line of another process inside the trace does not belong to it
        rt(&mut w, "FATAL EXCEPTION: main");
        assert!(w.feed("AndroidRuntime", 'E', 9, "other").is_some());
    }
}
