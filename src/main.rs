//! navin-engine CLI.
//!
//! Sprint 1 surface:
//!   navin-engine inspect [PATH]   discovery, prints the ProjectManifest
//!   navin-engine daemon [PATH]    run the Evolve daemon for a workspace
//!   navin-engine status [PATH]    query a running daemon over IPC
//! Sprint 2 surface:
//!   navin-engine baseline [PATH]  measure build/startup/latency/CPU/RSS
//!   navin-engine shadow ...       create/list/destroy/sweep shadows

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

use navin_engine::baseline::collector::{collect_baseline, collect_in_shadow};
use navin_engine::baseline::BaselineOptions;
use navin_engine::daemon::run_daemon;
use navin_engine::diagnose::diagnose_project;
use navin_engine::evolve::{run_evolve, EvolveContext};
use navin_engine::fix::{
    run_fix, BridgeGenerator, FixCandidate, FixContext, FixGenerator, FixReport, GateConfig,
    ProvidedPatchGenerator,
};
use navin_engine::ipc::server::call;
use navin_engine::progress::NoopSink;
use navin_engine::proof::ProofReport;
use navin_engine::promote::{self, git as promote_git};
use navin_engine::policy::config::EvolveConfig;
use navin_engine::project::inspect_project;
use navin_engine::proof::{run_proof_in_shadow, ProofPlan};
use navin_engine::shadow::ShadowManager;

