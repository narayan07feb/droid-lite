//! Library symbols from the Gradle cache: the project's dependencies (version catalog and build
//! scripts, plus their `.pom` dependencies) are read from `~/.gradle/caches/modules-2/files-2.1`.
//! Source jars give full declarations; class jars and aars give class names and Kotlin top-level
//! functions (`TextKt.Text`, marked when `@Composable`).

use crate::classfile::{self, ClassKind};
use crate::kotlin::Kind;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct LibSym {
    pub name: String,
    pub package: String,
    pub kind: Kind,
    pub composable: bool,
    /// The jar (or aar) it came from.
    pub archive: PathBuf,
    /// Its source file inside a `-sources.jar`, when one exists.
    pub entry: Option<String>,
    pub artifact: String,
}

impl LibSym {
    pub fn qualified(&self) -> String {
        if self.package.is_empty() {
            self.name.clone()
        } else {
            format!("{}.{}", self.package, self.name)
        }
    }
    /// The read-only location to open: `archive!/entry`.
    pub fn source_path(&self) -> Option<PathBuf> {
        self.entry
            .as_ref()
            .map(|e| PathBuf::from(format!("{}!/{e}", self.archive.display())))
    }
}

#[derive(Default)]
pub struct Libraries {
    pub syms: Vec<LibSym>,
    by_name: HashMap<String, Vec<usize>>,
    pub artifacts: usize,
}

impl Libraries {
    pub fn named(&self, name: &str) -> impl Iterator<Item = &LibSym> {
        self.by_name
            .get(name)
            .into_iter()
            .flatten()
            .map(|&i| &self.syms[i])
    }
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }
}

pub fn gradle_home() -> PathBuf {
    std::env::var_os("DROID_LITE_GRADLE_HOME")
        .or_else(|| std::env::var_os("GRADLE_USER_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".gradle")
        })
}

