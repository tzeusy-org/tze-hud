use tze_hud_config::loader::TzeHudConfig;
use tze_hud_scene::config::ConfigLoader;

/// Canonical app production config remains valid against current loader schema.
#[test]
fn canonical_app_production_toml_matches_loader_schema() {
    let toml = include_str!("../config/production.toml");
    let loader = TzeHudConfig::parse(toml).expect("production.toml should parse");
    let errors = loader.validate();
    assert!(
        errors.is_empty(),
        "canonical app production.toml should validate cleanly, got: {errors:?}"
    );

    let resolved = loader
        .freeze()
        .expect("canonical app production.toml should freeze");
    assert_eq!(resolved.profile.name, "full-display");
    assert!(
        !resolved.tab_names.is_empty(),
        "canonical app production config must declare at least one tab"
    );
}

/// The resident gRPC portal bridge (default-off) must be pre-registered with
/// `allow = ["tiles"]` only, so enabling it under production config works
/// without granting it zones, widgets, or the portal tools.
#[test]
fn canonical_app_production_registers_resident_grpc_portal_principal() {
    let toml = include_str!("../config/production.toml");
    let resolved = TzeHudConfig::parse(toml)
        .expect("production.toml should parse")
        .freeze()
        .expect("production.toml should freeze");

    let caps = resolved
        .agent_capabilities
        .get("resident-grpc-portal")
        .expect("resident-grpc-portal must be a registered agent under production config");

    for needed in ["create_tiles", "modify_own_tiles"] {
        assert!(
            caps.iter().any(|c| c == needed),
            "resident-grpc-portal needs {needed}, got: {caps:?}"
        );
    }
    assert!(
        !caps
            .iter()
            .any(|c| c == "*" || c == "resident_mcp" || c.starts_with("publish_")),
        "resident-grpc-portal must be limited to tiles, got: {caps:?}"
    );
}
