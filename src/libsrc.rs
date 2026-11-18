//! Sources of library and Android SDK declarations, for Go to declaration on a library call like
//! Android Studio: where a class's source is (the SDK's `sources/android-<compileSdk>`, a
//! library's `-sources.jar` in the Gradle cache or Droid Lite's own cache), how to download it
//! when it is missing (`sdkmanager "sources;android-N"`, the `-sources.jar` from the project's
//! Maven repositories), and the exact declaration in it: the member whose parameter types take
//! the call's arguments. Downloads run off the UI thread ([`fetch`] blocks; the app calls it on a
//! worker).

use crate::resolve::LibCall;
use std::path::{Path, PathBuf};

/// Packages whose classes come with the Android SDK (`android.jar`), not from Gradle.
pub fn is_platform(qualified: &str) -> bool {
    [
        "android.",
        "java.",
        "javax.",
        "dalvik.",
        "org.json.",
        "org.w3c.",
        "org.xml.",
    ]
    .iter()
    .any(|p| qualified.starts_with(p))
}

/// The Android SDK folder: `DROID_LITE_ANDROID_SDK` when set, else `ANDROID_HOME`,
/// `ANDROID_SDK_ROOT` or `~/Library/Android/sdk`, the first that exists.
pub fn sdk_dir() -> Option<PathBuf> {
    // set (tests): that folder or none, never another
    if let Some(d) = std::env::var_os("DROID_LITE_ANDROID_SDK") {
        return Some(PathBuf::from(d)).filter(|p| p.is_dir());
    }
    ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .chain(std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Android/sdk")))
        .find(|p| p.is_dir())
}

/// The project's compile SDK: the highest number given to `compileSdk` (any case, also a version
/// catalog key like `branchCompileSdk = "36"`, or `compileSdk { version = release(36) }`) in its
/// build scripts and catalog, comments left out; else the newest platform the SDK has.
pub fn compile_sdk(root: &Path, scripts: &[PathBuf], sdk: &Path) -> Option<u32> {
    let catalog = root.join("gradle/libs.versions.toml");
    let found = scripts
        .iter()
        .chain(std::iter::once(&catalog))
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .filter_map(|t| compile_sdk_in(&t))
        .max();
    found.or_else(|| {
        std::fs::read_dir(sdk.join("platforms"))
            .ok()?
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.strip_prefix("android-")?.parse::<u32>().ok()
            })
            .max()
    })
}

/// The highest compile SDK a build script or catalog gives (see [`compile_sdk`]).
fn compile_sdk_in(text: &str) -> Option<u32> {
    let lines: Vec<String> = text
        .lines()
        .map(|l| l.trim().to_ascii_lowercase())
        .map(|l| {
            if l.starts_with('#') || l.starts_with("//") {
                String::new()
            } else {
                l
            }
        })
        .collect();
    let digits = |s: &str| -> Option<u32> {
        s.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .ok()
    };
    let mut best = None;
    for (i, l) in lines.iter().enumerate() {
        let Some(at) = l.find("compilesdk") else {
            continue;
        };
        let rest = &l[at + "compilesdk".len()..];
        // `compileSdkExtension = 12`, `compileSdkMinor`, `compileSdkPreview`: not the SDK
        let word: String = rest.chars().take_while(|c| c.is_alphabetic()).collect();
        if !word.is_empty() && word != "version" {
            continue;
        }
        let after = rest[word.len()..].trim_start_matches(|c: char| " =(\"'".contains(c));
        let n = digits(after).or_else(|| {
            // `compileSdk { version = release(36) }`, on this line or the next few
            lines[i..lines.len().min(i + 4)].iter().find_map(|x| {
                let r = x.find("release(")?;
                digits(&x[r + "release(".len()..])
            })
        });
        best = best.max(n);
    }
    best
}

/// The source file of `qualified` in the SDK's sources for `api`, when it is there. A nested
/// class lives in its outer class's file.
pub fn sdk_source(sdk: &Path, api: u32, qualified: &str) -> Option<PathBuf> {
    let root = sdk.join(format!("sources/android-{api}"));
    let mut parts: Vec<&str> = qualified.split('.').collect();
    while parts.len() > 1 {
        let p = root.join(parts.join("/") + ".java");
        if p.is_file() {
            return Some(p);
        }
        if !parts
            .last()
            .is_some_and(|s| s.starts_with(char::is_uppercase))
        {
            break;
        }
        parts.pop();
    }
    None
}

/// A file of the SDK's sources or of Droid Lite's sources cache: opened read-only.
pub fn read_only_source(path: &Path) -> bool {
    path.starts_with(sources_cache())
        || sdk_dir().is_some_and(|s| path.starts_with(s.join("sources")))
}

/// Where downloaded `-sources.jar` files are kept: `DROID_LITE_SOURCES_DIR`, else
/// `~/Library/Caches/droid-lite/sources`.
pub fn sources_cache() -> PathBuf {
    std::env::var_os("DROID_LITE_SOURCES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join("Library/Caches/droid-lite/sources")
        })
}

/// `group:name:version`, each part a plain Maven name (letters, digits, `.`, `_`, `-`; not `.`
/// or `..`): parts that could leave the cache folder or the repository's path are refused.
fn coords(c: &str) -> Option<(&str, &str, &str)> {
    let mut it = c.split(':');
    let (g, n, v) = (it.next()?, it.next()?, it.next()?);
    let plain = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && !s.contains("..")
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    };
    (it.next().is_none() && plain(g) && plain(n) && plain(v)).then_some((g, n, v))
}

/// The cached `-sources.jar` of `group:name:version`.
pub fn cached_sources_jar(c: &str) -> Option<PathBuf> {
    let (g, n, v) = coords(c)?;
    Some(
        sources_cache()
            .join(g)
            .join(n)
            .join(v)
            .join(format!("{n}-{v}-sources.jar")),
    )
}

