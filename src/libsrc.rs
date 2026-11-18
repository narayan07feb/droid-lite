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

/// The project's compile SDK: the highest number written after `compileSdk` (any case, also a
/// version catalog key like `branchCompileSdk = "36"`) in its build scripts and catalog; else the
/// newest platform the SDK has.
pub fn compile_sdk(root: &Path, scripts: &[PathBuf], sdk: &Path) -> Option<u32> {
    let catalog = root.join("gradle/libs.versions.toml");
    let found = scripts
        .iter()
        .chain(std::iter::once(&catalog))
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .flat_map(|t| {
            t.lines()
                .filter_map(number_after_compile_sdk)
                .collect::<Vec<_>>()
        })
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

/// `compileSdk = 35`, `compileSdkVersion(34)`, `branchCompileSdk = "36"`: the number.
fn number_after_compile_sdk(line: &str) -> Option<u32> {
    let l = line.to_ascii_lowercase();
    let at = l.find("compilesdk")? + "compilesdk".len();
    let rest = &l[at..];
    let rest = rest.trim_start_matches(|c: char| c.is_alphabetic());
    let rest = rest.trim_start_matches(|c: char| " =(\"'".contains(c));
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
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

/// The cached `-sources.jar` of `group:name:version`.
pub fn cached_sources_jar(coords: &str) -> Option<PathBuf> {
    let mut it = coords.split(':');
    let (g, n, v) = (it.next()?, it.next()?, it.next()?);
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
        for line in t.lines().map(str::trim).filter(|l| !l.starts_with("//")) {
            if line.contains("google()") {
                add(&mut out, "https://dl.google.com/dl/android/maven2");
            }
            if line.contains("mavenCentral()") {
                add(&mut out, "https://repo.maven.apache.org/maven2");
            }
            for key in ["maven(\"", "uri(\"", "url \"", "url '"] {
                if let Some(i) = line.find(key) {
                    let rest = &line[i + key.len()..];
                    if let Some(end) = rest.find(['"', '\'']) {
                        let u = &rest[..end];
                        if u.starts_with("http") {
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
    out
}

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

/// Runs a download (blocking: call it on a worker thread).
pub fn fetch(f: &Fetch) -> Result<(), String> {
    match f {
        Fetch::Sdk { sdk, api } => {
            let tool = std::env::var_os("DROID_LITE_SDKMANAGER")
                .map(PathBuf::from)
                .unwrap_or_else(|| sdk.join("cmdline-tools/latest/bin/sdkmanager"));
            if !tool.exists() {
                return Err(format!(
                    "sdkmanager not found ({}): install the Android SDK Command-line Tools",
                    tool.display()
                ));
            }
            let out = std::process::Command::new(&tool)
                .arg(format!("--sdk_root={}", sdk.display()))
                .arg(format!("sources;android-{api}"))
                .stdin(std::process::Stdio::null())
                .output()
                .map_err(|e| format!("sdkmanager did not start: {e}"))?;
            if sdk.join(format!("sources/android-{api}")).is_dir() {
                Ok(())
            } else {
                let err = String::from_utf8_lossy(&out.stderr);
                let line = err
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("no sources");
                Err(format!("sdkmanager: {}", line.trim()))
            }
        }
        Fetch::Maven { coords, repos } => {
            let dest = cached_sources_jar(coords).ok_or("not group:name:version")?;
            let mut it = coords.split(':');
            let (g, n, v) = (
                it.next().unwrap_or(""),
                it.next().unwrap_or(""),
                it.next().unwrap_or(""),
            );
            let rel = format!("{}/{n}/{v}/{n}-{v}-sources.jar", g.replace('.', "/"));
            std::fs::create_dir_all(dest.parent().unwrap_or(Path::new(".")))
                .map_err(|e| e.to_string())?;
            let part = dest.with_extension("jar.part");
            for r in repos {
                let ok = std::process::Command::new("curl")
                    .args(["-fsSL", "--max-time", "120", "-o"])
                    .arg(&part)
                    .arg(format!("{r}/{rel}"))
                    .stdin(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success());
                if ok && std::fs::rename(&part, &dest).is_ok() {
                    return Ok(());
                }
            }
            let _ = std::fs::remove_file(&part);
            Err(format!(
                "no sources jar for {coords} in {} repositories",
                repos.len()
            ))
        }
    }
}

/// The entry of `qualified`'s source in a sources jar: `a/b/C.kt` or `a/b/C.java`, else a file of
/// that package that declares the class.
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
    for ext in ["kt", "java"] {
        let want = format!("{dir}{class}.{ext}");
        if names.contains(&want) {
            return Some(want);
        }
    }
    let decl = [
        format!("class {class}"),
        format!("interface {class}"),
        format!("object {class}"),
    ];
    names
        .iter()
        .filter(|n| n.starts_with(&dir) && !n[dir.len()..].contains('/'))
        .filter(|n| n.ends_with(".kt") || n.ends_with(".java"))
        .find(|n| {
            let mut s = String::new();
            z.by_name(n)
                .ok()
                .and_then(|mut e| std::io::Read::read_to_string(&mut e, &mut s).ok())
                .is_some_and(|_| decl.iter().any(|d| s.contains(d.as_str())))
        })
        .cloned()
}

/// The declarations of `call.member` in `class` of a library source (`text` of `path`) that the
/// call means, best first: those whose parameters take the arguments' types; else those that
/// take as many arguments; else every one of that name. A Kotlin property read as a Java getter
/// (`manager.allProviders` for `getAllProviders()`) finds the getter.
pub fn find_member(path: &Path, text: &str, class: &str, call: &LibCall) -> Vec<usize> {
    let syms = crate::kotlin::scan_file(path, text);
    let in_class = |d: &&crate::kotlin::Decl| {
        d.container
            .as_deref()
            .is_none_or(|c| c.rsplit('.').next() == Some(class))
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
        return ds.iter().map(|d| d.offset).collect();
    };
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
            let mut score = 0;
            for (a, p) in args.iter().zip(&d.params) {
                if a.is_empty() {
                    continue;
                }
                score += match_score(&p.ty, a)?;
            }
            Some((d.offset, score))
        })
        .collect();
    if let Some(best) = scored.iter().map(|(_, s)| *s).max() {
        return scored
            .into_iter()
            .filter(|(_, s)| *s == best)
            .map(|(o, _)| o)
            .collect();
    }
    if !by_count.is_empty() {
        return by_count.iter().map(|d| d.offset).collect();
    }
    ds.iter().map(|d| d.offset).collect()
}

/// How well a parameter of type `param` (as spelled in Java or Kotlin source) takes an argument
/// whose type and supertypes are `arg` (simple names, Kotlin spelling): 3 its own type, 2 a
/// supertype or a widening, 1 a parameter that takes anything, None when it does not take it.
fn match_score(param: &str, arg: &[String]) -> Option<usize> {
    let p = simple_type(param);
    if arg.first() == Some(&p) {
        return Some(3);
    }
    if p.is_empty()
        || p == "Object"
        || p == "Any"
        || (p.len() == 1 && p.starts_with(char::is_uppercase))
    {
        return Some(1); // anything, or a type parameter
    }
    arg.iter()
        .any(|a| {
            a == &p
                || match a.as_str() {
                    // a number literal widens to a bigger Java type; a Kotlin Int to its boxes
                    "Int" => [
                        "int", "Integer", "long", "Long", "short", "Short", "byte", "Byte",
                    ]
                    .contains(&p.as_str()),
                    "Long" => ["long", "Long"].contains(&p.as_str()),
                    "Float" => ["float", "Float"].contains(&p.as_str()),
                    "Double" => ["double", "Double"].contains(&p.as_str()),
                    "Boolean" => ["boolean", "Boolean"].contains(&p.as_str()),
                    "Char" => ["char", "Character"].contains(&p.as_str()),
                    "String" => ["String", "CharSequence"].contains(&p.as_str()),
                    _ => false,
                }
        })
        .then_some(2)
}

/// `@NonNull final java.util.List<String>...` gives `List`; `String?` gives `String`.
fn simple_type(ty: &str) -> String {
    let words: Vec<&str> = ty
        .split_whitespace()
        .filter(|w| !w.starts_with('@') && !matches!(*w, "final" | "vararg" | "out" | "in"))
        .collect();
    let t = words.last().copied().unwrap_or("");
    let t = t.split('<').next().unwrap_or(t);
    let t = t
        .trim_end_matches("...")
        .trim_end_matches("[]")
        .trim_end_matches('?');
    t.rsplit('.').next().unwrap_or(t).to_string()
}

/// A stub for a class without source, from its class file: its public methods as Java
/// declarations, one per line, so Go to declaration has a line to show.
pub fn stub(class_bytes: &[u8], note: &str) -> Option<String> {
    let c = crate::classfile::parse(class_bytes)?;
    let name = c.name.replace('/', ".");
    let (pkg, simple) = name.rsplit_once('.').unwrap_or(("", &name));
    let mut out = format!("// {note}\n");
    if !pkg.is_empty() {
        out.push_str(&format!("package {pkg};\n\n"));
    }
    out.push_str(&format!("public class {simple} {{\n"));
    for (m, desc) in &c.methods {
        out.push_str(&format!("    public {};\n", java_signature(m, desc)));
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
        }
    }

    const LM: &str = "package android.location;\n\npublic class LocationManager {\n    public void requestLocationUpdates(@NonNull String provider, long minTimeMs, float minDistanceM, @NonNull LocationListener listener) {}\n    public void requestLocationUpdates(@NonNull String provider, long minTimeMs, float minDistanceM, @NonNull PendingIntent pendingIntent) {}\n    public void requestLocationUpdates(@NonNull String provider, long minTimeMs, float minDistanceM, @NonNull LocationListener listener, @Nullable Looper looper) {}\n    public @NonNull List<String> getAllProviders() { return null; }\n}\n";

    #[test]
    fn the_overload_whose_parameter_types_take_the_arguments_is_the_one() {
        let p = Path::new("LocationManager.java");
        let listener = find_member(
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
        let pending = find_member(
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
        let mine = find_member(
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
        let best = find_member(Path::new("Locator.java"), src, "Locator", &l);
        assert!(best.len() == 1 && src[best[0]..].starts_with("locate(String name, long timeout)"));
        // types not known: every 4-argument one
        assert_eq!(
            find_member(
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
        let g = find_member(p, LM, "LocationManager", &prop);
        assert!(g.len() == 1 && LM[g[0]..].starts_with("getAllProviders"));
    }

    fn accepts_(param: &str, arg: &[String]) -> bool {
        match_score(param, arg).is_some()
    }

    #[test]
    fn types_and_literals() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(accepts_("long", &s(&["Int"])), "a number literal widens");
        assert!(!accepts_("float", &s(&["Int"])));
        assert!(accepts_(
            "@NonNull final java.lang.CharSequence",
            &s(&["String"])
        ));
        assert!(accepts_("T", &s(&["Anything"])));
        assert!(accepts_("List<String>?", &s(&["List"])));
        assert!(!accepts_("PendingIntent", &s(&["LocationListener"])));
        assert_eq!(
            number_after_compile_sdk("branchCompileSdk = \"36\""),
            Some(36)
        );
        assert_eq!(number_after_compile_sdk("    compileSdk = 35"), Some(35));
        assert_eq!(number_after_compile_sdk("compileSdkVersion(34)"), Some(34));
        assert_eq!(
            number_after_compile_sdk("compileSdk = libs.versions.x.get().toInt()"),
            None
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
