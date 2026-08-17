//! Resolve build/test/start commands per runtime, preferring what the
//! project declares (package.json scripts) over defaults.

use serde_json::Value;
use std::path::Path;

use super::manifest::LifecycleCommands;
use super::runtime::Runtime;

/// Detect the Node package manager from lockfiles.
pub fn node_package_manager(dir: &Path) -> &'static str {
    if dir.join("pnpm-lock.yaml").is_file() {
        "pnpm"
    } else if dir.join("yarn.lock").is_file() {
        "yarn"
    } else if dir.join("bun.lockb").is_file() || dir.join("bun.lock").is_file() {
        "bun"
    } else {
        "npm"
    }
}

/// Commands from a parsed package.json, using the detected package manager.
pub fn node_commands(pkg: &Value, pm: &str) -> LifecycleCommands {
    let scripts = pkg.get("scripts").and_then(Value::as_object);
    let has = |name: &str| scripts.map(|s| s.contains_key(name)).unwrap_or(false);
    let run = |name: &str| -> Option<String> {
        has(name).then(|| match pm {
            "yarn" => format!("yarn {name}"),
            _ => format!("{pm} run {name}"),
        })
    };
    LifecycleCommands {
        build: run("build"),
        test: run("test"),
        start: run("start"),
        dev: run("dev").or_else(|| run("serve")),
    }
}

pub fn default_commands(runtime: Runtime) -> LifecycleCommands {
    match runtime {
        Runtime::Rust => LifecycleCommands {
            build: Some("cargo build".into()),
            test: Some("cargo test".into()),
            start: Some("cargo run".into()),
            dev: None,
        },
        Runtime::Go => LifecycleCommands {
            build: Some("go build ./...".into()),
            test: Some("go test ./...".into()),
            start: Some("go run .".into()),
            dev: None,
        },
        Runtime::Python => LifecycleCommands {
            build: None,
            test: Some("pytest".into()),
            start: None,
            dev: None,
        },
        Runtime::Java => LifecycleCommands {
            build: Some("mvn package".into()),
            test: Some("mvn test".into()),
            start: None,
            dev: None,
        },
        _ => LifecycleCommands::default(),
    }
}
