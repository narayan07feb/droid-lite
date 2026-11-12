//! Library symbols from the Gradle cache: the project's dependencies (version catalog and build
//! scripts, plus their `.pom` dependencies) are read from `~/.gradle/caches/modules-2/files-2.1`.
//! Source jars give full declarations; class jars and aars give class names and Kotlin top-level
//! functions (`TextKt.Text`, marked when `@Composable`).

use crate::classfile::{self, ClassKind};
use crate::kotlin::Kind;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct LibSym {
    pub name: Arc<str>,
    pub package: Arc<str>,
    pub kind: Kind,
    pub composable: bool,
    /// The jar (or aar) it came from.
    pub archive: Arc<Path>,
    /// Its source file inside a `-sources.jar`, when one exists.
    pub entry: Option<Arc<str>>,
    pub artifact: Arc<str>,
}

impl LibSym {
    pub fn qualified(&self) -> String {
        if self.package.is_empty() {
            self.name.to_string()
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
    by_name: HashMap<Arc<str>, Vec<u32>>,
    pub artifacts: usize,
}

impl Libraries {
    pub fn named(&self, name: &str) -> impl Iterator<Item = &LibSym> {
        self.by_name
            .get(name)
            .into_iter()
            .flatten()
            .map(|&i| &self.syms[i as usize])
    }
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(|k| &**k)
    }
}

/// One symbol as read from an artifact (and as stored in the disk cache).
#[derive(Debug, PartialEq)]
struct Row {
    kind: Kind,
    composable: bool,
    /// Index into the artifact's files: the archive it came from.
    file: usize,
    package: String,
    name: String,
    entry: Option<String>,
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
    let arts = resolve(root, scripts);
    let rows = read_all(&arts, cache_dir().as_deref());
    // strings repeat a lot (packages, names, archives): keep one copy of each
    let mut pool: HashSet<Arc<str>> = HashSet::new();
    let mut atom = |s: &str| -> Arc<str> {
        if let Some(a) = pool.get(s) {
            return a.clone();
        }
        let a: Arc<str> = s.into();
        pool.insert(a.clone());
        a
    };
    let mut libs = Libraries {
        artifacts: arts.len(),
        ..Default::default()
    };
    let mut seen = HashSet::new();
    for (art, rows) in arts.iter().zip(rows) {
        if libs.syms.len() > 300_000 {
            break;
        }
        let artifact = atom(&art.id);
        let archives: Vec<Arc<Path>> = art.files.iter().map(|f| f.as_path().into()).collect();
        for r in rows {
            let (package, name) = (atom(&r.package), atom(&r.name));
            // the same class can come from two artifacts (an old and a new name): keep one
            if !seen.insert((package.clone(), name.clone(), r.kind, r.composable)) {
                continue;
            }
            libs.by_name
                .entry(name.clone())
                .or_default()
                .push(libs.syms.len() as u32);
            libs.syms.push(LibSym {
                name,
                package,
                kind: r.kind,
                composable: r.composable,
                archive: archives[r.file].clone(),
                entry: r.entry.as_deref().map(&mut atom),
                artifact: artifact.clone(),
            });
        }
    }
    libs
}

/// The project's dependencies and theirs (from the `.pom` files), at most 800.
fn resolve(root: &Path, scripts: &[PathBuf]) -> Vec<Artifact> {
    let cache = gradle_home().join("caches/modules-2/files-2.1");
    let mut queue: VecDeque<(String, String, Option<String>)> = direct_deps(root, scripts).into();
    let mut seen = HashSet::new();
    let mut arts = Vec::new();
    while let Some((g, a, v)) = queue.pop_front() {
        if arts.len() >= 800 || !seen.insert(format!("{g}:{a}")) {
            continue;
        }
        let Some(art) = find_artifact(&cache, &g, &a, v.as_deref()) else {
            continue;
        };
        for f in &art.files {
            if f.extension().is_some_and(|e| e == "pom")
                && let Ok(p) = std::fs::read_to_string(f)
            {
                queue.extend(pom_deps(&p));
            }
        }
        arts.push(art);
    }
    arts
}

/// Reads every artifact on all cores, each from the disk cache when it has one.
fn read_all(arts: &[Artifact], cache: Option<&Path>) -> Vec<Vec<Row>> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(8);
    let mut out: Vec<(usize, Vec<Row>)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(art) = arts.get(i) else { break };
                        done.push((i, cached_rows(art, cache)));
                    }
                    done
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    out.sort_by_key(|(i, _)| *i);
    out.into_iter().map(|(_, rows)| rows).collect()
}

