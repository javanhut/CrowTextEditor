//! Dependency version badges for package manifests: the version you have and
//! the latest the registry knows, inline next to each dependency, plus the
//! `dep_upgrade` rewrite. Rust (Cargo.toml), JS/TS (package.json), Python
//! (pyproject.toml, requirements.txt), Go (go.mod). Zig pins URLs and Odin
//! and C have no central registry — nothing to ask there.
//!
//! ponytail: shells out to `curl` against each registry instead of adding an
//! HTTP crate; an in-process client when curl-less machines itch.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    Cargo,
    Npm,
    Pypi,
    Requirements,
    Go,
}

/// One streamed result: (manifest kind, name, current version, latest).
pub type Info = (Kind, String, Option<String>, Option<String>);

/// The ecosystem a manifest file belongs to, by file name.
pub fn manifest_kind(file_name: &str) -> Option<Kind> {
    match file_name {
        "Cargo.toml" => Some(Kind::Cargo),
        "package.json" => Some(Kind::Npm),
        "pyproject.toml" => Some(Kind::Pypi),
        "requirements.txt" => Some(Kind::Requirements),
        "go.mod" => Some(Kind::Go),
        _ => None,
    }
}

/// Fetch current + latest for every dependency of `manifest`, streaming each
/// result over `tx` as it arrives.
pub fn fetch(kind: Kind, manifest: PathBuf, text: String, tx: Sender<Info>) {
    std::thread::spawn(move || {
        let locked = locked(kind, &manifest);
        for (name, current) in parse(kind, &text) {
            let current = locked.get(&name).cloned().or(current);
            let entry = (kind, name.clone(), current, latest(kind, &name));
            if tx.send(entry).is_err() {
                return; // the editor is gone
            }
        }
    });
}

/// (name, version from the manifest itself) for every dependency.
pub fn parse(kind: Kind, text: &str) -> Vec<(String, Option<String>)> {
    match kind {
        Kind::Cargo => cargo_deps(text),
        Kind::Npm => npm_deps(text),
        Kind::Pypi => pyproject_deps(text),
        Kind::Requirements => requirements_deps(text),
        Kind::Go => go_deps(text),
    }
}

/// Exact installed versions from the ecosystem's lockfile, when one sits
/// next to the manifest and has a format worth parsing.
fn locked(kind: Kind, manifest: &Path) -> HashMap<String, String> {
    let read = |name: &str| std::fs::read_to_string(manifest.with_file_name(name));
    match kind {
        Kind::Cargo => read("Cargo.lock")
            .map(|s| cargo_locked(&s))
            .unwrap_or_default(),
        Kind::Npm => read("package-lock.json")
            .map(|s| npm_locked(&s))
            .unwrap_or_default(),
        // Python lockfiles are a zoo (uv, poetry, pipenv…) and go.mod already
        // carries its versions; the manifest's own spec is the fallback.
        _ => HashMap::new(),
    }
}

/// The dependency named on a rendered manifest line, for the badge lookup
/// and `dep_upgrade`.
pub fn line_dep(kind: Kind, line: &str) -> Option<String> {
    let line = line.trim();
    match kind {
        Kind::Cargo => {
            let key = line.split_once('=')?.0.trim().trim_matches('"');
            let key = key.split('.').next().unwrap_or(key);
            (!key.is_empty()).then(|| key.to_string())
        }
        Kind::Npm => {
            let (items, _) = quoted_items(line);
            items.first().map(|s| s.to_string())
        }
        Kind::Pypi => {
            // An array item first ("fastapi>=0.1"), else a poetry key.
            let (items, _) = quoted_items(line);
            if let Some(name) = items.first().and_then(|i| req_name(i)) {
                return Some(name);
            }
            let key = line.split_once('=')?.0.trim().trim_matches('"');
            (!key.is_empty()).then(|| normalize_py(key))
        }
        Kind::Requirements => req_name(line),
        Kind::Go => {
            let l = line.split("//").next().unwrap_or(line).trim();
            let l = l.strip_prefix("require").unwrap_or(l).trim();
            let mut tokens = l.split_whitespace();
            let (module, version) = (tokens.next()?, tokens.next()?);
            (module.contains('.') && version.starts_with('v')).then(|| module.to_string())
        }
    }
}

