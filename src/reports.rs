//! Android lint and detekt results, read from the XML reports a Gradle run writes
//! (`build/reports/lint-results-*.xml`, `build/reports/detekt/*.xml`), for the Problems list.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One finding: file, line, column (1-based), error, message.
pub type Finding = (PathBuf, u32, u32, bool, String);

fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#10;", " ")
        .replace("&amp;", "&")
}

/// The value of `key="…"` in a tag.
fn attr(tag: &str, key: &str) -> Option<String> {
    let pat = format!("{key}=\"");
    let (i, _) = tag
        .match_indices(&pat)
        .find(|(i, _)| tag[..*i].ends_with(char::is_whitespace))?;
    let at = i + pat.len();
    let end = tag[at..].find('"')?;
    Some(unescape(&tag[at..at + end]))
}

/// The tags named `name` with their text up to the next tag of that name (attributes may start
/// on the next line, as lint writes them).
fn tags<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let open = format!("<{name}");
    let mut starts: Vec<usize> = xml
        .match_indices(&open)
        .map(|(i, _)| i)
        .filter(|i| xml[i + open.len()..].starts_with(char::is_whitespace))
        .collect();
    starts.push(xml.len());
    starts.windows(2).map(|w| &xml[w[0]..w[1]]).collect()
}

/// Android lint's XML report: `<issue id severity message><location file line column/>`.
pub fn lint_xml(xml: &str, base: &Path) -> Vec<Finding> {
    tags(xml, "issue")
        .into_iter()
        .filter_map(|issue| {
            let head = &issue[..issue.find('>')?];
            let loc = *tags(issue, "location").first()?;
            let file = attr(loc, "file")?;
            let num = |k| attr(loc, k).and_then(|v| v.parse().ok()).unwrap_or(1);
            let id = attr(head, "id").unwrap_or_default();
            let severity = attr(head, "severity").unwrap_or_default();
            Some((
                base.join(file),
                num("line"),
                num("column"),
                matches!(severity.as_str(), "Error" | "Fatal"),
                format!("[lint {id}] {}", attr(head, "message").unwrap_or_default()),
            ))
        })
        .collect()
}

/// detekt's checkstyle XML: `<file name><error line column severity message source/></file>`.
pub fn checkstyle(xml: &str, base: &Path) -> Vec<Finding> {
    let mut out = Vec::new();
    for file in tags(xml, "file") {
        let Some(name) = attr(&file[..file.find('>').unwrap_or(file.len())], "name") else {
            continue;
        };
        for e in tags(file, "error") {
            let num = |k| attr(e, k).and_then(|v| v.parse().ok()).unwrap_or(1);
            let rule = attr(e, "source").unwrap_or_default();
            let rule = rule.rsplit('.').next().unwrap_or(&rule).to_string();
            out.push((
                base.join(&name),
                num("line"),
                num("column"),
                attr(e, "severity").is_some_and(|s| s == "error"),
                format!("[detekt {rule}] {}", attr(e, "message").unwrap_or_default()),
            ));
        }
    }
    out
}

/// The findings of the reports in these module folders written at or after `since`.
pub fn collect(modules: &[PathBuf], root: &Path, since: SystemTime) -> Vec<Finding> {
    let mut out = Vec::new();
    for m in modules {
        for dir in [m.join("build/reports"), m.join("build/reports/detekt")] {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let fresh = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t >= since);
                if !fresh || !name.ends_with(".xml") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&p) else {
                    continue;
                };
                if name.starts_with("lint-results") {
                    out.extend(lint_xml(&text, root));
                } else if dir.ends_with("detekt") || text.contains("<checkstyle") {
                    out.extend(checkstyle(&text, root));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_lint_and_detekt_reports() {
        let lint = r#"<issues format="6" by="lint 8.5.0">
    <issue
        id="HardcodedText"
        severity="Warning"
        message="Hardcoded string &quot;Hi&quot;, should use `@string` resource"
        category="Internationalization">
        <location
            file="/p/app/src/main/res/layout/main.xml"
            line="12"
            column="9"/>
    </issue>
    <issue id="MissingPermission" severity="Error" message="Call requires permission">
        <location file="app/src/main/java/A.kt" line="3" column="5"/>
    </issue>
</issues>"#;
        let f = lint_xml(lint, Path::new("/p"));
        assert_eq!(
            f[0],
            (
                PathBuf::from("/p/app/src/main/res/layout/main.xml"),
                12,
                9,
                false,
                "[lint HardcodedText] Hardcoded string \"Hi\", should use `@string` resource"
                    .into()
            )
        );
        assert_eq!(f[1].0, PathBuf::from("/p/app/src/main/java/A.kt"));
        assert!(f[1].3, "Error is an error");
        let detekt = r#"<?xml version="1.0" encoding="UTF-8"?>
<checkstyle version="4.3">
<file name="/p/app/src/main/java/A.kt">
	<error line="4" column="1" severity="warning" message="The function f is too long." source="detekt.LongMethod" />
	<error line="9" column="3" severity="error" message="Magic number." source="detekt.MagicNumber" />
</file>
</checkstyle>"#;
        let d = checkstyle(detekt, Path::new("/p"));
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].4, "[detekt LongMethod] The function f is too long.");
        assert_eq!((d[1].1, d[1].2, d[1].3), (9, 3, true));
    }
}
