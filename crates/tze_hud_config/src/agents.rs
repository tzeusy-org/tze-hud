//! `[agents.<id>]` validation and PSK resolution.
//!
//! Each agent table names the environment variable holding that agent's PSK
//! (`psk_env`) and the surfaces it may use (`allow`). Budget overrides are
//! checked against the profile ceiling in `loader.rs`.

use std::collections::HashMap;

use tze_hud_scene::config::{ConfigError, ConfigErrorCode};

use crate::allow::validate_allow_entry;
use crate::raw::RawAgents;

/// Validate every agent's `allow` entries and `psk_env` name.
pub fn validate_agents(agents: &RawAgents, errors: &mut Vec<ConfigError>) {
    for (agent_id, agent) in agents {
        for entry in &agent.allow {
            if let Err(hint) = validate_allow_entry(entry) {
                errors.push(ConfigError {
                    code: ConfigErrorCode::UnknownAllowEntry,
                    field_path: format!("agents.{agent_id}.allow"),
                    expected: "zone:<name|*>, widget:<name|*>, portal, tiles, or *".into(),
                    got: entry.clone(),
                    hint,
                });
            }
        }
        if agent.psk_env.as_deref() == Some("") {
            errors.push(ConfigError {
                code: ConfigErrorCode::UnknownAllowEntry,
                field_path: format!("agents.{agent_id}.psk_env"),
                expected: "an environment variable name".into(),
                got: "\"\"".into(),
                hint: "set psk_env to the name of the variable holding this agent's PSK, \
                       or remove it to use the runtime PSK"
                    .into(),
            });
        }
    }
}

/// Resolve each agent's PSK from its `psk_env`, using `env_lookup`.
///
/// Agents whose variable is unset or empty are left out (they cannot
/// authenticate) and reported as warnings.
pub fn resolve_agent_psks_with_lookup<F>(
    agent_psk_env: &HashMap<String, String>,
    env_lookup: F,
) -> (HashMap<String, String>, Vec<AuthEnvWarning>)
where
    F: Fn(&str) -> Option<String>,
{
    let mut psks = HashMap::new();
    let mut warnings = Vec::new();
    for (agent_id, env_var_name) in agent_psk_env {
        match env_lookup(env_var_name) {
            Some(val) if !val.is_empty() => {
                psks.insert(agent_id.clone(), val);
            }
            _ => warnings.push(AuthEnvWarning {
                agent_name: agent_id.clone(),
                env_var_name: env_var_name.clone(),
            }),
        }
    }
    (psks, warnings)
}

/// [`resolve_agent_psks_with_lookup`] against the process environment.
pub fn resolve_agent_psks(
    agent_psk_env: &HashMap<String, String>,
) -> (HashMap<String, String>, Vec<AuthEnvWarning>) {
    resolve_agent_psks_with_lookup(agent_psk_env, |k| std::env::var(k).ok())
}

/// A warning about an unset agent PSK variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthEnvWarning {
    /// The agent whose PSK variable is unset.
    pub agent_name: String,
    /// The unset variable.
    pub env_var_name: String,
}

impl AuthEnvWarning {
    /// Produces a human-readable warning message suitable for logging.
    pub fn to_log_message(&self) -> String {
        format!(
            "WARNING: agent {:?} sets psk_env = {:?} but that variable is not set; \
             the agent cannot authenticate until it is",
            self.agent_name, self.env_var_name
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::RawAgent;

    #[test]
    fn unknown_allow_entry_is_a_config_error_with_hint() {
        let mut agents = RawAgents::new();
        agents.insert(
            "a".into(),
            RawAgent {
                allow: vec!["create_tiles".into(), "zone:subtitle".into()],
                ..Default::default()
            },
        );
        let mut errors = Vec::new();
        validate_agents(&agents, &mut errors);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, ConfigErrorCode::UnknownAllowEntry);
        assert!(errors[0].hint.contains("valid entries"));
    }

    #[test]
    fn psks_resolve_from_env_and_unset_vars_warn() {
        let env: HashMap<String, String> = [
            ("a".to_string(), "A_PSK".to_string()),
            ("b".to_string(), "B_PSK".to_string()),
        ]
        .into();
        let (psks, warnings) =
            resolve_agent_psks_with_lookup(&env, |k| (k == "A_PSK").then(|| "secret".into()));
        assert_eq!(psks.get("a").map(String::as_str), Some("secret"));
        assert!(!psks.contains_key("b"));
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].agent_name, "b");
    }
}