/// The char span of the version number to rewrite on a manifest line.
pub fn version_span(kind: Kind, line: &str) -> Option<(usize, usize)> {
    let chars: Vec<char> = line.chars().collect();
    let start = match kind {
        // The `vN.N.N` token after the module path; keep the `v`.
        Kind::Go => {
            let mut start = None;
            for i in 0..chars.len() {
                let word_start =
                    !chars[i].is_whitespace() && (i == 0 || chars[i - 1].is_whitespace());
                if word_start
                    && chars[i] == 'v'
                    && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit())
                {
                    start = Some(i + 1);
                }
            }
            start?
        }
        // Digits only after the `=`/`:` (or the name), so a digit in the
        // name itself — base64, tree-sitter-c-sharp — is never touched.
        _ => {
            let off = chars
                .iter()
                .position(|&c| c == '=' || c == ':')
                .map(|i| i + 1)
                .unwrap_or_else(|| {
                    chars
                        .iter()
                        .take_while(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
                        .count()
                });
            off + chars[off..].iter().position(|c| c.is_ascii_digit())?
        }
    };
    let len = chars[start..]
        .iter()
        .take_while(|c| c.is_ascii_digit() || **c == '.')
        .count();
    (len > 0).then_some((start, start + len))
}

// ---- manifest parsers -------------------------------------------------------

/// Dependency names in the `[…dependencies]` sections of a Cargo.toml.
fn cargo_deps(toml: &str) -> Vec<(String, Option<String>)> {
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
                out.push((name.to_string(), None)); // Cargo.lock has the truth
            }
        }
    }
    out
}

/// Locked versions from a Cargo.lock: in each `[[package]]` block the
/// `version` line follows the `name` line.
fn cargo_locked(lock: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
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

/// The dependency sections of a package.json.
fn npm_deps(text: &str) -> Vec<(String, Option<String>)> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for section in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        if let Some(map) = v[section].as_object() {
            for (name, spec) in map {
                out.push((name.clone(), spec.as_str().and_then(spec_version)));
            }
        }
    }
    out
}

/// Top-level installed versions from a package-lock.json.
fn npm_locked(text: &str) -> HashMap<String, String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    if let Some(pkgs) = v["packages"].as_object() {
        for (path, info) in pkgs {
            if let Some(name) = path.strip_prefix("node_modules/") {
                if !name.contains("node_modules") {
                    if let Some(ver) = info["version"].as_str() {
                        out.insert(name.to_string(), ver.to_string());
                    }
                }
            }
        }
    }
    out
}

/// PEP 621 `dependencies` arrays, dependency groups, and poetry tables in a
/// pyproject.toml.
fn pyproject_deps(text: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let mut section = String::new();
    let mut in_array = false;
    let push = |item: &str, out: &mut Vec<(String, Option<String>)>| {
        if let Some(name) = req_name(item) {
            out.push((name, spec_version(item)));
        }
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('#') {
            continue;
        }
        if in_array {
            let (items, closed) = quoted_items(line);
            for item in items {
                push(item, &mut out);
            }
            in_array = !closed;
            continue;
        }
        if line.starts_with('[') {
            section = line.trim_matches(['[', ']']).to_string();
            continue;
        }
        if section.starts_with("tool.poetry.") && section.ends_with("dependencies") {
            if let Some((key, spec)) = line.split_once('=') {
                let name = key.trim().trim_matches('"');
                if !name.is_empty() && name != "python" {
                    out.push((normalize_py(name), spec_version(spec)));
                }
            }
            continue;
        }
        let dep_array = (section == "project" && line.starts_with("dependencies"))
            || ((section == "project.optional-dependencies" || section == "dependency-groups")
                && line.contains('='));
        if dep_array {
            if let Some(open) = line.find('[') {
                let (items, closed) = quoted_items(&line[open + 1..]);
                for item in items {
                    push(item, &mut out);
                }
                in_array = !closed;
            }
        }
    }
    out
}

/// One requirement per line: `fastapi==0.100`, extras and markers ignored.
fn requirements_deps(text: &str) -> Vec<(String, Option<String>)> {
    text.lines()
        .filter_map(|raw| {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() || line.starts_with('-') {
                return None;
            }
            Some((req_name(line)?, spec_version(line)))
        })
        .collect()
}

/// `require` lines and blocks of a go.mod; the version is right there.
fn go_deps(text: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let mut in_block = false;
    for raw in text.lines() {
        let line = raw.split("//").next().unwrap_or("").trim();
        if line.starts_with("require (") {
            in_block = true;
            continue;
        }
        if in_block && line == ")" {
            in_block = false;
            continue;
        }
        let entry = match line.strip_prefix("require ") {
            Some(rest) => rest,
            None if in_block => line,
            None => continue,
        };
        let mut tokens = entry.split_whitespace();
        if let (Some(module), Some(version)) = (tokens.next(), tokens.next()) {
            if module.contains('.') && version.starts_with('v') {
                out.push((
                    module.to_string(),
                    Some(version.trim_start_matches('v').to_string()),
                ));
            }
        }
    }
    out
}

// ---- registries ---------------------------------------------------------------