/// Gradle never changes a downloaded artifact, so its symbols are read once and kept on disk.
/// `DROID_LITE_CACHE_DIR`, else `~/Library/Caches/droid-lite`.
fn cache_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("DROID_LITE_CACHE_DIR") {
        return Some(PathBuf::from(d).join("libs"));
    }
    if cfg!(test) {
        return None;
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    Some(home.join("Library/Caches/droid-lite/libs"))
}

const KINDS: [Kind; 7] = [
    Kind::Class,
    Kind::Interface,
    Kind::Object,
    Kind::Enum,
    Kind::Fun,
    Kind::Property,
    Kind::TypeAlias,
];
const CACHE_VERSION: &str = "droid-lite libs 1";

/// What the cache entry must match: the files with their size and change time.
fn cache_key(art: &Artifact) -> String {
    let mut key = format!("{CACHE_VERSION}\t{}", art.id);
    for f in &art.files {
        let m = f.metadata().ok();
        let len = m.as_ref().map_or(0, |m| m.len());
        let mtime = m
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        key += &format!("\t{}|{len}|{mtime}", f.display());
    }
    key
}

fn cached_rows(art: &Artifact, cache: Option<&Path>) -> Vec<Row> {
    let Some(dir) = cache else {
        return read_artifact(art);
    };
    let key = cache_key(art);
    let file = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut h);
        dir.join(format!("{:016x}.tsv", h.finish()))
    };
    if let Some(rows) = std::fs::read_to_string(&file)
        .ok()
        .and_then(|t| parse_cache(&t, &key, art.files.len()))
    {
        return rows;
    }
    let rows = read_artifact(art);
    if let Some(text) = write_cache(&rows, &key) {
        // write then rename, so a reader never sees half a file
        let tmp = file.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::create_dir_all(dir).is_ok() && std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &file);
        }
    }
    rows
}

/// `key` on the first line, then one `kind composable file package name entry` row per line.
/// None when a field has a tab or a line break (that artifact is then not cached).
fn write_cache(rows: &[Row], key: &str) -> Option<String> {
    let mut out = format!("{key}\n");
    for r in rows {
        let entry = r.entry.as_deref().unwrap_or("");
        if [r.package.as_str(), &r.name, entry]
            .iter()
            .any(|f| f.contains(['\t', '\n', '\r']))
        {
            return None;
        }
        let kind = KINDS.iter().position(|k| *k == r.kind)?;
        out += &format!(
            "{kind}\t{}\t{}\t{}\t{}\t{entry}\n",
            u8::from(r.composable),
            r.file,
            r.package,
            r.name
        );
    }
    Some(out)
}

/// The rows of a cache file, or None when it is for other files or damaged.
fn parse_cache(text: &str, key: &str, files: usize) -> Option<Vec<Row>> {
    let mut lines = text.split('\n');
    if lines.next()? != key || !text.ends_with('\n') {
        return None;
    }
    lines
        .filter(|l| !l.is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            let [kind, comp, file, package, name, entry] = f[..] else {
                return None;
            };
            let file: usize = file.parse().ok()?;
            Some(Row {
                kind: *KINDS.get(kind.parse::<usize>().ok()?)?,
                composable: comp == "1",
                file: (file < files).then_some(file)?,
                package: package.into(),
                name: name.into(),
                entry: (!entry.is_empty()).then(|| entry.into()),
            })
        })
        .collect()
}

fn read_artifact(art: &Artifact) -> Vec<Row> {
    let mut rows = Vec::new();
    let sources = art
        .files
        .iter()
        .position(|f| f.to_string_lossy().ends_with("-sources.jar"));
    if let Some(src) = sources {
        read_sources(&mut rows, &art.files[src], src);
        return rows;
    }
    for (i, f) in art.files.iter().enumerate() {
        match f.extension().and_then(|e| e.to_str()) {
            Some("jar") => {
                if let Ok(file) = std::fs::File::open(f) {
                    read_classes(&mut rows, file, i);
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
                    read_classes(&mut rows, std::io::Cursor::new(buf), i);
                }
            }
            _ => {}
        }
    }
    rows
}

fn read_classes<R: Read + std::io::Seek>(rows: &mut Vec<Row>, r: R, file: usize) {
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
                rows.push(Row {
                    kind: Kind::Fun,
                    composable,
                    file,
                    package: package.clone(),
                    name: m,
                    entry: None,
                });
            }
            continue;
        }
        let kind = match c.kind {
            ClassKind::Interface => Kind::Interface,
            ClassKind::Enum => Kind::Enum,
            _ => Kind::Class,
        };
        rows.push(Row {
            kind,
            composable: false,
            file,
            package,
            name: simple,
            entry: None,
        });
    }
}