/// The Maven repositories to download sources from: `DROID_LITE_MAVEN_REPOS` (comma separated),
/// else the project's `google()`, `mavenCentral()` and literal `maven("…")` / `url = uri("…")`
/// repositories (a URL held in a variable, usually with a password, is left out).
pub fn repos(root: &Path) -> Vec<String> {
    if let Ok(v) = std::env::var("DROID_LITE_MAVEN_REPOS") {
        return v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    let mut out: Vec<String> = Vec::new();
    let add = |out: &mut Vec<String>, u: &str| {
        let u = u.trim_end_matches('/').to_string();
        if !out.contains(&u) {
            out.push(u);
        }
    };
    for f in [
        "settings.gradle.kts",
        "settings.gradle",
        "build.gradle.kts",
        "build.gradle",
    ] {
        let Ok(t) = std::fs::read_to_string(root.join(f)) else {
            continue;
        };
        // only inside `repositories { … }` (a URL elsewhere is an API base or a doc link)
        let mut depth = 0i32;
        let mut inside: Option<i32> = None;
        for line in t.lines().map(str::trim).filter(|l| !l.starts_with("//")) {
            let opening = inside.is_none() && line.starts_with("repositories");
            if opening {
                inside = Some(depth);
            }
            depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
            let here = inside.is_some();
            // closed on this line (a one-line block too); `repositories` alone waits for its `{`
            if inside.is_some_and(|d| depth <= d) && (!opening || line.contains('{')) {
                inside = None;
            }
            if !here {
                continue;
            }
            if line.contains("google()") {
                add(&mut out, "https://dl.google.com/dl/android/maven2");
            }
            if line.contains("mavenCentral()") {
                add(&mut out, "https://repo.maven.apache.org/maven2");
            }
            for key in ["maven(\"", "uri(\"", "url \"", "url '", "url = \""] {
                if let Some(i) = line.find(key) {
                    let rest = &line[i + key.len()..];
                    if let Some(end) = rest.find(['"', '\'']) {
                        let u = &rest[..end];
                        // https only (a plain-http repository is not trusted for code), and no
                        // URL with a password in it (it would show in the command line)
                        // no credentials, tokens or fragments (the URL shows in `ps`)
                        if u.starts_with("https://") && !u.contains(['@', '?', '#']) {
                            add(&mut out, u);
                        }
                    }
                }
            }
        }
    }
    if out.is_empty() {
        add(&mut out, "https://dl.google.com/dl/android/maven2");
        add(&mut out, "https://repo.maven.apache.org/maven2");
    }
    // at most a few, each tried in turn for up to 2 minutes
    out.truncate(MAX_REPOS);
    out
}

/// How many repositories a sources jar is looked for in.
const MAX_REPOS: usize = 5;

/// A download that brings a missing source.
#[derive(Clone, Debug, PartialEq)]
pub enum Fetch {
    /// `sdkmanager "sources;android-<api>"` into the SDK.
    Sdk { sdk: PathBuf, api: u32 },
    /// A library's `-sources.jar` from one of these repositories into [`sources_cache`].
    Maven { coords: String, repos: Vec<String> },
}

impl Fetch {
    /// Names one download (the same one is never run twice at once).
    pub fn key(&self) -> String {
        match self {
            Fetch::Sdk { api, .. } => format!("sdk-sources-{api}"),
            Fetch::Maven { coords, .. } => coords.clone(),
        }
    }

    /// For the status line: "Android 36 sources", "androidx.core:core:1.13.1 sources".
    pub fn label(&self) -> String {
        match self {
            Fetch::Sdk { api, .. } => format!("Android {api} SDK sources"),
            Fetch::Maven { coords, .. } => format!("{coords} sources"),
        }
    }
}

/// The download's result is on disk already (the SDK's sources folder, the cached jar): running it
/// again would bring nothing new.
pub fn already_there(f: &Fetch) -> bool {
    match f {
        // a sources folder with the android packages (an empty or cut-off one is fetched again)
        Fetch::Sdk { sdk, api } => sdk.join(format!("sources/android-{api}/android")).is_dir(),
        Fetch::Maven { coords, .. } => cached_sources_jar(coords).is_some_and(|j| j.is_file()),
    }
}

/// Runs a download (blocking: call it on a worker thread).
pub fn fetch(f: &Fetch) -> Result<(), String> {
    match f {
        Fetch::Sdk { sdk, api } => {
            let tool = std::env::var_os("DROID_LITE_SDKMANAGER")
                .map(PathBuf::from)
                .unwrap_or_else(|| sdkmanager_in(sdk));
            if !tool.exists() {
                return Err(format!(
                    "sdkmanager not found ({}): install the Android SDK Command-line Tools",
                    tool.display()
                ));
            }
            // its output goes to a log (stdout and stderr: the license text is on stdout)
            let log = sources_cache().join(format!("sdkmanager-{api}.log"));
            std::fs::create_dir_all(sources_cache()).map_err(|e| e.to_string())?;
            let out = std::fs::File::create(&log).map_err(|e| e.to_string())?;
            let err = out.try_clone().map_err(|e| e.to_string())?;
            // its own process group: on a timeout the java it starts stops too
            use std::os::unix::process::CommandExt;
            let mut child = std::process::Command::new(&tool)
                .process_group(0)
                .arg(format!("--sdk_root={}", sdk.display()))
                .arg(format!("sources;android-{api}"))
                .stdin(std::process::Stdio::null())
                .stdout(out)
                .stderr(err)
                .spawn()
                .map_err(|e| format!("sdkmanager did not start: {e}"))?;
            wait_at_most(&mut child, SDK_TIMEOUT, "sdkmanager")?;
            if sdk.join(format!("sources/android-{api}")).is_dir() {
                Ok(())
            } else {
                // the last lines say why (a licence not accepted shows its question there)
                let text = std::fs::read_to_string(&log).unwrap_or_default();
                let mut last: Vec<&str> = text
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .rev()
                    .take(3)
                    .collect();
                last.reverse();
                let why = if last.is_empty() {
                    "no sources were installed".to_string()
                } else {
                    last.join(" / ")
                };
                Err(format!("sdkmanager: {why}"))
            }
        }
        Fetch::Maven { coords: c, repos } => {
            let dest = cached_sources_jar(c).ok_or("not a plain group:name:version")?;
            let (g, n, v) = coords(c).ok_or("not a plain group:name:version")?;
            let rel = format!("{}/{n}/{v}/{n}-{v}-sources.jar", g.replace('.', "/"));
            std::fs::create_dir_all(dest.parent().unwrap_or(Path::new(".")))
                .map_err(|e| e.to_string())?;
            // this process's part file: another Droid Lite downloading the same jar has its own
            let part = dest.with_extension(format!("jar.{}.part", std::process::id()));
            // only web repositories, unless the tests name a local one
            let local = std::env::var_os("DROID_LITE_MAVEN_REPOS").is_some();
            let mut tried = 0;
            for r in repos {
                let file = r.starts_with("file://");
                if file && !local || !file && !r.starts_with("https://") {
                    continue;
                }
                tried += 1;
                let mut child = std::process::Command::new("curl")
                    // -g: the URL is taken as written (no curl globbing)
                    .args(["-fsSLg", "--max-time", "120", "--max-filesize"])
                    .arg(MAX_DOWNLOAD.to_string())
                    .args(["--proto", if file { "=file" } else { "=https" }])
                    .args(["--proto-redir", if file { "=file" } else { "=https" }])
                    .arg("-o")
                    .arg(&part)
                    .arg(format!("{r}/{rel}"))
                    .stdin(std::process::Stdio::null())
                    .spawn()
                    .map_err(|e| format!("curl did not start: {e}"))?;
                // curl cannot cap a download whose size the server does not send: stopped here
                let ok = loop {
                    match child.try_wait() {
                        Ok(Some(s)) => break s.success(),
                        Ok(None) => {}
                        Err(_) => break false,
                    }
                    if std::fs::metadata(&part).is_ok_and(|m| m.len() > MAX_DOWNLOAD) {
                        let _ = child.kill();
                        let _ = child.wait();
                        break false;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                };
                // a whole zip (a cut-off download has no central directory)
                let zip = whole_zip(&part, MAX_DOWNLOAD);
                if ok && zip && std::fs::rename(&part, &dest).is_ok() {
                    return Ok(());
                }
            }
            let _ = std::fs::remove_file(&part);
            Err(format!("no sources jar for {c} in {tried} repositories"))
        }
    }
}

/// The SDK's sdkmanager: `cmdline-tools/latest`, else any installed `cmdline-tools/<version>`,
/// else the old `tools/bin`.
fn sdkmanager_in(sdk: &Path) -> PathBuf {
    let latest = sdk.join("cmdline-tools/latest/bin/sdkmanager");
    if latest.exists() {
        return latest;
    }
    let mut versions: Vec<PathBuf> = std::fs::read_dir(sdk.join("cmdline-tools"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("bin/sdkmanager"))
        .filter(|p| p.exists())
        .collect();
    versions.sort();
    versions
        .pop()
        .unwrap_or_else(|| sdk.join("tools/bin/sdkmanager"))
}

/// How long sdkmanager may run.
const SDK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Waits for `child`, stopping it when it runs longer than `limit`.
fn wait_at_most(
    child: &mut std::process::Child,
    limit: std::time::Duration,
    what: &str,
) -> Result<(), String> {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if start.elapsed() < limit => {
                std::thread::sleep(std::time::Duration::from_millis(200))
            }
            Ok(None) => {
                // the whole group (the script and the java it started)
                unsafe { libc::killpg(child.id() as i32, libc::SIGKILL) };
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{what} took over {} minutes", limit.as_secs() / 60));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// The entry of `qualified`'s source in a sources jar: `a/b/C.kt` or `a/b/C.java` (also under a
/// source set folder such as `commonMain/`), else a file of that package that declares the class.
pub fn jar_entry(jar: &Path, qualified: &str) -> Option<String> {
    let f = std::fs::File::open(jar).ok()?;
    let mut z = zip::ZipArchive::new(f).ok()?;
    let (pkg, class) = qualified.rsplit_once('.').unwrap_or(("", qualified));
    let dir = if pkg.is_empty() {
        String::new()
    } else {
        format!("{}/", pkg.replace('.', "/"))
    };
    let names: Vec<String> = z.file_names().map(String::from).collect();
    // the file itself, or under one source set folder (`commonMain/a/b/C.kt`), not another package
    let at = |n: &str, want: &str| {
        n == want
            || n.strip_suffix(want)
                .and_then(|p| p.strip_suffix('/'))
                .is_some_and(|p| !p.is_empty() && !p.contains('/'))
    };
    for ext in ["kt", "java"] {
        let want = format!("{dir}{class}.{ext}");
        if let Some(n) = names.iter().find(|n| at(n, &want)) {
            return Some(n.clone());
        }
    }
    let in_dir = |n: &str| {
        let parent = n.rsplit_once('/').map_or("", |(d, _)| d);
        format!("{parent}/") == dir || parent.ends_with(&format!("/{}", dir.trim_end_matches('/')))
    };
    names
        .iter()
        .filter(|n| (n.ends_with(".kt") || n.ends_with(".java")) && in_dir(n))
        .find(|n| {
            let Ok(e) = z.by_name(n) else {
                return false;
            };
            if e.size() > MAX_SCAN {
                return false; // a huge file is not read on a hunch
            }
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::Read::take(e, MAX_SCAN), &mut s).is_ok()
                && declares(&s, class)
        })
        .cloned()
}

/// The entries of a sources jar in `package` (also under a source set folder) that declare a
/// top-level function `name` (`fun name(` or `fun Receiver.name(`): where a library function
/// known only by its package and name lives.
pub fn fun_entries(jar: &Path, package: &str, name: &str) -> Vec<String> {
    let Some(mut z) = std::fs::File::open(jar)
        .ok()
        .and_then(|f| zip::ZipArchive::new(f).ok())
    else {
        return Vec::new();
    };
    let dir = package.replace('.', "/");
    let in_dir = |n: &str| {
        let parent = n.rsplit_once('/').map_or("", |(d, _)| d);
        parent == dir
            || parent
                .strip_suffix(dir.as_str())
                .and_then(|p| p.strip_suffix('/'))
                .is_some_and(|p| !p.is_empty() && !p.contains('/'))
    };
    let names: Vec<String> = z
        .file_names()
        .filter(|n| n.ends_with(".kt") && in_dir(n))
        .map(String::from)
        // a package's files only, and not without end (each is read up to MAX_SCAN)
        .take(MAX_PACKAGE_FILES)
        .collect();
    names
        .into_iter()
        .filter(|n| {
            let Ok(e) = z.by_name(n) else {
                return false;
            };
            if e.size() > MAX_SCAN {
                return false;
            }
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::Read::take(e, MAX_SCAN), &mut s).is_ok()
                && s.lines().any(|l| {
                    l.contains("fun ")
                        && (l.contains(&format!(".{name}(")) || l.contains(&format!(" {name}(")))
                })
        })
        .collect()
}

/// Files bigger than this are not read to look for a class.
const MAX_SCAN: u64 = 2 * 1024 * 1024;

/// `text` declares `class Name`, `interface Name` or `object Name` (the whole word).
pub fn declares(text: &str, class: &str) -> bool {
    class_offset(text, class).is_some()
}

/// Where `class Name` / `interface Name` / `object Name` / `enum Name` is declared (the name): the
/// keyword starts a word and its line is not a comment.
pub fn class_offset(text: &str, class: &str) -> Option<usize> {
    let code = |i: usize| {
        let line = text[..i].rfind('\n').map_or(0, |n| n + 1);
        let head = text[line..i].trim_start();
        let before = text[..i].chars().next_back();
        before.is_none_or(|c| c.is_whitespace() || c == '(')
            && !head.starts_with('*')
            && !head.starts_with("//")
            && !head.starts_with("/*")
    };
    ["class ", "interface ", "object ", "enum "]
        .iter()
        .find_map(|k| {
            text.match_indices(&format!("{k}{class}"))
                .filter(|(i, _)| code(*i))
                .map(|(i, _)| i + k.len())
                .find(|&e| {
                    !text[e + class.len()..].starts_with(|c: char| c.is_alphanumeric() || c == '_')
                })
        })
}

/// The supertypes `class` names in its source, each as the qualified names it may have, surest
/// first: the file's import of it; else its own package, `java.lang`, the `*` imports and the
/// name as written. `class A extends B implements C` (Java), `class A(…) : B(), C` (Kotlin).
pub fn supertypes(text: &str, class: &str) -> Vec<Vec<String>> {
    let Some(at) = class_offset(text, class) else {
        return Vec::new();
    };
    let head = &text[at + class.len()..];
    let mut end = head.find('{').unwrap_or(head.len()).min(600);
    while !head.is_char_boundary(end) {
        end -= 1; // never inside a multibyte character
    }
    let head = &head[..end];
    // Kotlin: after the `:` that follows the constructor; Java: after extends / implements
    let list: String = if let Some(i) = top_level_colon(head) {
        head[i + 1..].to_string()
    } else {
        // `extends` may start or end a line
        let flat = format!(
            " {} ",
            head.split_whitespace().collect::<Vec<_>>().join(" ")
        );
        flat.replace(" extends ", " , ")
            .replace(" implements ", " , ")
    };
    let package = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("package "))
        .map(|p| p.trim_end_matches(';').trim().to_string())
        .unwrap_or_default();
    let imports: Vec<&str> = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("import "))
        .map(|l| l.trim_end_matches(';').trim())
        .collect();
    let mut out: Vec<Vec<String>> = Vec::new();
    for part in split_top(&list) {
        let name: String = part
            .trim()
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
            .collect();
        if name.is_empty()
            || !name
                .rsplit('.')
                .next()
                .is_some_and(|s| s.starts_with(char::is_uppercase))
        {
            continue;
        }
        let first = name.split('.').next().unwrap_or(&name);
        let imported = imports
            .iter()
            .find(|l| l.rsplit('.').next() == Some(first))
            .map(|i| format!("{i}{}", &name[first.len()..]));
        let qs: Vec<String> = match imported {
            Some(q) => vec![q],
            None if name.contains('.') && name.starts_with(char::is_lowercase) => {
                vec![name.clone()]
            }
            None => {
                let mut v = Vec::new();
                if !package.is_empty() {
                    v.push(format!("{package}.{name}"));
                }
                v.push(format!("java.lang.{name}"));
                v.extend(
                    imports
                        .iter()
                        .filter_map(|l| l.strip_suffix(".*"))
                        .map(|p| format!("{p}.{name}")),
                );
                v.push(name.clone());
                v
            }
        };
        if !out.contains(&qs) {
            out.push(qs);
        }
    }
    out
}

/// The `:` of a Kotlin supertype list (outside brackets).
fn top_level_colon(head: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut prev = ' ';
    for (i, c) in head.char_indices() {
        match c {
            '(' | '<' => depth += 1,
            ')' => depth -= 1,
            '>' if prev != '-' => depth -= 1, // not the `->` of a function type
            ':' if depth == 0 => return Some(i),
            _ => {}
        }
        prev = c;
    }
    None
}

/// Splits at commas outside brackets.
fn split_top(s: &str) -> Vec<&str> {
    let (mut out, mut depth, mut from) = (Vec::new(), 0i32, 0);
    let mut prev = ' ';
    for (i, c) in s.char_indices() {
        match c {
            '(' | '<' => depth += 1,
            ')' => depth -= 1,
            '>' if prev != '-' => depth -= 1, // not the `->` of a function type
            ',' if depth == 0 => {
                out.push(&s[from..i]);
                from = i + 1;
            }
            _ => {}
        }
        prev = c;
    }
    out.push(&s[from..]);
    out
}

/// The class file of `qualified` in a jar: the package as folders, nested classes joined by `$`
/// (`android/app/Notification$Builder.class`).
pub fn class_entry(qualified: &str) -> String {
    let parts: Vec<&str> = qualified.split('.').collect();
    let k = parts
        .iter()
        .position(|s| s.starts_with(char::is_uppercase))
        .unwrap_or(parts.len().saturating_sub(1));
    format!("{}/{}.class", parts[..k].join("/"), parts[k..].join("$"))
        .trim_start_matches('/')
        .to_string()
}

/// The declarations a call means in a library source, best first, whether they are a sure match
/// (`exact`: the parameter types were weighed, or the name says it for a property) and how well
/// the arguments fit them (`score`, to weigh them against a supertype's overloads).
#[derive(Debug, Default, PartialEq)]
pub struct Members {
    pub offsets: Vec<usize>,
    pub exact: bool,
    pub score: usize,
}

/// The declarations of `call.member` in `class` of a library source (`text` of `path`): those
/// whose parameters take the arguments' types (exact); else those that take as many arguments;
/// else every one of that name. `class` may be a nested path (`Outer.Builder`): only the members
/// inside that class's body count. A Kotlin property read as a Java getter (`manager.allProviders`
/// for `getAllProviders()`) finds the getter.
pub fn find_member(path: &Path, text: &str, class: &str, call: &LibCall) -> Members {
    let syms = crate::kotlin::scan_file(path, text);
    let simple = class.rsplit('.').next().unwrap_or(class);
    // the class's body: for `Outer.Builder` the Builder inside Outer, not another file's or class's
    let outer = class.rsplit('.').nth(1);
    let body = syms
        .decls
        .iter()
        .filter(|d| d.kind.is_type() && d.name == simple)
        .find(|d| match outer {
            Some(o) => d
                .container
                .as_deref()
                .is_some_and(|c| c.rsplit('.').next() == Some(o)),
            None => true,
        })
        .and_then(|d| d.body.clone().or_else(|| brace_body(text, d.offset)));
    let in_class = |d: &&crate::kotlin::Decl| {
        d.container
            .as_deref()
            .is_some_and(|c| c.rsplit('.').next() == Some(simple))
            && body.as_ref().is_none_or(|b| b.contains(&d.offset))
    };
    let named = |name: &str| -> Vec<&crate::kotlin::Decl> {
        syms.decls
            .iter()
            .filter(|d| d.name == name && !d.kind.is_type())
            .filter(in_class)
            .collect()
    };
    let mut ds = named(&call.member);
    if ds.is_empty() && call.args.is_none() {
        let cap: String = call
            .member
            .chars()
            .take(1)
            .flat_map(char::to_uppercase)
            .chain(call.member.chars().skip(1))
            .collect();
        ds = named(&format!("get{cap}"));
        if ds.is_empty() {
            ds = named(&format!("is{cap}"));
        }
    }
    let Some(args) = &call.args else {
        // a property (or its getter): the name says it
        let exact = !ds.is_empty();
        return Members {
            offsets: ds.iter().map(|d| d.offset).collect(),
            exact,
            score: 0,
        };
    };
    let class_params = class_offset(text, simple)
        .map(|at| type_params(&text[at + simple.len()..]))
        .unwrap_or_default();
    rank(&ds, args, call.java, |d| {
        let mut v = class_params.clone();
        v.extend(method_type_params(text, d.offset));
        v
    })
}

/// The most Kotlin files of one package read to find a function's file.
const MAX_PACKAGE_FILES: usize = 500;

/// The largest sources jar downloaded (200 MB).
const MAX_DOWNLOAD: u64 = 200 << 20;

/// `p` is a whole zip (a cut-off download has no central directory) of at most `max` bytes.
fn whole_zip(p: &Path, max: u64) -> bool {
    std::fs::File::open(p).is_ok_and(|f| {
        f.metadata().is_ok_and(|m| m.len() <= max) && zip::ZipArchive::new(f).is_ok()
    })
}

/// The `{ … }` that follows `from` (a class's body when the scanner kept none): its range, by
/// counting braces outside strings, chars and comments.
fn brace_body(text: &str, from: usize) -> Option<std::ops::Range<usize>> {
    let start = from + text[from..].find('{')?;
    let b = text.as_bytes();
    let (mut depth, mut i) = (0usize, start);
    while i < b.len() {
        match b[i] {
            b'"' | b'\'' => {
                // a string or char: to its closing quote
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start..i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The overloads `ds` that `args` fit best (see [`Members`]); `tparams(d)`: the type parameters
/// declared for `d` (they take any argument).
fn rank(
    ds: &[&crate::kotlin::Decl],
    args: &[Vec<String>],
    java: bool,
    tparams: impl Fn(&crate::kotlin::Decl) -> Vec<String>,
) -> Members {
    let count = |d: &&&crate::kotlin::Decl| {
        let ps = &d.params;
        let required = ps.iter().filter(|p| !p.has_default && !p.vararg).count();
        (args.len() <= ps.len() || ps.iter().any(|p| p.vararg)) && args.len() >= required
    };
    let by_count: Vec<&&crate::kotlin::Decl> = ds.iter().filter(count).collect();
    // like the compiler, the most specific overload: an exact parameter type beats one that takes
    // anything (`Object`, a type parameter); several equally specific ones are all returned
    let scored: Vec<(usize, usize)> = by_count
        .iter()
        .filter_map(|d| {
            let tp = tparams(d);
            let mut score = 0;
            for (a, p) in args.iter().zip(&d.params) {
                if a.is_empty() {
                    continue;
                }
                // a Java vararg `String...` reads as `String[]`: it takes single values
                let ty = if p.vararg {
                    p.ty.trim_end_matches("[]")
                } else {
                    p.ty.as_str()
                };
                score += match_score(ty, a, &tp, java)?;
            }
            Some((d.offset, score))
        })
        .collect();
    if let Some(best) = scored.iter().map(|(_, s)| *s).max() {
        return Members {
            offsets: scored
                .into_iter()
                .filter(|(_, s)| *s == best)
                .map(|(o, _)| o)
                .collect(),
            exact: true,
            score: best,
        };
    }
    let offsets = if by_count.is_empty() {
        ds.iter().map(|d| d.offset).collect()
    } else {
        by_count.iter().map(|d| d.offset).collect()
    };
    Members {
        offsets,
        exact: false,
        score: 0,
    }
}

/// The names of the type parameters a `<…>` at the start of `s` declares (`<K, V : Any>` gives K
/// and V; `<T extends Foo>` gives T).
fn type_params(s: &str) -> Vec<String> {
    let s = s.trim_start();
    let Some(rest) = s.strip_prefix('<') else {
        return Vec::new();
    };
    let mut depth = 1;
    let mut end = rest.len();
    for (i, c) in rest.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    break;
                }
            }
            _ => {}
        }
    }
    split_top(&rest[..end])
        .into_iter()
        .filter_map(|p| {
            let p = p
                .trim()
                .trim_start_matches("reified ")
                .trim_start_matches("in ")
                .trim_start_matches("out ");
            let name: String = p
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            (!name.is_empty()).then_some(name)
        })
        .collect()
}

/// The type parameters a method declares: the `<…>` before its name on its line
/// (`public <TResult> Task<TResult> continueWith(` gives TResult).
fn method_type_params(text: &str, offset: usize) -> Vec<String> {
    // the declaration's head, from the end of the statement or comment before it (it may span
    // lines): `public static <T> T get(` or `fun <T> List<T>.get(`
    let from = ["*/", ";", "{", "}"]
        .iter()
        .filter_map(|p| text[..offset].rfind(p).map(|i| i + p.len()))
        .max()
        .unwrap_or(0);
    let head = &text[from..offset];
    let Some(i) = head.find('<') else {
        return Vec::new();
    };
    // `<` right after a modifier or `fun` opens type parameters; after a type (`List<String>
    // foo(`) it is that type's argument
    let word = head[..i].split_whitespace().last().unwrap_or("");
    let opens = word.is_empty()
        || word.starts_with('@')
        || matches!(
            word,
            "public"
                | "private"
                | "protected"
                | "static"
                | "final"
                | "abstract"
                | "synchronized"
                | "native"
                | "default"
                | "strictfp"
                | "fun"
                | "inline"
                | "suspend"
                | "override"
                | "open"
                | "internal"
                | "operator"
                | "infix"
                | "tailrec"
                | "external"
        );
    if opens {
        type_params(&head[i..])
    } else {
        Vec::new()
    }
}

/// Top-level extension functions `fun Receiver.member(…)` in a library source whose receiver is
/// one of `receivers` (simple names; a type parameter or `Any` takes every receiver), ranked by
/// the arguments like members.
pub fn find_extension(path: &Path, text: &str, receivers: &[String], call: &LibCall) -> Members {
    let syms = crate::kotlin::scan_file(path, text);
    let ds: Vec<&crate::kotlin::Decl> = syms
        .decls
        .iter()
        .filter(|d| d.name == call.member && d.container.is_none() && !d.kind.is_type())
        .filter(|d| {
            receiver_of(text, d.offset).is_some_and(|r| {
                r == "Any"
                    || (r.len() == 1 && r.starts_with(char::is_uppercase))
                    || receivers.contains(&r)
            })
        })
        .collect();
    match &call.args {
        Some(args) => rank(&ds, args, call.java, |d| method_type_params(text, d.offset)),
        None => Members {
            exact: !ds.is_empty(),
            offsets: ds.iter().map(|d| d.offset).collect(),
            score: 0,
        },
    }
}

/// The receiver of a Kotlin extension declared with its name at `offset` (`fun <T> List<T>.name`
/// gives `List`), or None when it is not an extension.
pub fn receiver_of(text: &str, offset: usize) -> Option<String> {
    let before = text[..offset].trim_end();
    let before = before.strip_suffix('.')?;
    let line = &before[before.rfind('\n').map_or(0, |i| i + 1)..];
    let at = line
        .find("fun ")
        .or_else(|| line.find("val "))
        .or_else(|| line.find("var "))?;
    let mut recv = line[at + 4..].trim_start();
    if recv.starts_with('<') {
        // `fun <T> T.name`: past the type parameters
        let mut depth = 0;
        let end = recv.char_indices().find_map(|(i, c)| {
            match c {
                '<' => depth += 1,
                '>' => depth -= 1,
                _ => {}
            }
            (depth == 0).then_some(i + 1)
        })?;
        recv = recv[end..].trim_start();
    }
    let ty = simple_type(recv);
    (!ty.is_empty()).then_some(ty)
}

/// Supertypes of Kotlin's built-in types, for their extensions (`String` takes a `CharSequence`
/// extension, `MutableList` an `Iterable` one).
pub fn kotlin_supers(t: &str) -> &'static [&'static str] {
    match t {
        "String" => &["String", "CharSequence", "Comparable"],
        "StringBuilder" => &["StringBuilder", "CharSequence", "Appendable"],
        "MutableList" | "ArrayList" => &[
            "MutableList",
            "List",
            "MutableCollection",
            "Collection",
            "MutableIterable",
            "Iterable",
        ],
        "List" => &["List", "Collection", "Iterable"],
        "MutableSet" | "HashSet" | "LinkedHashSet" => &[
            "MutableSet",
            "Set",
            "MutableCollection",
            "Collection",
            "MutableIterable",
            "Iterable",
        ],
        "Set" => &["Set", "Collection", "Iterable"],
        "MutableMap" | "HashMap" | "LinkedHashMap" => &["MutableMap", "Map"],
        "Collection" => &["Collection", "Iterable"],
        "MutableCollection" => &[
            "MutableCollection",
            "Collection",
            "MutableIterable",
            "Iterable",
        ],
        "Int" | "Long" | "Short" | "Byte" | "Double" | "Float" => &["Number", "Comparable"],
        "IntRange" | "LongRange" | "CharRange" => &["IntProgression", "ClosedRange", "Iterable"],
        "Sequence" => &["Sequence"],
        "Pair" | "Triple" => &["Serializable"],
        _ => &[],
    }
}

