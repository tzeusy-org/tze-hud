//! Offline scene authoring through the windowed build/capture seam.
//! No listener, configured credential, runtime process, or desktop capture.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tze_hud_compositor::widget::{WidgetRenderPlan, rasterize_widget_render_plan};
use tze_hud_compositor::{CapturedFrame, Compositor, FrameTarget};
use tze_hud_config::loader::TzeHudConfig;
use tze_hud_config::raw::{RawConfig, RawDesignTokens};
use tze_hud_config::themes::{DEFAULT_THEME, selected_theme_name};
use tze_hud_config::tokens::{CANONICAL_TOKENS, parse_color_hex};
use tze_hud_mcp::{CallerContext, McpConfig, McpServer};
use tze_hud_runtime::operator::screenshot::encode_png;
use tze_hud_runtime::run_scene_startup;
use tze_hud_scene::config::ConfigLoader;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::WidgetInstance;
use tze_hud_widget::loader::{BundleScanResult, LoadedBundle, load_bundle_dir_with_tokens};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const MAX_INPUT: usize = 1024 * 1024;
const LOCAL_IDENTITY: &str = "offline-render-scene-not-a-paired-credential";
const HELP: &str = "Usage: render-scene (--fixture FILE | --widget BUNDLE --params JSON) --output PNG\n\
    [--theme tonal-glass|classic|blueprint] [--tokens TOML] [--width PX] [--height PX]\n\
    [--capture-at-ms MS]\n\
Scene defaults: 1920x1080, tonal-glass, final fixture state at 0ms.\n\
Capture bounds: 1..8192 per axis, <=16M pixels; checkpoint <=5000ms.\n\
Output must not exist. JSON manifest is printed to stdout; compile time is separate.";

#[derive(Clone, Debug)]
struct Args {
    fixture: Option<PathBuf>,
    widget: Option<PathBuf>,
    params: Value,
    theme: Option<String>,
    tokens: Option<PathBuf>,
    width: u32,
    height: u32,
    capture_at_ms: u64,
    output: PathBuf,
}

fn error(message: impl Into<String>) -> Box<dyn std::error::Error> {
    std::io::Error::other(message.into()).into()
}

fn parse_args(values: impl IntoIterator<Item = String>) -> Result<Option<Args>> {
    let mut values = values.into_iter();
    let mut args = Args {
        fixture: None,
        widget: None,
        params: json!({}),
        theme: None,
        tokens: None,
        width: 1920,
        height: 1080,
        capture_at_ms: 0,
        output: PathBuf::new(),
    };
    let mut seen = std::collections::HashSet::new();
    while let Some(flag) = values.next() {
        if matches!(flag.as_str(), "--help" | "-h") {
            return Ok(None);
        }
        if !seen.insert(flag.clone()) {
            return Err(error(format!("duplicate argument {flag}")));
        }
        let value = values
            .next()
            .ok_or_else(|| error(format!("{flag} needs a value")))?;
        match flag.as_str() {
            "--fixture" => args.fixture = Some(value.into()),
            "--widget" => args.widget = Some(value.into()),
            "--params" => {
                if value.len() > MAX_INPUT {
                    return Err(error("params exceed 1MiB"));
                }
                args.params = serde_json::from_str(&value)?;
                if !args.params.is_object() {
                    return Err(error("params must be a JSON object"));
                }
            }
            "--theme" => args.theme = Some(value),
            "--tokens" => args.tokens = Some(value.into()),
            "--width" => args.width = value.parse()?,
            "--height" => args.height = value.parse()?,
            "--capture-at-ms" => args.capture_at_ms = value.parse()?,
            "--output" => args.output = value.into(),
            _ => return Err(error(format!("unknown argument {flag}; use --help"))),
        }
    }
    if args.fixture.is_some() == args.widget.is_some() {
        return Err(error("choose exactly one of --fixture and --widget"));
    }
    if args.fixture.is_some() && seen.contains("--params") {
        return Err(error("--params belongs to --widget"));
    }
    validate_args(&args)?;
    Ok(Some(args))
}

