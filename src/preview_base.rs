//! The renderer every Compose preview needs (layoutlib, Paparazzi's layoutlib SDK and the jars they
//! load, layoutlib's native runtime and framework resources), found or downloaded by Droid Lite
//! itself: no Gradle run, no pack. The set is fixed and checksummed ([`PIECES`]).
//!
//! Each piece is looked up at every worker start, in this order: Gradle's cache (read only, never
//! written), Droid Lite's own `m2/` folder, then a download with curl from its repository (resumable
//! `.part` file, retries, SHA-1 checked before the rename). The worker gets the found paths as they
//! are (no links, no copies), so a piece that goes away is found again or downloaded at the next
//! start. The two layoutlib data jars are unpacked once into `base/layoutlib/`. android.jar comes
//! from the Android SDK. If any piece is missing the worker does not start: the error names it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Repo {
    Central,
    Google,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Use {
    /// On the worker's classpath.
    Classpath,
    /// Unpacked once into `base/layoutlib/<name>`.
    Unpack(&'static str),
}

#[derive(Clone, Copy, Debug)]
struct Piece {
    group: &'static str,
    artifact: &'static str,
    version: &'static str,
    classifier: &'static str,
    sha1: &'static str,
    size: u64,
    repo: Repo,
    use_: Use,
}

impl Piece {
    fn file_name(&self) -> String {
        match self.classifier {
            "" => format!("{}-{}.jar", self.artifact, self.version),
            c => format!("{}-{}-{c}.jar", self.artifact, self.version),
        }
    }

    fn url(&self) -> String {
        let repo = match self.repo {
            Repo::Central => "https://repo1.maven.org/maven2",
            Repo::Google => "https://dl.google.com/dl/android/maven2",
        };
        format!(
            "{repo}/{}/{}/{}/{}",
            self.group.replace('.', "/"),
            self.artifact,
            self.version,
            self.file_name()
        )
    }

    fn label(&self) -> String {
        format!("{} ({})", self.artifact, mb(self.size))
    }
}

const fn jar(
    group: &'static str,
    artifact: &'static str,
    version: &'static str,
    sha1: &'static str,
    size: u64,
    repo: Repo,
) -> Piece {
    Piece {
        group,
        artifact,
        version,
        classifier: "",
        sha1,
        size,
        repo,
        use_: Use::Classpath,
    }
}

const LAYOUTLIB_RUNTIME: &str = "com.android.tools.layoutlib";

/// What the worker loads (measured on 7 BranchAndroid screens' class-load logs), with the SHA-1 and
/// size of each file as Maven Central or Google's Maven publish them. Both native runtimes are listed;
/// [`pieces`] takes the one for this Mac.
#[rustfmt::skip]
const PIECES: [Piece; 17] = [
    jar("com.android.tools.layoutlib", "layoutlib", "14.0.11", "1c2e551118aeeb0712ae3c7663deb449bee6ca11", 50566171, Repo::Google),
    jar("net.bytebuddy", "byte-buddy", "1.15.10", "635c873fadd853c084f84fdc3cbd58c5dd8537f9", 8470015, Repo::Central),
    jar("net.bytebuddy", "byte-buddy-agent", "1.15.10", "0e8eb255b2c378b9a6c7341e7b0e12f0a5636377", 365425, Repo::Central),
    jar("com.google.guava", "guava", "33.3.1-jre", "852f8b363da0111e819460021ca693cacca3e8db", 3079289, Repo::Central),
    jar("app.cash.paparazzi", "paparazzi", "1.3.5", "dcaa130064b67a4e65a482a7937d9d6ca50b0a41", 446032, Repo::Central),
    jar("com.android.tools.layoutlib", "layoutlib-api", "31.4.2", "5b4270989aafbce80f83599e345afd927cf6ba26", 122058, Repo::Google),
    jar("com.android.tools", "sdk-common", "31.4.2", "97562d4d24cf4d5dfa9b8a8dbb6d398cb58ba12e", 1483335, Repo::Google),
    jar("com.android.tools", "common", "31.4.2", "754a260908654def1d49b746f01d50eec47dd94a", 576756, Repo::Google),
    jar("com.squareup.okio", "okio-jvm", "3.9.1", "79696e3f4ff05d41ba2b3dc68adb5808a2e5dc53", 372276, Repo::Central),
    jar("org.jetbrains.intellij.deps", "trove4j", "1.0.20200330", "3afb14d5f9ceb459d724e907a21145e8ff394f02", 572985, Repo::Central),
    jar("net.sf.kxml", "kxml2", "2.3.0", "ccbc77a5fd907ef863c29f3596c6f54ffa4e9442", 43858, Repo::Central),
    jar("junit", "junit", "4.13.2", "8ac9e16d933b6fb43bc7f576336b8f4d7eb5ba12", 384581, Repo::Central),
    jar("com.google.code.findbugs", "jsr305", "3.0.2", "25ea2e8b0c338a877313bd4672d3fe056ea78f0d", 19936, Repo::Central),
    jar("com.google.jimfs", "jimfs", "1.1", "8fbd0579dc68aba6186935cc1bee21d2f3e7ec1c", 206691, Repo::Central),
    Piece {
        group: LAYOUTLIB_RUNTIME,
        artifact: "layoutlib-runtime",
        version: "14.0.11",
        classifier: "mac-arm",
        sha1: "7f9b87268fcbab0c243bfd52dac56cce5ad38f48",
        size: 75527902,
        repo: Repo::Google,
        use_: Use::Unpack("runtime"),
    },
    Piece {
        group: LAYOUTLIB_RUNTIME,
        artifact: "layoutlib-runtime",
        version: "14.0.11",
        classifier: "mac",
        sha1: "b88a5870afcf111de1ba398a0c95c64a15f10d18",
        size: 75978348,
        repo: Repo::Google,
        use_: Use::Unpack("runtime"),
    },
    Piece {
        group: LAYOUTLIB_RUNTIME,
        artifact: "layoutlib-resources",
        version: "14.0.11",
        classifier: "",
        sha1: "78cc95d5f729fc5e80fb51949ab3d6949d784dda",
        size: 33176496,
        repo: Repo::Google,
        use_: Use::Unpack("resources"),
    },
];

/// The pieces for this Mac (the native runtime of its CPU).
fn pieces() -> Vec<Piece> {
    let native = if std::env::consts::ARCH == "aarch64" {
        "mac-arm"
    } else {
        "mac"
    };
    PIECES
        .iter()
        .filter(|p| p.artifact != "layoutlib-runtime" || p.classifier == native)
        .copied()
        .collect()
}

/// What the worker runs with.
#[derive(Clone, Debug)]
pub struct Base {
    /// The renderer's jars, sorted by file name.
    pub runtime: Vec<PathBuf>,
    pub layoutlib_runtime: PathBuf,
    pub layoutlib_resources: PathBuf,
    pub android_jar: PathBuf,
}

/// Fetches one URL into a file, resuming what is already there.
trait Fetch {
    fn fetch(&self, url: &str, to: &Path, progress: &dyn Fn(u64)) -> Result<(), String>;
}

/// curl (always on macOS): resumable (`-C -`), retries, fails on HTTP errors. Progress is read from
/// the file's size while it runs.
struct Curl;

/// curl's options: a connection that stalls (under 1 KB/s for 30 s) fails instead of hanging; the
/// `.part` file resumes at the next try.
const CURL_ARGS: [&str; 13] = [
    "-fsSL",
    "-C",
    "-",
    "--retry",
    "3",
    "--retry-delay",
    "2",
    "--connect-timeout",
    "20",
    "--speed-limit",
    "1024",
    "--speed-time",
    "30",
];

impl Fetch for Curl {
    fn fetch(&self, url: &str, to: &Path, progress: &dyn Fn(u64)) -> Result<(), String> {
        let mut child = Command::new("curl")
            .args(CURL_ARGS)
            .arg("-o")
            .arg(to)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not run curl: {e}"))?;
        let status = loop {
            if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
                break s;
            }
            progress(size_of(to));
            std::thread::sleep(Duration::from_millis(250));
        };
        progress(size_of(to));
        if status.success() {
            return Ok(());
        }
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut e, &mut err);
        }
        Err(err
            .trim()
            .lines()
            .last()
            .unwrap_or("curl failed")
            .to_string())
    }
}