/// Two arguments' types (each its type and supertypes) can be the same parameter: one is the other
/// or among its supertypes, both are whole numbers (a literal `0` and `0L` call one `long`), or one
/// is a library type whose supertypes are not known here (an `Activity` and a `Service` may both
/// be a `Context`): which declaration each call means is checked after, by looking it up.
pub fn same_type(a: &[String], b: &[String]) -> bool {
    let whole = |x: &[String]| {
        x.first()
            .is_some_and(|t| matches!(t.as_str(), "Int" | "Long" | "Short" | "Byte"))
    };
    let unknown_supers = |x: &[String]| {
        x.len() == 1 && x[0].starts_with(char::is_uppercase) && java_primitive(&x[0]).is_none()
    };
    a.is_empty()
        || b.is_empty()
        || a.first().is_some_and(|t| b.contains(t))
        || b.first().is_some_and(|t| a.contains(t))
        || whole(a) && whole(b)
        || unknown_supers(a) && unknown_supers(b)
}

/// The class file `entry` from a jar, or from the `classes.jar` inside an `.aar`; None when it
/// is not there or is bigger than [`MAX_CLASS`].
pub fn class_bytes(archive: &Path, entry: &str) -> Option<Vec<u8>> {
    fn read<R: std::io::Read + std::io::Seek>(
        z: &mut zip::ZipArchive<R>,
        entry: &str,
    ) -> Option<Vec<u8>> {
        let e = z.by_name(entry).ok()?;
        if e.size() > MAX_CLASS {
            return None;
        }
        let mut v = Vec::new();
        std::io::Read::read_to_end(&mut std::io::Read::take(e, MAX_CLASS), &mut v).ok()?;
        Some(v)
    }
    if archive.extension().is_some_and(|e| e == "aar") {
        let jar = aar_classes(archive)?;
        let mut inner = zip::ZipArchive::new(std::io::Cursor::new(&jar[..])).ok()?;
        return read(&mut inner, entry);
    }
    let mut z = zip::ZipArchive::new(std::fs::File::open(archive).ok()?).ok()?;
    read(&mut z, entry)
}

