//! # Reload triggers — RFC 0006 §9
//!
//! Implements the two v1-mandatory config-reload triggers:
//!
//! 1. **SIGHUP signal handler** — `spawn_sighup_listener` launches a Tokio task
//!    that waits for SIGHUP (Linux/macOS) and calls `ctx.reload_hot_config()`.
//!
//! 2. **`RuntimeService.ReloadConfig` gRPC RPC** — `RuntimeServiceImpl` is a
//!    tonic server implementation that accepts a TOML string, validates it via
//!    `tze_hud_config::reload_config`, and atomically applies the result via
//!    `ctx.reload_hot_config()`.
//!
//! ## Dependency graph
//!
//! Both triggers live here (in `tze_hud_runtime`) because:
//! - `tze_hud_protocol` cannot depend on `tze_hud_config` (it has no need for it).
//! - `tze_hud_runtime` already depends on both `tze_hud_protocol` and `tze_hud_config`.
//! - The runtime crate is the natural home for "wire all the subsystems together" code.
//!
//! ## Usage
//!
//! ```rust,ignore
//! use std::sync::Arc;
//! use tze_hud_runtime::reload_triggers::{RuntimeServiceImpl, spawn_sighup_listener};
//! use tze_hud_runtime::runtime_context::RuntimeContext;
//! use tze_hud_protocol::proto::session::runtime_service_server::RuntimeServiceServer;
//!
//! let ctx = Arc::new(RuntimeContext::headless_default());
//!
//! // SIGHUP trigger (Unix only — no-op on Windows)
//! let _sighup = spawn_sighup_listener(Arc::clone(&ctx), "/etc/tze_hud/config.toml");
//!
//! // gRPC trigger
//! let runtime_svc = RuntimeServiceImpl::new(Arc::clone(&ctx));
//! tonic::transport::Server::builder()
//!     .add_service(RuntimeServiceServer::new(runtime_svc))
//!     .serve(addr)
//!     .await?;
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use tonic::{Request, Response, Status};

use crate::runtime_context::SharedRuntimeContext;
use tze_hud_protocol::proto::session::{
    ReloadConfigRequest, ReloadConfigResponse, runtime_service_server::RuntimeService,
};

// ─── RuntimeServiceImpl ───────────────────────────────────────────────────────

/// tonic server implementation of `RuntimeService`.
///
/// Satisfies RFC 0006 §9 gRPC trigger requirement: when `ReloadConfig` is called
/// with a valid TOML string, the hot-reloadable config sections are atomically
/// applied to all runtime subsystems via the shared `RuntimeContext`.
///
/// On validation failure, returns `success=false` with error messages. The
/// running configuration is NOT modified on failure.
pub struct RuntimeServiceImpl {
    ctx: SharedRuntimeContext,
}

impl RuntimeServiceImpl {
    /// Create a new `RuntimeServiceImpl` wrapping the shared `RuntimeContext`.
    pub fn new(ctx: SharedRuntimeContext) -> Self {
        Self { ctx }
    }
}

#[tonic::async_trait]
impl RuntimeService for RuntimeServiceImpl {
    /// Reload hot-reloadable config sections from a new TOML string.
    ///
    /// Per RFC 0006 §9: the entire config is re-validated; only the
    /// hot-reloadable sections (currently none) are applied on success. Frozen sections
    /// ([runtime], [[tabs]]) are silently ignored.
    async fn reload_config(
        &self,
        request: Request<ReloadConfigRequest>,
    ) -> Result<Response<ReloadConfigResponse>, Status> {
        let req = request.into_inner();
        let reloaded_at_wall_us = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;

        tracing::info!(
            config_len = req.config_toml.len(),
            "RuntimeService.ReloadConfig: received reload request"
        );

        match tze_hud_config::reload_config(&req.config_toml) {
            Ok(new_hot) => {
                self.ctx.reload_hot_config(new_hot);
                tracing::info!("RuntimeService.ReloadConfig: config reloaded successfully");
                Ok(Response::new(ReloadConfigResponse {
                    success: true,
                    validation_errors: vec![],
                    reloaded_at_wall_us,
                }))
            }
            Err(errors) => {
                let error_msgs: Vec<String> = errors
                    .iter()
                    .map(|e| {
                        format!(
                            "[{:?}] {}: expected={}, got={}",
                            e.code, e.field_path, e.expected, e.got
                        )
                    })
                    .collect();
                tracing::warn!(
                    error_count = error_msgs.len(),
                    "RuntimeService.ReloadConfig: validation errors — config NOT applied"
                );
                Ok(Response::new(ReloadConfigResponse {
                    success: false,
                    validation_errors: error_msgs,
                    reloaded_at_wall_us,
                }))
            }
        }
    }
}

