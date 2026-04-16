//! Minimal lockfile parsing for the fix engine.
//!
//! Unlike the full lockfile parser in the provider crate, this module only
//! needs to answer one question: "which packages depend on a given target
//! package?" This is used when a lockfile incident fires (e.g., transitive
//! `@patternfly/react-core@5.4.14` from `yarn.lock`) to find the parent
//! packages that need updating (e.g., `@patternfly/react-topology`).
//!
//! Supports `yarn.lock` (berry/v2+), `package-lock.json` (npm v2/v3), and
//! `pnpm-lock.yaml` (v6/v9).

use std::collections::HashSet;
use std::path::Path;

/// Lockfile names in priority order.
const LOCKFILE_NAMES: &[&str] = &["yarn.lock", "package-lock.json", "pnpm-lock.yaml"];

/// Check whether a file path points to a lockfile.
pub fn is_lockfile(path: &Path) -> bool {
    path.file_name()
        .and_then(|f| f.to_str())
        .is_some_and(|name| LOCKFILE_NAMES.contains(&name))
}

/// Parse a lockfile and return names of packages that depend on `target_package`.
///
/// The returned names are deduplicated. The target package itself is excluded
/// from the results (a package listing itself as a dependency is not useful).
pub fn find_dependent_packages(lockfile_path: &Path, target_package: &str) -> Vec<String> {
    let content = match std::fs::read_to_string(lockfile_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                lockfile = %lockfile_path.display(),
                error = %e,
                "Failed to read lockfile for dependent resolution"
            );
            return Vec::new();
        }
    };

    let file_name = lockfile_path
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("");

    let mut dependents = if file_name == "yarn.lock" {
        find_dependents_yarn(&content, target_package)
    } else if file_name == "package-lock.json" {
        find_dependents_npm(&content, target_package)
    } else if file_name == "pnpm-lock.yaml" {
        find_dependents_pnpm(&content, target_package)
    } else {
        Vec::new()
    };

    // Dedup and remove the target package itself
    let mut seen = HashSet::new();
    dependents.retain(|name| name != target_package && seen.insert(name.clone()));

    dependents
}

/// Parse the direct dependency names from a `package.json` file.
///
/// Returns the set of package names declared in `dependencies`,
/// `devDependencies`, and `peerDependencies`.
pub fn parse_direct_dep_names(pkg_json_path: &Path) -> HashSet<String> {
    let mut names = HashSet::new();

    let content = match std::fs::read_to_string(pkg_json_path) {
        Ok(c) => c,
        Err(_) => return names,
    };

    let parsed: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return names,
    };

    for section in &["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(deps) = parsed.get(*section).and_then(|v| v.as_object()) {
            for key in deps.keys() {
                names.insert(key.clone());
            }
        }
    }

    names
}

// ── yarn.lock (berry/v2+) ────────────────────────────────────────────

/// Find packages in `yarn.lock` that depend on `target_package`.
///
/// Yarn berry format has sections like:
/// ```text
/// "@patternfly/react-topology@npm:5.2.1":
///   version: 5.2.1
///   dependencies:
///     "@patternfly/react-core": "npm:^5.1.1"
/// ```
///
/// We scan for lines containing `"target_package":` inside a `dependencies:`
/// block and extract the parent entry name from the section header.
fn find_dependents_yarn(content: &str, target_package: &str) -> Vec<String> {
    let mut dependents = Vec::new();
    let dep_needle = format!("\"{}\":", target_package);

    let mut current_entry: Option<String> = None;
    let mut in_deps = false;

    for line in content.lines() {
        // Section header: starts with `"@scope/name@npm:...":` or non-whitespace
        if !line.starts_with(' ') && !line.starts_with('\t') && !line.is_empty() {
            // Extract package name from header like `"@patternfly/react-topology@npm:5.2.1":`
            current_entry = extract_yarn_entry_name(line);
            in_deps = false;
            continue;
        }

        let trimmed = line.trim();

        if trimmed == "dependencies:" || trimmed == "peerDependencies:" {
            in_deps = true;
            continue;
        }

        // Exit deps block on non-indented key at same level
        if in_deps && !trimmed.is_empty() && !trimmed.starts_with('"') && !trimmed.starts_with('\'')
        {
            in_deps = false;
        }

        if in_deps && trimmed.contains(&dep_needle) {
            if let Some(ref name) = current_entry {
                dependents.push(name.clone());
            }
        }
    }

    dependents
}