/// An `.aar`'s `classes.jar`, read once and kept for the last few aars (a lookup asks the same
/// aar many times).
fn aar_classes(aar: &Path) -> Option<std::sync::Arc<Vec<u8>>> {
    // by path and modification time: a rebuilt aar is read again
    type Kept = (
        PathBuf,
        Option<std::time::SystemTime>,
        std::sync::Arc<Vec<u8>>,
    );
    static KEPT: std::sync::Mutex<Vec<Kept>> = std::sync::Mutex::new(Vec::new());
    let stamp = std::fs::metadata(aar).and_then(|m| m.modified()).ok();
    if let Some((_, _, j)) = KEPT
        .lock()
        .ok()?
        .iter()
        .find(|(p, s, _)| p == aar && *s == stamp)
    {
        return Some(j.clone());
    }
    let mut z = zip::ZipArchive::new(std::fs::File::open(aar).ok()?).ok()?;
    let e = z.by_name("classes.jar").ok()?;
    let mut v = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(e, MAX_AAR_JAR), &mut v).ok()?;
    let j = std::sync::Arc::new(v);
    let mut kept = KEPT.lock().ok()?;
    if kept.len() >= 3 {
        kept.remove(0);
    }
    kept.retain(|(p, _, _)| p != aar);
    kept.push((aar.to_path_buf(), stamp, j.clone()));
    Some(j)
}

