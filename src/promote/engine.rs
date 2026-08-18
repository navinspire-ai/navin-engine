//! Promotion orchestration: certificate -> policy decision -> git branch
//! (+ optional fast-forward merge) -> durable record. Rollback undoes a
//! promotion non-destructively. This is the only code that writes to the
//! user's workspace, and only through an explicit, reversible path.

use anyhow::{bail, Context, Result};
use std::path::Path;
use tracing::{info, warn};

use crate::fix::model::FixReport;
use crate::fix::FixCandidate;
use crate::policy::config::EvolveConfig;

use super::certify::issue;
use super::git;
use super::model::{PromotionOutcome, PromotionRecord, PROMOTION_SCHEMA};
use super::policy::{decide, PolicyDecision};

/// Promote an accepted candidate under the workspace policy.
pub fn promote(
    project_root: &Path,
    report: &FixReport,
    candidate: &FixCandidate,
    config: &EvolveConfig,
) -> Result<PromotionRecord> {
    let epoch = epoch_secs();
    let slug = sanitize(&report.target_finding);
    let id = format!("promo-{slug}-{epoch}");

    let certificate = issue(project_root, report, candidate)?;
    let decision = decide(config, &candidate.family, certificate.is_valid());

    let mut record = PromotionRecord {
        schema: PROMOTION_SCHEMA.to_owned(),
        id: id.clone(),
        finding: report.target_finding.clone(),
        candidate_id: candidate.id.clone(),
        mode: config.evolve.mode.clone(),
        outcome: PromotionOutcome::Blocked,
        reasons: Vec::new(),
        branch: None,
        commit_sha: None,
        prev_head: None,
        merged: false,
        certificate: Some(certificate.clone()),
        diff: None,
        pushed_to: None,
        pull_request: None,
        created_at: format!("epoch:{epoch}"),
        rolled_back_at: None,
    };

    let (want_merge, reason) = match decision {
        PolicyDecision::Blocked(reason) => {
            record.reasons.push(reason);
            record.save(project_root)?;
            return Ok(record);
        }
        PolicyDecision::BranchOnly(reason) => (false, reason),
        PolicyDecision::Merge(reason) => (true, reason),
    };
    record.reasons.push(reason);

    if !git::is_repo(project_root) {
        record.reasons.push("workspace is not a git repository; cannot promote".to_owned());
        record.save(project_root)?;
        return Ok(record);
    }

    let prev_head = git::head_sha(project_root)?;
    record.prev_head = Some(prev_head.clone());
    let branch = format!("navin/evolve/{slug}-{epoch}");

    // Prepare the commit on a dedicated branch via a throwaway worktree.
    git::create_branch(project_root, &branch, &prev_head)
        .with_context(|| format!("creating branch {branch}"))?;
    let worktree = crate::engine_dir(project_root).join("promote").join(&id);
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    git::add_worktree(project_root, &worktree, &branch)?;

    // The diff is read from the worktree, which is about to disappear: a
    // promotion nobody can read is a promotion nobody should accept.
    let mut diff = None;
    let commit_sha = (|| -> Result<String> {
        crate::fix::patch::apply(&candidate.patch, &worktree)
            .context("applying the proposal to the promotion worktree")?;
        diff = crate::fix::diff::capture(&worktree);
        let message = commit_message(report, candidate, &certificate);
        git::commit_all(&worktree, &message)
    })();
    record.diff = diff;

    git::remove_worktree(project_root, &worktree).ok();

    let commit_sha = match commit_sha {
        Ok(sha) => sha,
        Err(err) => {
            // Nothing merged; drop the branch so no half-state remains.
            git::delete_branch(project_root, &branch).ok();
            record.reasons.push(format!("commit failed: {err:#}"));
            record.save(project_root)?;
            return Ok(record);
        }
    };
    record.branch = Some(branch.clone());
    record.commit_sha = Some(commit_sha.clone());
    info!("promotion {id}: committed {} on {branch}", &commit_sha[..12.min(commit_sha.len())]);

    if want_merge {
        if git::is_clean(project_root).unwrap_or(false) {
            match git::merge_ff_only(project_root, &branch) {
                Ok(()) => {
                    record.merged = true;
                    record.outcome = PromotionOutcome::Merged;
                    record.reasons.push("fast-forward merged into the active branch".to_owned());
                }
                Err(err) => {
                    record.outcome = PromotionOutcome::BranchOnly;
                    record.reasons.push(format!(
                        "merge skipped (not fast-forwardable): {err:#}; branch left for manual merge"
                    ));
                }
            }
        } else {
            record.outcome = PromotionOutcome::BranchOnly;
            record
                .reasons
                .push("working tree not clean; branch left for manual merge".to_owned());
        }
    } else {
        record.outcome = PromotionOutcome::BranchOnly;
    }

    record.save(project_root)?;
    Ok(record)
}