#[derive(Parser)]
#[command(name = "navin-engine", version, about = "Navin Evolve engine")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Discover how a project is built, tested and run.
    Inspect {
        /// Workspace root (defaults to the current directory).
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Run the Evolve daemon for a workspace.
    Daemon {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Show the status of a running daemon.
    Status {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Print the effective policy (defaults merged with .navin/evolve.toml).
    Policy {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Measure a baseline: build time, startup, latency P50/P95/P99, CPU, RSS.
    Baseline {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Run in an isolated shadow workspace instead of the project itself.
        #[arg(long)]
        shadow: bool,
        /// Build command (defaults to the one discovered by inspect).
        #[arg(long)]
        build: Option<String>,
        /// Start command (defaults to the one discovered by inspect).
        #[arg(long)]
        start: Option<String>,
        /// Local URL to probe, e.g. http://127.0.0.1:3000/
        #[arg(long)]
        url: Option<String>,
        /// Probe duration in seconds.
        #[arg(long, default_value_t = 10)]
        duration: u64,
        /// Skip the build step even if a build command is known.
        #[arg(long)]
        no_build: bool,
    },
    /// Manage shadow workspaces under .navin/shadow/.
    Shadow {
        #[command(subcommand)]
        action: ShadowAction,
    },
    /// Prove robustness: inject faults in a shadow and score the result.
    Proof {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Start command (defaults to the one discovered by inspect).
        #[arg(long)]
        start: Option<String>,
        /// Local URL to probe, e.g. http://127.0.0.1:3000/
        #[arg(long)]
        url: String,
        /// quick | standard | deep
        #[arg(long, default_value = "standard")]
        profile: String,
    },
    /// Diagnose root causes from a proof (runs one, or reads --report).
    Diagnose {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Diagnose an existing proof report JSON instead of running one.
        #[arg(long)]
        report: Option<PathBuf>,
        /// Start command (defaults to the one discovered by inspect).
        #[arg(long)]
        start: Option<String>,
        /// Local URL to probe, e.g. http://127.0.0.1:3000/
        #[arg(long)]
        url: Option<String>,
        /// quick | standard | deep
        #[arg(long, default_value = "standard")]
        profile: String,
    },
    /// Verify candidate patches against a finding in a shadow and propose
    /// the one that measurably helps (never writes the workspace).
    Fix {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Finding id to fix, e.g. `crash.load`.
        #[arg(long)]
        finding: String,
        /// JSON file: an array of fix candidates.
        #[arg(long)]
        candidates: PathBuf,
        /// Start command (defaults to the one discovered by inspect).
        #[arg(long)]
        start: Option<String>,
        /// Local URL to probe, e.g. http://127.0.0.1:3000/
        #[arg(long)]
        url: String,
        /// quick | standard | deep
        #[arg(long, default_value = "quick")]
        profile: String,
        /// Test command (defaults to the one discovered by inspect).
        #[arg(long)]
        test: Option<String>,
    },
    /// Promote an accepted fix under policy: branch + certificate, and
    /// merge only if the policy allows it. Never rewrites your tree blindly.
    Promote {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Finding whose accepted proposal should be promoted.
        #[arg(long)]
        finding: String,
        /// Fix report JSON (defaults to .navin/fixes/<HEAD>.json).
        #[arg(long)]
        fix_report: Option<PathBuf>,
        /// Candidate proposal JSON (defaults to the accepted proposal).
        #[arg(long)]
        candidate: Option<PathBuf>,
    },
    /// Roll back a promotion by id (revert merge, or delete the branch).
    Rollback {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        id: String,
    },
    /// List recorded promotions.
    Promotions {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Verify a promotion certificate: gate outcome, integrity, signature.
    VerifyCert {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Promotion record id (see `promotions`).
        #[arg(long)]
        id: String,
    },
    /// Run the whole loop: prove, diagnose, then generate/verify/promote
    /// fixes for the serious findings, all under policy.
    Evolve {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Start command (defaults to the one discovered by inspect).
        #[arg(long)]
        start: Option<String>,
        /// Local URL to probe, e.g. http://127.0.0.1:3000/
        #[arg(long)]
        url: String,
        /// quick | standard | deep
        #[arg(long, default_value = "quick")]
        profile: String,
        /// Max number of findings to attempt in one run.
        #[arg(long, default_value_t = 3)]
        max_findings: usize,
        /// Optional candidates JSON (used when no generator is configured).
        #[arg(long)]
        candidates: Option<PathBuf>,
        /// Test command (defaults to the one discovered by inspect).
        #[arg(long)]
        test: Option<String>,
    },
}

#[derive(Subcommand)]
enum ShadowAction {
    /// Create a shadow workspace pinned to the current commit.
    Create {
        run_id: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// List existing shadows.
    List {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Destroy one shadow.
    Destroy {
        run_id: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// Remove every leftover shadow (crash recovery).
    Sweep {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("NAVIN_ENGINE_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

fn main() -> Result<()> {
    init_logging();
    let cli = Cli::parse();
    match cli.command {
        Command::Inspect { path } => {
            let manifest = inspect_project(&path)?;
            println!("{}", serde_json::to_string_pretty(&manifest)?);
            Ok(())
        }
        Command::Policy { path } => {
            let root = path.canonicalize()?;
            let config = EvolveConfig::load(&root)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
            Ok(())
        }
        Command::Daemon { path } => runtime()?.block_on(run_daemon(&path)),
        Command::Status { path } => {
            let root = path.canonicalize()?;
            let engine_dir = navin_engine::engine_dir(&root);
            let status = runtime()?.block_on(call(&engine_dir, "engine.status", json!({})))?;
            println!("{}", serde_json::to_string_pretty(&status)?);
            Ok(())
        }
        Command::Baseline { path, shadow, build, start, url, duration, no_build } => {
            let root = path.canonicalize()?;
            let mut opts = BaselineOptions::defaults();
            opts.probe_duration = Duration::from_secs(duration);

            // Flags win; otherwise fall back to what discovery found.
            let manifest = inspect_project(&root)?;
            let discovered = manifest.units.first().map(|u| u.commands.clone());
            opts.build_cmd = build.or_else(|| discovered.as_ref().and_then(|c| c.build.clone()));
            opts.start_cmd = start.or_else(|| discovered.as_ref().and_then(|c| c.start.clone()));
            opts.url = url;
            if no_build {
                opts.build_cmd = None;
            }

            let report = if shadow {
                let run_id = format!("baseline-cli-{}", std::process::id());
                runtime()?.block_on(collect_in_shadow(&root, &run_id, &opts))?
            } else {
                runtime()?.block_on(collect_baseline(&root, &root, &opts))?
            };
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Command::Shadow { action } => match action {
            ShadowAction::Create { run_id, path } => {
                let shadow = ShadowManager::new(&path.canonicalize()?).create(&run_id)?;
                println!("{}", serde_json::to_string_pretty(&shadow)?);
                Ok(())
            }
            ShadowAction::List { path } => {
                let ids = ShadowManager::new(&path.canonicalize()?).list();
                println!("{}", serde_json::to_string_pretty(&ids)?);
                Ok(())
            }
            ShadowAction::Destroy { run_id, path } => {
                ShadowManager::new(&path.canonicalize()?).destroy(&run_id)?;
                println!("destroyed {run_id}");
                Ok(())
            }
            ShadowAction::Sweep { path } => {
                let swept = ShadowManager::new(&path.canonicalize()?).cleanup_stale();
                println!("swept {swept} stale shadow(s)");
                Ok(())
            }
        },
        Command::Proof { path, start, url, profile } => {
            let root = path.canonicalize()?;
            let manifest = inspect_project(&root)?;
            let start_cmd = start
                .or_else(|| manifest.units.first().and_then(|u| u.commands.start.clone()))
                .ok_or_else(|| {
                    anyhow::anyhow!("no start command: pass --start \"...\"")
                })?;
            let plan = ProofPlan::for_profile(&profile, 512);
            let run_id = format!("proof-cli-{}", std::process::id());
            let report = runtime()?.block_on(run_proof_in_shadow(
                &root,
                &run_id,
                &start_cmd,
                &url,
                &plan,
                Duration::from_secs(60),
                None,
                &NoopSink,
            ))?;
            report.save(&root)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Command::Diagnose { path, report, start, url, profile } => {
            let root = path.canonicalize()?;
            let diagnosis = if let Some(report_path) = report {
                // Diagnose an existing report; correlate with its log if present.
                let text = std::fs::read_to_string(&report_path)?;
                let parsed: ProofReport = serde_json::from_str(&text)?;
                diagnose_project(&root, &parsed)
            } else {
                // Run a fresh proof, then diagnose it.
                let url = url.ok_or_else(|| {
                    anyhow::anyhow!("provide --url to run a proof, or --report to diagnose an existing one")
                })?;
                let manifest = inspect_project(&root)?;
                let start_cmd = start
                    .or_else(|| manifest.units.first().and_then(|u| u.commands.start.clone()))
                    .ok_or_else(|| anyhow::anyhow!("no start command: pass --start \"...\""))?;
                let plan = ProofPlan::for_profile(&profile, 512);
                let run_id = format!("diagnose-cli-{}", std::process::id());
                let proof = runtime()?.block_on(run_proof_in_shadow(
                    &root,
                    &run_id,
                    &start_cmd,
                    &url,
                    &plan,
                    Duration::from_secs(60),
                    None,
                    &NoopSink,
                ))?;
                proof.save(&root)?;
                diagnose_project(&root, &proof)
            };
            diagnosis.save(&root)?;
            println!("{}", serde_json::to_string_pretty(&diagnosis)?);
            Ok(())
        }
        Command::Fix { path, finding, candidates, start, url, profile, test } => {
            let root = path.canonicalize()?;
            let manifest = inspect_project(&root)?;
            let start_cmd = start
                .or_else(|| manifest.units.first().and_then(|u| u.commands.start.clone()))
                .ok_or_else(|| anyhow::anyhow!("no start command: pass --start \"...\""))?;
            let test_cmd =
                test.or_else(|| manifest.units.first().and_then(|u| u.commands.test.clone()));
            let candidates: Vec<FixCandidate> =
                serde_json::from_str(&std::fs::read_to_string(&candidates)?)
                    .context("candidates file must be a JSON array of fix candidates")?;
            let ctx = FixContext {
                start_cmd,
                url,
                plan: ProofPlan::for_profile(&profile, 512),
                ready_timeout: Duration::from_secs(60),
                limits: None,
                test_cmd,
            };
            let generator = ProvidedPatchGenerator::new(candidates);
            let report = runtime()?.block_on(run_fix(
                &root,
                &ctx,
                &finding,
                &generator,
                &GateConfig::default(),
                &NoopSink,
            ))?;
            report.save(&root)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Command::Promote { path, finding, fix_report, candidate } => {
            let root = path.canonicalize()?;
            let config = EvolveConfig::load(&root)?;

            // Fix report: explicit path, or the one saved for HEAD.
            let report: FixReport = match fix_report {
                Some(p) => serde_json::from_str(&std::fs::read_to_string(&p)?)?,
                None => {
                    let commit = promote_git::head_sha(&root)
                        .context("need a git repo (or pass --fix-report)")?;
                    let p = root.join(".navin/fixes").join(format!("{commit}.json"));
                    serde_json::from_str(
                        &std::fs::read_to_string(&p)
                            .with_context(|| format!("no fix report at {}", p.display()))?,
                    )?
                }
            };

            // Candidate: explicit path, or the accepted proposal.
            let candidate: FixCandidate = match candidate {
                Some(p) => serde_json::from_str(&std::fs::read_to_string(&p)?)?,
                None => {
                    let accepted = report
                        .accepted
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("fix report has no accepted candidate"))?;
                    let p = root
                        .join(".navin/fixes/proposals")
                        .join(format!("{accepted}.json"));
                    serde_json::from_str(
                        &std::fs::read_to_string(&p)
                            .with_context(|| format!("no proposal at {}", p.display()))?,
                    )?
                }
            };

            anyhow::ensure!(
                report.target_finding == finding,
                "fix report targets `{}`, not `{finding}`",
                report.target_finding
            );
            let record = promote::promote(&root, &report, &candidate, &config)?;
            println!("{}", serde_json::to_string_pretty(&record)?);
            Ok(())
        }
        Command::Rollback { path, id } => {
            let root = path.canonicalize()?;
            let record = promote::rollback(&root, &id)?;
            println!("{}", serde_json::to_string_pretty(&record)?);
            Ok(())
        }
        Command::Promotions { path } => {
            let root = path.canonicalize()?;
            println!("{}", serde_json::to_string_pretty(&promote::list(&root))?);
            Ok(())
        }
        Command::VerifyCert { path, id } => {
            let root = path.canonicalize()?;
            let record = promote::PromotionRecord::load(&root, &id)?;
            let Some(cert) = record.certificate else {
                anyhow::bail!("promotion {id} carries no certificate");
            };
            let report = json!({
                "promotion": id,
                "finding": cert.finding,
                "candidate": cert.candidate_id,
                "score": { "before": cert.score_before, "after": cert.score_after },
                "gate_valid": cert.is_valid(),
                "checksum_ok": cert.checksum_matches(),
                "signature_ok": cert.signature_valid(),
                "authentic": cert.is_authentic(),
                "public_key": cert.public_key,
            });
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Command::Evolve { path, start, url, profile, max_findings, candidates, test } => {
            let root = path.canonicalize()?;
            let config = EvolveConfig::load(&root)?;
            let manifest = inspect_project(&root)?;
            let start_cmd = start
                .or_else(|| manifest.units.first().and_then(|u| u.commands.start.clone()))
                .ok_or_else(|| anyhow::anyhow!("no start command: pass --start \"...\""))?;
            let test_cmd =
                test.or_else(|| manifest.units.first().and_then(|u| u.commands.test.clone()));

            // Pick the generator: the configured bridge wins; otherwise fall
            // back to provided candidates (handy for tests / offline runs).
            let generator: Box<dyn FixGenerator> = if !config.evolve.generator.command.is_empty() {
                Box::new(BridgeGenerator::new(
                    config.evolve.generator.command.clone(),
                    Duration::from_secs(config.evolve.generator.timeout_secs),
                ))
            } else {
                let list: Vec<FixCandidate> = match candidates {
                    Some(p) => serde_json::from_str(&std::fs::read_to_string(&p)?)
                        .context("candidates file must be a JSON array")?,
                    None => Vec::new(),
                };
                Box::new(ProvidedPatchGenerator::new(list))
            };

            let ctx = EvolveContext {
                start_cmd,
                url,
                profile,
                ready_timeout: Duration::from_secs(60),
                limits: None,
                max_findings,
                test_cmd,
            };
            let report =
                runtime()?.block_on(run_evolve(&root, &ctx, generator.as_ref(), &config, &NoopSink))?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}
