//! # RuntimeContext
//!
//! Runtime context built at startup from the validated configuration.
//!
//! ## Purpose
//!
//! `RuntimeContext` is the **single source of truth** for configuration-derived
//! runtime parameters. It is built once from a `ResolvedConfig` and then shared
//! (via `Arc`) across all runtime subsystems.
//!
//! The following configuration dimensions are surfaced:
//!
//! - **Profile budgets** — max tiles, max texture MB, max agents, target/min FPS.
//! - **Agent registry** — per-agent `allow`-derived permissions and resolved
//!   PSKs from `[agents.<id>]`, exposed as an [`AgentDirectory`].
//!
//! Every config section is frozen; a restart is required to change any of
//! them. `hot` (an empty `HotReloadableConfig` behind an `ArcSwap`) remains
//! as the reload seam for SIGHUP / `ReloadConfig`.
//!
//! An agent without an `[agents.<id>]` table gets the `fallback_policy`
//! (`Guest`: nothing; `Unrestricted`: `*`, dev only).
//!
//! ## Usage
//!
//! ```rust,ignore
//! use tze_hud_runtime::{RuntimeContext, FallbackPolicy};
//! use tze_hud_scene::config::ResolvedConfig;
//!
//! let ctx = RuntimeContext::from_config(resolved_config, FallbackPolicy::Guest);
//! let agents = ctx.agent_directory(&runtime_psk);
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tze_hud_config::HotReloadableConfig;
use tze_hud_scene::config::{
    AgentDirectory, DisplayProfile, RegisteredAgentBudgetOverrides, ResolvedConfig,
};
use tze_hud_scene::types::ResourceBudget;

use crate::mutation_budget_bridge::DEFAULT_MAX_GUEST_SESSIONS;

/// Absolute maximum tiles any agent may hold, regardless of config.
const HARD_MAX_TILES: u32 = 64;
/// Absolute maximum texture memory any agent may hold, regardless of config.
const HARD_MAX_TEXTURE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Absolute maximum update rate any agent may sustain, regardless of config.
const HARD_MAX_UPDATE_RATE_HZ: f32 = 120.0;

// ─── FallbackPolicy ───────────────────────────────────────────────────────────

/// What permissions to give agents without an `[agents.<id>]` table.
///
/// - `Guest` — no capabilities granted (safest default for v1).
/// - `Unrestricted` — all capabilities granted (useful for single-agent dev setups).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackPolicy {
    /// Unknown agents receive no capabilities (guest mode).
    Guest,
    /// Unknown agents receive unrestricted capabilities (dev mode only).
    Unrestricted,
}

impl Default for FallbackPolicy {
    fn default() -> Self {
        Self::Guest
    }
}

// ─── Operational runtime envelope ────────────────────────────────────────────

const MIB_BYTES: u64 = 1024 * 1024;

/// Frozen runtime-owned resident-memory ceilings in accounted bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResidentMemoryEnvelope {
    pub max_aggregate_bytes: u64,
    pub max_resource_bytes: u64,
    pub max_widget_asset_bytes: u64,
    pub max_widget_raster_bytes: u64,
    pub max_font_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeResidentStoreLimits {
    pub resource_bytes: usize,
    pub widget_source_bytes: u64,
    pub widget_namespace_bytes: u64,
    pub widget_raster_bytes: u64,
    pub font_bytes: usize,
}

/// Immutable profile-derived limits consumed by runtime admission paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationalRuntimeEnvelope {
    pub profile_name: String,
    pub max_resident_sessions: u32,
    pub max_leased_tiles: u32,
    pub max_agent_leased_texture_bytes: u64,
    pub max_agent_update_hz: u32,
    pub resident_memory: ResidentMemoryEnvelope,
}

impl OperationalRuntimeEnvelope {
    fn from_profile(profile: &DisplayProfile) -> Self {
        Self {
            profile_name: profile.name.clone(),
            max_resident_sessions: profile.max_agents,
            max_leased_tiles: profile.max_tiles,
            max_agent_leased_texture_bytes: u64::from(profile.max_texture_mb) * MIB_BYTES,
            max_agent_update_hz: profile.max_agent_update_hz,
            resident_memory: ResidentMemoryEnvelope {
                max_aggregate_bytes: u64::from(profile.max_runtime_resident_mb) * MIB_BYTES,
                max_resource_bytes: u64::from(profile.max_resource_resident_mb) * MIB_BYTES,
                max_widget_asset_bytes: u64::from(profile.max_widget_asset_resident_mb) * MIB_BYTES,
                max_widget_raster_bytes: u64::from(profile.max_widget_raster_cache_mb) * MIB_BYTES,
                max_font_bytes: u64::from(profile.max_font_resident_mb) * MIB_BYTES,
            },
        }
    }
}