fn validate_args(args: &Args) -> Result<()> {
    if args.width == 0
        || args.height == 0
        || args.width > 8192
        || args.height > 8192
        || u64::from(args.width) * u64::from(args.height) > 16 * 1024 * 1024
    {
        return Err(error(
            "dimensions must be positive, <=8192 per axis and <=16M pixels",
        ));
    }
    if args.capture_at_ms > 5000 {
        return Err(error("capture checkpoint exceeds 5000ms"));
    }
    if args.output.as_os_str().is_empty() {
        return Err(error("--output is required"));
    }
    if args.output.exists() {
        return Err(error("output already exists; choose a fresh PNG path"));
    }
    Ok(())
}

fn bounded_read(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take((MAX_INPUT + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_INPUT {
        return Err(error("input exceeds 1MiB"));
    }
    Ok(bytes)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenFile {
    design_tokens: RawDesignTokens,
}

fn raw_config(args: &Args) -> Result<RawConfig> {
    // This is the actual canonical scene layout, not a separately styled scene.
    let mut raw: RawConfig = toml::from_str(include_str!(
        "../../../../app/tze_hud_app/config/production.toml"
    ))?;
    let mut tokens = match &args.tokens {
        Some(path) => {
            toml::from_str::<TokenFile>(std::str::from_utf8(&bounded_read(path)?)?)?.design_tokens
        }
        None => RawDesignTokens::default(),
    };
    for token in CANONICAL_TOKENS {
        if parse_color_hex(token.default_value).is_some()
            && tokens
                .0
                .get(token.key)
                .is_some_and(|value| parse_color_hex(value).is_none())
        {
            return Err(error(format!(
                "invalid color override for {}: expected #RRGGBB or #RRGGBBAA",
                token.key
            )));
        }
    }
    if let Some(theme) = &args.theme {
        tokens.0.insert("theme".into(), theme.clone());
    }
    raw.design_tokens = Some(tokens);
    let text = toml::to_string(&raw)?;
    let mut config = TzeHudConfig::parse(&text).map_err(|e| {
        error(format!(
            "config parse at {}:{}: {}",
            e.line, e.column, e.message
        ))
    })?;
    config.normalize();
    let errors = config.validate();
    if !errors.is_empty() {
        return Err(error(format!("invalid config/theme/tokens: {errors:?}")));
    }
    Ok(raw)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Message {
    #[serde(default = "publish_action")]
    action: String,
    zone: Option<String>,
    widget: Option<String>,
    content: Option<Value>,
    params: Option<Value>,
    key: Option<String>,
    ttl_ms: Option<u64>,
    delay_ms: Option<u64>,
}

fn publish_action() -> String {
    "publish".into()
}

impl Message {
    fn request(&self, id: usize) -> Result<Value> {
        let surface = match (&self.zone, &self.widget) {
            (Some(zone), None) => format!("zone:{zone}"),
            (None, Some(widget)) => format!("widget:{widget}"),
            _ => return Err(error("each message needs exactly one zone or widget")),
        };
        let mut args = serde_json::Map::new();
        args.insert("surface".into(), surface.into());
        let name = match self.action.as_str() {
            "publish" => {
                match (&self.zone, &self.content, &self.params) {
                    (Some(_), Some(content), None) => {
                        args.insert("content".into(), content.clone());
                    }
                    (None, None, Some(params)) if params.is_object() => {
                        args.insert("params".into(), params.clone());
                    }
                    _ => {
                        return Err(error(
                            "publish needs zone content or widget params, never both",
                        ));
                    }
                }
                args.insert("ttl_ms".into(), self.ttl_ms.unwrap_or(60_000).into());
                if let Some(value) = &self.key {
                    args.insert("key".into(), value.clone().into());
                }
                if let Some(value) = self.delay_ms {
                    args.insert("delay_ms".into(), value.into());
                }
                "hud_publish"
            }
            "clear" | "hold" => {
                if self.content.is_some()
                    || self.params.is_some()
                    || self.key.is_some()
                    || self.delay_ms.is_some()
                {
                    return Err(error("clear/hold do not accept publish fields"));
                }
                if self.action == "clear" {
                    if self.ttl_ms.is_some() {
                        return Err(error("clear does not take ttl_ms"));
                    }
                    "hud_clear"
                } else {
                    args.insert("ttl_ms".into(), self.ttl_ms.unwrap_or(0).into());
                    "hud_hold"
                }
            }
            _ => {
                return Err(error(
                    "unsupported fixture action; use publish, clear or hold",
                ));
            }
        };
        Ok(
            json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}}),
        )
    }
}

fn server(scene: SceneGraph, tokens: &HashMap<String, String>) -> McpServer {
    let mut config = McpConfig::with_psk(LOCAL_IDENTITY);
    config.widget_transition_ms =
        tze_hud_config::tokens::resolve_motion_duration_ms(tokens, "motion.state.ms");
    McpServer::new(scene).with_config(config)
}

async fn dispatch(server: &McpServer, request: &Value) -> Result<()> {
    let response: Value = serde_json::from_str(
        &server
            .dispatch(
                &request.to_string(),
                &CallerContext::with_bearer(LOCAL_IDENTITY),
            )
            .await,
    )?;
    if let Some(err) = response.get("error") {
        return Err(error(format!("fixture request rejected: {err}")));
    }
    let result = response
        .get("result")
        .ok_or_else(|| error("missing MCP result"))?;
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or("typed fixture rejected");
        return Err(error(format!(
            "fixture rejected: {}",
            text.chars().take(512).collect::<String>()
        )));
    }
    Ok(())
}