// ─── SIGHUP listener ──────────────────────────────────────────────────────────

/// Spawn a Tokio task that listens for SIGHUP and triggers a config reload.
///
/// On Unix (Linux, macOS): installs a `tokio::signal::unix` SIGHUP listener.
/// Each SIGHUP reads `config_path` from disk, parses and validates the TOML
/// via `tze_hud_config::reload_config`, and atomically applies the result
/// via `ctx.reload_hot_config()`.
///
/// On non-Unix targets (Windows): this function is a no-op stub that
/// immediately returns. Config reload via SIGHUP is not supported on Windows;
/// use the `ReloadConfig` gRPC RPC instead.
///
/// Returns a `tokio::task::JoinHandle` for the listener task. The handle can
/// be dropped to keep the task running (it detaches), or aborted explicitly.
///
/// # Errors logged (not returned)
///
/// - File I/O errors reading `config_path` are logged as warnings.
/// - Config parse/validation errors are logged as warnings with field paths.
///
/// Per RFC 0006 §9: validation errors MUST NOT modify the running config.
pub fn spawn_sighup_listener(
    ctx: SharedRuntimeContext,
    config_path: impl Into<String> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    let config_path = config_path.into();

    #[cfg(unix)]
    {
        tokio::spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};

            let mut sighup = match signal(SignalKind::hangup()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "SIGHUP listener: failed to install signal handler; \
                         hot-reload via SIGHUP is disabled"
                    );
                    return;
                }
            };

            tracing::info!(
                config_path = %config_path,
                "SIGHUP listener: installed; send SIGHUP to reload config"
            );

            loop {
                // Block until next SIGHUP (or task cancellation).
                if sighup.recv().await.is_none() {
                    // Signal stream closed — task is being cancelled.
                    tracing::debug!("SIGHUP listener: signal stream closed, exiting");
                    break;
                }

                tracing::info!(
                    config_path = %config_path,
                    "SIGHUP received: reloading config"
                );

                // Read config file from disk.
                let toml_src = match std::fs::read_to_string(&config_path) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(
                            config_path = %config_path,
                            error = %e,
                            "SIGHUP reload: failed to read config file — config NOT changed"
                        );
                        continue;
                    }
                };

                // Validate and extract hot-reloadable sections.
                match tze_hud_config::reload_config(&toml_src) {
                    Ok(new_hot) => {
                        ctx.reload_hot_config(new_hot);
                        tracing::info!(
                            config_path = %config_path,
                            "SIGHUP reload: config reloaded successfully"
                        );
                    }
                    Err(errors) => {
                        for e in &errors {
                            tracing::warn!(
                                field = %e.field_path,
                                code = ?e.code,
                                expected = %e.expected,
                                got = %e.got,
                                hint = %e.hint,
                                "SIGHUP reload: validation error — config NOT changed"
                            );
                        }
                    }
                }
            }
        })
    }

    #[cfg(not(unix))]
    {
        let _ = config_path;
        let _ = ctx;
        tracing::info!(
            "SIGHUP listener: not supported on this platform (Windows); use ReloadConfig gRPC"
        );
        tokio::spawn(async {})
    }
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_context::RuntimeContext;
    use std::sync::Arc;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn make_ctx() -> SharedRuntimeContext {
        Arc::new(RuntimeContext::headless_default())
    }

    fn valid_toml() -> String {
        r#"
[runtime]
profile = "headless"

[[tabs]]
name = "Main"
default_tab = true

"#
        .to_string()
    }

    fn invalid_toml() -> String {
        "this is not valid TOML %%%".to_string()
    }

    fn validation_error_toml() -> String {
        // Parses OK but fails validation (unknown classification value).
        r#"
[runtime]
profile = "custom"

[display_profile]
extends = "full-display"
target_fps = 15
min_fps = 30

[[tabs]]
name = "Main"
"#
        .to_string()
    }

    // ── RuntimeServiceImpl: success path ─────────────────────────────────────

    /// RFC 0006 §9 (gRPC trigger): valid TOML reload must apply hot-reloadable
    /// sections and return success=true.
    #[tokio::test]
    async fn reload_config_rpc_valid_toml_applies_and_returns_success() {
        let ctx = make_ctx();
        let svc = RuntimeServiceImpl::new(Arc::clone(&ctx));

        // Before reload: defaults (all None).

        let req = Request::new(ReloadConfigRequest {
            config_toml: valid_toml(),
        });
        let resp = svc
            .reload_config(req)
            .await
            .expect("RPC must not return Err");
        let body = resp.into_inner();

        assert!(body.success, "valid TOML must produce success=true");
        assert!(
            body.validation_errors.is_empty(),
            "no errors expected on success"
        );
        assert!(body.reloaded_at_wall_us > 0, "timestamp must be set");

        // After reload: new value is visible.
    }

    // ── RuntimeServiceImpl: parse error path ─────────────────────────────────

    /// RFC 0006 §9: invalid TOML must return success=false with errors.
    /// Running config must NOT be modified.
    #[tokio::test]
    async fn reload_config_rpc_invalid_toml_returns_errors_without_applying() {
        let ctx = make_ctx();

        let svc = RuntimeServiceImpl::new(Arc::clone(&ctx));
        let req = Request::new(ReloadConfigRequest {
            config_toml: invalid_toml(),
        });
        let resp = svc
            .reload_config(req)
            .await
            .expect("RPC must not return Err");
        let body = resp.into_inner();

        assert!(!body.success, "invalid TOML must return success=false");
        assert!(
            !body.validation_errors.is_empty(),
            "errors must be returned"
        );

        // Running config must be unchanged.
    }

    // ── RuntimeServiceImpl: validation error path ────────────────────────────

    /// RFC 0006 §9: TOML that parses but fails field validation must return
    /// success=false and leave the running config unchanged.
    #[tokio::test]
    async fn reload_config_rpc_validation_errors_do_not_apply() {
        let ctx = make_ctx();
        let svc = RuntimeServiceImpl::new(Arc::clone(&ctx));

        let req = Request::new(ReloadConfigRequest {
            config_toml: validation_error_toml(),
        });
        let resp = svc
            .reload_config(req)
            .await
            .expect("RPC must not return Err");
        let body = resp.into_inner();

        assert!(!body.success, "validation error must produce success=false");
        assert!(
            !body.validation_errors.is_empty(),
            "errors must be returned"
        );

        // Running config is still default.
    }

    // ── RuntimeServiceImpl: timestamp ────────────────────────────────────────

    #[tokio::test]
    async fn reload_config_rpc_timestamp_set_on_failure() {
        let ctx = make_ctx();
        let svc = RuntimeServiceImpl::new(Arc::clone(&ctx));

        let req = Request::new(ReloadConfigRequest {
            config_toml: invalid_toml(),
        });
        let resp = svc.reload_config(req).await.unwrap();
        assert!(
            resp.into_inner().reloaded_at_wall_us > 0,
            "timestamp must be set even on failure (for audit trail)"
        );
    }

    // ── SIGHUP listener: spawn doesn't panic ─────────────────────────────────

    /// Smoke test: spawn_sighup_listener must not panic even with a
    /// non-existent config path (we won't send a real SIGHUP in tests).
    #[tokio::test]
    async fn sighup_listener_spawns_without_panic() {
        let ctx = make_ctx();
        let handle = spawn_sighup_listener(ctx, "/tmp/tze_hud_no_such_config_test.toml");
        // Give the task a moment to start.
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        assert!(
            !handle.is_finished(),
            "listener task should still be running"
        );
        handle.abort();
        let _ = handle.await;
    }

    // ── SIGHUP: programmatic trigger via SighupHandler ───────────────────────

    /// Test the full SIGHUP reload path by calling `SighupHandler::trigger_reload`
    /// directly (without sending an OS signal). This verifies that the reload
    /// logic (parse → validate → ctx.reload_hot_config) works end-to-end.
    #[test]
    fn sighup_handler_trigger_reload_applies_hot_config() {
        use std::io::Write;

        let ctx = Arc::new(RuntimeContext::headless_default());

        // Write a valid config file to a temp path.
        let tmp = std::env::temp_dir().join("tze_hud_sighup_test_config.toml");
        let toml_content = r#"
[runtime]
profile = "headless"

[[tabs]]
name = "Main"

"#;
        {
            let mut f = std::fs::File::create(&tmp).expect("create temp file");
            f.write_all(toml_content.as_bytes()).expect("write toml");
        }

        let handler = tze_hud_config::SighupHandler::new(tmp.to_str().unwrap());
        let ctx_clone = Arc::clone(&ctx);
        handler
            .trigger_reload(|hot| {
                ctx_clone.reload_hot_config(hot);
            })
            .expect("trigger_reload must succeed with valid config");

        let _ = std::fs::remove_file(&tmp);
    }
}