fn resident_ledger_for(envelope: &OperationalRuntimeEnvelope) -> tze_hud_resource::ResidentLedger {
    let memory = &envelope.resident_memory;
    tze_hud_resource::ResidentLedger::new(tze_hud_resource::ResidentLedgerLimits {
        aggregate_bytes: memory.max_aggregate_bytes,
        resource_bytes: memory.max_resource_bytes,
        widget_source_bytes: memory.max_widget_asset_bytes,
        widget_raster_bytes: memory.max_widget_raster_bytes,
        font_bytes: memory.max_font_bytes,
    })
}

/// `psk_env` naming this variable always means the runtime PSK, however it
/// was supplied (`--psk` or the environment).
pub const RUNTIME_PSK_ENV: &str = "TZE_HUD_PSK";

fn resolve_agent_psks_logged(psk_env: &HashMap<String, String>) -> HashMap<String, String> {
    let external: HashMap<String, String> = psk_env
        .iter()
        .filter(|(_, env)| env.as_str() != RUNTIME_PSK_ENV)
        .map(|(id, env)| (id.clone(), env.clone()))
        .collect();
    let (psks, warnings) = tze_hud_config::resolve_agent_psks(&external);
    for w in warnings {
        tracing::warn!(
            agent = %w.agent_name,
            env = %w.env_var_name,
            "agent psk_env is unset or empty; that agent cannot authenticate"
        );
    }
    psks
}

fn runtime_psk_agents(psk_env: &HashMap<String, String>) -> Vec<String> {
    psk_env
        .iter()
        .filter(|(_, env)| env.as_str() == RUNTIME_PSK_ENV)
        .map(|(id, _)| id.clone())
        .collect()
}

// ─── RuntimeContext ───────────────────────────────────────────────────────────

/// Runtime context derived from validated configuration.
///
/// Built once at startup; shared via `Arc<RuntimeContext>` across all subsystems.
///
/// **Frozen fields** (`profile`, `agent_capabilities`,
/// `fallback_policy`) are immutable after construction. A restart is required
/// to change them.
///
/// **Hot-reloadable fields** are held in `hot` as an `ArcSwap<HotReloadableConfig>`.
/// Call `reload_hot_config()` to atomically swap in a freshly validated config subset
/// with no locks and no restart. Every section is frozen today, so it is empty.
///
/// Per spec §Configuration Reload (lines 263-274, v1-mandatory): SIGHUP and the
/// `RuntimeService.ReloadConfig` gRPC call both trigger a live reload of the
/// hot-reloadable sections. The frozen sections require a full process restart.
#[derive(Debug)]
pub struct RuntimeContext {
    // ── Frozen fields ─────────────────────────────────────────────────────────
    // Immutable after construction. Require restart to change.
    /// Resolved display profile with budget values.
    pub profile: DisplayProfile,

    /// Profile-derived operational limits, constructed once at startup.
    pub operational_envelope: OperationalRuntimeEnvelope,

    /// Shared physical resident-allocation authority for all cache classes.
    pub resident_ledger: tze_hud_resource::ResidentLedger,

    /// Per-agent permission strings (expanded from `allow`) keyed by agent id.
    agent_capabilities: HashMap<String, Vec<String>>,

    /// Per-agent PSKs resolved from each agent's `psk_env` at startup.
    agent_psks: HashMap<String, String>,

    /// Agents whose `psk_env` is [`RUNTIME_PSK_ENV`]: their PSK is the runtime PSK.
    runtime_psk_agents: Vec<String>,

    /// Validated per-agent budget overrides, frozen at startup.
    agent_budget_overrides: HashMap<String, RegisteredAgentBudgetOverrides>,

    /// Policy for agents without an `[agents.<id>]` table.
    pub fallback_policy: FallbackPolicy,