async fn settle(server: &McpServer, at_ms: u64) -> Result<()> {
    tokio::time::sleep(Duration::from_millis(at_ms)).await;
    let handle = server.scene_handle();
    let mut scene = handle.lock().await;
    for result in scene.apply_due_batches() {
        if !result.applied {
            return Err(error(format!(
                "scheduled fixture rejected: {:?}",
                result.error
            )));
        }
    }
    scene.drain_expired_zone_publications();
    scene.drain_expired_widget_publications();
    Ok(())
}

#[derive(Clone, Copy)]
enum AlphaDomain {
    SrgbBytes,
    LinearFramebuffer,
}

fn straight_alpha(mut frame: CapturedFrame, domain: AlphaDomain) -> Result<CapturedFrame> {
    if frame.rgba.len() != frame.width as usize * frame.height as usize * 4 {
        return Err(error("invalid RGBA buffer length"));
    }
    for pixel in frame.rgba.chunks_exact_mut(4) {
        let alpha = u32::from(pixel[3]);
        for channel in &mut pixel[..3] {
            *channel = if alpha == 0 {
                0
            } else {
                match domain {
                    AlphaDomain::SrgbBytes => {
                        ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8
                    }
                    AlphaDomain::LinearFramebuffer => {
                        // An sRGB render attachment encodes the premultiplied
                        // linear result. Undo that transfer before dividing.
                        let encoded = f64::from(*channel) / 255.0;
                        let linear = if encoded <= 0.04045 {
                            encoded / 12.92
                        } else {
                            ((encoded + 0.055) / 1.055).powf(2.4)
                        };
                        let straight = (linear * 255.0 / f64::from(alpha)).min(1.0);
                        let encoded = if straight <= 0.0031308 {
                            12.92 * straight
                        } else {
                            1.055 * straight.powf(1.0 / 2.4) - 0.055
                        };
                        (encoded * 255.0).round() as u8
                    }
                }
            };
        }
    }
    Ok(frame)
}