/// The class file `entry` is in the jar (or the `.aar`'s `classes.jar`).
pub fn has_class(archive: &Path, entry: &str) -> bool {
    if archive.extension().is_some_and(|e| e == "aar") {
        return class_bytes(archive, entry).is_some();
    }
    std::fs::File::open(archive)
        .ok()
        .and_then(|f| zip::ZipArchive::new(f).ok())
        .is_some_and(|mut z| z.by_name(entry).is_ok())
}

/// A class file bigger than this is not read for a stub.
const MAX_CLASS: u64 = 4 * 1024 * 1024;
/// An `.aar`'s `classes.jar` bigger than this is not read.
const MAX_AAR_JAR: u64 = 64 * 1024 * 1024;

/// How well a parameter of type `param` (as spelled in Java or Kotlin source) takes an argument
/// whose type and supertypes are `arg` (simple names, Kotlin spelling, nearest first), like the
/// compiler ranks overloads: 100 its own type (a Kotlin `Int` is Java `int`), 90 less one per
/// step for a supertype, 50 a widening (a number literal), 40 a box (`Int` to `Integer`), 10 a
/// parameter that takes anything (`Object`, a type parameter, a function type for a lambda),
/// None when it does not take it.
fn match_score(param: &str, arg: &[String], tparams: &[String], java: bool) -> Option<usize> {
    if param.contains("->") {
        return Some(10); // a function type: what a lambda is, whose type is not read
    }
    let p = simple_type(param);
    let own = arg.first()?;
    if *own == p || java_primitive(own) == Some(p.as_str()) || array_of(own) == Some(p.as_str()) {
        return Some(100);
    }
    if p.ends_with("[]") && own == "Array" {
        return Some(90); // Array<T>: the element type is not read
    }
    if let Some(d) = arg.iter().skip(1).position(|a| *a == p) {
        return Some(89 - d.min(39));
    }
    // a Kotlin number is a Java Number and Comparable
    if kotlin_supers(own).contains(&p.as_str()) {
        return Some(80);
    }
    // Kotlin converts only a number literal, and only between whole numbers; the argument's type
    // does not say it was a literal, so any Int may (a variable that does not fit would not
    // compile, so it is not in the code)
    // a Kotlin type is its Java box (`Int` is an `Integer`): nearer than a widening
    let twin = match own.as_str() {
        "Int" => "Integer",
        "Char" => "Character",
        o => o,
    };
    if p == twin && java_primitive(own).is_some() {
        return Some(60);
    }
    let widen: &[&str] = match own.as_str() {
        "Int" => &["long", "short", "byte"],
        "String" => &["CharSequence"],
        _ => &[],
    };
    if widen.contains(&p.as_str()) {
        return Some(50);
    }
    // a Java caller widens further: int to float or double (after long, like javac), long to
    // float or double, float to double; kotlinc does not
    let java_widen: &[&str] = match own.as_str() {
        "Int" => &["float", "double"],
        "Long" => &["float", "double"],
        "Float" => &["double"],
        _ => &[],
    };
    if java && java_widen.contains(&p.as_str()) {
        return Some(45);
    }
    let boxed: &[&str] = match own.as_str() {
        "Int" => &["Long", "Short", "Byte"],
        _ => &[],
    };
    if boxed.contains(&p.as_str()) {
        return Some(40);
    }
    if p.is_empty()
        || p == "Object"
        || p == "Any"
        || tparams.contains(&p)
        || (p.len() == 1 && p.starts_with(char::is_uppercase))
    {
        return Some(10); // anything, or a type parameter
    }
    None
}