    // ── Hot-reloadable fields ─────────────────────────────────────────────────
    // Atomically swappable via SIGHUP or ReloadConfig RPC.
    /// Hot-reload seam; every section is currently frozen, so this is empty.
    ///
    /// Access the current snapshot via `self.hot.load()`. Update atomically
    /// via `self.reload_hot_config(new_hot)`.
    hot: ArcSwap<HotReloadableConfig>,
}

impl RuntimeContext {
    // ── Constructors ─────────────────────────────────────────────────────────

    /// Build a `RuntimeContext` from a fully validated `ResolvedConfig`.
    ///
    /// The `fallback_policy` is applied to any agent without an
    /// `[agents.<id>]` table. For production use, pass `FallbackPolicy::Guest`.
    pub fn from_config(config: ResolvedConfig, fallback_policy: FallbackPolicy) -> Self {
        let operational_envelope = OperationalRuntimeEnvelope::from_profile(&config.profile);
        let resident_ledger = resident_ledger_for(&operational_envelope);
        Self {
            profile: config.profile,
            operational_envelope,
            resident_ledger,
            agent_capabilities: config.agent_capabilities,
            agent_psks: resolve_agent_psks_logged(&config.agent_psk_env),
            runtime_psk_agents: runtime_psk_agents(&config.agent_psk_env),
            agent_budget_overrides: config.agent_budget_overrides,
            fallback_policy,
            hot: ArcSwap::from_pointee(HotReloadableConfig::default()),
        }
    }

    /// Build a `RuntimeContext` from a `ResolvedConfig` and an initial
    /// `HotReloadableConfig`.
    ///
    /// Use this constructor when a config file is available at startup and you
    /// want the hot-reloadable sections to reflect the initial file contents
    /// immediately, rather than waiting for the first SIGHUP.
    pub fn from_config_with_hot(
        config: ResolvedConfig,
        fallback_policy: FallbackPolicy,
        hot: HotReloadableConfig,
    ) -> Self {
        let operational_envelope = OperationalRuntimeEnvelope::from_profile(&config.profile);
        let resident_ledger = resident_ledger_for(&operational_envelope);
        Self {
            profile: config.profile,
            operational_envelope,
            resident_ledger,
            agent_capabilities: config.agent_capabilities,
            agent_psks: resolve_agent_psks_logged(&config.agent_psk_env),
            runtime_psk_agents: runtime_psk_agents(&config.agent_psk_env),
            agent_budget_overrides: config.agent_budget_overrides,
            fallback_policy,
            hot: ArcSwap::from_pointee(hot),
        }
    }

    /// Build a minimal `RuntimeContext` using the headless profile defaults.
    ///
    /// Used in tests and headless mode when no config file is present.
    /// All unrecognized agents are treated as guests (no capabilities).
    /// Hot-reloadable sections are initialized to defaults.
    pub fn headless_default() -> Self {
        let profile = DisplayProfile::headless();
        let operational_envelope = OperationalRuntimeEnvelope::from_profile(&profile);
        let resident_ledger = resident_ledger_for(&operational_envelope);
        Self {
            profile,
            operational_envelope,
            resident_ledger,
            agent_capabilities: HashMap::new(),
            agent_psks: HashMap::new(),
            runtime_psk_agents: Vec::new(),
            agent_budget_overrides: HashMap::new(),
            fallback_policy: FallbackPolicy::Guest,
            hot: ArcSwap::from_pointee(HotReloadableConfig::default()),
        }
    }

    /// Replace the policy for agents without an `[agents.<id>]` table.
    pub fn with_fallback_policy(mut self, fallback_policy: FallbackPolicy) -> Self {
        self.fallback_policy = fallback_policy;
        self
    }

    // ── Hot-reload ────────────────────────────────────────────────────────────

    /// Atomically replace the hot-reloadable configuration sections.
    ///
    /// This is the integration point for SIGHUP and `RuntimeService.ReloadConfig`.
    /// The caller is responsible for calling `tze_hud_config::reload_config()` first
    /// to parse and validate the new TOML; this method only stores the result.
    ///
    /// Subsystems that hold a loaded snapshot (via `ctx.hot.load()`) will see stale
    /// values until their next `load()` call. This is intentional — the swap is
    /// atomic and lock-free; subsystems do not need to coordinate.
    ///
    /// Every section is currently frozen, so a reload changes nothing.
    pub fn reload_hot_config(&self, new_hot: HotReloadableConfig) {
        self.hot.store(Arc::new(new_hot));
    }