async fn render_fixture(args: &Args, raw: &RawConfig) -> Result<(CapturedFrame, Value)> {
    let path = args
        .fixture
        .as_ref()
        .ok_or_else(|| error("missing fixture"))?;
    let bytes = bounded_read(path)?;
    let messages: Vec<Message> = serde_json::from_slice(&bytes)?;
    if messages.is_empty() || messages.len() > 1024 {
        return Err(error("fixture must contain 1..1024 messages"));
    }
    let requests = messages
        .iter()
        .enumerate()
        .map(|(i, m)| m.request(i + 1))
        .collect::<Result<Vec<_>>>()?;
    // Allocate the device before the scene/publications: GPU initialization
    // must not consume their TTL or move a requested delayed checkpoint.
    // Typed dispatch still completes before frame construction or output.
    let mut compositor = Compositor::new_headless(args.width, args.height).await?;
    let adapter = compositor.adapter_info();
    if adapter.device_type != "Cpu"
        || !(adapter.name.to_lowercase().contains("llvmpipe")
            || adapter.driver.to_lowercase().contains("llvmpipe"))
    {
        return Err(error(
            "scene capture requires Mesa llvmpipe; use just render-scene on Linux/WSL",
        ));
    }
    let adapter = json!({"name":adapter.name,"backend":adapter.backend,"device_type":adapter.device_type,"driver":adapter.driver,"driver_info":adapter.driver_info});
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    compositor.init_text_renderer(format);
    compositor.init_widget_renderer(format);
    compositor.overlay_mode =
        raw.runtime.as_ref().and_then(|r| r.profile.as_deref()) == Some("overlay");
    let mut scene = SceneGraph::new(args.width as f32, args.height as f32);
    let startup = run_scene_startup(raw, None, &mut scene);
    let server = server(scene, &startup.global_tokens);
    compositor.set_token_map(startup.global_tokens);
    let renderer = compositor
        .widget_renderer_mut()
        .ok_or_else(|| error("widget renderer unavailable"))?;
    for (kind, file, bytes) in startup.widget_svg_assets {
        renderer.register_svg(&kind, &file, bytes);
    }
    for request in &requests {
        dispatch(&server, request).await?;
    }
    settle(&server, args.capture_at_ms).await?;
    let handle = server.scene_handle();
    let mut scene = handle.lock().await;
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    let build = compositor.build_windowed_frame(&mut scene, args.width, args.height);
    let frame = compositor.capture_windowed_frame(
        &build,
        &FrameTarget::primary(args.width, args.height),
        format,
    )?;
    Ok((
        frame,
        json!({"pipeline":"windowed build/capture","fixture":path,"fixture_sha256":hash(&bytes),"messages_applied":messages.len(),"checkpoint":"after all ordered messages; latest-wins per surface","capture_at_ms":args.capture_at_ms,"adapter":adapter}),
    ))
}

fn widget_instance(bundle: &LoadedBundle, scene: &SceneGraph) -> Result<WidgetInstance> {
    Ok(WidgetInstance {
        id: tze_hud_scene::types::SceneId::new(),
        widget_type_name: bundle.definition.id.clone(),
        tab_id: scene.active_tab.ok_or_else(|| error("no startup tab"))?,
        geometry_override: None,
        contention_override: None,
        instance_name: "render-widget".into(),
        current_params: bundle
            .definition
            .parameter_schema
            .iter()
            .map(|p| (p.name.clone(), p.default_value.clone()))
            .collect(),
    })
}