fn read_sources(rows: &mut Vec<Row>, jar: &Path, file: usize) {
    let Ok(f) = std::fs::File::open(jar) else {
        return;
    };
    let Ok(mut z) = zip::ZipArchive::new(f) else {
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
            rows.push(Row {
                kind: d.kind,
                composable: d.is_composable(),
                file,
                package: syms.package.clone(),
                name: d.name.clone(),
                entry: Some(n.clone()),
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
    fn symbols_are_cached_on_disk_until_the_artifact_changes() {
        let home = std::env::temp_dir().join(format!("droid-lite-libcache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        if !fake_cache(&home) {
            eprintln!("no javac: skipped");
            return;
        }
        let root = home.join("project");
        std::fs::create_dir_all(root.join("gradle")).unwrap();
        std::fs::write(
            root.join("gradle/libs.versions.toml"),
            "[libraries]\ndemo-ui = { module = \"demo.lib:ui\", version = \"1.0\" }\n",
        )
        .unwrap();
        let cache = home.join("cache");
        let _guard = crate::app::test_lock();
        // SAFETY: only these tests read the variables, and they run under the e2e lock.
        unsafe {
            std::env::set_var("DROID_LITE_GRADLE_HOME", &home);
            std::env::set_var("DROID_LITE_CACHE_DIR", &cache);
        }
        let names = |l: &Libraries| {
            let mut v: Vec<String> = l.syms.iter().map(|s| s.qualified()).collect();
            v.sort();
            v
        };
        let first = index(&root, &[]);
        let files: Vec<PathBuf> = std::fs::read_dir(cache.join("libs"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 2, "one cache file per artifact: {files:?}");
        // a cached artifact is not read again: change its cache file and the change shows
        let core = files
            .iter()
            .find(|f| std::fs::read_to_string(f).unwrap().contains("CoreButton"))
            .unwrap();
        let text = std::fs::read_to_string(core).unwrap();
        std::fs::write(core, text.replace("CoreButton", "CachedButton")).unwrap();
        let second = index(&root, &[]);
        assert!(
            second.named("CachedButton").next().is_some(),
            "read from the cache"
        );
        assert_eq!(second.syms.len(), first.syms.len());
        let src = second.named("CachedButton").next().unwrap();
        assert!(
            src.composable
                && src
                    .source_path()
                    .unwrap()
                    .to_string_lossy()
                    .contains("core-2.0-sources.jar!/demo/core/Repo.kt")
        );
        // a damaged cache file is read again from the jar
        std::fs::write(core, &text[..text.len() / 2]).unwrap();
        let third = index(&root, &[]);
        assert_eq!(names(&third), names(&first));
        // a changed jar (other size) does not use the old cache
        std::fs::write(core, text.replace("CoreButton", "CachedButton")).unwrap();
        let jar =
            home.join("caches/modules-2/files-2.1/demo.lib/core/2.0/ccc/core-2.0-sources.jar");
        let mut bytes = std::fs::read(&jar).unwrap();
        bytes.extend_from_slice(&[0; 7]);
        std::fs::write(&jar, bytes).unwrap();
        let fourth = index(&root, &[]);
        assert!(fourth.named("CachedButton").next().is_none());
        assert!(fourth.named("CoreButton").next().is_some());
        unsafe {
            std::env::remove_var("DROID_LITE_GRADLE_HOME");
            std::env::remove_var("DROID_LITE_CACHE_DIR");
        }
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cache_rows_round_trip() {
        let rows = vec![
            Row {
                kind: Kind::Fun,
                composable: true,
                file: 1,
                package: "a.b".into(),
                name: "Text".into(),
                entry: None,
            },
            Row {
                kind: Kind::TypeAlias,
                composable: false,
                file: 0,
                package: String::new(),
                name: "T".into(),
                entry: Some("a/T.kt".into()),
            },
        ];
        let text = write_cache(&rows, "key").unwrap();
        assert_eq!(parse_cache(&text, "key", 2), Some(rows));
        assert_eq!(
            parse_cache(&text, "other", 2),
            None,
            "another artifact's file"
        );
        assert_eq!(
            parse_cache(&text, "key", 1),
            None,
            "file index out of range"
        );
        assert_eq!(
            parse_cache("key\n4\t0\t0\ta\tb\t", "key", 1),
            None,
            "cut short"
        );
        let tab = Row {
            kind: Kind::Fun,
            composable: false,
            file: 0,
            package: "a".into(),
            name: "x\ty".into(),
            entry: None,
        };
        assert_eq!(write_cache(&[tab], "key"), None);
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