    /// Return a snapshot of the hot-reloadable configuration.
    ///
    /// The returned `Arc` keeps the current `HotReloadableConfig` alive for as long
    /// as there are strong references to it. Use this to access
    /// dynamic policy settings without exposing the
    /// internal hot-reload mechanism.
    pub fn hot_config(&self) -> Arc<HotReloadableConfig> {
        self.hot.load_full()
    }

    // ── Agent identity ────────────────────────────────────────────────────────

    /// Build the credential → agent directory used by MCP and gRPC.
    pub fn agent_directory(&self, runtime_psk: &str) -> AgentDirectory {
        AgentDirectory {
            runtime_psk: runtime_psk.to_string(),
            agent_psks: self.snapshot_agent_psks(runtime_psk),
            permissions: self.agent_capabilities.clone(),
            fallback_permissions: match self.fallback_policy {
                FallbackPolicy::Guest => Vec::new(),
                FallbackPolicy::Unrestricted => vec!["*".to_string()],
            },
        }
    }

    /// Per-agent PSKs: those resolved at startup, plus `runtime_psk` for
    /// agents whose `psk_env` is [`RUNTIME_PSK_ENV`].
    pub fn snapshot_agent_psks(&self, runtime_psk: &str) -> HashMap<String, String> {
        let mut psks = self.agent_psks.clone();
        if !runtime_psk.is_empty() {
            for id in &self.runtime_psk_agents {
                psks.insert(id.clone(), runtime_psk.to_string());
            }
        }
        psks
    }

    /// Return the registered capability list for the given agent, if any.
    ///
    /// Returns `None` if the agent has no `[agents.<id>]` table.
    pub fn agent_capabilities(&self, agent_name: &str) -> Option<&[String]> {
        self.agent_capabilities.get(agent_name).map(Vec::as_slice)
    }

    /// Return a cloned snapshot of the full agent capability registry.
    ///
    /// Used at server startup to wire the capability map into `HudSessionImpl`.
    /// This is a one-time clone at startup, not on the per-request hot path.
    pub fn snapshot_agent_capabilities(&self) -> HashMap<String, Vec<String>> {
        self.agent_capabilities.clone()
    }

    /// Return the frozen effective budget for every registered agent.
    pub fn snapshot_agent_resource_budgets(&self) -> HashMap<String, ResourceBudget> {
        self.agent_capabilities
            .keys()
            .chain(self.agent_budget_overrides.keys())
            .map(|name| (name.clone(), self.effective_resource_budget_for(name)))
            .collect()
    }

    /// Return the frozen budget used for unregistered/guest agents.
    pub fn fallback_resource_budget(&self) -> ResourceBudget {
        self.effective_resource_budget_for("")
    }

    /// Exact startup limits consumed by production cache/store constructors.
    pub fn resident_store_limits(&self) -> RuntimeResidentStoreLimits {
        let resident = &self.operational_envelope.resident_memory;
        RuntimeResidentStoreLimits {
            resource_bytes: usize::try_from(resident.max_resource_bytes).unwrap_or(usize::MAX),
            widget_source_bytes: resident.max_widget_asset_bytes,
            widget_namespace_bytes: resident.max_widget_asset_bytes.min(16 * MIB_BYTES),
            widget_raster_bytes: resident.max_widget_raster_bytes,
            font_bytes: usize::try_from(resident.max_font_bytes).unwrap_or(usize::MAX),
        }
    }

