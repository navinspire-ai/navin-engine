//! Answer "how does this project start?" so nobody has to type it.
//!
//! The manifest already knows the lifecycle commands of every unit it
//! found. This layer picks the one that actually serves traffic and adds
//! the sources a per-unit detector cannot see: Procfile and Makefile.

use std::fs;
use std::path::Path;

use super::manifest::{ProjectManifest, ProjectUnit, ServiceKind};

/// Default ports of the frameworks we recognise, used as first guesses
/// when probing where an app answers.
const FRAMEWORK_PORTS: &[(&str, u16)] = &[
    ("flask", 5000),
    ("fastapi", 8000),
    ("django", 8000),
    ("express", 3000),
    ("nest", 3000),
    ("next", 3000),
    ("nuxt", 3000),
    ("react", 3000),
    ("angular", 4200),
    ("svelte", 5173),
    ("vite", 5173),
    ("rails", 3000),
    ("laravel", 8000),
    ("symfony", 8000),
    ("spring", 8080),
];

/// The command most likely to start the application under test.
///
/// A Procfile is the most explicit statement a project can make, so it
/// wins. Otherwise the best unit answers, and a Makefile closes the march.
pub fn start_command(root: &Path, manifest: &ProjectManifest) -> Option<String> {
    procfile_web(root)
        .or_else(|| unit_start(manifest))
        .or_else(|| makefile_target(root))
}

/// Ports the project itself points at, best first. They are hints for the
/// runtime discovery, never a promise.
pub fn suggested_ports(manifest: &ProjectManifest) -> Vec<u16> {
    let mut ports: Vec<u16> = manifest
        .services
        .iter()
        .filter(|service| service.kind == ServiceKind::App)
        .flat_map(|service| service.ports.iter().copied())
        .collect();
    for unit in &manifest.units {
        let Some(framework) = unit.framework.as_deref() else { continue };
        if let Some((_, port)) = FRAMEWORK_PORTS.iter().find(|(name, _)| *name == framework) {
            ports.push(*port);
        }
    }
    ports.dedup();
    ports
}

/// The outermost unit that knows how to run, preferring its production
/// start over its dev server. Units arrive root-first, so a nested tool
/// never outranks the application it belongs to.
fn unit_start(manifest: &ProjectManifest) -> Option<String> {
    manifest.units.iter().find_map(|unit: &ProjectUnit| {
        unit.commands
            .start
            .as_ref()
            .or(unit.commands.dev.as_ref())
            .map(|cmd| in_unit_dir(&unit.path, cmd))
    })
}

/// A command belonging to a sub-directory has to run there.
fn in_unit_dir(unit_path: &str, command: &str) -> String {
    match unit_path {
        "." | "" => command.to_owned(),
        dir => format!("cd {dir} && {command}"),
    }
}

/// `web:` line of a Procfile, the deployment contract of the project.
fn procfile_web(root: &Path) -> Option<String> {
    let text = fs::read_to_string(root.join("Procfile")).ok()?;
    text.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim() == "web")
        .map(|(_, command)| command.trim().to_owned())
}

/// A Makefile target everybody uses to run the thing locally.
fn makefile_target(root: &Path) -> Option<String> {
    let text = fs::read_to_string(root.join("Makefile")).ok()?;
    let targets: Vec<&str> = text
        .lines()
        .filter(|line| !line.starts_with([' ', '\t', '#']))
        .filter_map(|line| line.split_once(':'))
        .map(|(target, _)| target.trim())
        .collect();
    ["run", "start", "serve", "dev", "up"]
        .into_iter()
        .find(|wanted| targets.contains(wanted))
        .map(|target| format!("make {target}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::inspect_project;

    #[test]
    fn a_procfile_wins_over_everything_else() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("Procfile"), "web: gunicorn app:server -b :8000\n").unwrap();
        fs::write(tmp.path().join("package.json"), r#"{"scripts":{"start":"node ."}}"#).unwrap();

        let manifest = inspect_project(tmp.path()).unwrap();
        assert_eq!(
            start_command(tmp.path(), &manifest).as_deref(),
            Some("gunicorn app:server -b :8000")
        );
    }

    #[test]
    fn a_sub_directory_command_runs_in_its_directory() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("pyproject.toml"), "[project]\nname = \"api\"\n").unwrap();
        let web = tmp.path().join("web");
        fs::create_dir_all(&web).unwrap();
        fs::write(web.join("package.json"), r#"{"scripts":{"dev":"vite"}}"#).unwrap();

        let manifest = inspect_project(tmp.path()).unwrap();
        assert_eq!(start_command(tmp.path(), &manifest).as_deref(), Some("cd web && npm run dev"));
    }

    #[test]
    fn a_makefile_is_the_last_resort() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("Makefile"), "build:\n\tgo build ./...\nserve:\n\t./bin/app\n")
            .unwrap();

        let manifest = inspect_project(tmp.path()).unwrap();
        assert_eq!(start_command(tmp.path(), &manifest).as_deref(), Some("make serve"));
    }

    #[test]
    fn framework_defaults_feed_the_port_hints() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("package.json"), r#"{"dependencies":{"next":"15"}}"#).unwrap();

        let manifest = inspect_project(tmp.path()).unwrap();
        assert_eq!(suggested_ports(&manifest), vec![3000]);
    }
}