async fn render_widget(args: &Args, raw: &RawConfig) -> Result<(CapturedFrame, Value)> {
    let path = args
        .widget
        .as_ref()
        .ok_or_else(|| error("missing bundle"))?;
    let mut scene = SceneGraph::new(args.width as f32, args.height as f32);
    let startup = run_scene_startup(raw, None, &mut scene);
    let bundle = match load_bundle_dir_with_tokens(path, &startup.global_tokens) {
        BundleScanResult::Ok(bundle) => bundle,
        BundleScanResult::Err(err) => return Err(error(format!("invalid widget bundle: {err}"))),
    };
    scene
        .widget_registry
        .register_definition(bundle.definition.clone());
    let instance = widget_instance(&bundle, &scene)?;
    scene.widget_registry.register_instance(instance);
    let server = server(scene, &startup.global_tokens);
    let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"hud_publish","arguments":{"surface":"widget:render-widget","params":args.params,"ttl_ms":0}}});
    dispatch(&server, &request).await?;
    settle(&server, args.capture_at_ms).await?;
    let handle = server.scene_handle();
    let scene = handle.lock().await;
    let params = &scene.widget_registry.instances["render-widget"].current_params;
    let layers = bundle
        .definition
        .layers
        .iter()
        .map(|layer| {
            let bytes = bundle
                .svg_contents
                .get(&layer.svg_file)
                .ok_or_else(|| error("bundle layer missing"))?;
            Ok((std::str::from_utf8(bytes)?, layer.bindings.as_slice()))
        })
        .collect::<Result<Vec<_>>>()?;
    let plan = WidgetRenderPlan::compile(&layers);
    let constraints = bundle
        .definition
        .parameter_schema
        .iter()
        .filter_map(|p| {
            p.constraints.as_ref().map(|c| {
                (
                    p.name.clone(),
                    (c.f32_min.unwrap_or(0.0), c.f32_max.unwrap_or(1.0)),
                )
            })
        })
        .collect();
    let pixmap = rasterize_widget_render_plan(&plan, &constraints, params, args.width, args.height)
        .ok_or_else(|| error("widget layers failed to rasterize"))?;
    let layer_hashes = bundle
        .definition
        .layers
        .iter()
        .map(|l| json!({"file":l.svg_file,"sha256":hash(&bundle.svg_contents[&l.svg_file])}))
        .collect::<Vec<_>>();
    Ok((
        CapturedFrame {
            width: args.width,
            height: args.height,
            rgba: pixmap.data().to_vec(),
        },
        json!({"pipeline":"WidgetRenderPlan primitive/resvg CPU","bundle":path,"manifest_sha256":hash(&bounded_read(&path.join("widget.toml"))?),"resolved_layer_hashes":layer_hashes,"params_sha256":hash(&serde_json::to_vec(&args.params)?),"capture_at_ms":args.capture_at_ms,"adapter":null}),
    ))
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn write_png(path: &Path, bytes: &[u8]) -> Result<()> {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".render-scene-{}-{}.tmp",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| -> Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        // Same-filesystem link is atomic and refuses an existing destination.
        fs::hard_link(&temporary, path)?;
        Ok(())
    })();
    drop(file);
    fs::remove_file(&temporary)?;
    result
}