    /// Machine-readable startup snapshot for operators and tests.
    pub fn resident_accounting_snapshot(&self) -> serde_json::Value {
        let limits = self.resident_ledger.limits();
        let usage = self.resident_ledger.snapshot();
        let fallback = self.fallback_resource_budget();
        serde_json::json!({
            "profile": self.operational_envelope.profile_name,
            "admission": {
                "max_resident_sessions": self.operational_envelope.max_resident_sessions,
                "max_guest_sessions": DEFAULT_MAX_GUEST_SESSIONS,
                "max_leased_tiles": self.operational_envelope.max_leased_tiles,
                "max_agent_leased_texture_bytes": self.operational_envelope.max_agent_leased_texture_bytes,
                "max_agent_update_hz": self.operational_envelope.max_agent_update_hz,
                "fallback_session": {
                    "max_tiles": fallback.max_tiles,
                    "max_nodes_per_tile": fallback.max_nodes_per_tile,
                    "max_texture_bytes": fallback.max_texture_bytes,
                    "max_update_rate_hz": fallback.max_update_rate_hz,
                },
                "absolute_hard_max": {
                    "tiles": HARD_MAX_TILES,
                    "texture_bytes": HARD_MAX_TEXTURE_BYTES,
                    "update_rate_hz": HARD_MAX_UPDATE_RATE_HZ,
                },
            },
            "limits": {
                "aggregate_bytes": limits.aggregate_bytes,
                "resource_bytes": limits.resource_bytes,
                "widget_source_bytes": limits.widget_source_bytes,
                "widget_raster_bytes": limits.widget_raster_bytes,
                "font_bytes": limits.font_bytes,
            },
            "usage": {
                "aggregate_bytes": usage.aggregate_bytes,
                "resource_bytes": usage.resource_bytes,
                "widget_source_bytes": usage.widget_source_bytes,
                "widget_raster_bytes": usage.widget_raster_bytes,
                "font_bytes": usage.font_bytes,
                "allocation_count": usage.allocation_count,
            },
            "counters": {
                "class_denials": usage.class_denial_count,
                "aggregate_denials": usage.aggregate_denial_count,
                "evictions": usage.eviction_count,
            },
            "accounted_byte_rules": {
                "resource_cpu": "retained decoded/source allocation bytes",
                "resource_gpu": "texture width * height * bytes_per_pixel * samples * mip factor",
                "widget_source": "retained SVG byte vector length per owned copy",
                "widget_raster": "RGBA8 width * height * 4 per cached GPU texture",
                "font": "retained uploaded font source byte length per owned raw/font-system copy",
                "measurement_scope": "deterministic admission quantities; excludes allocator metadata, driver padding, shared heaps, and process RSS",
            },
            "consumers": {
                "session_admission": ["HudSessionImpl", "RuntimeMutationBudgetEnforcer"],
                "lease_defaults": ["SceneGraph::try_grant_lease_for_session_with_budget"],
                "aggregate_scene_resources": ["RuntimeMutationBudgetEnforcer"],
                "resource": ["ResourceStore", "Compositor::image_bytes", "Compositor::image_texture_cache"],
                "widget_source": ["WidgetAssetStore(gRPC fallback)", "WidgetAssetRegistry(MCP metadata-only)", "WidgetRenderer::svgs"],
                "widget_raster": ["WidgetRenderer::textures"],
                "font": ["FontBytesStore", "glyphon::FontSystem"],
                "durable_disk_excluded": ["RuntimeWidgetStore"],
            },
        })
    }

    /// Return validated startup budget overrides for a registered agent.
    pub fn agent_budget_overrides(
        &self,
        agent_name: &str,
    ) -> Option<&RegisteredAgentBudgetOverrides> {
        self.agent_budget_overrides.get(agent_name)
    }

    /// Resolve a per-session resource budget using config precedence.
    ///
    /// A registered override replaces the canonical default for its dimension,
    /// then the value is capped by the selected profile and absolute runtime
    /// hard maximum. Missing overrides retain canonical defaults.
    pub fn effective_resource_budget_for(&self, agent_name: &str) -> ResourceBudget {
        let canonical = ResourceBudget::default();
        let overrides = self
            .agent_budget_overrides(agent_name)
            .copied()
            .unwrap_or_default();

        let requested_texture_bytes = overrides
            .max_texture_mb
            .map(|mib| u64::from(mib) * MIB_BYTES)
            .unwrap_or(canonical.max_texture_bytes);
        let requested_update_hz = overrides
            .max_update_hz
            .map(|hz| hz as f32)
            .unwrap_or(canonical.max_update_rate_hz);

        ResourceBudget {
            max_tiles: overrides
                .max_tiles
                .unwrap_or(canonical.max_tiles)
                .min(self.operational_envelope.max_leased_tiles)
                .min(HARD_MAX_TILES),
            max_texture_bytes: requested_texture_bytes
                .min(self.operational_envelope.max_agent_leased_texture_bytes)
                .min(HARD_MAX_TEXTURE_BYTES),
            max_update_rate_hz: requested_update_hz
                .min(self.operational_envelope.max_agent_update_hz as f32)
                .min(HARD_MAX_UPDATE_RATE_HZ),
            ..canonical
        }
    }
}