/// The newest version the ecosystem's registry knows for `name`.
fn latest(kind: Kind, name: &str) -> Option<String> {
    match kind {
        Kind::Cargo => {
            let url = format!("https://index.crates.io/{}", crates_index_path(name));
            let body = curl(&url, &[])?;
            best_version(body.lines().filter_map(|l| {
                let v = serde_json::from_str::<serde_json::Value>(l).ok()?;
                if v["yanked"].as_bool().unwrap_or(false) {
                    return None;
                }
                v["vers"].as_str().map(str::to_string)
            }))
        }
        Kind::Npm => {
            let url = format!(
                "https://registry.npmjs.org/{}/latest",
                name.replace('/', "%2F")
            );
            let v: serde_json::Value = serde_json::from_str(&curl(&url, &[])?).ok()?;
            v["version"].as_str().map(str::to_string)
        }
        Kind::Pypi | Kind::Requirements => {
            let url = format!("https://pypi.org/simple/{}/", normalize_py(name));
            let body = curl(&url, &["Accept: application/vnd.pypi.simple.v1+json"])?;
            let v: serde_json::Value = serde_json::from_str(&body).ok()?;
            best_version(
                v["versions"]
                    .as_array()?
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_string)),
            )
        }
        Kind::Go => {
            let url = format!("https://proxy.golang.org/{}/@latest", go_escape(name));
            let v: serde_json::Value = serde_json::from_str(&curl(&url, &[])?).ok()?;
            v["Version"]
                .as_str()
                .map(|s| s.trim_start_matches('v').to_string())
        }
    }
}

