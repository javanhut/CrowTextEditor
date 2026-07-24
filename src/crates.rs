//! Cargo.toml version badges: the locked version (Cargo.lock) and the latest
//! on crates.io, shown inline next to each dependency.
//!
//! ponytail: shells out to `curl` against the sparse index instead of adding
//! an HTTP crate; an in-process client when curl-less machines itch.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

/// One streamed result: (crate name, locked version, latest version).
pub type Info = (String, Option<String>, Option<String>);

/// Fetch locked + latest for every dependency of `manifest`, streaming each
/// crate's result over `tx` as it arrives.
pub fn fetch(manifest: PathBuf, toml_text: String, tx: Sender<Info>) {
    std::thread::spawn(move || {
        let locked = std::fs::read_to_string(manifest.with_file_name("Cargo.lock"))
            .map(|s| locked_versions(&s))
            .unwrap_or_default();
        for name in dep_names(&toml_text) {
            let entry = (name.clone(), locked.get(&name).cloned(), latest(&name));
            if tx.send(entry).is_err() {
                return; // the editor is gone
            }
        }
    });
}

/// Dependency names in the `[…dependencies]` sections of a Cargo.toml.
pub fn dep_names(toml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_deps = false;
    for line in toml.lines() {
        let line = line.trim();
        if let Some(section) = line.strip_prefix('[') {
            in_deps = section.trim_end_matches(']').ends_with("dependencies");
            continue;
        }
        if !in_deps || line.starts_with('#') {
            continue;
        }
        if let Some((key, _)) = line.split_once('=') {
            // `serde.workspace = true` names the crate before the dot.
            let name = key.trim().trim_matches('"');
            let name = name.split('.').next().unwrap_or(name);
            if !name.is_empty() {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// Locked versions from a Cargo.lock: name -> version. In each `[[package]]`
/// block the `version` line follows the `name` line.
pub fn locked_versions(lock: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let mut name: Option<String> = None;
    for line in lock.lines() {
        let line = line.trim();
        if line.starts_with("[[") {
            name = None;
        } else if let Some(v) = line.strip_prefix("name = ") {
            name = Some(v.trim_matches('"').to_string());
        } else if let Some(v) = line.strip_prefix("version = ") {
            if let Some(n) = name.take() {
                out.entry(n)
                    .or_insert_with(|| v.trim_matches('"').to_string());
            }
        }
    }
    out
}

/// Loose semver sort key: `major.minor.patch`, ignoring pre-release and
/// build-metadata tails.
pub fn semver_key(v: &str) -> (u64, u64, u64) {
    let core = v.split(['-', '+']).next().unwrap_or(v);
    let mut nums = core.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    (
        nums.next().unwrap_or(0),
        nums.next().unwrap_or(0),
        nums.next().unwrap_or(0),
    )
}

/// The sparse-index path for a crate name.
fn index_path(name: &str) -> String {
    let n = name.to_lowercase();
    match n.len() {
        1 => format!("1/{n}"),
        2 => format!("2/{n}"),
        3 => format!("3/{}/{n}", &n[..1]),
        _ => format!("{}/{}/{n}", &n[..2], &n[2..4]),
    }
}

/// The newest non-yanked version on crates.io, preferring stable releases
/// over pre-releases. None on any network or parse failure.
fn latest(name: &str) -> Option<String> {
    let url = format!("https://index.crates.io/{}", index_path(name));
    let out = std::process::Command::new("curl")
        .args(["-sf", "--max-time", "10", &url])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let body = String::from_utf8_lossy(&out.stdout);
    let mut stable: Option<String> = None;
    let mut pre: Option<String> = None;
    for line in body.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["yanked"].as_bool().unwrap_or(false) {
            continue;
        }
        let Some(vers) = v["vers"].as_str() else {
            continue;
        };
        let slot = if vers.contains('-') {
            &mut pre
        } else {
            &mut stable
        };
        if slot
            .as_deref()
            .is_none_or(|best| semver_key(vers) > semver_key(best))
        {
            *slot = Some(vers.to_string());
        }
    }
    stable.or(pre)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dep_names_cover_the_section_variants() {
        let names = dep_names(
            r#"
[package]
name = "crow"

[dependencies]
ropey = "1.6"
crossterm = { version = "0.27", features = ["x"] }
serde.workspace = true
# regex = "1"

[dev-dependencies]
tempfile = "3"

[profile.release]
lto = true
"#,
        );
        assert_eq!(names, ["ropey", "crossterm", "serde", "tempfile"]);
    }

    #[test]
    fn lock_parse_takes_the_first_version_per_name() {
        let locked = locked_versions(
            r#"
[[package]]
name = "ropey"
version = "1.6.1"
dependencies = [
 "smallvec",
]

[[package]]
name = "smallvec"
version = "1.13.2"
"#,
        );
        assert_eq!(locked.get("ropey").map(String::as_str), Some("1.6.1"));
        assert_eq!(locked.get("smallvec").map(String::as_str), Some("1.13.2"));
    }

    #[test]
    fn semver_keys_order_and_ignore_tails() {
        assert!(semver_key("0.29.0") > semver_key("0.27.0"));
        assert!(semver_key("1.0.0") > semver_key("0.99.99"));
        assert_eq!(semver_key("1.2.3-beta.1"), semver_key("1.2.3+build5"));
    }

    #[test]
    fn index_paths_follow_the_sparse_layout() {
        assert_eq!(index_path("a"), "1/a");
        assert_eq!(index_path("io"), "2/io");
        assert_eq!(index_path("fnv"), "3/f/fnv");
        assert_eq!(index_path("ropey"), "ro/pe/ropey");
    }
}
