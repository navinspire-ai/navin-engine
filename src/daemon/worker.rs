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

/// Start command and probe URL for a job. Both parameters are optional in
/// the protocol: what the caller omits, the engine works out from the
/// project itself, so a campaign can be launched with a path alone.
async fn job_target(
    root: &Path,
    params: &serde_json::Value,
    sink: &dyn ProgressSink,
) -> Result<crate::target::Target, String> {
    let field = |name: &str| params.get(name).and_then(|v| v.as_str()).map(String::from);
    crate::target::resolve(root, field("start"), field("url"), sink)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Build the candidate generator from policy: the configured bridge (with
/// the operator's per-campaign model preset, when given) or none.
fn make_generator(
    config: &EvolveConfig,
    preset: Option<String>,
) -> Box<dyn crate::fix::FixGenerator> {
    if config.evolve.generator.command.is_empty() {
        Box::new(crate::fix::ProvidedPatchGenerator::new(Vec::new()))
    } else {
        Box::new(
            crate::fix::BridgeGenerator::new(
                config.evolve.generator.command.clone(),
                std::time::Duration::from_secs(config.evolve.generator.timeout_secs),
            )
            .with_preset(preset),
        )
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
    preset: Option<String>,
    sink: &dyn ProgressSink,
) -> Result<crate::evolve::EvolveReport> {
    let config = EvolveConfig::load(root)?;
    // Fall back to the manifest's test command, like the CLI does.
    let test_cmd = test.or_else(|| {
        inspect_project(root)
            .ok()
            .and_then(|m| m.units.first().and_then(|u| u.commands.test.clone()))
    });
    let generator = make_generator(&config, preset);
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

/// Run an optimization campaign as a daemon job.
async fn run_optimize_job(
    root: &Path,
    start: String,
    url: String,
    objective: String,
    test: Option<String>,
    params: &serde_json::Value,
    sink: &dyn ProgressSink,
) -> Result<crate::optimize::OptimizeReport> {
    let config = EvolveConfig::load(root)?;
    let preset = params
        .get("preset")
        .and_then(|v| v.as_str())
        .map(String::from);
    let generator = make_generator(&config, preset);
    let test_cmd = test.or_else(|| {
        inspect_project(root)
            .ok()
            .and_then(|m| m.units.first().and_then(|u| u.commands.test.clone()))
    });
    let ctx = crate::optimize::OptimizeContext {
        start_cmd: start,
        url,
        ready_timeout: std::time::Duration::from_secs(60),
        limits: None,
        test_cmd,
        bench_duration: std::time::Duration::from_secs(
            params.get("duration").and_then(|v| v.as_u64()).unwrap_or(10),
        ),
        bench_concurrency: params
            .get("concurrency")
            .and_then(|v| v.as_u64())
            .unwrap_or(16) as usize,
        bench_repeats: params.get("repeats").and_then(|v| v.as_u64()).unwrap_or(3) as usize,
        max_variants: params.get("max_variants").and_then(|v| v.as_u64()).unwrap_or(4) as usize,
        min_gain_percent: params.get("min_gain").and_then(|v| v.as_f64()).unwrap_or(5.0),
        objective: crate::optimize::Objective::parse(&objective)?,
        diff_vectors: params.get("diff_vectors").and_then(|v| v.as_u64()).unwrap_or(24) as usize,
    };
    crate::optimize::run_optimize(root, &ctx, generator.as_ref(), &config, sink).await
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
            // A baseline still means something without a running app, so a
            // target we cannot resolve degrades instead of failing.
            if let Ok(target) = job_target(&root, &job.params, &sink).await {
                opts.start_cmd = Some(target.start_cmd);
                opts.url = Some(target.url);
            }
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
            let profile = job
                .params
                .get("profile")
                .and_then(|v| v.as_str())
                .unwrap_or("standard")
                .to_owned();
            match job_target(&root, &job.params, &sink).await {
                Ok(target) => {
                    let plan = crate::proof::ProofPlan::for_profile(&profile, 512);
                    let run_id = format!("proof-{}", job.id);
                    crate::proof::run_proof_in_shadow(
                        &root,
                        &run_id,
                        &target.start_cmd,
                        &target.url,
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
                Err(message) => Err(message),
            }
        }
        "diagnose.run" => {
            let root = job
                .params
                .get("path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            let profile = job
                .params
                .get("profile")
                .and_then(|v| v.as_str())
                .unwrap_or("standard")
                .to_owned();
            match job_target(&root, &job.params, &sink).await {
                Ok(target) => {
                    let plan = crate::proof::ProofPlan::for_profile(&profile, 512);
                    let run_id = format!("diagnose-{}", job.id);
                    crate::proof::run_proof_in_shadow(
                        &root,
                        &run_id,
                        &target.start_cmd,
                        &target.url,
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
                Err(message) => Err(message),
            }
        }
        "fix.run" => {
            let root = job
                .params
                .get("path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
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
            let target = match finding.is_some() {
                true => Some(job_target(&root, &job.params, &sink).await),
                false => None,
            };
            match (target, finding, candidates) {
                (Some(Ok(target)), Some(finding), Ok(candidates)) => {
                    let test_cmd =
                        job.params.get("test").and_then(|v| v.as_str()).map(String::from);
                    let ctx = crate::fix::FixContext {
                        start_cmd: target.start_cmd,
                        url: target.url,
                        plan: crate::proof::ProofPlan::for_profile(&profile, 512),
                        ready_timeout: std::time::Duration::from_secs(60),
                        limits: None,
                        test_cmd,
                        invariants: EvolveConfig::load(&root)
                            .map(|c| c.invariants)
                            .unwrap_or_default(),
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
                (_, _, Err(message)) | (Some(Err(message)), _, _) => Err(message),
                _ => Err("fix.run requires a `finding`".to_owned()),
            }
        }
        "evolve.run" => {
            let root = job
                .params
                .get("path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
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
            let preset = job.params.get("preset").and_then(|v| v.as_str()).map(String::from);
            match job_target(&root, &job.params, &sink).await {
                Ok(target) => run_evolve_job(
                    &root,
                    target.start_cmd,
                    target.url,
                    profile,
                    max_findings,
                    test,
                    preset,
                    &sink,
                )
                .await
                .map_err(|e| format!("{e:#}"))
                .map(|report| serde_json::to_value(report).unwrap_or_default()),
                Err(message) => Err(message),
            }
        }
        "optimize.run" => {
            let root = job
                .params
                .get("path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            let objective = job
                .params
                .get("objective")
                .and_then(|v| v.as_str())
                .unwrap_or("p95")
                .to_owned();
            let test = job.params.get("test").and_then(|v| v.as_str()).map(String::from);
            match job_target(&root, &job.params, &sink).await {
                Ok(target) => run_optimize_job(
                    &root,
                    target.start_cmd,
                    target.url,
                    objective,
                    test,
                    &job.params,
                    &sink,
                )
                .await
                .map_err(|e| format!("{e:#}"))
                .map(|report| serde_json::to_value(report).unwrap_or_default()),
                Err(message) => Err(message),
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
        "promote.merge" => {
            let root = job
                .params
                .get("path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            match job.params.get("id").and_then(|v| v.as_str()) {
                Some(id) => crate::promote::merge(&root, id)
                    .map_err(|e| format!("{e:#}"))
                    .map(|record| serde_json::to_value(record).unwrap_or_default()),
                None => Err("promote.merge requires `id`".to_owned()),
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