fn size_of(p: &Path) -> u64 {
    std::fs::metadata(p).map_or(0, |m| m.len())
}

fn mb(bytes: u64) -> String {
    format!("{} MB", (bytes as f64 / 1e6).round().max(1.0) as u64)
}

/// The piece in Gradle's cache: `<group>/<artifact>/<version>/<sha1>/<file>`, the folder named by the
/// file's SHA-1 (Gradle drops leading zeros), with the right size.
fn in_gradle(p: &Piece, gradle: &Path) -> Option<PathBuf> {
    let want = p.sha1.trim_start_matches('0');
    let dir = gradle.join(p.group).join(p.artifact).join(p.version);
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().trim_start_matches('0') == want)
        .map(|e| e.path().join(p.file_name()))
        .find(|f| size_of(f) == p.size)
}

/// Where Droid Lite keeps a downloaded piece.
fn own_path(p: &Piece, own: &Path) -> PathBuf {
    own.join(p.group)
        .join(p.artifact)
        .join(p.version)
        .join(p.file_name())
}

/// Each piece's file if it is on this Mac now (Gradle's cache first, then the own folder), else None:
/// a download is needed.
fn plan(pieces: &[Piece], gradle: Option<&Path>, own: &Path) -> Vec<Option<PathBuf>> {
    pieces
        .iter()
        .map(|p| {
            gradle.and_then(|g| in_gradle(p, g)).or_else(|| {
                let f = own_path(p, own);
                (size_of(&f) == p.size && f.is_file()).then_some(f)
            })
        })
        .collect()
}

