//! What a preview compiles besides the open file, read from the project's files only (no Gradle):
//! the project's own source files it needs (followed through imports, a package at a time, main
//! source sets only) and `R` classes made from the `res` folders of the modules it uses.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};

/// Main-source Kotlin and Java files by package.
#[derive(Default)]
pub struct Packages {
    by_package: HashMap<String, Vec<PathBuf>>,
    /// The names each file declares (classes, functions, properties, type aliases).
    names: HashMap<PathBuf, BTreeSet<String>>,
    /// Module namespace (from `namespace = "…"`) -> the module's res folder.
    res_by_namespace: HashMap<String, Vec<PathBuf>>,
    /// The project's annotation classes (a multipreview like `@ThemePreviews`): name -> the annotations
    /// written above it.
    pub annotation_classes: HashMap<String, String>,
    /// Each source file's package and annotation classes, and each build file's namespace: a file
    /// added or removed changes only its own entries.
    sources: HashMap<PathBuf, (String, Vec<(String, String)>)>,
    builds: HashMap<PathBuf, String>,
    /// Files read so far, for tests.
    pub reads: usize,
}

/// A Kotlin or Java file outside test code and build output: no folder on its path is a test source
/// set (`test`, `androidTest`, `javatest`, `testFixtures`, …) or `build`.
/// Under `src/`, only what a debug build uses counts (`main`, `debug`, and Kotlin Multiplatform's
/// `commonMain`, `androidMain`, `jvmMain`): another build type or flavor folder (`src/release`,
/// `src/staging`) holds a second copy of a class. A module without `src/` (its own layout) counts.
fn is_main_source(p: &Path) -> bool {
    let s = p.to_string_lossy();
    if !(s.ends_with(".kt") || s.ends_with(".java")) {
        return false;
    }
    let parts: Vec<String> = p.parent().into_iter().flat_map(|d| d.components()).map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect();
    if parts.iter().any(|c| c == "build" || c.ends_with("test") || c.ends_with("tests") || c.starts_with("testfixtures")) {
        return false;
    }
    match parts.iter().rposition(|c| c == "src") {
        // a debug build: `main` plus `debug` (a module may keep a class only in its build-type folders)
        Some(i) => parts.get(i + 1).is_some_and(|set| matches!(set.as_str(), "main" | "debug" | "commonmain" | "androidmain" | "jvmmain")),
        None => true,
    }
}