/// The Java primitive a Kotlin number, boolean or char type is.
fn java_primitive(k: &str) -> Option<&'static str> {
    Some(match k {
        "Int" => "int",
        "Long" => "long",
        "Short" => "short",
        "Byte" => "byte",
        "Float" => "float",
        "Double" => "double",
        "Boolean" => "boolean",
        "Char" => "char",
        _ => return None,
    })
}

/// The Java array a Kotlin primitive array is (`IntArray` is `int[]`).
fn array_of(k: &str) -> Option<&'static str> {
    Some(match k {
        "IntArray" => "int[]",
        "LongArray" => "long[]",
        "ByteArray" => "byte[]",
        "CharArray" => "char[]",
        "FloatArray" => "float[]",
        "DoubleArray" => "double[]",
        "BooleanArray" => "boolean[]",
        "ShortArray" => "short[]",
        _ => return None,
    })
}

/// `@NonNull final java.util.Map<String, Integer>` gives `Map`; `String?` gives `String`;
/// `String[]` stays `String[]`; a vararg `String...` gives `String` (it takes single values).
fn simple_type(ty: &str) -> String {
    let mut s = ty.trim();
    loop {
        if let Some(r) = s.strip_prefix('@') {
            // an annotation: its name and its (arguments)
            let end = r
                .find(|c: char| !(c.is_alphanumeric() || c == '.' || c == '_'))
                .unwrap_or(r.len());
            let mut rest = &r[end..];
            if rest.starts_with('(') {
                rest = rest.find(')').map_or("", |c| &rest[c + 1..]);
            }
            s = rest.trim_start();
        } else if let Some(r) = ["final ", "vararg "].iter().find_map(|k| s.strip_prefix(k)) {
            s = r.trim_start();
        } else {
            break;
        }
    }
    let s = s.trim_end_matches('?');
    let array = s.ends_with("[]");
    let base = s.split(['<', '[', ' ']).next().unwrap_or("");
    let base = base.trim_end_matches("...").trim_end_matches('?');
    let base = base.rsplit('.').next().unwrap_or(base);
    if array {
        format!("{base}[]")
    } else {
        base.to_string()
    }
}

/// A stub for a class without source, from its class file: its public and protected constructors
/// and methods as Java declarations, one per line, so Go to declaration has a line to show.
pub fn stub(class_bytes: &[u8], note: &str) -> Option<String> {
    let c = crate::classfile::parse(class_bytes)?;
    let name = c.name.replace('/', ".");
    let (pkg, simple) = name.rsplit_once('.').unwrap_or(("", &name));
    let simple = simple.rsplit('$').next().unwrap_or(simple);
    let mut out = format!("// {note}\n");
    if !pkg.is_empty() {
        out.push_str(&format!("package {pkg};\n\n"));
    }
    let kind = match c.kind {
        crate::classfile::ClassKind::Interface => "interface",
        crate::classfile::ClassKind::Enum => "enum",
        crate::classfile::ClassKind::Annotation => "@interface",
        crate::classfile::ClassKind::Class => "class",
    };
    out.push_str(&format!("public {kind} {simple} {{\n"));
    for (m, desc, access) in &c.methods {
        let vis = if access & 0x0001 != 0 {
            "public"
        } else {
            "protected"
        };
        let stat = if access & 0x0008 != 0 { "static " } else { "" };
        let sig = if m == "<init>" {
            java_signature(simple, desc)
                .split_once(' ')
                .map_or(String::new(), |(_, s)| s.to_string())
        } else {
            java_signature(m, desc)
        };
        out.push_str(&format!("    {vis} {stat}{sig};\n"));
    }
    out.push_str("}\n");
    Some(out)
}