/// Every piece's file: found, or downloaded into `own` and checked. Progress goes to `status` as
/// "Downloading <artifact> (<size>)" with (MB done, MB total). Err names every piece still missing.
fn resolve(
    pieces: &[Piece],
    gradle: Option<&Path>,
    own: &Path,
    fetch: &dyn Fetch,
    status: &dyn Fn(String, Option<(u64, u64)>),
) -> Result<Vec<PathBuf>, String> {
    let mut found = plan(pieces, gradle, own);
    let total: u64 = pieces
        .iter()
        .zip(&found)
        .filter(|(_, f)| f.is_none())
        .map(|(p, _)| p.size)
        .sum();
    let mut done = 0u64;
    let mut failed = Vec::new();
    for (p, slot) in pieces.iter().zip(found.iter_mut()) {
        if slot.is_some() {
            continue;
        }
        let text = format!("Downloading {}", p.label());
        let report = |now: u64| {
            status(
                text.clone(),
                Some(((done + now.min(p.size)) / 1_000_000, total / 1_000_000)),
            )
        };
        match download(p, own, fetch, &report) {
            Ok(f) => *slot = Some(f),
            Err(e) => failed.push(format!("  {} {}: {e}", p.label(), p.version)),
        }
        done += p.size;
    }
    if !failed.is_empty() {
        return Err(format!(
            "The preview renderer is not complete: these files are missing and could not be downloaded \
             (offline?). Open the preview again to retry.\n{}",
            failed.join("\n")
        ));
    }
    Ok(found.into_iter().flatten().collect())
}

/// An exclusive lock on `path` (made if missing), held until the file is dropped: two engines never
/// write one download or one unpacked folder at once.
fn lock(path: &Path) -> Result<std::fs::File, String> {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    f.lock().map_err(|e| e.to_string())?;
    Ok(f)
}

/// One piece into `own`: through `<file>.part` (kept for a resume after a failure), renamed only when
/// its size and SHA-1 are right. Under `<file>.lock`: another engine fetching the same piece is
/// waited for, and its finished file used.
fn download(
    p: &Piece,
    own: &Path,
    fetch: &dyn Fetch,
    progress: &dyn Fn(u64),
) -> Result<PathBuf, String> {
    let to = own_path(p, own);
    std::fs::create_dir_all(to.parent().unwrap()).map_err(|e| e.to_string())?;
    let _lock = lock(&to.with_extension("jar.lock"))?;
    if size_of(&to) == p.size && to.is_file() {
        return Ok(to);
    }
    let part = to.with_extension("jar.part");
    if size_of(&part) > p.size {
        let _ = std::fs::remove_file(&part);
    }
    if size_of(&part) < p.size {
        fetch.fetch(&p.url(), &part, progress)?;
    }
    let sha = sha1_file(&part).map_err(|e| e.to_string())?;
    if size_of(&part) != p.size || sha != p.sha1 {
        let _ = std::fs::remove_file(&part); // a bad file: the next try starts over
        return Err(format!(
            "the download did not match its checksum (sha1 {sha}, expected {})",
            p.sha1
        ));
    }
    std::fs::rename(&part, &to).map_err(|e| e.to_string())?;
    Ok(to)
}

