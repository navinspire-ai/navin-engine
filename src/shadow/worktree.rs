//! Git worktree operations via the git CLI (always present where a repo is).

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .context("git not found on PATH")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub fn is_git_repo(root: &Path) -> bool {
    root.join(".git").exists()
}

pub fn head_sha(root: &Path) -> Result<String> {
    git(root, &["rev-parse", "HEAD"])
}

/// Create a detached worktree of `sha` at `dest`.
pub fn add_worktree(root: &Path, dest: &Path, sha: &str) -> Result<()> {
    let dest_str = dest.to_string_lossy();
    git(root, &["worktree", "add", "--detach", &dest_str, sha])?;
    Ok(())
}

/// Remove a worktree even if it has local changes, then prune metadata.
pub fn remove_worktree(root: &Path, dest: &Path) -> Result<()> {
    let dest_str = dest.to_string_lossy();
    // --force twice also removes worktrees with dirty or locked state.
    let result = git(root, &["worktree", "remove", "--force", "--force", &dest_str]);
    // Whatever happened, make sure the directory and the metadata are gone:
    // a crashed run must never leave a zombie worktree behind.
    if dest.exists() {
        std::fs::remove_dir_all(dest).ok();
    }
    git(root, &["worktree", "prune"]).ok();
    result.map(|_| ())
}

#[cfg(test)]
pub mod testutil {
    use super::*;

    /// Init a repo with one commit; returns nothing, panics on failure.
    pub fn init_repo(root: &Path) {
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "test@navin.local"],
            vec!["config", "user.name", "navin-test"],
        ] {
            git(root, &args).unwrap();
        }
        std::fs::write(root.join("app.txt"), "v1").unwrap();
        git(root, &["add", "."]).unwrap();
        git(root, &["commit", "-q", "-m", "init"]).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_lifecycle_pins_the_sha() {
        let tmp = tempfile::tempdir().unwrap();
        testutil::init_repo(tmp.path());
        let sha = head_sha(tmp.path()).unwrap();

        let dest = tmp.path().join(".navin").join("shadow").join("wt-1");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        add_worktree(tmp.path(), &dest, &sha).unwrap();
        assert!(dest.join("app.txt").is_file());

        // Mutating the shadow must not touch the real project.
        std::fs::write(dest.join("app.txt"), "mutated").unwrap();
        assert_eq!(std::fs::read_to_string(tmp.path().join("app.txt")).unwrap(), "v1");

        remove_worktree(tmp.path(), &dest).unwrap();
        assert!(!dest.exists());
    }
}
