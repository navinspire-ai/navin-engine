//! Drains the scheduler queue. Sprint 1 knows one real job kind
//! (`project.inspect`); proof and evolve campaigns plug in here later.

use anyhow::{Context, Result};
use serde_json::json;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::fix::{FixCandidate, FixReport};
use crate::ipc::events::EventBus;
use crate::ipc::protocol::Event;
use crate::policy::config::EvolveConfig;
use crate::progress::ProgressSink;
use crate::project::inspect_project;
use crate::promote::{self, PromotionRecord};

use super::scheduler::{Job, JobState, Scheduler};

/// Bridges engine progress onto the daemon event bus. Every event is tagged
/// with the job id so an IPC client can follow several runs at once.
struct BusSink {
    bus: EventBus,
    job: u64,
    kind: String,
}

impl ProgressSink for BusSink {
    fn emit(&self, stage: &str, event: &str, data: serde_json::Value) {
        self.bus.publish(Event::new(
            "run.progress",
            json!({
                "job": self.job,
                "kind": self.kind,
                "stage": stage,
                "event": event,
                "data": data,
            }),
        ));
    }
}

/// Run the full evolve pipeline as a daemon job, choosing the generator
/// from the workspace policy (the configured bridge, or none).
async fn run_evolve_job(
    root: &Path,
    start: String,
    url: String,
    profile: String,
    max_findings: usize,
    test: Option<String>,
    sink: &dyn ProgressSink,
) -> Result<crate::evolve::EvolveReport> {
    let config = EvolveConfig::load(root)?;
    // Fall back to the manifest's test command, like the CLI does.
    let test_cmd = test.or_else(|| {
        inspect_project(root)
            .ok()
            .and_then(|m| m.units.first().and_then(|u| u.commands.test.clone()))
    });
    let generator: Box<dyn crate::fix::FixGenerator> =
        if config.evolve.generator.command.is_empty() {
            Box::new(crate::fix::ProvidedPatchGenerator::new(Vec::new()))
        } else {
            Box::new(crate::fix::BridgeGenerator::new(
                config.evolve.generator.command.clone(),
                std::time::Duration::from_secs(config.evolve.generator.timeout_secs),
            ))
        };
    let ctx = crate::evolve::EvolveContext {
        start_cmd: start,
        url,
        profile,
        ready_timeout: std::time::Duration::from_secs(60),
        limits: None,
        max_findings,
        test_cmd,
    };
    crate::evolve::run_evolve(root, &ctx, generator.as_ref(), &config, sink).await
}

/// Load the HEAD fix report and its accepted proposal, then promote it.
fn promote_from_disk(root: &Path, finding: &str) -> Result<PromotionRecord> {
    let config = EvolveConfig::load(root)?;
    let commit = promote::git::head_sha(root).context("workspace must be a git repo to promote")?;
    let report_path = root.join(".navin/fixes").join(format!("{commit}.json"));
    let report: FixReport = serde_json::from_str(
        &std::fs::read_to_string(&report_path)
            .with_context(|| format!("no fix report at {}", report_path.display()))?,
    )?;
    anyhow::ensure!(
        report.target_finding == finding,
        "fix report targets `{}`, not `{finding}`",
        report.target_finding
    );
    let accepted = report
        .accepted
        .clone()
        .context("fix report has no accepted candidate")?;
    let proposal_path = root
        .join(".navin/fixes/proposals")
        .join(format!("{accepted}.json"));
    let candidate: FixCandidate = serde_json::from_str(
        &std::fs::read_to_string(&proposal_path)
            .with_context(|| format!("no proposal at {}", proposal_path.display()))?,
    )?;
    promote::promote(root, &report, &candidate, &config)
}

pub async fn run_worker(
    mut rx: mpsc::Receiver<Job>,
    scheduler: Scheduler,
    events: EventBus,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return;
                }
            }
            job = rx.recv() => {
                let Some(job) = job else { return };
                execute(job, &scheduler, &events).await;
            }
        }
    }
}