/// A layoutlib data jar unpacked into `to`, once per checksum (`<to>.sha1` says which jar it holds).
/// Under `<to>.lock`: another engine's unpack is waited for, never replaced under its worker.
fn unpacked(jar: &Path, sha1: &str, to: &Path) -> Result<(), String> {
    if is_unpacked(to, sha1) {
        return Ok(());
    }
    std::fs::create_dir_all(to.parent().unwrap()).map_err(|e| e.to_string())?;
    let _lock = lock(&to.with_extension("lock"))?;
    if is_unpacked(to, sha1) {
        return Ok(()); // the other engine unpacked it meanwhile
    }
    sweep_unpack_leftovers(to);
    let tmp = to.with_extension(format!("new-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let file = std::fs::File::open(jar).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    zip.extract(&tmp)
        .map_err(|e| format!("could not unpack {}: {e}", jar.display()))?;
    let _ = std::fs::remove_dir_all(to);
    std::fs::rename(&tmp, to).map_err(|e| e.to_string())?;
    std::fs::write(to.with_extension("sha1"), sha1).map_err(|e| e.to_string())
}

/// Deletes `<to>.new-<pid>` folders that a killed or failed unpack left behind (this process's,
/// or one whose process has ended); one being written by a live process stays.
fn sweep_unpack_leftovers(to: &Path) {
    let prefix = to.with_extension("new-");
    let (Some(dir), Some(prefix)) = (to.parent(), prefix.file_name()) else {
        return;
    };
    let prefix = prefix.to_string_lossy().into_owned();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.strip_prefix(&prefix) else {
            continue;
        };
        let live = pid
            .parse::<i32>()
            .is_ok_and(|p| p as u32 != std::process::id() && unsafe { libc::kill(p, 0) } == 0);
        if !live {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

fn is_unpacked(to: &Path, sha1: &str) -> bool {
    to.is_dir()
        && std::fs::read_to_string(to.with_extension("sha1"))
            .ok()
            .as_deref()
            == Some(sha1)
}

/// android.jar of the Android SDK: platforms/android-35, else the newest installed.
fn android_jar() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let sdks = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
        .iter()
        .filter_map(|v| std::env::var_os(v).map(PathBuf::from))
        .chain(home.map(|h| h.join("Library/Android/sdk")));
    sdks.into_iter()
        .find_map(|sdk| newest_android_jar(&sdk.join("platforms")))
}

fn newest_android_jar(platforms: &Path) -> Option<PathBuf> {
    let preferred = platforms.join("android-35/android.jar");
    if preferred.is_file() {
        return Some(preferred);
    }
    let level = |name: &str| -> Option<(u32, u32)> {
        let v = name.strip_prefix("android-")?;
        let (major, minor) = v.split_once('.').unwrap_or((v, "0"));
        Some((major.parse().ok()?, minor.parse().ok()?))
    };
    std::fs::read_dir(platforms)
        .ok()?
        .flatten()
        .filter_map(|e| {
            Some((
                level(&e.file_name().to_string_lossy())?,
                e.path().join("android.jar"),
            ))
        })
        .filter(|(_, j)| j.is_file())
        .max_by_key(|(l, _)| *l)
        .map(|(_, j)| j)
}

/// What the renderer is, for the preview cache key: every piece's checksum and the SDK's
/// android.jar (path, time, size). Cheap: nothing is downloaded or read.
pub fn fingerprint() -> String {
    let mut s: String = pieces()
        .iter()
        .map(|p| p.sha1)
        .collect::<Vec<_>>()
        .join(",");
    if let Some(j) = android_jar() {
        let m = std::fs::metadata(&j).ok();
        let t = m.as_ref().and_then(|m| m.modified().ok());
        s.push_str(&format!("|{}|{t:?}|{:?}", j.display(), m.map(|m| m.len())));
    }
    s
}

/// The base for a worker start: every piece found or downloaded, the data jars unpacked under
/// `base`, android.jar from the SDK. `DROID_LITE_PREVIEW_NO_GRADLE_CACHE` skips Gradle's cache (to
/// test the download).
pub fn ensure(
    base: &Path,
    own: &Path,
    status: &dyn Fn(String, Option<(u64, u64)>),
) -> Result<Base, String> {
    let android_jar = android_jar().ok_or(
        "Previews need android.jar from the Android SDK (platforms/android-35): install that platform with \
         Android Studio's SDK Manager, or set ANDROID_HOME.",
    )?;
    let gradle = crate::libs::gradle_home().join("caches/modules-2/files-2.1");
    let gradle = std::env::var_os("DROID_LITE_PREVIEW_NO_GRADLE_CACHE")
        .is_none()
        .then_some(gradle.as_path());
    let runtime = assemble(base, &pieces(), gradle, own, &Curl, status)?;
    Ok(Base {
        runtime,
        layoutlib_runtime: base.join("layoutlib/runtime"),
        layoutlib_resources: base.join("layoutlib/resources"),
        android_jar,
    })
}

/// The classpath jars (sorted by file name), with the data jars unpacked under `base/layoutlib/`. A
/// data jar already unpacked is not needed again; one downloaded into `own` is deleted once unpacked
/// (no second copy on disk).
fn assemble(
    base: &Path,
    pieces: &[Piece],
    gradle: Option<&Path>,
    own: &Path,
    fetch: &dyn Fetch,
    status: &dyn Fn(String, Option<(u64, u64)>),
) -> Result<Vec<PathBuf>, String> {
    let target = |p: &Piece| match p.use_ {
        Use::Unpack(name) => Some(base.join("layoutlib").join(name)),
        Use::Classpath => None,
    };
    let needed: Vec<Piece> = pieces
        .iter()
        .filter(|p| !target(p).is_some_and(|to| is_unpacked(&to, p.sha1)))
        .copied()
        .collect();
    let files = resolve(&needed, gradle, own, fetch, status)?;
    let mut runtime = Vec::new();
    for (p, f) in needed.iter().zip(files) {
        match target(p) {
            None => runtime.push(f),
            Some(to) => {
                status(format!("Unpacking {}", p.label()), None);
                unpacked(&f, p.sha1, &to)?;
                if f.starts_with(own) {
                    let _ = std::fs::remove_file(&f);
                }
            }
        }
    }
    runtime.sort_by_key(|f| f.file_name().map(|n| n.to_os_string()));
    Ok(runtime)
}

/// SHA-1 of a file, lower-case hex.
fn sha1_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha1::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = std::io::Read::read(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.hex())
}

/// SHA-1 (FIPS 180-4), only to check downloads against the repositories' published sums.
struct Sha1 {
    state: [u32; 5],
    block: Vec<u8>,
    len: u64,
}

impl Sha1 {
    fn new() -> Sha1 {
        Sha1 {
            state: [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0],
            block: Vec::with_capacity(64),
            len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.len += data.len() as u64;
        while !data.is_empty() {
            let take = (64 - self.block.len()).min(data.len());
            self.block.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.block.len() == 64 {
                let b: [u8; 64] = self.block[..].try_into().unwrap();
                self.compress(&b);
                self.block.clear();
            }
        }
    }

    fn compress(&mut self, b: &[u8; 64]) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b2, mut c, mut d, mut e] = self.state;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b2 & c) | (!b2 & d), 0x5A827999),
                20..=39 => (b2 ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b2 & c) | (b2 & d) | (c & d), 0x8F1BBCDC),
                _ => (b2 ^ c ^ d, 0xCA62C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b2.rotate_left(30);
            b2 = a;
            a = t;
        }
        for (s, v) in self.state.iter_mut().zip([a, b2, c, d, e]) {
            *s = s.wrapping_add(v);
        }
    }

    fn hex(mut self) -> String {
        let bits = self.len * 8;
        self.update(&[0x80]);
        while self.block.len() != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        self.state.iter().map(|s| format!("{s:08x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// "hello": 5 bytes, SHA-1 aaf4c61d….
    const HELLO_SHA1: &str = "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d";

    fn piece(artifact: &'static str, sha1: &'static str, size: u64) -> Piece {
        jar("demo.lib", artifact, "1.0", sha1, size, Repo::Central)
    }

    /// Writes fixed bytes for every URL and records the URLs; no network.
    struct Fake {
        bytes: &'static [u8],
        urls: RefCell<Vec<String>>,
    }

    impl Fetch for Fake {
        fn fetch(&self, url: &str, to: &Path, progress: &dyn Fn(u64)) -> Result<(), String> {
            self.urls.borrow_mut().push(url.to_string());
            std::fs::write(to, self.bytes).map_err(|e| e.to_string())?;
            progress(self.bytes.len() as u64);
            Ok(())
        }
    }

    fn fake(bytes: &'static [u8]) -> Fake {
        Fake {
            bytes,
            urls: RefCell::new(Vec::new()),
        }
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("droid-lite-base-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn quiet(_: String, _: Option<(u64, u64)>) {}

    #[test]
    fn sha1_matches_the_standard_vectors() {
        let d = temp("sha1");
        for (text, sum) in [
            ("", "da39a3ee5e6b4b0d3255bfef95601890afd80709"),
            ("abc", "a9993e364706816aba3e25717850c26c9cd0d89d"),
            ("hello", HELLO_SHA1),
            (
                "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "84983e441c3bd26ebaae4aa1f95129e5e54670f1",
            ),
        ] {
            std::fs::write(d.join("f"), text).unwrap();
            assert_eq!(sha1_file(&d.join("f")).unwrap(), sum, "{text:?}");
        }
    }

    #[test]
    fn gradle_cache_hit_is_used_as_it_is_and_nothing_downloads() {
        let (g, own) = (temp("g-hit"), temp("g-hit-own"));
        // Gradle names the folder by the SHA-1 without its leading zeros
        let p = piece("lib", "0e8eb255b2c378b9a6c7341e7b0e12f0a5636377", 5);
        let f = g.join("demo.lib/lib/1.0/e8eb255b2c378b9a6c7341e7b0e12f0a5636377/lib-1.0.jar");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, "hello").unwrap();
        let net = fake(b"hello");
        assert_eq!(
            resolve(&[p], Some(&g), &own, &net, &quiet).unwrap(),
            vec![f]
        );
        assert!(net.urls.borrow().is_empty());
        assert!(
            std::fs::read_dir(&own).unwrap().next().is_none(),
            "nothing copied out of Gradle's cache"
        );
    }

    #[test]
    fn gradle_cache_file_of_another_checksum_or_size_is_not_used() {
        let (g, own) = (temp("g-miss"), temp("g-miss-own"));
        let p = piece("lib", HELLO_SHA1, 5);
        let other = g.join("demo.lib/lib/1.0/1111111111111111111111111111111111111111/lib-1.0.jar");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, "hello").unwrap();
        let short = g.join(format!("demo.lib/lib/1.0/{HELLO_SHA1}/lib-1.0.jar"));
        std::fs::create_dir_all(short.parent().unwrap()).unwrap();
        std::fs::write(&short, "hell").unwrap();
        assert_eq!(plan(&[p], Some(&g), &own), vec![None]);
    }

    #[test]
    fn own_cache_hit_when_gradle_has_none() {
        let own = temp("own-hit");
        let p = piece("lib", HELLO_SHA1, 5);
        let f = own.join("demo.lib/lib/1.0/lib-1.0.jar");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, "hello").unwrap();
        let net = fake(b"hello");
        assert_eq!(
            resolve(&[p], Some(&temp("own-hit-g")), &own, &net, &quiet).unwrap(),
            vec![f]
        );
        assert!(net.urls.borrow().is_empty());
    }

    #[test]
    fn missing_piece_is_planned_then_downloaded_from_its_repository_with_progress() {
        let own = temp("dl");
        let p = jar(
            "com.android.tools",
            "common",
            "31.4.2",
            HELLO_SHA1,
            5,
            Repo::Google,
        );
        assert_eq!(plan(&[p], None, &own), vec![None]);
        let net = fake(b"hello");
        let seen = RefCell::new(Vec::new());
        let got = resolve(&[p], None, &own, &net, &|t, n| {
            seen.borrow_mut().push((t, n))
        })
        .unwrap();
        let f = own.join("com.android.tools/common/31.4.2/common-31.4.2.jar");
        assert_eq!(got, vec![f.clone()]);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "hello");
        assert!(!f.with_extension("jar.part").exists());
        assert_eq!(
            *net.urls.borrow(),
            vec![
                "https://dl.google.com/dl/android/maven2/com/android/tools/common/31.4.2/common-31.4.2.jar"
            ]
        );
        assert_eq!(seen.borrow().last().unwrap().0, "Downloading common (1 MB)");
        assert_eq!(
            plan(&[p], None, &own),
            vec![Some(f)],
            "the next start finds it"
        );
    }

    #[test]
    fn a_found_file_that_goes_away_is_resolved_again_at_the_next_start() {
        let (g, own) = (temp("gone"), temp("gone-own"));
        let p = piece("lib", HELLO_SHA1, 5);
        let f = g.join(format!("demo.lib/lib/1.0/{HELLO_SHA1}/lib-1.0.jar"));
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, "hello").unwrap();
        let net = fake(b"hello");
        assert_eq!(
            resolve(&[p], Some(&g), &own, &net, &quiet).unwrap(),
            vec![f.clone()]
        );
        std::fs::remove_file(&f).unwrap(); // Gradle cleaned its cache: the folder stays, the file goes
        let again = resolve(&[p], Some(&g), &own, &net, &quiet).unwrap();
        assert_eq!(again, vec![own.join("demo.lib/lib/1.0/lib-1.0.jar")]);
        assert!(again[0].is_file());
        assert_eq!(net.urls.borrow().len(), 1);
    }

    #[test]
    fn checksum_mismatch_is_rejected_and_names_the_piece() {
        let own = temp("bad");
        let p = piece("lib", HELLO_SHA1, 5);
        let net = fake(b"jello");
        let e = resolve(&[p], None, &own, &net, &quiet).unwrap_err();
        assert!(
            e.contains("lib (1 MB) 1.0: the download did not match its checksum"),
            "{e}"
        );
        let f = own.join("demo.lib/lib/1.0/lib-1.0.jar");
        assert!(
            !f.exists() && !f.with_extension("jar.part").exists(),
            "a bad file is never kept"
        );
        assert_eq!(plan(&[p], None, &own), vec![None]);
    }

    #[test]
    fn offline_names_every_missing_piece_and_keeps_the_found_ones_out_of_it() {
        struct Offline;
        impl Fetch for Offline {
            fn fetch(&self, _: &str, _: &Path, _: &dyn Fn(u64)) -> Result<(), String> {
                Err("Could not resolve host".into())
            }
        }
        let own = temp("offline");
        let here = piece("here", HELLO_SHA1, 5);
        let f = own.join("demo.lib/here/1.0/here-1.0.jar");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, "hello").unwrap();
        let e = resolve(
            &[here, piece("a", HELLO_SHA1, 5), piece("b", HELLO_SHA1, 5)],
            None,
            &own,
            &Offline,
            &quiet,
        )
        .unwrap_err();
        assert!(
            e.contains("  a (1 MB) 1.0: Could not resolve host\n  b (1 MB) 1.0"),
            "{e}"
        );
        assert!(!e.contains("here"), "{e}");
    }

    #[test]
    fn a_partial_download_resumes_and_a_complete_part_is_not_fetched_again() {
        let own = temp("part");
        let p = piece("lib", HELLO_SHA1, 5);
        let part = own.join("demo.lib/lib/1.0/lib-1.0.jar.part");
        std::fs::create_dir_all(part.parent().unwrap()).unwrap();
        std::fs::write(&part, "hello").unwrap();
        let net = fake(b"xxxxx");
        resolve(&[p], None, &own, &net, &quiet).unwrap();
        assert!(net.urls.borrow().is_empty());
    }

    #[test]
    fn the_manifest_names_one_native_runtime_for_this_mac_and_real_checksums() {
        let p = pieces();
        assert_eq!(p.len(), 16);
        assert_eq!(
            p.iter()
                .filter(|p| p.artifact == "layoutlib-runtime")
                .count(),
            1
        );
        assert!(
            PIECES
                .iter()
                .all(|p| p.sha1.len() == 40 && p.sha1.bytes().all(|b| b.is_ascii_hexdigit()))
        );
        let google = PIECES
            .iter()
            .filter(|p| p.repo == Repo::Google)
            .map(|p| p.group)
            .all(|g| g.starts_with("com.android.tools"));
        assert!(google);
    }

    #[test]
    fn android_jar_prefers_35_else_the_newest_platform() {
        let p = temp("sdk");
        for v in ["android-34", "android-36", "android-36.1"] {
            std::fs::create_dir_all(p.join(v)).unwrap();
            std::fs::write(p.join(v).join("android.jar"), "").unwrap();
        }
        assert_eq!(
            newest_android_jar(&p),
            Some(p.join("android-36.1/android.jar"))
        );
        std::fs::create_dir_all(p.join("android-35")).unwrap();
        std::fs::write(p.join("android-35/android.jar"), "").unwrap();
        assert_eq!(
            newest_android_jar(&p),
            Some(p.join("android-35/android.jar"))
        );
    }

    #[test]
    fn a_downloaded_data_jar_is_unpacked_then_deleted_and_never_fetched_again() {
        let (d, own) = (temp("assemble"), temp("assemble-own"));
        let jar = d.join("data.jar");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&jar).unwrap());
            z.start_file("data/icu.dat", zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut z, b"icu").unwrap();
            z.finish().unwrap();
        }
        let bytes: &'static [u8] = std::fs::read(&jar).unwrap().leak();
        let sha: &'static str = sha1_file(&jar).unwrap().leak();
        let data = Piece {
            use_: Use::Unpack("runtime"),
            ..piece("data", sha, bytes.len() as u64)
        };
        let cp = piece("cp", HELLO_SHA1, 5);
        let f = own.join("demo.lib/cp/1.0/cp-1.0.jar");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, "hello").unwrap();
        let net = fake(bytes);
        let base = d.join("base");
        assert_eq!(
            assemble(&base, &[cp, data], None, &own, &net, &quiet).unwrap(),
            vec![f.clone()]
        );
        assert_eq!(
            std::fs::read_to_string(base.join("layoutlib/runtime/data/icu.dat")).unwrap(),
            "icu"
        );
        assert!(
            !own.join("demo.lib/data/1.0/data-1.0.jar").exists(),
            "no second copy on disk"
        );
        assert_eq!(
            assemble(&base, &[cp, data], None, &own, &net, &quiet).unwrap(),
            vec![f]
        );
        assert_eq!(
            net.urls.borrow().len(),
            1,
            "unpacked once: not downloaded again"
        );
    }

    #[test]
    fn a_stalled_download_fails_instead_of_hanging() {
        let args = CURL_ARGS.join(" ");
        assert!(
            args.contains("--speed-limit 1024 --speed-time 30"),
            "{args}"
        );
        assert!(args.contains("--connect-timeout 20"), "{args}");
    }

    #[test]
    fn the_fingerprint_names_every_piece_of_the_renderer() {
        let f = fingerprint();
        assert!(pieces().iter().all(|p| f.contains(p.sha1)), "{f}");
    }

    #[test]
    fn leftover_unpack_folders_of_ended_runs_are_swept() {
        let d = temp("sweep");
        let to = d.join("layoutlib/runtime");
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        // a process that has ended, and one that is still running
        let mut ended = Command::new("true").spawn().unwrap();
        let ended_pid = ended.id();
        ended.wait().unwrap();
        let mut running = Command::new("sleep").arg("30").spawn().unwrap();
        let (gone, kept) = (
            to.with_extension(format!("new-{ended_pid}")),
            to.with_extension(format!("new-{}", running.id())),
        );
        let junk = to.with_extension("new-x");
        for p in [&gone, &kept, &junk] {
            std::fs::create_dir_all(p.join("data")).unwrap();
        }
        let jar = d.join("data.jar");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&jar).unwrap());
            z.start_file("data/a.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut z, b"a").unwrap();
            z.finish().unwrap();
        }
        unpacked(&jar, "s1", &to).unwrap();
        assert!(!gone.exists(), "a killed run's folder");
        assert!(!junk.exists());
        assert!(kept.exists(), "a live run's folder stays");
        let _ = running.kill();
        let _ = running.wait();
    }

    #[test]
    fn layoutlib_data_is_unpacked_once_per_checksum() {
        let d = temp("unpack");
        let jar = d.join("data.jar");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&jar).unwrap());
            z.start_file("data/fonts/a.ttf", zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut z, b"font").unwrap();
            z.finish().unwrap();
        }
        let to = d.join("layoutlib/runtime");
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        unpacked(&jar, "s1", &to).unwrap();
        assert_eq!(
            std::fs::read_to_string(to.join("data/fonts/a.ttf")).unwrap(),
            "font"
        );
        std::fs::write(to.join("data/fonts/a.ttf"), "kept").unwrap();
        unpacked(&jar, "s1", &to).unwrap();
        assert_eq!(
            std::fs::read_to_string(to.join("data/fonts/a.ttf")).unwrap(),
            "kept",
            "same checksum: not again"
        );
        unpacked(&jar, "s2", &to).unwrap();
        assert_eq!(
            std::fs::read_to_string(to.join("data/fonts/a.ttf")).unwrap(),
            "font",
            "another jar: again"
        );
    }

    /// Counts fetches, from any thread.
    struct Counted(std::sync::Mutex<usize>);

    impl Fetch for Counted {
        fn fetch(&self, _: &str, to: &Path, _: &dyn Fn(u64)) -> Result<(), String> {
            *self.0.lock().unwrap() += 1;
            std::fs::write(to, "hello").map_err(|e| e.to_string())
        }
    }

    #[test]
    fn a_download_another_engine_holds_is_waited_for_and_its_file_used() {
        let own = temp("lock-dl");
        let p = piece("lib", HELLO_SHA1, 5);
        let to = own_path(&p, &own);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        let held = lock(&to.with_extension("jar.lock")).unwrap(); // another engine downloading
        let net = Counted(Default::default());
        std::thread::scope(|s| {
            let t = s.spawn(|| download(&p, &own, &net, &|_| {}));
            std::thread::sleep(Duration::from_millis(300));
            assert!(!t.is_finished(), "waits for the other engine");
            std::fs::write(&to, "hello").unwrap(); // its finished file
            drop(held);
            assert_eq!(t.join().unwrap(), Ok(to.clone()));
        });
        assert_eq!(
            *net.0.lock().unwrap(),
            0,
            "never a second curl into the same .part"
        );
    }

    #[test]
    fn an_unpack_another_engine_holds_is_waited_for_not_done_again() {
        let d = temp("lock-unpack");
        let jar = d.join("data.jar");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&jar).unwrap());
            z.start_file("data/a.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut z, b"ours").unwrap();
            z.finish().unwrap();
        }
        let to = d.join("layoutlib/runtime");
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        let held = lock(&to.with_extension("lock")).unwrap(); // another engine unpacking
        std::thread::scope(|s| {
            let t = s.spawn(|| unpacked(&jar, "s1", &to));
            std::thread::sleep(Duration::from_millis(300));
            assert!(!t.is_finished(), "waits for the other engine");
            // its folder, which its worker now uses
            std::fs::create_dir_all(to.join("data")).unwrap();
            std::fs::write(to.join("data/a.txt"), "theirs").unwrap();
            std::fs::write(to.with_extension("sha1"), "s1").unwrap();
            drop(held);
            assert_eq!(t.join().unwrap(), Ok(()));
        });
        assert_eq!(
            std::fs::read_to_string(to.join("data/a.txt")).unwrap(),
            "theirs",
            "not removed and unpacked again under its worker"
        );
    }
}