// ─── Shared runtime context type alias ───────────────────────────────────────

/// Cheaply-cloneable handle to the shared runtime context.
pub type SharedRuntimeContext = Arc<RuntimeContext>;

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tze_hud_scene::config::ResolvedConfig;

    fn make_config(caps: Vec<(&str, Vec<&str>)>) -> ResolvedConfig {
        let mut agent_capabilities = HashMap::new();
        for (name, agent_caps) in caps {
            agent_capabilities.insert(
                name.to_string(),
                agent_caps.into_iter().map(str::to_string).collect(),
            );
        }
        ResolvedConfig {
            profile: DisplayProfile::headless(),
            tab_names: vec!["main".to_string()],
            agent_capabilities,
            agent_budget_overrides: HashMap::new(),
            agent_psk_env: HashMap::new(),
            source_path: None,
        }
    }

    #[test]
    fn runtime_psk_env_resolves_to_runtime_psk_without_env_var() {
        let mut config = make_config(vec![("claude", vec!["*"])]);
        config
            .agent_psk_env
            .insert("claude".to_string(), RUNTIME_PSK_ENV.to_string());
        let ctx = RuntimeContext::from_config(config, FallbackPolicy::Guest);
        let id = ctx
            .agent_directory("cli-psk")
            .resolve("cli-psk", "")
            .unwrap();
        assert_eq!(id.agent_id, "claude");
        assert!(id.allows("publish_zone:subtitle"));
    }

    #[test]
    fn agent_directory_uses_allow_permissions_and_fallback() {
        let ctx = RuntimeContext::from_config(
            make_config(vec![("weather-agent", vec!["create_tiles"])]),
            FallbackPolicy::Guest,
        );
        let dir = ctx.agent_directory("psk");
        let weather = dir.resolve("psk", "weather-agent").unwrap();
        assert_eq!(weather.permissions, vec!["create_tiles".to_string()]);
        assert!(
            dir.resolve("psk", "stranger")
                .unwrap()
                .permissions
                .is_empty()
        );

        let dev = RuntimeContext::from_config(make_config(vec![]), FallbackPolicy::Unrestricted);
        let any = dev.agent_directory("psk").resolve("psk", "anyone").unwrap();
        assert!(any.allows("publish_zone:subtitle"));
    }

    // ── from_config ───────────────────────────────────────────────────────────

    #[test]
    fn from_config_populates_profile() {
        let config = make_config(vec![]);
        let ctx = RuntimeContext::from_config(config, FallbackPolicy::Guest);
        assert_eq!(ctx.profile.name, "headless");
        assert_eq!(ctx.profile.max_tiles, 256);
    }

    #[test]
    fn from_config_derives_immutable_operational_envelope() {
        let config = make_config(vec![]);
        let ctx = RuntimeContext::from_config(config, FallbackPolicy::Guest);

        assert_eq!(ctx.operational_envelope.profile_name, "headless");
        assert_eq!(ctx.operational_envelope.max_resident_sessions, 8);
        assert_eq!(ctx.operational_envelope.max_leased_tiles, 256);
        assert_eq!(
            ctx.operational_envelope.max_agent_leased_texture_bytes,
            512 * 1024 * 1024
        );
        assert_eq!(ctx.operational_envelope.max_agent_update_hz, 60);
        assert_eq!(
            ctx.operational_envelope.resident_memory.max_aggregate_bytes,
            512 * 1024 * 1024
        );
        assert_eq!(
            ctx.operational_envelope.resident_memory.max_resource_bytes,
            256 * 1024 * 1024
        );
        assert_eq!(
            ctx.operational_envelope
                .resident_memory
                .max_widget_asset_bytes,
            64 * 1024 * 1024
        );
        assert_eq!(
            ctx.operational_envelope
                .resident_memory
                .max_widget_raster_bytes,
            128 * 1024 * 1024
        );
        assert_eq!(
            ctx.operational_envelope.resident_memory.max_font_bytes,
            64 * 1024 * 1024
        );
    }

    #[test]
    fn from_config_retains_registered_agent_budget_overrides() {
        let mut config = make_config(vec![]);
        config.agent_budget_overrides.insert(
            "agent-a".into(),
            RegisteredAgentBudgetOverrides {
                max_tiles: Some(3),
                max_texture_mb: Some(24),
                max_update_hz: Some(7),
            },
        );
        let ctx = RuntimeContext::from_config(config, FallbackPolicy::Guest);

        assert_eq!(
            ctx.agent_budget_overrides("agent-a"),
            Some(&RegisteredAgentBudgetOverrides {
                max_tiles: Some(3),
                max_texture_mb: Some(24),
                max_update_hz: Some(7),
            })
        );
    }

    #[test]
    fn effective_budget_uses_canonical_defaults_capped_by_profile() {
        let mut config = make_config(vec![]);
        config.profile.max_tiles = 4;
        config.profile.max_texture_mb = 128;
        config.profile.max_agent_update_hz = 20;
        let ctx = RuntimeContext::from_config(config, FallbackPolicy::Guest);

        let budget = ctx.effective_resource_budget_for("unregistered");
        assert_eq!(budget.max_tiles, 4);
        assert_eq!(budget.max_texture_bytes, 128 * 1024 * 1024);
        assert_eq!(budget.max_update_rate_hz, 20.0);
    }

    #[test]
    fn registered_overrides_replace_defaults_then_obey_profile_and_hard_caps() {
        let mut config = make_config(vec![]);
        config.profile.max_tiles = 100;
        config.profile.max_texture_mb = 4096;
        config.profile.max_agent_update_hz = 240;
        config.agent_budget_overrides.insert(
            "agent-a".into(),
            RegisteredAgentBudgetOverrides {
                max_tiles: Some(80),
                max_texture_mb: Some(3072),
                max_update_hz: Some(180),
            },
        );
        let ctx = RuntimeContext::from_config(config, FallbackPolicy::Guest);

        let budget = ctx.effective_resource_budget_for("agent-a");
        assert_eq!(budget.max_tiles, HARD_MAX_TILES);
        assert_eq!(budget.max_texture_bytes, HARD_MAX_TEXTURE_BYTES);
        assert_eq!(budget.max_update_rate_hz, HARD_MAX_UPDATE_RATE_HZ);
    }

    #[test]
    fn from_config_populates_agent_capabilities() {
        let config = make_config(vec![(
            "weather-agent",
            vec!["create_tiles", "modify_own_tiles"],
        )]);
        let ctx = RuntimeContext::from_config(config, FallbackPolicy::Guest);
        let caps = ctx.agent_capabilities("weather-agent").unwrap();
        assert!(caps.contains(&"create_tiles".to_string()));
        assert!(caps.contains(&"modify_own_tiles".to_string()));
    }

    // ── from_config_with_hot ──────────────────────────────────────────────────

    // ── headless_default ─────────────────────────────────────────────────────

    #[test]
    fn headless_default_has_headless_profile() {
        let ctx = RuntimeContext::headless_default();
        assert_eq!(ctx.profile.name, "headless");
    }

    // ── reload_hot_config ─────────────────────────────────────────────────────

    // ── capability_policy_for ─────────────────────────────────────────────────

    // ── Spec: Agent Registration with Per-Agent Budget Overrides ─────────────
    // configuration/spec.md lines 136-147

    #[test]
    fn headless_production_consumers_share_exact_store_limits() {
        let ctx = RuntimeContext::headless_default();
        assert_eq!(
            ctx.resident_store_limits(),
            RuntimeResidentStoreLimits {
                resource_bytes: 256 * 1024 * 1024,
                widget_source_bytes: 64 * 1024 * 1024,
                widget_namespace_bytes: 16 * 1024 * 1024,
                widget_raster_bytes: 128 * 1024 * 1024,
                font_bytes: 64 * 1024 * 1024,
            }
        );
        let snapshot = ctx.resident_accounting_snapshot();
        assert_eq!(snapshot["limits"]["aggregate_bytes"], 512 * 1024 * 1024_u64);
        assert_eq!(snapshot["usage"]["allocation_count"], 0);
        assert_eq!(snapshot["admission"]["max_resident_sessions"], 8);
        assert_eq!(snapshot["admission"]["max_leased_tiles"], 256);
        assert_eq!(snapshot["counters"]["class_denials"], 0);
        assert!(snapshot["accounted_byte_rules"]["measurement_scope"].is_string());
        assert!(snapshot["consumers"]["resource"].is_array());
        assert!(snapshot["consumers"]["widget_source"].is_array());
        assert!(snapshot["consumers"]["font"].is_array());
    }
}
