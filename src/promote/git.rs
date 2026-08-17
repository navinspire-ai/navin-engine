//! Git operations for promotion. Commits land on a dedicated branch via a
//! temporary worktree, so the user's working tree is never disturbed while
//! the change is prepared. Merging is fast-forward-only to avoid conflicts.

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

pub fn is_repo(root: &Path) -> bool {
    root.join(".git").exists()
}

pub fn head_sha(root: &Path) -> Result<String> {
    git(root, &["rev-parse", "HEAD"])
}

pub fn current_branch(root: &Path) -> Result<String> {
    git(root, &["rev-parse", "--abbrev-ref", "HEAD"])
}

/// True when no *tracked* file is modified (safe to fast-forward merge).
/// Untracked files are ignored: engine artefacts under `.navin/` are always
/// untracked, and a fast-forward can only touch tracked paths.
pub fn is_clean(root: &Path) -> Result<bool> {
    Ok(git(root, &["status", "--porcelain", "--untracked-files=no"])?.is_empty())
}

pub fn branch_exists(root: &Path, name: &str) -> bool {
    git(root, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{name}")]).is_ok()
}

pub fn create_branch(root: &Path, name: &str, at: &str) -> Result<()> {
    git(root, &["branch", name, at])?;
    Ok(())
}

pub fn delete_branch(root: &Path, name: &str) -> Result<()> {
    git(root, &["branch", "-D", name])?;
    Ok(())
}

pub fn add_worktree(root: &Path, dest: &Path, branch: &str) -> Result<()> {
    git(root, &["worktree", "add", &dest.to_string_lossy(), branch])?;
    Ok(())
}

pub fn remove_worktree(root: &Path, dest: &Path) -> Result<()> {
    git(root, &["worktree", "remove", "--force", &dest.to_string_lossy()]).ok();
    if dest.exists() {
        std::fs::remove_dir_all(dest).ok();
    }
    git(root, &["worktree", "prune"]).ok();
    Ok(())
}

/// Stage everything in `worktree_dir` and commit; returns the new sha.
pub fn commit_all(worktree_dir: &Path, message: &str) -> Result<String> {
    git(worktree_dir, &["add", "-A"])?;
    git(worktree_dir, &["commit", "--no-verify", "-m", message])?;
    head_sha(worktree_dir)
}

/// Fast-forward the active branch to `branch`. Errors if not a fast-forward
/// (we never create merge commits or risk conflicts automatically).
pub fn merge_ff_only(root: &Path, branch: &str) -> Result<()> {
    git(root, &["merge", "--ff-only", branch])?;
    Ok(())
}

/// Create an inverse commit that undoes `sha` (non-destructive rollback).
pub fn revert(root: &Path, sha: &str) -> Result<String> {
    git(root, &["revert", "--no-edit", sha])?;
    head_sha(root)
}

#[cfg(test)]
pub mod testutil {
    use super::*;

    pub fn init_repo(root: &Path) {
        for args in [
            vec!["init", "-q", "-b", "main"],
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
    fn branch_commit_via_worktree_leaves_main_tree_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        testutil::init_repo(root);
        let base = head_sha(root).unwrap();

        create_branch(root, "navin/evolve/x", &base).unwrap();
        let wt = root.join(".navin/promote/x");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        add_worktree(root, &wt, "navin/evolve/x").unwrap();
        std::fs::write(wt.join("app.txt"), "v2").unwrap();
        let sha = commit_all(&wt, "navin: fix").unwrap();
        remove_worktree(root, &wt).unwrap();

        // The active branch is still at the base commit; its file untouched.
        assert_eq!(head_sha(root).unwrap(), base);
        assert_eq!(std::fs::read_to_string(root.join("app.txt")).unwrap(), "v1");

        // Fast-forward merge brings the change in.
        merge_ff_only(root, "navin/evolve/x").unwrap();
        assert_eq!(head_sha(root).unwrap(), sha);
        assert_eq!(std::fs::read_to_string(root.join("app.txt")).unwrap(), "v2");

        // Revert undoes it without rewriting history.
        revert(root, &sha).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("app.txt")).unwrap(), "v1");
    }
}