fn curl(url: &str, headers: &[&str]) -> Option<String> {
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-sf", "--max-time", "10"]);
    for h in headers {
        cmd.args(["-H", h]);
    }
    let out = cmd.arg(url).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The highest version, preferring plain `N.N.N` releases; pre-releases
/// (anything with letters) only when nothing else exists.
fn best_version(versions: impl Iterator<Item = String>) -> Option<String> {
    let mut stable: Option<String> = None;
    let mut pre: Option<String> = None;
    for v in versions {
        let plain = v.chars().all(|c| c.is_ascii_digit() || c == '.');
        let slot = if plain { &mut stable } else { &mut pre };
        if slot
            .as_deref()
            .is_none_or(|best| semver_key(&v) > semver_key(best))
        {
            *slot = Some(v);
        }
    }
    stable.or(pre)
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

/// The crates.io sparse-index path for a crate name.
fn crates_index_path(name: &str) -> String {
    let n = name.to_lowercase();
    match n.len() {
        1 => format!("1/{n}"),
        2 => format!("2/{n}"),
        3 => format!("3/{}/{n}", &n[..1]),
        _ => format!("{}/{}/{n}", &n[..2], &n[2..4]),
    }
}

/// Module-proxy escaping: an uppercase letter becomes `!` + lowercase.
fn go_escape(module: &str) -> String {
    let mut out = String::with_capacity(module.len());
    for c in module.chars() {
        if c.is_ascii_uppercase() {
            out.push('!');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

// ---- little parsers -----------------------------------------------------------

/// The quoted strings in a line, plus whether an unquoted `]` closed an array.
fn quoted_items(line: &str) -> (Vec<&str>, bool) {
    let mut items = Vec::new();
    let mut closed = false;
    let mut rest = line;
    loop {
        match rest.find('"') {
            None => {
                closed |= rest.contains(']');
                break;
            }
            Some(i) => {
                closed |= rest[..i].contains(']');
                let Some(j) = rest[i + 1..].find('"') else {
                    break;
                };
                items.push(&rest[i + 1..i + 1 + j]);
                rest = &rest[i + 2 + j..];
            }
        }
    }
    (items, closed)
}

/// The package name at the front of a Python requirement, normalized.
fn req_name(req: &str) -> Option<String> {
    let name: String = req
        .chars()
        .take_while(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    (!name.is_empty()).then(|| normalize_py(&name))
}

/// PEP 503 normalization: lowercase, `-`/`_`/`.` runs become one `-`.
fn normalize_py(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !out.ends_with('-') {
                out.push('-');
            }
        } else {
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

/// The concrete version inside a range spec: `^1.2.3` / `>=0.100` -> digits.
fn spec_version(spec: &str) -> Option<String> {
    let start = spec.find(|c: char| c.is_ascii_digit())?;
    let v: String = spec[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    (!v.is_empty()).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_deps_cover_the_section_variants() {
        let names: Vec<String> = cargo_deps(
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
        )
        .into_iter()
        .map(|(n, _)| n)
        .collect();
        assert_eq!(names, ["ropey", "crossterm", "serde", "tempfile"]);
    }

    #[test]
    fn cargo_lock_takes_the_first_version_per_name() {
        let locked = cargo_locked(
            "[[package]]\nname = \"ropey\"\nversion = \"1.6.1\"\ndependencies = [\n \"smallvec\",\n]\n\n[[package]]\nname = \"smallvec\"\nversion = \"1.13.2\"\n",
        );
        assert_eq!(locked.get("ropey").map(String::as_str), Some("1.6.1"));
        assert_eq!(locked.get("smallvec").map(String::as_str), Some("1.13.2"));
    }

    #[test]
    fn npm_deps_and_lock_parse() {
        let deps = npm_deps(
            r#"{"dependencies": {"react": "^18.2.0"}, "devDependencies": {"vite": "~5.0"}}"#,
        );
        assert_eq!(
            deps,
            [
                ("react".to_string(), Some("18.2.0".to_string())),
                ("vite".to_string(), Some("5.0".to_string())),
            ]
        );
        let locked = npm_locked(
            r#"{"packages": {"": {}, "node_modules/react": {"version": "18.3.1"},
                "node_modules/react/node_modules/x": {"version": "1.0.0"}}}"#,
        );
        assert_eq!(locked.get("react").map(String::as_str), Some("18.3.1"));
        assert!(!locked.contains_key("react/node_modules/x"));
    }

    #[test]
    fn pyproject_deps_cover_pep621_and_poetry() {
        let deps = pyproject_deps(
            r#"
[project]
dependencies = [
    "fastapi>=0.100",
    "uvicorn[standard]>=0.23",
]

[project.optional-dependencies]
dev = ["pytest>=8"]

[tool.poetry.dependencies]
python = "^3.11"
Django = "^5.0"
"#,
        );
        let names: Vec<&str> = deps.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["fastapi", "uvicorn", "pytest", "django"]);
        assert_eq!(deps[0].1.as_deref(), Some("0.100"));
    }

    #[test]
    fn requirements_and_go_parse() {
        let reqs =
            requirements_deps("fastapi==0.100.0\n# comment\n-r other.txt\nFlask_Login>=0.6\n");
        assert_eq!(
            reqs,
            [
                ("fastapi".to_string(), Some("0.100.0".to_string())),
                ("flask-login".to_string(), Some("0.6".to_string())),
            ]
        );
        let gos = go_deps(
            "module example.com/x\n\nrequire golang.org/x/tools v0.1.0\n\nrequire (\n\tgithub.com/go-chi/chi/v5 v5.0.12 // indirect\n)\n",
        );
        assert_eq!(
            gos,
            [
                ("golang.org/x/tools".to_string(), Some("0.1.0".to_string())),
                (
                    "github.com/go-chi/chi/v5".to_string(),
                    Some("5.0.12".to_string())
                ),
            ]
        );
    }

    #[test]
    fn line_dep_reads_each_manifest_style() {
        assert_eq!(
            line_dep(Kind::Cargo, "ropey = \"1.6\""),
            Some("ropey".into())
        );
        assert_eq!(
            line_dep(Kind::Npm, "    \"react\": \"^18.2.0\","),
            Some("react".into())
        );
        assert_eq!(
            line_dep(Kind::Pypi, "    \"fastapi>=0.100\","),
            Some("fastapi".into())
        );
        assert_eq!(
            line_dep(Kind::Pypi, "Django = \"^5.0\""),
            Some("django".into())
        );
        assert_eq!(
            line_dep(Kind::Requirements, "fastapi==0.100"),
            Some("fastapi".into())
        );
        assert_eq!(
            line_dep(Kind::Go, "\tgithub.com/go-chi/chi/v5 v5.0.12 // indirect"),
            Some("github.com/go-chi/chi/v5".into())
        );
        assert_eq!(line_dep(Kind::Cargo, "[dependencies]"), None);
    }

    #[test]
    fn version_span_skips_digits_in_names() {
        let line = "base64 = \"0.21\"";
        let (s, e) = version_span(Kind::Cargo, line).unwrap();
        assert_eq!(&line[s..e], "0.21");
        let line = "\tgithub.com/go-chi/chi/v5 v5.0.12";
        let (s, e) = version_span(Kind::Go, line).unwrap();
        assert_eq!(&line[s..e], "5.0.12");
    }

    #[test]
    fn semver_keys_order_and_ignore_tails() {
        assert!(semver_key("0.29.0") > semver_key("0.27.0"));
        assert!(semver_key("1.0.0") > semver_key("0.99.99"));
        assert_eq!(semver_key("1.2.3-beta.1"), semver_key("1.2.3+build5"));
    }

    #[test]
    fn registry_paths_follow_each_layout() {
        assert_eq!(crates_index_path("a"), "1/a");
        assert_eq!(crates_index_path("io"), "2/io");
        assert_eq!(crates_index_path("fnv"), "3/f/fnv");
        assert_eq!(crates_index_path("ropey"), "ro/pe/ropey");
        assert_eq!(
            go_escape("github.com/Masterminds/semver"),
            "github.com/!masterminds/semver"
        );
        assert_eq!(normalize_py("Flask_Login"), "flask-login");
    }
}
