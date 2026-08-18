//! `.navin/evolve.toml` with safe defaults.
//!
//! Safe mode is the default: nothing is auto-merged, destructive families
//! are off, and resource ceilings are conservative. A missing file means
//! defaults; a broken file is an error rather than silently permissive.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::NAVIN_DIR;

pub const CONFIG_FILE: &str = "evolve.toml";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct EvolveConfig {
    pub proof: ProofSection,
    pub evolve: EvolveSection,
    /// Business invariants: commands that must exit 0 for a candidate to be
    /// promotable. They run inside the shadow, after the test suite.
    ///
    /// ```toml
    /// [[invariants]]
    /// name = "no_duplicate_payments"
    /// command = "python verify_payments.py"
    /// ```
    pub invariants: Vec<InvariantSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvariantSpec {
    pub name: String,
    pub command: String,
    #[serde(default = "default_invariant_timeout")]
    pub timeout_secs: u64,
}

fn default_invariant_timeout() -> u64 {
    120
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProofSection {
    pub enabled: bool,
    /// quick | standard | deep | nightly
    pub profile: String,
}

impl Default for ProofSection {
    fn default() -> Self {
        ProofSection { enabled: true, profile: "standard".to_owned() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EvolveSection {
    pub enabled: bool,
    /// safe | trusted | autonomous. Safe is the public default.
    pub mode: String,
    pub allowed: AllowedFamilies,
    pub promotion: PromotionSection,
    pub resources: ResourceLimits,
    pub budget: BudgetSection,
    pub generator: GeneratorSection,
}

impl Default for EvolveSection {
    fn default() -> Self {
        EvolveSection {
            enabled: false,
            mode: "safe".to_owned(),
            allowed: AllowedFamilies::default(),
            promotion: PromotionSection::default(),
            resources: ResourceLimits::default(),
            budget: BudgetSection::default(),
            generator: GeneratorSection::default(),
        }
    }
}

/// How candidate fixes are synthesised. The engine never calls an LLM
/// itself; it shells out to an external bridge (provided by the desktop
/// app) that receives a finding on stdin and returns candidates on stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneratorSection {
    /// Command line of the bridge, e.g. "python3 -m navin.evolve.bridge".
    /// When empty, no candidates are generated (proof/diagnose only).
    pub command: String,
    /// Hard timeout for one bridge invocation.
    pub timeout_secs: u64,
}

impl Default for GeneratorSection {
    fn default() -> Self {
        GeneratorSection { command: String::new(), timeout_secs: 120 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AllowedFamilies {
    pub performance: bool,
    pub memory: bool,
    pub database: bool,
    pub reliability: bool,
    pub concurrency: bool,
    pub security: bool,
    pub dependencies: bool,
}

impl Default for AllowedFamilies {
    fn default() -> Self {
        AllowedFamilies {
            performance: true,
            memory: true,
            database: true,
            reliability: true,
            concurrency: false,
            // Off by default: these change externally visible behavior.
            security: false,
            dependencies: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PromotionSection {
    pub auto_merge: bool,
}

impl Default for PromotionSection {
    fn default() -> Self {
        PromotionSection { auto_merge: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceLimits {
    pub max_cpu_percent: u8,
    pub max_memory_mb: u64,
    pub max_disk_mb: u64,
    pub max_runtime_minutes: u64,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        ResourceLimits {
            max_cpu_percent: 15,
            max_memory_mb: 512,
            max_disk_mb: 4096,
            max_runtime_minutes: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetSection {
    pub max_candidates: u32,
    pub max_runtime_minutes: u64,
    pub max_llm_cost_usd: f64,
}

impl Default for BudgetSection {
    fn default() -> Self {
        BudgetSection {
            max_candidates: 100,
            max_runtime_minutes: 30,
            max_llm_cost_usd: 2.0,
        }
    }
}

impl EvolveConfig {
    /// Load from `<root>/.navin/evolve.toml`; defaults when absent.
    pub fn load(project_root: &Path) -> Result<Self> {
        let path = project_root.join(NAVIN_DIR).join(CONFIG_FILE);
        if !path.is_file() {
            return Ok(EvolveConfig::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe() {
        let config = EvolveConfig::default();
        assert_eq!(config.evolve.mode, "safe");
        assert!(!config.evolve.promotion.auto_merge);
        assert!(!config.evolve.allowed.security);
        assert!(!config.evolve.allowed.dependencies);
        assert_eq!(config.evolve.resources.max_cpu_percent, 15);
    }

    #[test]
    fn partial_file_keeps_defaults_for_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let navin = tmp.path().join(NAVIN_DIR);
        std::fs::create_dir_all(&navin).unwrap();
        std::fs::write(
            navin.join(CONFIG_FILE),
            "[proof]\nprofile = \"quick\"\n\n[evolve.resources]\nmax_memory_mb = 1024\n",
        )
        .unwrap();

        let config = EvolveConfig::load(tmp.path()).unwrap();
        assert_eq!(config.proof.profile, "quick");
        assert_eq!(config.evolve.resources.max_memory_mb, 1024);
        // Untouched sections keep their safe defaults.
        assert_eq!(config.evolve.mode, "safe");
        assert_eq!(config.evolve.resources.max_cpu_percent, 15);
    }

    #[test]
    fn invariants_are_parsed_with_a_default_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let navin = tmp.path().join(NAVIN_DIR);
        std::fs::create_dir_all(&navin).unwrap();
        std::fs::write(
            navin.join(CONFIG_FILE),
            "[[invariants]]\nname = \"orders\"\ncommand = \"python check.py\"\n\n\
             [[invariants]]\nname = \"payments\"\ncommand = \"sh pay.sh\"\ntimeout_secs = 30\n",
        )
        .unwrap();

        let config = EvolveConfig::load(tmp.path()).unwrap();
        assert_eq!(config.invariants.len(), 2);
        assert_eq!(config.invariants[0].name, "orders");
        assert_eq!(config.invariants[0].timeout_secs, 120);
        assert_eq!(config.invariants[1].timeout_secs, 30);
    }

    #[test]
    fn broken_file_is_an_error_not_permissive_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let navin = tmp.path().join(NAVIN_DIR);
        std::fs::create_dir_all(&navin).unwrap();
        std::fs::write(navin.join(CONFIG_FILE), "not [ valid toml").unwrap();
        assert!(EvolveConfig::load(tmp.path()).is_err());
    }
}