/// `requestLocationUpdates` and `(Ljava/lang/String;JFLandroid/location/LocationListener;)V`
/// give `void requestLocationUpdates(String p0, long p1, float p2, LocationListener p3)`.
fn java_signature(name: &str, desc: &str) -> String {
    let mut params = Vec::new();
    let mut ret = String::from("void");
    let mut in_params = false;
    let mut chars = desc.chars();
    while let Some(mut c) = chars.next() {
        match c {
            '(' => in_params = true,
            ')' => in_params = false,
            _ => {
                let mut dims = 0;
                while c == '[' {
                    dims += 1;
                    c = chars.next().unwrap_or('V');
                }
                let base = match c {
                    'B' => "byte".to_string(),
                    'C' => "char".to_string(),
                    'D' => "double".to_string(),
                    'F' => "float".to_string(),
                    'I' => "int".to_string(),
                    'J' => "long".to_string(),
                    'S' => "short".to_string(),
                    'Z' => "boolean".to_string(),
                    'V' => "void".to_string(),
                    'L' => {
                        let s: String = chars.by_ref().take_while(|&x| x != ';').collect();
                        s.rsplit(['/', '$']).next().unwrap_or(&s).to_string()
                    }
                    _ => "?".to_string(),
                };
                let ty = format!("{base}{}", "[]".repeat(dims));
                if in_params {
                    params.push(ty);
                } else {
                    ret = ty;
                }
            }
        }
    }
    let params: Vec<String> = params
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{t} p{i}"))
        .collect();
    format!("{ret} {name}({})", params.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fm(path: &Path, text: &str, class: &str, call: &LibCall) -> Vec<usize> {
        find_member(path, text, class, call).offsets
    }

    fn call(member: &str, args: &[&[&str]]) -> LibCall {
        LibCall {
            class: "LocationManager".into(),
            qualified: Some("android.location.LocationManager".into()),
            stars: Vec::new(),
            member: member.into(),
            args: Some(
                args.iter()
                    .map(|a| a.iter().map(|s| s.to_string()).collect())
                    .collect(),
            ),
            builtin: false,
            java: false,
            fallback: false,
        }
    }

    const LM: &str = "package android.location;\n\npublic class LocationManager {\n    public void requestLocationUpdates(@NonNull String provider, long minTimeMs, float minDistanceM, @NonNull LocationListener listener) {}\n    public void requestLocationUpdates(@NonNull String provider, long minTimeMs, float minDistanceM, @NonNull PendingIntent pendingIntent) {}\n    public void requestLocationUpdates(@NonNull String provider, long minTimeMs, float minDistanceM, @NonNull LocationListener listener, @Nullable Looper looper) {}\n    public @NonNull List<String> getAllProviders() { return null; }\n}\n";

    #[test]
    fn the_overload_whose_parameter_types_take_the_arguments_is_the_one() {
        let p = Path::new("LocationManager.java");
        let listener = fm(
            p,
            LM,
            "LocationManager",
            &call(
                "requestLocationUpdates",
                &[&["String"], &["Int"], &["Float"], &["LocationListener"]],
            ),
        );
        assert_eq!(listener.len(), 1);
        assert!(LM[listener[0]..].starts_with("requestLocationUpdates(@NonNull String provider, long minTimeMs, float minDistanceM, @NonNull LocationListener listener)"));
        // the same count, another type: the PendingIntent one
        let pending = fm(
            p,
            LM,
            "LocationManager",
            &call(
                "requestLocationUpdates",
                &[
                    &["String"],
                    &["Int"],
                    &["Float"],
                    &["PendingIntent", "Parcelable"],
                ],
            ),
        );
        assert_eq!(pending.len(), 1);
        assert!(LM[pending[0]..].contains("PendingIntent pendingIntent) {}"));
        assert_ne!(listener, pending);
        // a project class that implements the listener is taken by its supertype
        let mine = fm(
            p,
            LM,
            "LocationManager",
            &call(
                "requestLocationUpdates",
                &[
                    &[],
                    &["Long"],
                    &["Float"],
                    &["MyListener", "LocationListener"],
                ],
            ),
        );
        assert_eq!(mine, listener);
        // the most specific: `long` beats `Object`
        let src = "class Locator {\n    public void locate(String name, Object tag) {}\n    public void locate(String name, long timeout) {}\n}\n";
        let mut l = call("locate", &[&["String"], &["Long"]]);
        l.class = "Locator".into();
        let best = fm(Path::new("Locator.java"), src, "Locator", &l);
        assert!(best.len() == 1 && src[best[0]..].starts_with("locate(String name, long timeout)"));
        // types not known: every 4-argument one
        assert_eq!(
            fm(
                p,
                LM,
                "LocationManager",
                &call("requestLocationUpdates", &[&[], &[], &[], &[]])
            )
            .len(),
            2
        );
        // a Kotlin property read is the Java getter
        let mut prop = call("allProviders", &[]);
        prop.args = None;
        let g = fm(p, LM, "LocationManager", &prop);
        assert!(g.len() == 1 && LM[g[0]..].starts_with("getAllProviders"));
    }

    fn accepts_(param: &str, arg: &[String]) -> bool {
        match_score(param, arg, &[], false).is_some()
    }

    #[test]
    fn overloads_rank_like_the_compiler() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // a Kotlin Int is Java int: append(int) beats append(long) and append(Integer)
        let sb = "class StringBuilder {\n    public StringBuilder append(long l) { return this; }\n    public StringBuilder append(int i) { return this; }\n    public StringBuilder append(Integer i) { return this; }\n    public StringBuilder append(Object o) { return this; }\n}\n";
        let mut c = call("append", &[&["Int"]]);
        c.class = "StringBuilder".into();
        let f = fm(Path::new("SB.java"), sb, "StringBuilder", &c);
        assert!(
            f.len() == 1 && sb[f[0]..].starts_with("append(int i)"),
            "{f:?}"
        );
        // the nearest supertype wins: Leaf goes to f(Mid), not f(Base)
        let h = "class H {\n    void f(Base b) {}\n    void f(Mid m) {}\n}\n";
        let mut c = call("f", &[&["Leaf", "Mid", "Base"]]);
        c.class = "H".into();
        let f = fm(Path::new("H.java"), h, "H", &c);
        assert!(f.len() == 1 && h[f[0]..].starts_with("f(Mid m)"));
        // String and String[] are told apart; IntArray is int[]; a lambda fits a function type
        assert!(match_score("String[]", &s(&["String"]), &[], false).is_none());
        assert_eq!(
            match_score("int[]", &s(&["IntArray"]), &[], false),
            Some(100)
        );
        assert!(match_score("(Int) -> Unit", &s(&[]), &[], false).is_some());
        // a real tie: two declarations that fit as well
        let t2 = "class T {\n    void g(Object a) {}\n    void g(Object b) {}\n}\n";
        let mut c = call("g", &[&["Thing"]]);
        c.class = "T".into();
        assert_eq!(fm(Path::new("T.java"), t2, "T", &c).len(), 2);
    }

    #[test]
    fn long_headers_with_non_ascii_text_and_whole_number_arguments() {
        // a class header over 600 bytes with no `{` and multibyte text: no panic
        let text = format!("class X : Base(\"{}\")", "é".repeat(400));
        let _ = supertypes(&text, "X");
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(
            same_type(&s(&["Int"]), &s(&["Long"])),
            "0 and 0L call one long"
        );
        assert!(!same_type(
            &s(&["LocationListener"]),
            &s(&["PendingIntent", "Parcelable"])
        ));
        assert!(same_type(&s(&["Leaf", "Base"]), &s(&["Base"])));
    }

    #[test]
    fn type_parameters_of_any_name_and_nested_classes_of_one_name() {
        // `TResult` is a type parameter of the method: it takes any argument
        let task = "class Task {\n    public <TResult> void put(TResult value) {}\n}\n";
        let mut c = call("put", &[&["Thing"]]);
        c.class = "Task".into();
        let m = find_member(Path::new("Task.java"), task, "Task", &c);
        assert!(m.exact && m.offsets.len() == 1, "{m:?}");
        // two nested Builders in one file: Outer.Builder is Outer's
        let two = "class Outer {\n    static class Builder {\n        void build(int a) {}\n    }\n}\nclass Other {\n    static class Builder {\n        void build(int b) {}\n    }\n}\n";
        let mut c = call("build", &[&["Int"]]);
        c.class = "Builder".into();
        let m = find_member(Path::new("X.java"), two, "Outer.Builder", &c);
        assert_eq!(m.offsets, [two.find("build(int a)").unwrap()]);
        let m = find_member(Path::new("X.java"), two, "Other.Builder", &c);
        assert_eq!(m.offsets, [two.find("build(int b)").unwrap()]);
    }

    #[test]
    fn type_parameters_only_where_a_declaration_opens_them_and_whole_zips_only() {
        let at = |s: &str| method_type_params(s, s.find(" get(").map_or(0, |i| i + 1));
        assert_eq!(at("class A {\n    public static <T> T get(T t) {}"), ["T"]);
        assert_eq!(at("class A {\n    fun <T> get(t: T) {}"), ["T"]);
        assert!(at("class A {\n    public List<String> get(String s) {}").is_empty());
        assert_eq!(
            at("class A {\n    /** a <b>doc</b> */\n    public <K,\n            V> V get(K k) {}"),
            ["K", "V"],
            "over two lines, not the comment's <b>"
        );
        let d = std::env::temp_dir().join(format!("dl-zip-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        // the smallest whole zip: an empty central directory
        let empty = [
            b'P', b'K', 5, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        std::fs::write(d.join("ok.jar"), empty).unwrap();
        std::fs::write(d.join("cut.jar"), b"PK\x03\x04 cut off here").unwrap();
        assert!(whole_zip(&d.join("ok.jar"), 1000));
        assert!(
            !whole_zip(&d.join("cut.jar"), 1000),
            "PK at the start is not enough"
        );
        assert!(!whole_zip(&d.join("ok.jar"), 10), "too big");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_kotlin_number_is_a_java_number() {
        let int = ["Int".to_string()];
        assert_eq!(match_score("Number", &int, &[], false), Some(80));
        assert_eq!(match_score("Comparable", &int, &[], false), Some(80));
        assert_eq!(match_score("String", &int, &[], false), None);
        assert!(
            match_score("Integer", &int, &[], false) > match_score("long", &int, &[], false),
            "Int is an Integer"
        );
    }

    #[test]
    fn arrows_literals_and_repository_blocks() {
        let kt = "package a\n\nclass A(val f: () -> Unit) : B(), C {\n}\n";
        assert_eq!(
            supertypes(kt, "A").len(),
            2,
            "the `->` of a function type is no bracket"
        );
        let java = "package a;\n\npublic class A extends\n        B implements\n        C {\n}\n";
        assert_eq!(
            supertypes(java, "A").len(),
            2,
            "extends and implements on their own lines"
        );
        let d = std::env::temp_dir().join(format!("dl-repos-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("settings.gradle.kts"), "val api = uri(\"https://api.example.com/v1\")\ndependencyResolutionManagement {\n    repositories {\n        google()\n        maven { url = uri(\"https://repo.example.com/maven\") }\n        maven { url = uri(\"https://private.example.com/m?token=secret\") }\n    }\n}\n").unwrap();
        // SAFETY: a unit test; the env var is put back
        let saved = std::env::var_os("DROID_LITE_MAVEN_REPOS");
        unsafe { std::env::remove_var("DROID_LITE_MAVEN_REPOS") };
        let r = repos(&d);
        std::fs::write(
            d.join("settings.gradle.kts"),
            "repositories { google() }\nval api = uri(\"https://api.example.com/v1\")\n",
        )
        .unwrap();
        let one_line = repos(&d);
        if let Some(s) = saved {
            unsafe { std::env::set_var("DROID_LITE_MAVEN_REPOS", s) };
        }
        assert_eq!(
            r,
            [
                "https://dl.google.com/dl/android/maven2",
                "https://repo.example.com/maven"
            ],
            "not the API URL"
        );
        assert_eq!(
            one_line,
            ["https://dl.google.com/dl/android/maven2"],
            "a one-line block ends on its line"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn coordinates_that_could_leave_the_cache_are_refused() {
        assert!(cached_sources_jar("androidx.core:core:1.13.1").is_some());
        for bad in [
            "..:x:1",
            "a:..:1",
            "a:b:../../x",
            "a/b:c:1",
            "a:b:1:extra",
            "a:b",
            "a:b:1 2",
        ] {
            assert!(cached_sources_jar(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn supertypes_nested_classes_and_jar_entries() {
        let java = "package android.app;\n\nimport android.os.Parcelable;\n\npublic class Notification implements Parcelable {\n    public static class Builder extends BaseBuilder<Builder> implements Cloneable {}\n}\n";
        let sup = supertypes(java, "Builder");
        assert_eq!(
            sup[0][..2],
            ["android.app.BaseBuilder", "java.lang.BaseBuilder"]
        );
        assert_eq!(
            sup[1][..2],
            ["android.app.Cloneable", "java.lang.Cloneable"],
            "java.lang too"
        );
        assert_eq!(
            supertypes(java, "Notification"),
            [["android.os.Parcelable"]]
        );
        let kt = "package a.b\n\nimport c.d.Base\n\nclass Leaf(x: Int) : Base(x), Comparable<Leaf> {\n}\n";
        let sup = supertypes(kt, "Leaf");
        assert_eq!(sup[0], ["c.d.Base"]);
        assert_eq!(sup[1][0], "a.b.Comparable");
        // a class named in a comment is not its declaration
        let doc = "/**\n * the class Spot is old\n */\npublic class Spot extends Base {}\n";
        assert_eq!(class_offset(doc, "Spot"), doc.find("Spot extends"));
        // generic parameter types with spaces, and annotations with arguments
        assert_eq!(simple_type("Map<String, Integer>"), "Map");
        assert_eq!(simple_type("@IntRange(from = 0) long"), "long");
        assert_eq!(
            simple_type("java.util.function.BiConsumer<A, B>"),
            "BiConsumer"
        );
        assert_eq!(simple_type("String[]"), "String[]");
        assert_eq!(simple_type("String?"), "String");
        assert_eq!(
            class_entry("android.app.Notification.Builder"),
            "android/app/Notification$Builder.class"
        );
        assert_eq!(
            class_entry("android.location.LocationManager"),
            "android/location/LocationManager.class"
        );
        assert!(declares("class Inner {}", "Inner") && !declares("class InnerFoo {}", "Inner"));
        // a Kotlin Multiplatform sources jar keeps files under a source set folder
        let d = std::env::temp_dir().join(format!("dl-kmp-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let jar = d.join("x-sources.jar");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&jar).unwrap());
        for (n, b) in [
            (
                "commonMain/demo/geo/Locator.kt",
                "package demo.geo\nclass Locator\n",
            ),
            (
                "commonMain/demo/geo/Other.kt",
                "package demo.geo\nclass Spot\n",
            ),
        ] {
            z.start_file(n, zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut z, b.as_bytes()).unwrap();
        }
        z.finish().unwrap();
        assert_eq!(
            jar_entry(&jar, "demo.geo.Locator").as_deref(),
            Some("commonMain/demo/geo/Locator.kt")
        );
        assert_eq!(
            jar_entry(&jar, "demo.geo.Spot").as_deref(),
            Some("commonMain/demo/geo/Other.kt")
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn types_and_literals() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(accepts_("long", &s(&["Int"])), "a number literal widens");
        // a Java caller widens an int to a float too, after a long (like javac)
        let js = |p: &str, a: &str| match_score(p, &s(&[a]), &[], true);
        assert!(
            js("float", "Int") < js("long", "Int"),
            "javac: int to long before float"
        );
        assert!(js("double", "Float").is_some() && js("double", "Long").is_some());
        assert_eq!(
            match_score("float", &s(&["Int"]), &[], false),
            None,
            "kotlinc does not widen"
        );
        assert!(!accepts_("boolean", &s(&["Int"])));
        assert!(accepts_(
            "@NonNull final java.lang.CharSequence",
            &s(&["String"])
        ));
        assert!(accepts_("T", &s(&["Anything"])));
        assert!(accepts_("List<String>?", &s(&["List"])));
        assert!(!accepts_("PendingIntent", &s(&["LocationListener"])));
        assert_eq!(compile_sdk_in("branchCompileSdk = \"36\""), Some(36));
        assert_eq!(compile_sdk_in("    compileSdk = 35"), Some(35));
        assert_eq!(compile_sdk_in("compileSdkVersion(34)"), Some(34));
        assert_eq!(
            compile_sdk_in("compileSdk = libs.versions.x.get().toInt()"),
            None
        );
        assert_eq!(
            compile_sdk_in("android {\n    compileSdk {\n        version = release(36)\n    }\n}"),
            Some(36),
            "the new DSL"
        );
        assert_eq!(
            compile_sdk_in("// compileSdk = 99\ncompileSdkExtension = 12\ncompileSdk = 35\n"),
            Some(35),
            "comments and the extension level are not the SDK"
        );
        assert_eq!(
            java_signature(
                "requestLocationUpdates",
                "(Ljava/lang/String;JFLandroid/location/LocationListener;)V"
            ),
            "void requestLocationUpdates(String p0, long p1, float p2, LocationListener p3)"
        );
        assert_eq!(
            java_signature("names", "()[Ljava/lang/String;"),
            "String[] names()"
        );
    }
}