/// The names a file declares: `class|interface|object|fun|val|var|typealias Name` (an extension
/// function `fun Type.name` gives `name`).
fn declared(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let ident = |s: &str| -> String { s.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect() };
    for line in text.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        for (i, w) in words.iter().enumerate() {
            match *w {
                "class" | "interface" | "object" | "typealias" | "val" | "var" => {
                    if let Some(n) = words.get(i + 1) {
                        // `val Type.name` is an extension property: the name is after the last '.'
                        let n = ident(n.rsplit('.').next().unwrap_or(""));
                        if !n.is_empty() {
                            out.insert(n);
                        }
                    }
                }
                "fun" => {
                    // `fun <T> Array<out T>.name(` : the identifier right before the '('
                    let rest = words[i + 1..].join(" ");
                    let head = rest.split('(').next().unwrap_or("");
                    let n = ident(head.rsplit(['.', ' ', '>']).next().unwrap_or("").trim());
                    if !n.is_empty() {
                        out.insert(n);
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn words(text: &str) -> BTreeSet<&str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| w.starts_with(|c: char| c.is_alphabetic())).collect()
}

/// `package a.b.c` of a source text.
pub fn package_of(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("package "))
        .map(|p| p.trim_end_matches(';').trim().to_string())
        .unwrap_or_default()
}

fn imports_of(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix("import "))
        // Java's `import static a.b.C.x;` points into a.b like any other import
        .map(|i| i.trim_start_matches("static ").trim_end_matches(';').split(" as ").next().unwrap_or("").trim().to_string())
        .collect()
}

impl Packages {
    /// Reads the package line of every main source file once (about 0.1 s for 11k files).
    pub fn read(files: &[PathBuf]) -> Packages {
        let mut pk = Packages::default();
        for f in files {
            pk.add(f);
        }
        pk
    }

    /// Follows a file list that changed: reads only the files added and drops the files removed
    /// (an editor's file list after a new screen or a moved folder), not all 11k files again.
    pub fn update(&mut self, added: &[&PathBuf], removed: &[&PathBuf]) {
        for f in removed {
            self.remove(f);
        }
        for f in added {
            self.add(f);
        }
    }

    fn add(&mut self, f: &Path) {
        if is_main_source(f) {
            if self.sources.contains_key(f) {
                return;
            }
            self.reads += 1;
            if let Ok(text) = std::fs::read_to_string(f) {
                let pkg = package_of(&text);
                self.by_package.entry(pkg.clone()).or_default().push(f.to_path_buf());
                self.names.insert(f.to_path_buf(), declared(&text));
                let ann = if text.contains("annotation class ") { crate::preview_spec::annotation_classes(&text) } else { Vec::new() };
                self.annotation_classes.extend(ann.iter().cloned());
                self.sources.insert(f.to_path_buf(), (pkg, ann));
            }
        } else if f.file_name().is_some_and(|n| n == "build.gradle.kts" || n == "build.gradle")
            && let Some(dir) = f.parent()
        {
            self.reads += 1;
            // the namespace from the build file, else the manifest's package (older modules)
            let ns = std::fs::read_to_string(f).ok().and_then(|t| namespace_of(&t)).or_else(|| {
                ["src/main/AndroidManifest.xml", "AndroidManifest.xml"].iter().find_map(|m| {
                    let x = std::fs::read_to_string(dir.join(m)).ok()?;
                    x.split("package=\"").nth(1)?.split('"').next().map(String::from)
                })
            });
            let res = res_dirs(dir);
            if let Some(ns) = ns
                && !res.is_empty()
            {
                self.res_by_namespace.insert(ns.clone(), res);
                self.builds.insert(f.to_path_buf(), ns);
            }
        }
    }

    fn remove(&mut self, f: &Path) {
        if let Some((pkg, ann)) = self.sources.remove(f) {
            if let Some(v) = self.by_package.get_mut(&pkg) {
                v.retain(|g| g != f);
                if v.is_empty() {
                    self.by_package.remove(&pkg);
                }
            }
            self.names.remove(f);
            if !ann.is_empty() {
                // another file may declare the same name: made again from the files left
                self.annotation_classes = self.sources.values().flat_map(|(_, a)| a.iter().cloned()).collect();
            }
        } else if let Some(ns) = self.builds.remove(f) {
            self.res_by_namespace.remove(&ns);
        }
    }

    /// The package an import points into: the longest prefix that is a project package.
    pub fn package_for(&self, import: &str) -> Option<&str> {
        let parts: Vec<&str> = import.trim_end_matches(".*").split('.').collect();
        (1..=parts.len())
            .rev()
            .map(|n| parts[..n].join("."))
            .find_map(|p| self.by_package.get_key_value(&p).map(|(k, _)| k.as_str()))
    }

    /// The project files `open` needs (not `open` itself), and the namespaces whose `R` it uses:
    /// for each import, the files of that package that declare the imported name (all of them for a
    /// `*` import); for names used without an import, the files of the same package that declare
    /// them. Followed through every file taken, at most `limit` files.
    pub fn closure(&self, open: &Path, open_text: &str, limit: usize) -> Result<(Vec<PathBuf>, Vec<String>), String> {
        let mut files: BTreeSet<PathBuf> = BTreeSet::new();
        let mut r: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<(PathBuf, Option<String>)> = VecDeque::new();
        queue.push_back((open.to_path_buf(), Some(open_text.to_string())));
        while let Some((f, text)) = queue.pop_front() {
            let Some(text) = text.or_else(|| std::fs::read_to_string(&f).ok()) else { continue };
            let used = words(&text);
            let own = package_of(&text);
            let mut wanted: Vec<(String, Option<String>)> = vec![]; // (package, name or all)
            for i in imports_of(&text) {
                if let Some(ns) = i.strip_suffix(".R").or_else(|| i.split_once(".R.").map(|(a, _)| a))
                    && self.res_by_namespace.contains_key(ns)
                {
                    r.insert(ns.to_string());
                    continue;
                }
                let Some(p) = self.package_for(&i) else { continue };
                let rest = i.trim_end_matches(".*").strip_prefix(p).unwrap_or("").trim_start_matches('.');
                let name = rest.split('.').next().filter(|n| !n.is_empty()).map(String::from);
                wanted.push((p.to_string(), name));
            }
            for ns in self.res_by_namespace.keys() {
                if text.contains(&format!("{ns}.R.")) || (*ns == own && used.contains("R")) {
                    r.insert(ns.clone());
                }
            }
            let mut take: Vec<&PathBuf> = Vec::new();
            for (p, name) in &wanted {
                let files = self.by_package.get(p).map(Vec::as_slice).unwrap_or_default();
                let declaring: Vec<&PathBuf> = files.iter().filter(|g| name.as_ref().is_none_or(|n| self.names[*g].contains(n))).collect();
                if !declaring.is_empty() {
                    take.extend(declaring);
                } else if let Some(n) = name {
                    // nobody declares it: Java calling a Kotlin file's facade class (`FooKt`, `@file:JvmName`)
                    let stem = n.strip_suffix("Kt").unwrap_or(n);
                    let by_file: Vec<&PathBuf> = files.iter().filter(|g| g.file_stem().is_some_and(|s| s == stem)).collect();
                    take.extend(if by_file.is_empty() { files.iter().collect() } else { by_file });
                }
            }
            for g in self.by_package.get(&own).into_iter().flatten() {
                if self.names[g].iter().any(|n| used.contains(n.as_str())) {
                    take.push(g);
                }
            }
            for g in take {
                if g != open && files.insert(g.clone()) {
                    if files.len() > limit {
                        return Err(format!(
                            "This preview needs more than {limit} of the project's files; not drawn."
                        ));
                    }
                    queue.push_back((g.clone(), None));
                }
            }
        }
        // project code that calls back into the open file (an Activity that shows the open screen):
        // the open file must be compiled with it, or that code does not compile
        let own = declared(open_text);
        if files.iter().any(|f| std::fs::read_to_string(f).is_ok_and(|t| own.iter().any(|n| words(&t).contains(n.as_str())))) {
            files.insert(open.to_path_buf());
        }
        Ok((files.into_iter().collect(), r.into_iter().collect()))
    }

    /// Every module's namespace and res folders.
    pub fn modules(&self) -> impl Iterator<Item = (&String, &Vec<PathBuf>)> {
        self.res_by_namespace.iter()
    }
}

/// A module's res folders: `src/main/res` and `src/debug/res`, and in a module with its own layout
/// every `res` folder under it (core-ui keeps `java/<part>/res`), not those of a module inside it
/// (a folder with its own build file), of tests, or of build output.
fn res_dirs(module: &Path) -> Vec<PathBuf> {
    fn walk(d: &Path, root: bool, depth: u32, out: &mut Vec<PathBuf>) {
        if depth > 7 || (!root && (d.join("build.gradle.kts").exists() || d.join("build.gradle").exists())) {
            return;
        }
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let n = e.file_name().to_string_lossy().to_lowercase();
            if n == "build" || n.starts_with('.') || n.ends_with("test") || n.ends_with("tests") || n == "release" || n == "staging" {
                continue;
            }
            if n == "res" {
                out.push(p);
            } else {
                walk(&p, false, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(module, true, 0, &mut out);
    out.sort();
    out
}

/// `namespace = "co.branch.feature.onboarding"` (or `android.namespace = …`) in a build file.
fn namespace_of(build: &str) -> Option<String> {
    build.lines().map(str::trim).find_map(|l| {
        let rest = l.strip_prefix("android.").unwrap_or(l).strip_prefix("namespace")?.trim_start();
        let rest = rest.strip_prefix('=').unwrap_or(rest).trim();
        Some(rest.trim_matches(|c| c == '"' || c == '\'').to_string()).filter(|s| s.contains('.'))
    })
}

/// `R.java` for `namespace` from a res folder: every resource name with a made-up id, enough to
/// compile code that uses `R.string.x`, `R.drawable.y`, `R.styleable.Z_a`.
/// Ids come from `next`, so they are unique across all the project's modules (the renderer maps each id
/// back to one resource).
pub fn r_java(namespace: &str, res_dirs: &[PathBuf], next: &mut u32) -> String {
    let mut types: BTreeMap<&'static str, BTreeSet<String>> = BTreeMap::new();
    let mut styleables: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut add = |t: &'static str, n: &str| {
        types.entry(t).or_default().insert(n.replace(['.', '-'], "_"));
    };
    let dirs: Vec<PathBuf> = res_dirs
        .iter()
        .flat_map(|res| std::fs::read_dir(res).map(|d| d.flatten().map(|e| e.path()).collect::<Vec<_>>()).unwrap_or_default())
        .collect();
    for d in dirs {
        let kind = d.file_name().map_or(String::new(), |n| n.to_string_lossy().split('-').next().unwrap_or("").to_string());
        let files = std::fs::read_dir(&d).map(|x| x.flatten().map(|e| e.path()).collect::<Vec<_>>()).unwrap_or_default();
        for f in files {
            let stem = f.file_name().map_or(String::new(), |n| n.to_string_lossy().split('.').next().unwrap_or("").to_string());
            let text = if f.extension().is_some_and(|e| e == "xml") { std::fs::read_to_string(&f).unwrap_or_default() } else { String::new() };
            for id in text.split("@+id/").skip(1) {
                let n: String = id.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                if !n.is_empty() {
                    add("id", &n);
                }
            }
            if kind == "values" {
                let mut current: Option<String> = None;
                for tag in text.split('<').skip(1) {
                    let name = tag.split("name=\"").nth(1).and_then(|s| s.split('"').next()).unwrap_or("");
                    let head: String = tag.chars().take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '/').collect();
                    match head.as_str() {
                        "string" => add("string", name),
                        "color" => add("color", name),
                        "dimen" => add("dimen", name),
                        "bool" => add("bool", name),
                        "integer" => add("integer", name),
                        "plurals" => add("plurals", name),
                        "string-array" | "integer-array" | "array" => add("array", name),
                        "style" => add("style", name),
                        "attr" if !name.is_empty() => {
                            if let Some(s) = &current {
                                styleables.entry(s.clone()).or_default().push(name.replace(':', "_"));
                            }
                            if !name.contains(':') {
                                add("attr", name);
                            }
                        }
                        "declare-styleable" => {
                            current = Some(name.to_string());
                            styleables.entry(name.to_string()).or_default();
                        }
                        "/declare-styleable" => current = None,
                        "item" if tag.contains("type=\"") => {
                            let t = tag.split("type=\"").nth(1).and_then(|s| s.split('"').next()).unwrap_or("");
                            if let Some(t) = ["id", "dimen", "string", "color", "bool", "integer", "drawable", "style"].iter().find(|k| **k == t) {
                                add(t, name);
                            }
                        }
                        _ => {}
                    }
                }
            } else if !stem.is_empty() {
                let t = match kind.as_str() {
                    "drawable" => "drawable", "mipmap" => "mipmap", "layout" => "layout", "raw" => "raw",
                    "font" => "font", "anim" => "anim", "animator" => "animator", "xml" => "xml",
                    "menu" => "menu", "color" => "color", "navigation" => "navigation", "transition" => "transition",
                    _ => continue,
                };
                add(t, &stem);
            }
        }
    }
    let mut out = format!("package {namespace};\n\npublic final class R {{\n");
    for (t, names) in &types {
        out.push_str(&format!("  public static final class {t} {{\n"));
        for n in names {
            *next += 1;
            out.push_str(&format!("    public static final int {n} = 0x{:08x};\n", *next));
        }
        out.push_str("  }\n");
    }
    if !styleables.is_empty() {
        out.push_str("  public static final class styleable {\n");
        for (s, attrs) in &styleables {
            let s = s.replace('.', "_");
            out.push_str(&format!("    public static final int[] {s} = new int[{}];\n", attrs.len()));
            for (i, a) in attrs.iter().enumerate() {
                out.push_str(&format!("    public static final int {s}_{a} = {i};\n"));
            }
        }
        out.push_str("  }\n");
    }
    out.push_str("}\n");
    out
}

/// `R.java` of a library from its `R.txt` (an aar ships one): `int string title 0x7f0b0001`,
/// `int[] styleable Neem { 0x1, 0x2 }`, `int styleable Neem_tint 0`. A library's R.txt has
/// placeholder ids (the app build gives the real ones), and Android rejects some of them (View.setTag
/// needs an app id), so every resource gets a fresh `0x7f…` id from `next`; styleable indexes stay.
pub fn r_txt_java(package: &str, r_txt: &str, next: &mut u32) -> String {
    let mut types: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for l in r_txt.lines() {
        let mut w = l.splitn(4, ' ');
        let (Some(kind), Some(ty), Some(name), Some(value)) = (w.next(), w.next(), w.next(), w.next()) else { continue };
        let field = match kind {
            "int" if ty == "styleable" => format!("    public static final int {name} = {value};"),
            "int" => {
                *next += 1;
                format!("    public static final int {name} = 0x{:08x};", *next)
            }
            "int[]" => format!("    public static final int[] {name} = new int[] {};", value.trim()),
            _ => continue,
        };
        types.entry(ty.to_string()).or_default().push(field);
    }
    let mut out = format!("package {package};\n\npublic final class R {{\n");
    for (t, fields) in types {
        out.push_str(&format!("  public static final class {t} {{\n{}\n  }}\n", fields.join("\n")));
    }
    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) -> PathBuf {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn imports_pull_whole_packages_and_r_namespaces_but_not_tests() {
        let d = std::env::temp_dir().join(format!("dl-psrc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let screen = write(&d, "feature/ui/src/main/kotlin/a/ui/Screen.kt",
            "package a.ui\n\nimport a.state.UiState\nimport a.ui.R\nimport androidx.compose.runtime.Composable\n\nfun Screen() { helper() }\n");
        let helper = write(&d, "feature/ui/src/main/kotlin/a/ui/Helper.kt", "package a.ui\n\nfun helper() {}\n");
        let state = write(&d, "feature/state/src/main/kotlin/a/state/UiState.kt",
            "package a.state\n\nimport a.model.Thing\n\ndata class UiState(val t: Thing)\n");
        let model = write(&d, "core/model/src/main/kotlin/a/model/Thing.kt", "package a.model\n\nclass Thing\n");
        let test = write(&d, "feature/state/src/test/kotlin/a/state/UiStateTest.kt", "package a.state\n\nclass UiStateTest\n");
        let other = write(&d, "core/other/src/main/kotlin/a/other/Other.kt", "package a.other\n\nclass Other\n");
        let unused = write(&d, "feature/state/src/main/kotlin/a/state/Di.kt", "package a.state\n\nimport dagger.Module\n\nobject StateModule\n");
        let javatest = write(&d, "core-ui/javatest/a/state/Odd.kt", "package a.state\n\nclass UiState2\n");
        let build = write(&d, "feature/ui/build.gradle.kts", "android.namespace = \"a.ui\"\n");
        write(&d, "feature/ui/src/main/res/values/strings.xml", "<resources>\n  <string name=\"title\">T</string>\n</resources>\n");
        let flavor = write(&d, "feature/state/src/staging/kotlin/a/state/UiState.kt", "package a.state\n\ndata class UiState(val t: Int)\n");
        let pk = Packages::read(&[screen.clone(), helper.clone(), state.clone(), model.clone(), test, other, build, unused, javatest, flavor]);
        let text = std::fs::read_to_string(&screen).unwrap();
        let (files, r) = pk.closure(&screen, &text, 100).unwrap();
        assert_eq!(files, [model, state, helper], "imported names, used same-package names, and theirs; no tests (javatest too), no flavor copy, no unused file of an imported package (sorted)");
        assert_eq!(r, ["a.ui"]);
        assert!(pk.closure(&screen, &text, 2).is_err(), "a limit stops a closure that grows too big");
        // a file the open one needs that calls back into it: the open file is compiled with the project code
        let host = write(&d, "feature/ui/src/main/kotlin/a/ui/Host.kt", "package a.ui\n\nfun host() { PrimerScreen() }\n");
        let primer = write(&d, "feature/ui/src/main/kotlin/a/ui/PrimerScreen.kt", "package a.ui\n\ninternal fun PrimerScreen() { host() }\n");
        let pk3 = Packages::read(&[host.clone(), primer.clone()]);
        let (f3, _) = pk3.closure(&primer, &std::fs::read_to_string(&primer).unwrap(), 10).unwrap();
        assert_eq!(f3, [host, primer.clone()], "the open file comes along when project code calls it");
        let (f4, _) = pk.closure(&screen, &text, 100).unwrap();
        assert!(!f4.contains(&screen), "not when nothing calls back");
        let fq = write(&d, "feature/ui/src/main/kotlin/a/ui/sub/Font.kt", "package a.ui.sub\n\nval f = a.ui.R.font.x\n");
        let pk2 = Packages::read(&[fq.clone(), d.join("feature/ui/build.gradle.kts")]);
        let (_, r2) = pk2.closure(&fq, &std::fs::read_to_string(&fq).unwrap(), 10).unwrap();
        assert_eq!(r2, ["a.ui"], "a fully qualified R counts too");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_module_with_its_own_layout_has_all_its_res_folders() {
        let d = std::env::temp_dir().join(format!("dl-resdirs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for f in ["core-ui/java/typography/res/values/a.xml", "core-ui/java/theme/res/values/b.xml", "core-ui/javatest/res/values/c.xml",
            "core-ui/build/res/values/d.xml", "core-ui/sub/build.gradle.kts", "core-ui/sub/src/main/res/values/e.xml"] {
            write(&d, f, "x");
        }
        let dirs = res_dirs(&d.join("core-ui"));
        assert_eq!(dirs, [d.join("core-ui/java/theme/res"), d.join("core-ui/java/typography/res")], "own ones only: no test, build or inner module");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_kotlin_facade_class_import_takes_its_file() {
        let d = std::env::temp_dir().join(format!("dl-facade-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let java = write(&d, "a/src/main/java/a/ui/Frag.java", "package a.ui;\n\nimport static a.ktx.FragmentExtKt.show;\n\nclass Frag {}\n");
        let ext = write(&d, "b/src/main/kotlin/a/ktx/FragmentExt.kt", "package a.ktx\n\nfun show() {}\n");
        let other = write(&d, "b/src/main/kotlin/a/ktx/Other.kt", "package a.ktx\n\nfun other() {}\n");
        let pk = Packages::read(&[java.clone(), ext.clone(), other]);
        let (f, _) = pk.closure(&java, &std::fs::read_to_string(&java).unwrap(), 10).unwrap();
        assert_eq!(f, [ext], "FragmentExtKt is FragmentExt.kt");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn java_static_imports_point_into_their_package() {
        assert_eq!(imports_of("import static co.b.views.Helper.show;\nimport a.B;\n"), ["co.b.views.Helper.show", "a.B"]);
    }

    #[test]
    fn declared_names_cover_extensions_and_generics() {
        let d = declared("internal fun Array<out LoggerIntegration>.breadcrumbLogger(x: Int) = 1\nfun <T> List<T>.second(): T = this[1]\nval String.shout get() = this\ndata class UiState(val a: Int)\nprivate object Keys\nsuspend fun load() {}\n");
        for n in ["breadcrumbLogger", "second", "shout", "UiState", "Keys", "load"] {
            assert!(d.contains(n), "{n} in {d:?}");
        }
    }

    #[test]
    fn library_r_comes_from_r_txt() {
        let mut next = 0x7f00_0000;
        let java = r_txt_java("co.branch.designsystem.font", "int font circular_std_book 0x0\nint id holder 0x0\nint[] styleable Neem { 0x7f010001, 0x7f010002 }\nint styleable Neem_tint 0\n", &mut next);
        assert!(java.contains("package co.branch.designsystem.font;"));
        assert!(java.contains("class font {\n    public static final int circular_std_book = 0x7f000001;"), "{java}");
        assert!(java.contains("public static final int holder = 0x7f000002;"), "placeholder ids become app ids: {java}");
        assert!(java.contains("public static final int[] Neem = new int[] { 0x7f010001, 0x7f010002 };"));
        assert!(java.contains("public static final int Neem_tint = 0;"));
    }

    #[test]
    fn r_java_lists_every_resource_name() {
        let d = std::env::temp_dir().join(format!("dl-rjava-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        write(&d, "values/strings.xml", "<resources>\n<string name=\"phone_title\">P</string>\n<plurals name=\"days\"><item quantity=\"one\">d</item></plurals>\n<style name=\"Theme.App\"/>\n<declare-styleable name=\"Neem\"><attr name=\"tint\" format=\"color\"/><attr name=\"android:text\"/></declare-styleable>\n<item name=\"anchor\" type=\"id\"/>\n</resources>\n");
        write(&d, "drawable/ic_phone.xml", "<vector/>");
        write(&d, "drawable-night/ic_phone.xml", "<vector/>");
        write(&d, "layout/row.xml", "<LinearLayout android:id=\"@+id/row_root\"/>");
        let mut next = 0x7f00_0000;
        let java = r_java("a.ui", std::slice::from_ref(&d), &mut next);
        for want in ["package a.ui;", "class string", "phone_title", "class plurals", "days", "class style", "Theme_App",
            "class drawable", "ic_phone", "class layout", "row", "row_root", "anchor", "int[] Neem = new int[2]",
            "Neem_tint = 0", "Neem_android_text = 1", "class attr", "tint"] {
            assert!(java.contains(want), "{want} missing in:\n{java}");
        }
        assert_eq!(java.matches("ic_phone").count(), 1, "one name per resource across qualifiers");
        let _ = std::fs::remove_dir_all(&d);
    }
}

