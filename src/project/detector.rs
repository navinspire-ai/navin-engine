//! Walk a workspace and build its [`ProjectManifest`].
//!
//! Discovery is read-only and bounded: it scans the root plus two levels of
//! subdirectories, skipping dependency and build folders, so a giant
//! monorepo cannot stall the daemon.

use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::Path;

use super::commands::{default_commands, node_commands, node_package_manager};
use super::manifest::{ProjectManifest, ProjectUnit, MANIFEST_SCHEMA};
use super::runtime::Runtime;
use super::topology::discover_services;

const MAX_DEPTH: usize = 2;
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    ".git",
    ".navin",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    ".turbo",
    "vendor",
];

/// Inspect a workspace root and produce its manifest.
pub fn inspect_project(root: &Path) -> Result<ProjectManifest> {
    let root = root
        .canonicalize()
        .with_context(|| format!("cannot resolve project root {}", root.display()))?;
    let mut units = Vec::new();
    walk(&root, &root, 0, &mut units);
    // Root units first, then by path for a stable output.
    units.sort_by(|a, b| (a.path.len(), &a.path).cmp(&(b.path.len(), &b.path)));

    let env_files = ENV_FILES
        .iter()
        .filter(|name| root.join(name).is_file())
        .map(|name| (*name).to_owned())
        .collect();

    Ok(ProjectManifest {
        schema: MANIFEST_SCHEMA.to_owned(),
        monorepo: units.len() > 1,
        services: discover_services(&root),
        env_files,
        dockerfile: root.join("Dockerfile").is_file(),
        git: root.join(".git").exists(),
        root,
        units,
    })
}

const ENV_FILES: &[&str] = &[".env", ".env.local", ".env.example", ".env.production"];

fn walk(root: &Path, dir: &Path, depth: usize, units: &mut Vec<ProjectUnit>) {
    if let Some(unit) = detect_unit(root, dir) {
        units.push(unit);
    }
    if depth >= MAX_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
            continue;
        }
        walk(root, &path, depth + 1, units);
    }
}

fn detect_unit(root: &Path, dir: &Path) -> Option<ProjectUnit> {
    let rel = dir
        .strip_prefix(root)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| ".".to_owned());

    if dir.join("package.json").is_file() {
        let pkg: Value = fs::read_to_string(dir.join("package.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or(Value::Null);
        let pm = node_package_manager(dir);
        return Some(ProjectUnit {
            path: rel,
            runtime: Runtime::Node,
            framework: node_framework(&pkg),
            package_manager: Some(pm.to_owned()),
            commands: node_commands(&pkg, pm),
        });
    }
    if dir.join("Cargo.toml").is_file() {
        return Some(ProjectUnit {
            path: rel,
            runtime: Runtime::Rust,
            framework: None,
            package_manager: Some("cargo".to_owned()),
            commands: default_commands(Runtime::Rust),
        });
    }
    if dir.join("pyproject.toml").is_file()
        || dir.join("requirements.txt").is_file()
        || dir.join("setup.py").is_file()
    {
        let pm = if dir.join("uv.lock").is_file() {
            "uv"
        } else if dir.join("poetry.lock").is_file() {
            "poetry"
        } else {
            "pip"
        };
        return Some(ProjectUnit {
            path: rel,
            runtime: Runtime::Python,
            framework: python_framework(dir),
            package_manager: Some(pm.to_owned()),
            commands: default_commands(Runtime::Python),
        });
    }
    if dir.join("go.mod").is_file() {
        return Some(ProjectUnit {
            path: rel,
            runtime: Runtime::Go,
            framework: None,
            package_manager: Some("go".to_owned()),
            commands: default_commands(Runtime::Go),
        });
    }
    if dir.join("pom.xml").is_file() || dir.join("build.gradle").is_file() {
        let pm = if dir.join("pom.xml").is_file() { "maven" } else { "gradle" };
        return Some(ProjectUnit {
            path: rel,
            runtime: Runtime::Java,
            framework: None,
            package_manager: Some(pm.to_owned()),
            commands: default_commands(Runtime::Java),
        });
    }
    None
}

fn node_framework(pkg: &Value) -> Option<String> {
    let deps = |key: &str| pkg.get(key).and_then(Value::as_object).cloned();
    let mut all = deps("dependencies").unwrap_or_default();
    if let Some(dev) = deps("devDependencies") {
        all.extend(dev);
    }
    // Order matters: next implies react, vite is a build tool under both.
    for (name, label) in [
        ("next", "next"),
        ("nuxt", "nuxt"),
        ("@sveltejs/kit", "sveltekit"),
        ("astro", "astro"),
        ("react", "react"),
        ("vue", "vue"),
        ("svelte", "svelte"),
        ("express", "express"),
        ("fastify", "fastify"),
        ("@nestjs/core", "nest"),
        ("vite", "vite"),
    ] {
        if all.contains_key(name) {
            return Some(label.to_owned());
        }
    }
    None
}

fn python_framework(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join("pyproject.toml")).ok()?;
    for (needle, label) in [
        ("fastapi", "fastapi"),
        ("django", "django"),
        ("flask", "flask"),
    ] {
        if text.contains(needle) {
            return Some(label.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn node_project_is_detected_with_scripts_and_framework() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("package.json"),
            r#"{"dependencies":{"react":"^19"},"scripts":{"build":"vite build","test":"vitest","dev":"vite"}}"#,
        )
        .unwrap();
        fs::write(tmp.path().join("pnpm-lock.yaml"), "").unwrap();
        fs::write(tmp.path().join(".env"), "SECRET=1").unwrap();

        let manifest = inspect_project(tmp.path()).unwrap();
        assert_eq!(manifest.units.len(), 1);
        let unit = &manifest.units[0];
        assert_eq!(unit.runtime, Runtime::Node);
        assert_eq!(unit.framework.as_deref(), Some("react"));
        assert_eq!(unit.package_manager.as_deref(), Some("pnpm"));
        assert_eq!(unit.commands.build.as_deref(), Some("pnpm run build"));
        // Env files are listed by name only; contents must never leak.
        assert_eq!(manifest.env_files, vec![".env".to_owned()]);
        assert!(!manifest.monorepo);
    }

    #[test]
    fn monorepo_units_are_found_two_levels_deep() {
        let tmp = tempfile::tempdir().unwrap();
        let api = tmp.path().join("services").join("api");
        fs::create_dir_all(&api).unwrap();
        fs::write(api.join("go.mod"), "module example.com/api").unwrap();
        let web = tmp.path().join("web");
        fs::create_dir_all(&web).unwrap();
        fs::write(web.join("package.json"), "{}").unwrap();

        let manifest = inspect_project(tmp.path()).unwrap();
        assert!(manifest.monorepo);
        let paths: Vec<&str> = manifest.units.iter().map(|u| u.path.as_str()).collect();
        assert!(paths.contains(&"web"));
        assert!(paths.iter().any(|p| p.ends_with("api")));
    }

    #[test]
    fn dependency_folders_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let dep = tmp.path().join("node_modules").join("leftpad");
        fs::create_dir_all(&dep).unwrap();
        fs::write(dep.join("package.json"), "{}").unwrap();

        let manifest = inspect_project(tmp.path()).unwrap();
        assert!(manifest.units.is_empty());
    }
}
