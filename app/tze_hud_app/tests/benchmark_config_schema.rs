use tze_hud_config::loader::TzeHudConfig;
use tze_hud_runtime::HeadlessRuntime;
use tze_hud_runtime::headless::HeadlessConfig;
use tze_hud_scene::config::ConfigLoader;

const BENCHMARK_CONFIG: &str = include_str!("../config/benchmark.toml");
const REPO_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

fn freeze_benchmark_config() -> tze_hud_scene::config::ResolvedConfig {
    let loader = TzeHudConfig::parse(BENCHMARK_CONFIG).expect("benchmark.toml should parse");
    let errors = loader.validate();
    assert!(
        errors.is_empty(),
        "benchmark.toml should validate cleanly, got: {errors:?}"
    );
    loader.freeze().expect("benchmark.toml should freeze")
}

fn benchmark_config_for_headless() -> String {
    let mut config: toml::Value = BENCHMARK_CONFIG
        .parse()
        .expect("benchmark.toml must be valid TOML");

    config["widget_bundles"]["paths"] = toml::Value::Array(vec![
        toml::Value::String(format!("{REPO_ROOT}/widget_bundles")),
        toml::Value::String(format!("{REPO_ROOT}/assets/widget_bundles")),
    ]);

    toml::to_string(&config).expect("headless benchmark config must serialize")
}

fn benchmark_headless_config() -> HeadlessConfig {
    HeadlessConfig {
        width: 320,
        height: 240,
        grpc_port: 0,
        agents: tze_hud_scene::config::AgentDirectory::unrestricted("benchmark-config-test"),
        config_toml: Some(benchmark_config_for_headless()),
    }
}

#[test]
fn benchmark_config_matches_loader_schema() {
    let resolved = freeze_benchmark_config();
    assert_eq!(resolved.profile.name, "full-display");
    assert!(
        resolved.tab_names.iter().any(|name| name == "Main"),
        "benchmark config must declare the Main tab"
    );
}

#[tokio::test]
async fn benchmark_config_boot_registers_widgets_for_live_publish() {
    let runtime = HeadlessRuntime::new(benchmark_headless_config())
        .await
        .expect("runtime must start with benchmark config");

    let scene_handle = {
        let state = runtime.shared_state().lock().await;
        state.scene.clone()
    };
    let scene = scene_handle.lock().await;

    for instance in ["main-gauge", "main-progress", "main-status"] {
        assert!(
            scene.widget_registry.instances.contains_key(instance),
            "expected widget instance `{instance}` from benchmark config"
        );
    }
}
