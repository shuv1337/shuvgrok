//! Install JS deps only when the lockfile/manifest fingerprint changed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use rapidhash::v3::rapidhash_v3;

const STAMP_NAME: &str = ".shuvgrok-deps-stamp";

fn package_manager_field(text: &str) -> Option<&str> {
    let key = "\"packageManager\"";
    let i = text.find(key)?;
    let after = text.get(i + key.len()..)?;
    let colon = after.find(':')?;
    let rest = after.get(colon + 1..)?.trim_start();
    let quoted = rest.strip_prefix('"')?;
    quoted.split('"').next()
}

const FINGERPRINT_FILES: &[&str] = &[
    "package.json",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "bun.lock",
    "bun.lockb",
    "yarn.lock",
    "package-lock.json",
    "bunfig.toml",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsPackageManager {
    Bun,
    Pnpm,
    Yarn,
    Npm,
}

impl JsPackageManager {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bun => "bun",
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
            Self::Npm => "npm",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnsureJsDepsOutcome {
    SkippedNoJsProject,
    SkippedStampMatch { manager: JsPackageManager },
    TrustedExisting { manager: JsPackageManager },
    Installed { manager: JsPackageManager },
}

/// Detect bun / pnpm / yarn / npm from `packageManager` then lockfiles.
pub fn detect_js_package_manager(root: &Path) -> Option<JsPackageManager> {
    if let Ok(text) = fs::read_to_string(root.join("package.json"))
        && let Some(pm) = package_manager_field(&text)
    {
        let name = pm.split('@').next().unwrap_or("");
        match name {
            "bun" => return Some(JsPackageManager::Bun),
            "pnpm" => return Some(JsPackageManager::Pnpm),
            "yarn" => return Some(JsPackageManager::Yarn),
            "npm" => return Some(JsPackageManager::Npm),
            _ => {}
        }
    }
    if root.join("bun.lockb").is_file() || root.join("bun.lock").is_file() {
        return Some(JsPackageManager::Bun);
    }
    if root.join("pnpm-lock.yaml").is_file() {
        return Some(JsPackageManager::Pnpm);
    }
    if root.join("yarn.lock").is_file() {
        return Some(JsPackageManager::Yarn);
    }
    if root.join("package-lock.json").is_file() {
        return Some(JsPackageManager::Npm);
    }
    None
}

pub fn js_deps_fingerprint(root: &Path) -> String {
    let mut buf = Vec::new();
    for name in FINGERPRINT_FILES {
        let path = root.join(name);
        if let Ok(bytes) = fs::read(&path) {
            buf.extend_from_slice(name.as_bytes());
            buf.push(0);
            buf.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            buf.extend_from_slice(&bytes);
        }
    }
    format!("{:016x}", rapidhash_v3(&buf))
}

fn stamp_path(root: &Path) -> PathBuf {
    root.join("node_modules").join(STAMP_NAME)
}

/// Trust CoW `node_modules` when present; install only on stamp mismatch or miss.
pub fn ensure_js_deps(root: &Path) -> Result<EnsureJsDepsOutcome> {
    let Some(manager) = detect_js_package_manager(root) else {
        return Ok(EnsureJsDepsOutcome::SkippedNoJsProject);
    };
    let want = js_deps_fingerprint(root);
    let node_modules = root.join("node_modules");
    let stamp = stamp_path(root);
    if node_modules.is_dir() {
        if let Ok(have) = fs::read_to_string(&stamp) {
            if have.trim() == want {
                return Ok(EnsureJsDepsOutcome::SkippedStampMatch { manager });
            }
        } else {
            fs::create_dir_all(&node_modules)?;
            fs::write(&stamp, format!("{want}\n"))?;
            return Ok(EnsureJsDepsOutcome::TrustedExisting { manager });
        }
    }

    let mut cmd = match manager {
        JsPackageManager::Bun => {
            let mut c = Command::new("bun");
            c.args(["install", "--frozen-lockfile"]);
            c
        }
        JsPackageManager::Pnpm => {
            let mut c = Command::new("pnpm");
            c.args(["install", "--frozen-lockfile"]);
            c
        }
        JsPackageManager::Yarn => {
            let mut c = Command::new("yarn");
            c.args(["install", "--immutable"]);
            c
        }
        JsPackageManager::Npm => {
            let mut c = Command::new("npm");
            if root.join("package-lock.json").is_file() {
                c.arg("ci");
            } else {
                c.arg("install");
            }
            c
        }
    };
    let output = cmd
        .current_dir(root)
        .output()
        .with_context(|| format!("{} install failed to start", manager.as_str()))?;
    if !output.status.success() {
        anyhow::bail!(
            "{} install failed (status {:?}): {}",
            manager.as_str(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fs::create_dir_all(&node_modules)?;
    let want = js_deps_fingerprint(root);
    fs::write(&stamp, format!("{want}\n"))?;
    Ok(EnsureJsDepsOutcome::Installed { manager })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn detects_package_manager_field() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"packageManager":"pnpm@10.17.1"}"#,
        )
        .unwrap();
        assert_eq!(
            detect_js_package_manager(dir.path()),
            Some(JsPackageManager::Pnpm)
        );
    }

    #[test]
    fn detects_lockfile_when_field_missing() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        fs::write(dir.path().join("bun.lock"), "x").unwrap();
        assert_eq!(
            detect_js_package_manager(dir.path()),
            Some(JsPackageManager::Bun)
        );
    }

    #[test]
    fn trusts_existing_node_modules_without_install() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"packageManager":"pnpm@10.17.1"}"#,
        )
        .unwrap();
        fs::write(dir.path().join("pnpm-lock.yaml"), "lockfileVersion: 9").unwrap();
        fs::create_dir_all(dir.path().join("node_modules")).unwrap();
        let first = ensure_js_deps(dir.path()).unwrap();
        assert!(matches!(first, EnsureJsDepsOutcome::TrustedExisting { .. }));
        let second = ensure_js_deps(dir.path()).unwrap();
        assert!(matches!(second, EnsureJsDepsOutcome::SkippedStampMatch { .. }));
    }
}