/// Extract the package name from a yarn.lock section header.
///
/// `"@patternfly/react-topology@npm:5.2.1":` → `@patternfly/react-topology`
/// `"lodash@npm:^4.17.21, lodash@npm:^4.17.20":` → `lodash`
fn extract_yarn_entry_name(line: &str) -> Option<String> {
    let line = line.trim().trim_end_matches(':');
    // Remove outer quotes if present
    let line = line.trim_matches('"');

    // Take the first specifier (before any comma)
    let first = line
        .split(',')
        .next()
        .unwrap_or(line)
        .trim()
        .trim_matches('"');

    // Split on `@npm:` or `@workspace:` to get the name
    if let Some(pos) = first.rfind("@npm:").or_else(|| first.rfind("@workspace:")) {
        let name = &first[..pos];
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }

    // Fallback: split on last @ that's not the scope prefix
    if first.starts_with('@') {
        // Scoped: find @ after the /
        if let Some(slash_pos) = first.find('/') {
            if let Some(at_pos) = first[slash_pos..].find('@') {
                let name = &first[..slash_pos + at_pos];
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
    } else if let Some(at_pos) = first.find('@') {
        let name = &first[..at_pos];
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }

    None
}

// ── package-lock.json (npm v2/v3) ────────────────────────────────────

/// Find packages in `package-lock.json` that depend on `target_package`.
fn find_dependents_npm(content: &str, target_package: &str) -> Vec<String> {
    let mut dependents = Vec::new();

    let parsed: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to parse package-lock.json");
            return dependents;
        }
    };

    let packages = match parsed.get("packages").and_then(|v| v.as_object()) {
        Some(p) => p,
        None => return dependents,
    };

    for (key, value) in packages {
        // Skip root entry
        if key.is_empty() {
            continue;
        }

        // Check dependencies, devDependencies, peerDependencies
        let has_dep = ["dependencies", "devDependencies", "peerDependencies"]
            .iter()
            .any(|section| {
                value
                    .get(*section)
                    .and_then(|deps| deps.get(target_package))
                    .is_some()
            });

        if has_dep {
            // Extract package name from key like "node_modules/@patternfly/react-topology"
            let name = key.strip_prefix("node_modules/").unwrap_or(key);
            // Handle nested node_modules (take the last segment)
            let name = if let Some(last_nm) = name.rfind("node_modules/") {
                &name[last_nm + "node_modules/".len()..]
            } else {
                name
            };
            if !name.is_empty() {
                dependents.push(name.to_string());
            }
        }
    }

    dependents
}

// ── pnpm-lock.yaml (v6/v9) ──────────────────────────────────────────

/// Find packages in `pnpm-lock.yaml` that depend on `target_package`.
fn find_dependents_pnpm(content: &str, target_package: &str) -> Vec<String> {
    let mut dependents = Vec::new();

    let parsed: serde_yaml::Value = match serde_yaml::from_str(content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to parse pnpm-lock.yaml");
            return dependents;
        }
    };

    let version_str = parsed
        .get("lockfileVersion")
        .map(|v| match v {
            serde_yaml::Value::String(s) => s.clone(),
            serde_yaml::Value::Number(n) => n.to_string(),
            _ => String::new(),
        })
        .unwrap_or_default();

    // For v9, dependencies are in `snapshots`; for v6, they're in `packages`
    let sections_to_check = if version_str.starts_with('9') {
        vec!["snapshots", "packages"]
    } else {
        vec!["packages"]
    };

    for section_name in sections_to_check {
        if let Some(section) = parsed.get(section_name).and_then(|v| v.as_mapping()) {
            for (key_val, value) in section {
                let key = match key_val.as_str() {
                    Some(k) => k,
                    None => continue,
                };

                // Check if this entry has target_package in its dependencies
                let has_dep = ["dependencies", "peerDependencies", "optionalDependencies"]
                    .iter()
                    .any(|dep_section| {
                        value
                            .get(*dep_section)
                            .and_then(|deps| deps.as_mapping())
                            .is_some_and(|deps| {
                                deps.keys().any(|k| k.as_str() == Some(target_package))
                            })
                    });

                if has_dep {
                    if let Some(name) = extract_pnpm_package_name(key) {
                        dependents.push(name);
                    }
                }
            }
        }
    }

    dependents
}