/// (group, artifact, version) declared by the project: catalog entries and `"g:a:v"` strings.
pub fn direct_deps(root: &Path, scripts: &[PathBuf]) -> Vec<(String, String, Option<String>)> {
    let mut out = Vec::new();
    if let Ok(toml) = std::fs::read_to_string(root.join("gradle/libs.versions.toml")) {
        let mut versions = HashMap::new();
        let mut section = "";
        for line in toml.lines() {
            let t = line.trim();
            if let Some(s) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                section = s.trim();
                continue;
            }
            let Some((key, value)) = t.split_once('=') else {
                continue;
            };
            let quoted = |k: &str| {
                value
                    .split(&format!("{k} = \""))
                    .nth(1)
                    .or_else(|| value.split(&format!("{k}=\"")).nth(1))
                    .and_then(|r| r.split('"').next())
                    .map(String::from)
            };
            match section {
                "versions" => {
                    versions.insert(
                        key.trim().to_string(),
                        value.trim().trim_matches('"').to_string(),
                    );
                }
                "libraries" => {
                    let version = quoted("version")
                        .or_else(|| quoted("version.ref").and_then(|r| versions.get(&r).cloned()));
                    if let Some(m) = quoted("module") {
                        if let Some((g, a)) = m.split_once(':') {
                            out.push((g.to_string(), a.to_string(), version));
                        }
                    } else if let (Some(g), Some(a)) = (quoted("group"), quoted("name")) {
                        out.push((g, a, version));
                    } else if let Some(s) = value
                        .trim()
                        .strip_prefix('"')
                        .and_then(|v| v.strip_suffix('"'))
                    {
                        let p: Vec<&str> = s.split(':').collect();
                        if p.len() >= 2 {
                            out.push((p[0].into(), p[1].into(), p.get(2).map(|v| v.to_string())));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    for s in scripts {
        let Ok(text) = std::fs::read_to_string(s) else {
            continue;
        };
        for lit in text.split('"').skip(1).step_by(2) {
            let p: Vec<&str> = lit.split(':').collect();
            if p.len() == 3
                && p.iter()
                    .all(|x| !x.is_empty() && !x.contains(' ') && !x.contains('$'))
                && p[0].contains('.')
            {
                out.push((p[0].into(), p[1].into(), Some(p[2].into())));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Version strings compared by their numbers (1.10 > 1.9); a release beats its pre-releases
/// (1.10.0 > 1.10.0-alpha01).
fn version_key(v: &str) -> (Vec<u64>, bool) {
    let (main, pre) = v.split_once('-').map_or((v, false), |(m, _)| (m, true));
    (
        main.split('.')
            .take(4)
            .map(|s| s.parse().unwrap_or(0))
            .collect(),
        !pre,
    )
}

struct Artifact {
    id: String,
    files: Vec<PathBuf>,
}

/// The files of one version of `group:name`: the wanted version when cached, else the newest.
fn version_files(
    cache: &Path,
    group: &str,
    name: &str,
    version: Option<&str>,
) -> Option<(String, Vec<PathBuf>)> {
    let dir = cache.join(group).join(name);
    let mut versions: Vec<String> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    versions.sort_by_key(|v| version_key(v));
    let chosen = version
        .filter(|v| versions.iter().any(|x| x == v))
        .map(String::from)
        .or_else(|| versions.last().cloned())?;
    let files = std::fs::read_dir(dir.join(&chosen))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .flat_map(|h| {
            std::fs::read_dir(h.path())
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|f| f.path())
        })
        .collect();
    Some((format!("{group}:{name}:{chosen}"), files))
}

fn has_code(files: &[PathBuf]) -> bool {
    files
        .iter()
        .any(|f| f.extension().is_some_and(|e| e == "jar" || e == "aar"))
}

/// `group:name` in the cache. A Kotlin Multiplatform root keeps its pom (for the dependencies)
/// and takes the code from its `-android` or `-jvm` variant.
fn find_artifact(cache: &Path, group: &str, name: &str, version: Option<&str>) -> Option<Artifact> {
    let root = version_files(cache, group, name, version);
    if let Some((id, files)) = &root
        && has_code(files)
    {
        return Some(Artifact {
            id: id.clone(),
            files: files.clone(),
        });
    }
    let variant = ["-android", "-jvm"].iter().find_map(|s| {
        version_files(cache, group, &format!("{name}{s}"), version).filter(|(_, f)| has_code(f))
    });
    match (root, variant) {
        (Some((_, mut files)), Some((id, code))) => {
            files.extend(code);
            Some(Artifact { id, files })
        }
        (None, Some((id, files))) | (Some((id, files)), None) => Some(Artifact { id, files }),
        (None, None) => None,
    }
}

/// `<dependency>` entries of a pom that ship with the library (compile / runtime, not optional).
fn pom_deps(pom: &str) -> Vec<(String, String, Option<String>)> {
    let tag = |block: &str, t: &str| {
        block
            .split(&format!("<{t}>"))
            .nth(1)
            .and_then(|r| r.split(&format!("</{t}>")).next())
            .map(|s| s.trim().to_string())
    };
    pom.split("<dependency>")
        .skip(1)
        .filter_map(|b| {
            let b = b.split("</dependency>").next()?;
            if matches!(tag(b, "scope").as_deref(), Some("test" | "provided"))
                || tag(b, "optional").as_deref() == Some("true")
            {
                return None;
            }
            let v = tag(b, "version").filter(|v| !v.contains("${") && !v.starts_with('['));
            Some((tag(b, "groupId")?, tag(b, "artifactId")?, v))
        })
        .collect()
}

/// Follows the dependencies breadth first and reads every artifact's symbols.
pub fn index(root: &Path, scripts: &[PathBuf]) -> Libraries {
    let cache = gradle_home().join("caches/modules-2/files-2.1");
    let mut queue: VecDeque<(String, String, Option<String>)> = direct_deps(root, scripts).into();
    let mut seen = HashSet::new();
    let mut libs = Libraries::default();
    while let Some((g, a, v)) = queue.pop_front() {
        if !seen.insert(format!("{g}:{a}")) || libs.artifacts >= 800 {
            continue;
        }
        let Some(art) = find_artifact(&cache, &g, &a, v.as_deref()) else {
            continue;
        };
        libs.artifacts += 1;
        for f in &art.files {
            if f.extension().is_some_and(|e| e == "pom")
                && let Ok(p) = std::fs::read_to_string(f)
            {
                queue.extend(pom_deps(&p));
            }
        }
        read_artifact(&mut libs, &art);
        if libs.syms.len() > 300_000 {
            break;
        }
    }
    // the same class can come from two artifacts (an old and a new name): keep one
    let mut seen = HashSet::new();
    libs.syms
        .retain(|s| seen.insert((s.qualified(), s.kind, s.composable)));
    for (i, s) in libs.syms.iter().enumerate() {
        libs.by_name.entry(s.name.clone()).or_default().push(i);
    }
    libs
}

fn read_artifact(libs: &mut Libraries, art: &Artifact) {
    let sources = art
        .files
        .iter()
        .find(|f| f.to_string_lossy().ends_with("-sources.jar"));
    if let Some(src) = sources {
        read_sources(libs, src, &art.id);
        return;
    }
    for f in &art.files {
        match f.extension().and_then(|e| e.to_str()) {
            Some("jar") => {
                if let Ok(file) = std::fs::File::open(f) {
                    read_classes(libs, file, f, &art.id);
                }
            }
            Some("aar") => {
                // an aar is a zip with the classes in classes.jar
                let inner = std::fs::File::open(f)
                    .ok()
                    .and_then(|file| zip::ZipArchive::new(file).ok())
                    .and_then(|mut z| {
                        let mut e = z.by_name("classes.jar").ok()?;
                        let mut buf = Vec::new();
                        e.read_to_end(&mut buf).ok()?;
                        Some(buf)
                    });
                if let Some(buf) = inner {
                    read_classes(libs, std::io::Cursor::new(buf), f, &art.id);
                }
            }
            _ => {}
        }
    }
}

fn read_classes<R: Read + std::io::Seek>(
    libs: &mut Libraries,
    r: R,
    archive: &Path,
    artifact: &str,
) {
    let Ok(mut z) = zip::ZipArchive::new(r) else {
        return;
    };
    for i in 0..z.len() {
        let Ok(mut e) = z.by_index(i) else { continue };
        let n = e.name().to_string();
        if !n.ends_with(".class")
            || n.contains('$')
            || n.starts_with("META-INF")
            || n.ends_with("module-info.class")
            || n.ends_with("package-info.class")
        {
            continue;
        }
        let mut buf = Vec::with_capacity(e.size() as usize);
        if e.read_to_end(&mut buf).is_err() {
            continue;
        }
        let Some(c) = classfile::parse(&buf) else {
            continue;
        };
        if !c.public {
            continue;
        }
        let (package, simple) = c
            .name
            .rsplit_once('/')
            .map_or((String::new(), c.name.clone()), |(p, s)| {
                (p.replace('/', "."), s.to_string())
            });
        if simple.ends_with("Kt") {
            // a Kotlin file facade: its static methods are the file's top-level functions
            // Kotlin mangles functions with inline-class parameters: `Text-Nvy7gAk` is `Text`
            let mut seen = HashSet::new();
            for (m, composable) in c.statics {
                let m = m.split('-').next().unwrap_or(&m).to_string();
                if m.is_empty() || !seen.insert((m.clone(), composable)) {
                    continue;
                }
                libs.syms.push(LibSym {
                    name: m,
                    package: package.clone(),
                    kind: Kind::Fun,
                    composable,
                    archive: archive.to_path_buf(),
                    entry: None,
                    artifact: artifact.into(),
                });
            }
            continue;
        }
        let kind = match c.kind {
            ClassKind::Interface => Kind::Interface,
            ClassKind::Enum => Kind::Enum,
            _ => Kind::Class,
        };
        libs.syms.push(LibSym {
            name: simple,
            package,
            kind,
            composable: false,
            archive: archive.to_path_buf(),
            entry: None,
            artifact: artifact.into(),
        });
    }
}

fn read_sources(libs: &mut Libraries, jar: &Path, artifact: &str) {
    let Ok(file) = std::fs::File::open(jar) else {
        return;
    };
    let Ok(mut z) = zip::ZipArchive::new(file) else {
        return;
    };
    for i in 0..z.len() {
        let Ok(mut e) = z.by_index(i) else { continue };
        let n = e.name().to_string();
        if !(n.ends_with(".kt") || n.ends_with(".java")) {
            continue;
        }
        let mut text = String::new();
        if e.read_to_string(&mut text).is_err() {
            continue;
        }
        let syms = crate::kotlin::scan(&text);
        for d in syms.decls.iter().filter(|d| {
            d.container.is_none() && !d.has_modifier("private") && !d.has_modifier("internal")
        }) {
            libs.syms.push(LibSym {
                name: d.name.clone(),
                package: syms.package.clone(),
                kind: d.kind,
                composable: d.is_composable(),
                archive: jar.to_path_buf(),
                entry: Some(n.clone()),
                artifact: artifact.into(),
            });
        }
    }
}

/// Reads `archive!/entry` (a file inside a source jar) for a read-only editor tab.
pub fn read_entry(path: &Path) -> Option<String> {
    let s = path.to_string_lossy();
    let (archive, entry) = s.split_once("!/")?;
    let mut z = zip::ZipArchive::new(std::fs::File::open(archive).ok()?).ok()?;
    let mut e = z.by_name(entry).ok()?;
    let mut text = String::new();
    e.read_to_string(&mut text).ok()?;
    Some(text)
}

pub fn is_archive_path(path: &Path) -> bool {
    path.to_string_lossy().contains("!/")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::classfile::tests::{COMPOSABLE_SRC, TEXT_SRC, javac};
    use std::io::Write;

    fn jar(path: &Path, entries: &[(String, Vec<u8>)]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut z = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        for (n, b) in entries {
            z.start_file(n.as_str(), zip::write::SimpleFileOptions::default())
                .unwrap();
            z.write_all(b).unwrap();
        }
        z.finish().unwrap();
    }

    /// A fake Gradle home with: demo:ui (an aar, KMP root -> ui-android) that depends on demo:core
    /// (a jar with sources).
    pub fn fake_cache(home: &Path) -> bool {
        let tmp = home.join("javac");
        let Some(classes) = javac(
            &tmp,
            &[
                COMPOSABLE_SRC,
                TEXT_SRC,
                (
                    "demo/ui/Modifier.java",
                    "package demo.ui; public interface Modifier {}",
                ),
            ],
        ) else {
            return false;
        };
        let c = home.join("caches/modules-2/files-2.1");
        let ui = c.join("demo.lib/ui/1.0/aaa");
        std::fs::create_dir_all(&ui).unwrap();
        std::fs::write(ui.join("ui-1.0.pom"), "<project><dependencies><dependency><groupId>demo.lib</groupId><artifactId>core</artifactId><version>2.0</version><scope>runtime</scope></dependency><dependency><groupId>junit</groupId><artifactId>junit</artifactId><scope>test</scope></dependency></dependencies></project>").unwrap();
        // the classes live in the -android variant, as an aar
        let mut inner = Vec::new();
        {
            let mut z = zip::ZipWriter::new(std::io::Cursor::new(&mut inner));
            for (n, b) in classes.iter().filter(|(n, _)| n.starts_with("demo/")) {
                z.start_file(n.as_str(), zip::write::SimpleFileOptions::default())
                    .unwrap();
                z.write_all(b).unwrap();
            }
            z.finish().unwrap();
        }
        jar(
            &c.join("demo.lib/ui-android/1.0/bbb/ui-android-1.0.aar"),
            &[
                ("classes.jar".into(), inner),
                ("AndroidManifest.xml".into(), b"<manifest/>".to_vec()),
            ],
        );
        jar(&c.join("demo.lib/core/2.0/ccc/core-2.0-sources.jar"), &[("demo/core/Repo.kt".into(), b"package demo.core\ninterface Repo { fun load(): Int }\n@Composable\nfun CoreButton() {}\ninternal fun hidden() {}\n".to_vec())]);
        jar(&c.join("demo.lib/core/2.0/ccc/core-2.0.jar"), &[]);
        jar(&c.join("demo.lib/core/1.0/ddd/core-1.0.jar"), &[]);
        true
    }

    #[test]
    fn indexes_dependencies_from_the_gradle_cache() {
        let home = std::env::temp_dir().join(format!("droid-lite-gradle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        if !fake_cache(&home) {
            eprintln!("no javac: skipped");
            return;
        }
        let root = home.join("project");
        std::fs::create_dir_all(root.join("gradle")).unwrap();
        std::fs::write(root.join("gradle/libs.versions.toml"), "[versions]\nui = \"1.0\"\n[libraries]\ndemo-ui = { module = \"demo.lib:ui\", version.ref = \"ui\" }\n").unwrap();
        // SAFETY: only this test reads DROID_LITE_GRADLE_HOME, and it runs under the e2e lock.
        let _guard = crate::app::test_lock();
        unsafe { std::env::set_var("DROID_LITE_GRADLE_HOME", &home) };
        let libs = index(&root, &[]);
        unsafe { std::env::remove_var("DROID_LITE_GRADLE_HOME") };
        assert_eq!(
            libs.artifacts, 2,
            "ui (through ui-android) and core from its pom; junit is test scope"
        );
        let text = libs.named("Text").next().expect("Text from TextKt");
        assert!(text.composable && text.kind == Kind::Fun && text.qualified() == "demo.ui.Text");
        assert_eq!(libs.named("Modifier").next().unwrap().kind, Kind::Interface);
        assert!(libs.named("helper").next().is_some_and(|h| !h.composable));
        let repo = libs.named("Repo").next().unwrap();
        assert!(
            repo.artifact.ends_with(":2.0"),
            "the pom's version, not 1.0: {}",
            repo.artifact
        );
        assert!(
            read_entry(&repo.source_path().unwrap())
                .unwrap()
                .contains("interface Repo")
        );
        assert!(libs.named("CoreButton").next().unwrap().composable);
        assert!(libs.named("hidden").next().is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn versions_and_poms() {
        let mut v = vec!["1.9.0", "1.10.0", "1.10.0-alpha01", "1.2"];
        v.sort_by_key(|x| version_key(x));
        assert_eq!(v, vec!["1.2", "1.9.0", "1.10.0-alpha01", "1.10.0"]);
        let deps = pom_deps(
            "<dependency><groupId>a</groupId><artifactId>b</artifactId><version>${x}</version></dependency><dependency><groupId>c</groupId><artifactId>d</artifactId><optional>true</optional></dependency>",
        );
        assert_eq!(deps, vec![("a".to_string(), "b".to_string(), None)]);
    }
}