async fn execute(args: &Args) -> Result<Value> {
    validate_args(args)?;
    let start = Instant::now();
    let raw = raw_config(args)?;
    let (frame, mut manifest) = if args.fixture.is_some() {
        render_fixture(args, &raw).await?
    } else {
        render_widget(args, &raw).await?
    };
    let domain = if args.fixture.is_some() {
        AlphaDomain::LinearFramebuffer
    } else {
        AlphaDomain::SrgbBytes
    };
    let png = encode_png(&straight_alpha(frame, domain)?)
        .map_err(|e| error(format!("PNG encoding failed: {e:?}")))?;
    write_png(&args.output, &png)?;
    let git = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned());
    manifest["checkout_head_at_invocation"] = json!(git);
    manifest["compiled_example_source_sha256"] = json!(hash(include_bytes!("render_scene.rs")));
    manifest["theme"] = json!(
        raw.design_tokens
            .as_ref()
            .map(|t| selected_theme_name(&t.0))
            .unwrap_or(DEFAULT_THEME)
    );
    let tokens = tze_hud_config::themes::resolve_config_tokens(
        &raw.design_tokens
            .as_ref()
            .ok_or_else(|| error("missing tokens"))?
            .0,
    )
    .into_iter()
    .collect::<BTreeMap<_, _>>();
    manifest["resolved_tokens_sha256"] = json!(hash(&serde_json::to_vec(&tokens)?));
    manifest["dimensions"] = json!([args.width, args.height]);
    manifest["output"] = json!(args.output);
    manifest["png_sha256"] = json!(hash(&png));
    manifest["png_bytes"] = json!(png.len());
    manifest["alpha"] = json!(if args.fixture.is_some() {
        "straight sRGB RGBA: decode GPU sRGB attachment, unpremultiply linear RGB, encode sRGB; alpha0 transparent black"
    } else {
        "straight sRGB RGBA: unpremultiply tiny-skia sRGB bytes; alpha0 transparent black"
    });
    manifest["elapsed_ms"] = json!(start.elapsed().as_secs_f64() * 1000.0);
    manifest["timing_scope"] = json!(
        "inside execute including validation/startup/raster/capture/encode/write; use external wall time for full invocation, exclude compilation"
    );
    Ok(manifest)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let Some(args) = parse_args(std::env::args().skip(1))? else {
        println!("{HELP}");
        return Ok(());
    };
    println!("{}", serde_json::to_string_pretty(&execute(&args).await?)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CPU widget PNG/input behavior; GPU capture content is covered by the
    /// retained compositor fixture and the required actual WSL scene matrix.
    #[tokio::test]
    async fn render_scene_validates_typed_input_and_writes_a_real_png() {
        let dir = std::env::temp_dir().join(format!(
            "render-scene-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let output = dir.join("widget.png");
        let bundle = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/widget_bundles/status-indicator");
        let args = Args {
            fixture: None,
            widget: Some(bundle),
            params: json!({"status":"online","theme":"friendly","label":"PNG","reason":"offline authoring"}),
            theme: Some("tonal-glass".into()),
            tokens: None,
            width: 252,
            height: 96,
            capture_at_ms: 0,
            output: output.clone(),
        };
        for (width, height) in [(0, 1), (1, 0), (8193, 1), (8192, 8192)] {
            let bad = Args {
                width,
                height,
                ..args.clone()
            };
            assert!(execute(&bad).await.is_err());
            assert!(!output.exists());
        }
        let bad = Args {
            theme: Some("missing-theme".into()),
            ..args.clone()
        };
        assert!(execute(&bad).await.is_err());
        assert!(!output.exists());
        let bad = Args {
            params: json!({"status":"not-a-status"}),
            ..args.clone()
        };
        assert!(execute(&bad).await.is_err());
        assert!(!output.exists());
        let tokens = dir.join("bad-tokens.toml");
        fs::write(
            &tokens,
            "[design_tokens]\n\"color.primary\" = \"not-a-color\"\n",
        )
        .unwrap();
        let bad = Args {
            tokens: Some(tokens),
            ..args.clone()
        };
        assert!(execute(&bad).await.is_err());
        assert!(!output.exists());
        let fixture = dir.join("bad-fixture.json");
        for body in [
            "{",
            r#"[{"zone":"subtitle","content":"x","unknown":true}]"#,
            r#"[{"widget":"main-status","action":"launch"}]"#,
        ] {
            fs::write(&fixture, body).unwrap();
            let bad = Args {
                fixture: Some(fixture.clone()),
                widget: None,
                ..args.clone()
            };
            assert!(execute(&bad).await.is_err());
            assert!(!output.exists());
        }
        let alpha = straight_alpha(
            CapturedFrame {
                width: 2,
                height: 1,
                rgba: vec![17, 19, 23, 0, 0, 128, 0, 128],
            },
            AlphaDomain::SrgbBytes,
        )
        .unwrap();
        assert_eq!(alpha.rgba, [0, 0, 0, 0, 0, 255, 0, 128]);
        let alpha = straight_alpha(
            CapturedFrame {
                width: 1,
                height: 1,
                rgba: vec![188, 0, 0, 128],
            },
            AlphaDomain::LinearFramebuffer,
        )
        .unwrap();
        assert_eq!(alpha.rgba, [255, 0, 0, 128]);
        let manifest = execute(&args).await.expect("typed widget must rasterize");
        let bytes = fs::read(&output).unwrap();
        let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
            .unwrap()
            .to_rgba8();
        assert_eq!(decoded.dimensions(), (252, 96));
        assert!(decoded.pixels().any(|p| p[3] > 0));
        assert_eq!(manifest["png_sha256"], hash(&bytes));
        assert!(execute(&args).await.is_err());
        assert_eq!(
            fs::read(&output).unwrap(),
            bytes,
            "collision must retain original bytes"
        );
        assert!(
            parse_args(["--fixture", "a", "--widget", "b", "--output", "c"].map(str::to_owned))
                .is_err()
        );
        // The task controller retains this fixture directory as evidence.
    }
}
