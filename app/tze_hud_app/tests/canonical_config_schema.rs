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
