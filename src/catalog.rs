//! Gradle version catalogs: `libs.androidx.core.ktx` in a build script → its line in
//! `gradle/libs.versions.toml`.

/// The dotted accessor around `at`, e.g. `libs.plugins.kotlin.android`.
pub fn accessor_at(text: &str, at: usize) -> Option<String> {
    let ok = |c: char| c.is_alphanumeric() || c == '_' || c == '.';
    let start = text[..at]
        .char_indices()
        .rev()
        .take_while(|(_, c)| ok(*c))
        .last()
        .map_or(at, |(i, _)| i);
    let end = text[at..]
        .char_indices()
        .find(|(_, c)| !ok(*c))
        .map_or(text.len(), |(i, _)| at + i);
    let s = text[start..end].trim_matches('.');
    (s.starts_with("libs.") && s.len() > 5).then(|| s.to_string())
}

/// Byte offset of the catalog entry that `accessor` (`libs.x.y`, `libs.plugins.x`,
/// `libs.versions.x`, `libs.bundles.x`) points to.
pub fn find(toml: &str, accessor: &str) -> Option<usize> {
    let rest = accessor.strip_prefix("libs.")?;
    let (section, key) = match rest.split_once('.') {
        Some(("plugins", k)) => ("plugins", k),
        Some(("versions", k)) => ("versions", k),
        Some(("bundles", k)) => ("bundles", k),
        _ => ("libraries", rest),
    };
    let norm = |k: &str| k.trim().trim_matches('"').replace(['-', '_'], ".");
    let want = norm(key);
    let mut current = "";
    let mut at = 0;
    for line in toml.split_inclusive('\n') {
        let t = line.trim();
        if let Some(name) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            current = name.trim();
        } else if current == section
            && let Some((k, _)) = t.split_once('=')
            && norm(k) == want
        {
            return Some(at + line.len() - line.trim_start().len());
        }
        at += line.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOML: &str = "[versions]\ncompose = \"1.7\"\n\n[libraries]\nandroidx-core-ktx = { module = \"androidx.core:core-ktx\", version = \"1.13\" }\ncompose_ui = { group = \"androidx.compose.ui\", name = \"ui\" }\n\n[plugins]\nkotlin-android = { id = \"org.jetbrains.kotlin.android\" }\n\n[bundles]\ncompose = [\"compose-ui\"]\n";

    #[test]
    fn finds_libraries_plugins_versions_bundles() {
        let line = |a: &str| find(TOML, a).map(|o| TOML[o..].lines().next().unwrap().to_string());
        assert_eq!(
            line("libs.androidx.core.ktx").unwrap(),
            "androidx-core-ktx = { module = \"androidx.core:core-ktx\", version = \"1.13\" }"
        );
        assert!(line("libs.compose.ui").unwrap().starts_with("compose_ui"));
        assert!(
            line("libs.plugins.kotlin.android")
                .unwrap()
                .starts_with("kotlin-android")
        );
        assert!(
            line("libs.versions.compose")
                .unwrap()
                .starts_with("compose = \"1.7\"")
        );
        assert!(
            line("libs.bundles.compose")
                .unwrap()
                .starts_with("compose = [")
        );
        assert_eq!(find(TOML, "libs.missing"), None);
    }

    #[test]
    fn accessor_under_the_cursor() {
        let t = "implementation(libs.androidx.core.ktx)\nalias(libs.plugins.kotlin.android)";
        assert_eq!(
            accessor_at(t, t.find("core").unwrap()).as_deref(),
            Some("libs.androidx.core.ktx")
        );
        assert_eq!(
            accessor_at(t, t.find("kotlin").unwrap()).as_deref(),
            Some("libs.plugins.kotlin.android")
        );
        assert_eq!(accessor_at(t, 2), None);
    }
}
