//! Run on a device: which module is the app, and starting the installed app or one Compose
//! preview on a device with `adb` (after Gradle's `installDebug`).

use crate::project::{Module, Project};
use std::path::Path;
use std::process::Command;

/// The Android Studio preview host, from `androidx.compose.ui:ui-tooling` (debug builds).
pub const PREVIEW_ACTIVITY: &str = "androidx.compose.ui.tooling.PreviewActivity";

/// The application modules (`com.android.application`), `:app` first, with their application id.
pub fn app_modules(p: &Project) -> Vec<(Module, Option<String>)> {
    let mut apps: Vec<(Module, Option<String>)> = p
        .modules
        .iter()
        .filter_map(|m| {
            let files: Vec<std::path::PathBuf> = ["build.gradle.kts", "build.gradle"]
                .iter()
                .map(|f| m.dir.join(f))
                .filter(|f| f.is_file())
                .collect();
            let text = files
                .iter()
                .filter_map(|f| std::fs::read_to_string(f).ok())
                .collect::<String>();
            let is_app = text.contains("com.android.application")
                || text.contains("android.application")
                || text.contains("androidApplication");
            is_app.then(|| {
                (
                    m.clone(),
                    crate::logcat::application_ids(&files).into_iter().next(),
                )
            })
        })
        .collect();
    apps.sort_by_key(|(m, _)| m.name != ":app");
    apps
}

/// What to start after the install.
#[derive(Clone, Debug, PartialEq)]
pub enum Launch {
    /// The app's launcher activity.
    App,
    /// One `@Preview`, as `com.app.ui.ScreenKt.ScreenPreview`.
    Preview(String),
}

/// The class and method `PreviewActivity` calls for a preview function: a top-level function
/// lives in `<package>.<File>Kt`, a member in its class.
pub fn preview_name(package: &str, file: &Path, container: Option<&str>, function: &str) -> String {
    let class = container.map_or_else(
        || {
            let stem = file
                .file_stem()
                .map_or(String::new(), |s| s.to_string_lossy().into_owned());
            format!("{stem}Kt")
        },
        String::from,
    );
    if package.is_empty() {
        format!("{class}.{function}")
    } else {
        format!("{package}.{class}.{function}")
    }
}

fn shell(adb: &Path, serial: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(adb)
        .args(["-s", serial, "shell"])
        .args(args)
        .output()
        .map_err(|e| format!("adb: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
}

/// The installed package for `app_id`: itself, or with a debug suffix (`com.app.debug`).
fn installed(adb: &Path, serial: &str, app_id: &str) -> Result<String, String> {
    let list = shell(adb, serial, &["pm", "list", "packages", app_id])?;
    let mut pkgs: Vec<&str> = list
        .lines()
        .filter_map(|l| l.trim().strip_prefix("package:"))
        .filter(|p| p.starts_with(app_id))
        .collect();
    pkgs.sort_by_key(|p| p.len());
    pkgs.first()
        .map(|p| p.to_string())
        .ok_or_else(|| format!("{app_id} is not installed on {serial}"))
}

/// Starts the app or the preview on `serial`. Returns what was started, or why it failed.
pub fn launch(adb: &Path, serial: &str, app_id: &str, what: &Launch) -> Result<String, String> {
    let pkg = installed(adb, serial, app_id)?;
    let component = match what {
        Launch::App => {
            let out = shell(
                adb,
                serial,
                &[
                    "cmd",
                    "package",
                    "resolve-activity",
                    "--brief",
                    "-a",
                    "android.intent.action.MAIN",
                    "-c",
                    "android.intent.category.LAUNCHER",
                    &pkg,
                ],
            )?;
            out.lines()
                .rev()
                .map(str::trim)
                .find(|l| l.contains('/'))
                .map(String::from)
                .ok_or_else(|| format!("{pkg} has no launcher activity"))?
        }
        Launch::Preview(_) => format!("{pkg}/{PREVIEW_ACTIVITY}"),
    };
    let mut args = vec![
        "am",
        "start",
        "-n",
        &component,
        "-a",
        "android.intent.action.MAIN",
    ];
    if let Launch::Preview(name) = what {
        args.extend(["--es", "composable", name.as_str()]);
    }
    let out = shell(adb, serial, &args)?;
    if out.contains("Error") || out.contains("does not exist") {
        return Err(match what {
            Launch::Preview(_) if out.contains("does not exist") => format!(
                "{pkg} has no {PREVIEW_ACTIVITY}: add debugImplementation(\"androidx.compose.ui:ui-tooling\") to the app module"
            ),
            _ => out.trim().to_string(),
        });
    }
    Ok(match what {
        Launch::App => format!("Started {component} on {serial}"),
        Launch::Preview(name) => format!("Showing preview {name} on {serial}"),
    })
}

/// The Android emulator: `DROID_LITE_EMULATOR`, else next to the SDK's `platform-tools/adb`.
pub fn emulator(adb: Option<&Path>) -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("DROID_LITE_EMULATOR") {
        return Some(p.into()).filter(|p: &std::path::PathBuf| p.is_file());
    }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    adb.and_then(|a| Some(a.parent()?.parent()?.to_path_buf()))
        .into_iter()
        .chain(
            ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
                .iter()
                .filter_map(|v| std::env::var_os(v).map(Into::into)),
        )
        .chain(home.map(|h| h.join("Library/Android/sdk")))
        .map(|sdk| sdk.join("emulator/emulator"))
        .find(|p| p.is_file())
}

/// The virtual devices (`emulator -list-avds`).
pub fn avds(emulator: &Path) -> Vec<String> {
    Command::new(emulator)
        .arg("-list-avds")
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with("INFO") && !l.contains(' '))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Starts a virtual device; it keeps running on its own.
pub fn start_avd(emulator: &Path, avd: &str) -> std::io::Result<()> {
    Command::new(emulator)
        .args(["-avd", avd])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Opens `uri` in the app on `serial` (`am start -a VIEW -d`), like a tapped link.
pub fn open_link(adb: &Path, serial: &str, app_id: &str, uri: &str) -> Result<String, String> {
    let pkg = installed(adb, serial, app_id)?;
    // the device's shell reads the command line: quote the link (it may hold `&` or `?`)
    let quoted = format!("'{}'", uri.replace('\'', "'\\''"));
    let out = shell(
        adb,
        serial,
        &[
            "am",
            "start",
            "-W",
            "-a",
            "android.intent.action.VIEW",
            "-d",
            &quoted,
            &pkg,
        ],
    )?;
    if out.contains("Error") {
        return Err(out
            .lines()
            .find(|l| l.contains("Error"))
            .unwrap_or(&out)
            .trim()
            .to_string());
    }
    Ok(format!("Opened {uri} on {serial}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_names_follow_the_jvm_class() {
        let f = Path::new("/p/app/src/main/kotlin/com/demo/ui/UserCard.kt");
        assert_eq!(
            preview_name("com.demo.ui", f, None, "UserCardPreview"),
            "com.demo.ui.UserCardKt.UserCardPreview"
        );
        assert_eq!(
            preview_name("com.demo.ui", f, Some("Previews"), "Dark"),
            "com.demo.ui.Previews.Dark"
        );
        assert_eq!(preview_name("", f, None, "P"), "UserCardKt.P");
    }
}