async fn execute(job: Job, scheduler: &Scheduler, events: &EventBus) {
    scheduler.set_state(job.id, JobState::Running, None);
    events.publish(Event::new("run.started", json!({ "job": job.id, "kind": job.kind })));
    let sink = BusSink { bus: events.clone(), job: job.id, kind: job.kind.clone() };

    let outcome = match job.kind.as_str() {
        "project.inspect" => {
            let root = job
                .params
                .get("path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            // Discovery reads many small files; keep the runtime responsive.
            tokio::task::spawn_blocking(move || inspect_project(&root))
                .await
                .map_err(|e| e.to_string())
                .and_then(|res| res.map_err(|e| format!("{e:#}")))
                .map(|manifest| serde_json::to_value(manifest).unwrap_or_default())
        }
        "baseline.run" => {
            let root = job
                .params
                .get("path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            let mut opts = crate::baseline::BaselineOptions::defaults();
            opts.build_cmd = job.params.get("build").and_then(|v| v.as_str()).map(String::from);
            opts.start_cmd = job.params.get("start").and_then(|v| v.as_str()).map(String::from);
            opts.url = job.params.get("url").and_then(|v| v.as_str()).map(String::from);
            if let Some(secs) = job.params.get("duration").and_then(|v| v.as_u64()) {
                opts.probe_duration = std::time::Duration::from_secs(secs);
            }
            let run_id = format!("baseline-{}", job.id);
            crate::baseline::collector::collect_in_shadow(&root, &run_id, &opts)
                .await
                .map_err(|e| format!("{e:#}"))
                .map(|report| serde_json::to_value(report).unwrap_or_default())
        }
        "proof.run" => {
            let root = job
                .params
                .get("path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            let start = job.params.get("start").and_then(|v| v.as_str()).map(String::from);
            let url = job.params.get("url").and_then(|v| v.as_str()).map(String::from);
            let profile = job
                .params
                .get("profile")
                .and_then(|v| v.as_str())
                .unwrap_or("standard")
                .to_owned();
            match (start, url) {
                (Some(start), Some(url)) => {
                    let plan = crate::proof::ProofPlan::for_profile(&profile, 512);
                    let run_id = format!("proof-{}", job.id);
                    crate::proof::run_proof_in_shadow(
                        &root,
                        &run_id,
                        &start,
                        &url,
                        &plan,
                        std::time::Duration::from_secs(60),
                        None,
                        &sink,
                    )
                    .await
                    .and_then(|report| {
                        report.save(&root)?;
                        Ok(report)
                    })
                    .map_err(|e| format!("{e:#}"))
                    .map(|report| serde_json::to_value(report).unwrap_or_default())
                }
                _ => Err("proof.run requires both `start` and `url`".to_owned()),
            }
        }
        "diagnose.run" => {
            let root = job
                .params
                .get("path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            let start = job.params.get("start").and_then(|v| v.as_str()).map(String::from);
            let url = job.params.get("url").and_then(|v| v.as_str()).map(String::from);
            let profile = job
                .params
                .get("profile")
                .and_then(|v| v.as_str())
                .unwrap_or("standard")
                .to_owned();
            match (start, url) {
                (Some(start), Some(url)) => {
                    let plan = crate::proof::ProofPlan::for_profile(&profile, 512);
                    let run_id = format!("diagnose-{}", job.id);
                    crate::proof::run_proof_in_shadow(
                        &root,
                        &run_id,
                        &start,
                        &url,
                        &plan,
                        std::time::Duration::from_secs(60),
                        None,
                        &sink,
                    )
                    .await
                    .and_then(|report| {
                        report.save(&root)?;
                        let diagnosis = crate::diagnose::diagnose_project(&root, &report);
                        diagnosis.save(&root)?;
                        Ok(diagnosis)
                    })
                    .map_err(|e| format!("{e:#}"))
                    .map(|diagnosis| serde_json::to_value(diagnosis).unwrap_or_default())
                }
                _ => Err("diagnose.run requires both `start` and `url`".to_owned()),
            }
        }
        "fix.run" => {
            let root = job
                .params
                .get("path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            let start = job.params.get("start").and_then(|v| v.as_str()).map(String::from);
            let url = job.params.get("url").and_then(|v| v.as_str()).map(String::from);
            let finding = job.params.get("finding").and_then(|v| v.as_str()).map(String::from);
            let profile = job
                .params
                .get("profile")
                .and_then(|v| v.as_str())
                .unwrap_or("quick")
                .to_owned();
            let candidates: Result<Vec<crate::fix::FixCandidate>, String> = job
                .params
                .get("candidates")
                .cloned()
                .map(|v| serde_json::from_value(v).map_err(|e| format!("invalid candidates: {e}")))
                .unwrap_or_else(|| Ok(Vec::new()));
            match (start, url, finding, candidates) {
                (Some(start), Some(url), Some(finding), Ok(candidates)) => {
                    let test_cmd =
                        job.params.get("test").and_then(|v| v.as_str()).map(String::from);
                    let ctx = crate::fix::FixContext {
                        start_cmd: start,
                        url,
                        plan: crate::proof::ProofPlan::for_profile(&profile, 512),
                        ready_timeout: std::time::Duration::from_secs(60),
                        limits: None,
                        test_cmd,
                    };
                    let generator = crate::fix::ProvidedPatchGenerator::new(candidates);
                    crate::fix::run_fix(
                        &root,
                        &ctx,
                        &finding,
                        &generator,
                        &crate::fix::GateConfig::default(),
                        &sink,
                    )
                    .await
                        .and_then(|report| {
                            report.save(&root)?;
                            Ok(report)
                        })
                        .map_err(|e| format!("{e:#}"))
                        .map(|report| serde_json::to_value(report).unwrap_or_default())
                }
                (_, _, _, Err(message)) => Err(message),
                _ => Err("fix.run requires `start`, `url` and `finding`".to_owned()),
            }
        }
        "evolve.run" => {
            let root = job
                .params
                .get("path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            let start = job.params.get("start").and_then(|v| v.as_str()).map(String::from);
            let url = job.params.get("url").and_then(|v| v.as_str()).map(String::from);
            let profile = job
                .params
                .get("profile")
                .and_then(|v| v.as_str())
                .unwrap_or("quick")
                .to_owned();
            let max_findings = job
                .params
                .get("max_findings")
                .and_then(|v| v.as_u64())
                .unwrap_or(3) as usize;
            let test = job.params.get("test").and_then(|v| v.as_str()).map(String::from);
            match (start, url) {
                (Some(start), Some(url)) => {
                    run_evolve_job(&root, start, url, profile, max_findings, test, &sink)
                        .await
                        .map_err(|e| format!("{e:#}"))
                        .map(|report| serde_json::to_value(report).unwrap_or_default())
                }
                _ => Err("evolve.run requires `start` and `url`".to_owned()),
            }
        }
        "promote.run" => {
            let root = job
                .params
                .get("path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            let finding = job.params.get("finding").and_then(|v| v.as_str()).map(String::from);
            match finding {
                Some(finding) => promote_from_disk(&root, &finding)
                    .map_err(|e| format!("{e:#}"))
                    .map(|record| serde_json::to_value(record).unwrap_or_default()),
                None => Err("promote.run requires `finding`".to_owned()),
            }
        }
        "rollback.run" => {
            let root = job
                .params
                .get("path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            match job.params.get("id").and_then(|v| v.as_str()) {
                Some(id) => crate::promote::rollback(&root, id)
                    .map_err(|e| format!("{e:#}"))
                    .map(|record| serde_json::to_value(record).unwrap_or_default()),
                None => Err("rollback.run requires `id`".to_owned()),
            }
        }
        other => Err(format!("unknown job kind: {other}")),
    };

    match outcome {
        Ok(result) => {
            info!("job {} ({}) completed", job.id, job.kind);
            scheduler.set_state(job.id, JobState::Completed, None);
            events.publish(Event::new(
                "run.completed",
                json!({ "job": job.id, "kind": job.kind, "result": result }),
            ));
        }
        Err(message) => {
            warn!("job {} ({}) failed: {message}", job.id, job.kind);
            scheduler.set_state(job.id, JobState::Failed, Some(message.clone()));
            events.publish(Event::new(
                "run.failed",
                json!({ "job": job.id, "kind": job.kind, "error": message }),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_sink_tags_progress_events_with_the_job() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let sink = BusSink { bus: bus.clone(), job: 7, kind: "proof.run".to_owned() };
        sink.emit("proof", "started", json!({ "profile": "quick" }));

        let event = rx.try_recv().expect("one event on the bus");
        assert_eq!(event.event, "run.progress");
        assert_eq!(event.payload["job"], 7);
        assert_eq!(event.payload["kind"], "proof.run");
        assert_eq!(event.payload["stage"], "proof");
        assert_eq!(event.payload["event"], "started");
        assert_eq!(event.payload["data"]["profile"], "quick");
    }
}