/// Extract the package name from a pnpm lockfile key.
///
/// Handles v6 keys (leading `/`) and v9 keys (no leading `/`),
/// scoped and unscoped packages, and peer suffixes.
///
/// `/@patternfly/react-topology@5.2.1(react@18.2.0)` → `@patternfly/react-topology`
/// `@patternfly/react-topology@5.2.1` → `@patternfly/react-topology`
/// `lodash@4.17.21` → `lodash`
fn extract_pnpm_package_name(key: &str) -> Option<String> {
    let key = key.strip_prefix('/').unwrap_or(key);

    if key.is_empty() {
        return None;
    }

    // Find the @ that separates name from version
    let version_at = if key.starts_with('@') {
        // Scoped package: find / then the next @
        let slash_pos = key.find('/')?;
        let rest = &key[slash_pos + 1..];
        rest.find('@').map(|p| slash_pos + 1 + p)
    } else {
        key.find('@')
    };

    match version_at {
        Some(pos) => {
            let name = &key[..pos];
            if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            }
        }
        None => Some(key.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_lockfile ──────────────────────────────────────────────────

    #[test]
    fn test_is_lockfile() {
        assert!(is_lockfile(Path::new("/project/yarn.lock")));
        assert!(is_lockfile(Path::new("/project/package-lock.json")));
        assert!(is_lockfile(Path::new("/project/pnpm-lock.yaml")));
        assert!(!is_lockfile(Path::new("/project/package.json")));
        assert!(!is_lockfile(Path::new("/project/src/main.ts")));
    }

    // ── yarn.lock dependents ─────────────────────────────────────────

    #[test]
    fn test_find_dependents_yarn() {
        let content = r#"
"@patternfly/react-core@npm:^5.1.1":
  version: 5.4.14
  dependencies:
    "@patternfly/react-icons": "npm:^5.1.1"
    "@patternfly/react-styles": "npm:^5.1.1"
    tslib: "npm:^2.0.0"

"@patternfly/react-topology@npm:5.2.1":
  version: 5.2.1
  dependencies:
    "@patternfly/react-core": "npm:^5.1.1"
    "@patternfly/react-icons": "npm:^5.1.1"
    d3: "npm:^7.0.0"

"@patternfly/react-log-viewer@npm:5.3.0":
  version: 5.3.0
  dependencies:
    "@patternfly/react-core": "npm:^5.1.1"
    ansi_up: "npm:^5.0.0"

"lodash@npm:^4.17.21":
  version: 4.17.21
"#;

        let result = find_dependents_yarn(content, "@patternfly/react-core");
        assert_eq!(result.len(), 2);
        assert!(result.contains(&"@patternfly/react-topology".to_string()));
        assert!(result.contains(&"@patternfly/react-log-viewer".to_string()));
    }

    #[test]
    fn test_find_dependents_yarn_excludes_self() {
        let content = r#"
"@patternfly/react-core@npm:^5.1.1":
  version: 5.4.14
  dependencies:
    "@patternfly/react-icons": "npm:^5.1.1"
"#;

        // react-icons is not listed as depending on itself
        let result = find_dependents_yarn(content, "@patternfly/react-icons");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "@patternfly/react-core");
    }

    #[test]
    fn test_find_dependents_yarn_peer_deps() {
        let content = r#"
"some-plugin@npm:2.0.0":
  version: 2.0.0
  peerDependencies:
    "@patternfly/react-core": "npm:^5.0.0"
"#;

        let result = find_dependents_yarn(content, "@patternfly/react-core");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "some-plugin");
    }

    // ── package-lock.json dependents ─────────────────────────────────

    #[test]
    fn test_find_dependents_npm() {
        let content = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "my-app", "version": "1.0.0" },
                "node_modules/@patternfly/react-core": {
                    "version": "5.4.14"
                },
                "node_modules/@patternfly/react-topology": {
                    "version": "5.2.1",
                    "dependencies": {
                        "@patternfly/react-core": "^5.1.1",
                        "d3": "^7.0.0"
                    }
                },
                "node_modules/@patternfly/react-log-viewer": {
                    "version": "5.3.0",
                    "peerDependencies": {
                        "@patternfly/react-core": "^5.1.1"
                    }
                },
                "node_modules/lodash": {
                    "version": "4.17.21"
                }
            }
        }"#;

        let result = find_dependents_npm(content, "@patternfly/react-core");
        assert_eq!(result.len(), 2);
        assert!(result.contains(&"@patternfly/react-topology".to_string()));
        assert!(result.contains(&"@patternfly/react-log-viewer".to_string()));
    }

    #[test]
    fn test_find_dependents_npm_nested_node_modules() {
        let content = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "root", "version": "1.0.0" },
                "node_modules/@patternfly/react-topology/node_modules/@patternfly/react-core": {
                    "version": "5.4.14"
                },
                "node_modules/@patternfly/react-topology": {
                    "version": "5.2.1",
                    "dependencies": {
                        "@patternfly/react-core": "^5.1.1"
                    }
                }
            }
        }"#;

        let result = find_dependents_npm(content, "@patternfly/react-core");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "@patternfly/react-topology");
    }

    // ── pnpm-lock.yaml dependents ────────────────────────────────────

    #[test]
    fn test_find_dependents_pnpm_v6() {
        let content = r#"
lockfileVersion: '6.0'

packages:
  /@patternfly/react-core@5.1.1(react@18.2.0):
    resolution: {integrity: sha512-abc}
    dependencies:
      '@patternfly/react-icons': 5.1.1
    dev: false

  /@patternfly/react-topology@5.2.1(react@18.2.0):
    resolution: {integrity: sha512-def}
    dependencies:
      '@patternfly/react-core': 5.1.1(react@18.2.0)
      d3: 7.8.5
    dev: false

  /lodash@4.17.21:
    resolution: {integrity: sha512-ghi}
    dev: false
"#;

        let result = find_dependents_pnpm(content, "@patternfly/react-core");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "@patternfly/react-topology");
    }

    #[test]
    fn test_find_dependents_pnpm_v9() {
        let content = r#"
lockfileVersion: '9.0'

packages:
  '@patternfly/react-core@5.1.1':
    resolution: {integrity: sha512-abc}

  '@patternfly/react-topology@5.2.1':
    resolution: {integrity: sha512-def}

snapshots:
  '@patternfly/react-core@5.1.1(react@18.2.0)':
    dependencies:
      '@patternfly/react-icons': 5.1.1

  '@patternfly/react-topology@5.2.1(react@18.2.0)':
    dependencies:
      '@patternfly/react-core': 5.1.1(react@18.2.0)
      d3: 7.8.5
"#;

        let result = find_dependents_pnpm(content, "@patternfly/react-core");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "@patternfly/react-topology");
    }

    // ── extract helpers ──────────────────────────────────────────────

    #[test]
    fn test_extract_yarn_entry_name() {
        assert_eq!(
            extract_yarn_entry_name("\"@patternfly/react-topology@npm:5.2.1\":"),
            Some("@patternfly/react-topology".into())
        );
        assert_eq!(
            extract_yarn_entry_name("\"lodash@npm:^4.17.21, lodash@npm:^4.17.20\":"),
            Some("lodash".into())
        );
        assert_eq!(
            extract_yarn_entry_name("\"react@npm:^18.2.0\":"),
            Some("react".into())
        );
    }

    #[test]
    fn test_extract_pnpm_package_name() {
        assert_eq!(
            extract_pnpm_package_name("/@patternfly/react-topology@5.2.1(react@18.2.0)"),
            Some("@patternfly/react-topology".into())
        );
        assert_eq!(
            extract_pnpm_package_name("@patternfly/react-topology@5.2.1"),
            Some("@patternfly/react-topology".into())
        );
        assert_eq!(
            extract_pnpm_package_name("/lodash@4.17.21"),
            Some("lodash".into())
        );
        assert_eq!(
            extract_pnpm_package_name("lodash@4.17.21"),
            Some("lodash".into())
        );
    }

    // ── find_dependent_packages (integration) ────────────────────────

    #[test]
    fn test_find_dependent_packages_excludes_target() {
        let dir = tempfile::tempdir().unwrap();
        let lockfile = dir.path().join("yarn.lock");

        let content = r#"
"@patternfly/react-core@npm:^5.1.1":
  version: 5.4.14
  dependencies:
    "@patternfly/react-icons": "npm:^5.1.1"

"@patternfly/react-topology@npm:5.2.1":
  version: 5.2.1
  dependencies:
    "@patternfly/react-core": "npm:^5.1.1"
"#;
        std::fs::write(&lockfile, content).unwrap();

        let result = find_dependent_packages(&lockfile, "@patternfly/react-core");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "@patternfly/react-topology");
        // react-core should not be in results even though it appears as an entry
        assert!(!result.contains(&"@patternfly/react-core".to_string()));
    }

    #[test]
    fn test_parse_direct_dep_names() {
        let dir = tempfile::tempdir().unwrap();
        let pkg_json = dir.path().join("package.json");

        let content = r#"{
            "name": "my-app",
            "dependencies": {
                "@patternfly/react-core": "^5.2.1",
                "lodash": "^4.17.21"
            },
            "devDependencies": {
                "@patternfly/react-topology": "5.2.1",
                "@patternfly/react-log-viewer": "5.3.0"
            }
        }"#;
        std::fs::write(&pkg_json, content).unwrap();

        let names = parse_direct_dep_names(&pkg_json);
        assert!(names.contains("@patternfly/react-core"));
        assert!(names.contains("lodash"));
        assert!(names.contains("@patternfly/react-topology"));
        assert!(names.contains("@patternfly/react-log-viewer"));
        assert_eq!(names.len(), 4);
    }
}