/// One-click merge of a branch-only promotion, on explicit operator demand.
/// The gate never reopens: the certificate must still be authentic (valid,
/// checksum intact, Ed25519 signature verified), the promotion untouched,
/// and the working tree clean. Fast-forward only, like every merge here.
pub fn merge(project_root: &Path, id: &str) -> Result<PromotionRecord> {
    let mut record = PromotionRecord::load(project_root, id)?;
    if record.rolled_back_at.is_some() {
        bail!("promotion {id} was rolled back; nothing to merge");
    }
    if record.merged {
        bail!("promotion {id} is already merged");
    }
    if record.outcome != PromotionOutcome::BranchOnly {
        bail!("promotion {id} has no merge-ready branch (outcome {:?})", record.outcome);
    }
    let branch = record.branch.clone().context("promotion record has no branch")?;

    let certificate = record.certificate.as_ref().context("promotion has no certificate")?;
    if !certificate.is_authentic() {
        bail!(
            "certificate of {id} failed authenticity (valid: {}, checksum: {}, signature: {})",
            certificate.is_valid(),
            certificate.checksum_matches(),
            certificate.signature_valid()
        );
    }

    if !git::is_repo(project_root) {
        bail!("workspace is not a git repository");
    }
    if !git::is_clean(project_root).unwrap_or(false) {
        bail!("working tree has uncommitted changes; commit or stash them first");
    }

    git::merge_ff_only(project_root, &branch)
        .with_context(|| format!("fast-forward merging {branch}"))?;
    record.merged = true;
    record.outcome = PromotionOutcome::Merged;
    record.reasons.push("merged on explicit operator demand (one-click merge)".to_owned());
    record.save(project_root)?;
    info!("promotion {id}: one-click merged {branch}");
    Ok(record)
}

/// Undo a promotion. Merged changes are reverted with an inverse commit;
/// branch-only promotions have their branch deleted.
pub fn rollback(project_root: &Path, id: &str) -> Result<PromotionRecord> {
    let mut record = PromotionRecord::load(project_root, id)?;
    if record.rolled_back_at.is_some() {
        bail!("promotion {id} was already rolled back");
    }
    if !git::is_repo(project_root) {
        bail!("workspace is not a git repository");
    }

    match record.outcome {
        PromotionOutcome::Merged => {
            let sha = record
                .commit_sha
                .as_ref()
                .context("merged record without a commit sha")?;
            git::revert(project_root, sha).context("reverting the merged commit")?;
            if let Some(branch) = &record.branch {
                git::delete_branch(project_root, branch).ok();
            }
            info!("promotion {id}: reverted merged commit");
        }
        PromotionOutcome::BranchOnly => {
            if let Some(branch) = &record.branch {
                git::delete_branch(project_root, branch)
                    .with_context(|| format!("deleting branch {branch}"))?;
            }
            info!("promotion {id}: deleted branch");
        }
        PromotionOutcome::Blocked => {
            warn!("promotion {id} was blocked; nothing to roll back");
        }
    }

    record.rolled_back_at = Some(crate::proof::now_epoch());
    record.save(project_root)?;
    Ok(record)
}

/// List promotion record ids, newest last.
pub fn list(project_root: &Path) -> Vec<String> {
    let dir = super::model::promotions_dir(project_root);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
        .filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect();
    ids.sort();
    ids
}

fn commit_message(
    report: &FixReport,
    candidate: &FixCandidate,
    cert: &super::model::Certificate,
) -> String {
    format!(
        "navin evolve: fix {finding}\n\n\
         {rationale}\n\n\
         Candidate: {candidate}\n\
         Family: {family}\n\
         Robustness {before} -> {after} (verdict {verdict:?})\n\
         Certificate: {checksum}\n\n\
         Prepared by navin-engine {version}. Review before keeping.",
        finding = report.target_finding,
        rationale = candidate.rationale,
        candidate = candidate.id,
        family = candidate.family,
        before = cert.score_before,
        after = cert.score_after,
        verdict = cert.verdict_after,
        checksum = cert.checksum,
        version = crate::ENGINE_VERSION,
    )
}

/// Git-safe branch/id token: keep alnum, dash, underscore; collapse rest.
fn sanitize(finding: &str) -> String {
    let mut out: String = finding
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').to_owned()
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_makes_a_git_safe_token() {
        assert_eq!(sanitize("crash.load"), "crash-load");
        assert_eq!(sanitize("error_rate.load"), "error_rate-load");
        assert_eq!(sanitize("weird//name.."), "weird-name");
    }
}
