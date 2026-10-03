use super::*;

mod allow;
mod events;
mod invariants;
mod lease;
mod lifecycle_verbs;
mod portal;
mod render_wake;
mod resource_upload;
mod resume;
mod safe_mode;
mod scene_mutations;
mod session_handshake;
mod timing;
mod widget_publish;
mod zones;

use crate::proto::session::hud_session_client::HudSessionClient;
use crate::proto::session::hud_session_server::HudSessionServer;
use std::collections::HashMap;
use tokio_stream::StreamExt;
use tze_hud_scene::graph::SceneGraph;

/// Load an [`ElementStore`] from a TOML file on disk.
///
/// Test-only helper that replaces the former `ElementStore::load_or_default`
/// method.  Missing files are treated as first boot and return an empty store.
fn load_element_store_for_test(
    path: &std::path::Path,
) -> std::io::Result<tze_hud_scene::element_store::ElementStore> {
    match std::fs::read_to_string(path) {
        Ok(content) => toml::from_str(&content).map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid element_store TOML: {err}"),
            )
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(tze_hud_scene::element_store::ElementStore::default())
        }
        Err(err) => Err(err),
    }
}

/// Consume the next message from a stream.
async fn next_server_msg(
    stream: &mut tonic::Streaming<crate::proto::session::ServerMessage>,
) -> crate::proto::session::ServerMessage {
    stream.next().await.unwrap().unwrap()
}

/// Agents whose `allow` list is deliberately narrow, for the denial tests.
/// Every other agent id the dev PSK claims is unrestricted.
fn restricted_test_agents() -> HashMap<String, Vec<String>> {
    let agents: [(&str, &[&str]); 5] = [
        ("no-input-agent", &["create_tiles", "read_scene_topology"]),
        ("zone-only-agent", &["publish_zone:subtitle"]),
        ("widget-no-cap-agent", &["create_tiles"]),
        ("asset-no-cap", &["create_tiles"]),
        ("resource-no-cap", &["create_tiles"]),
    ];
    agents
        .into_iter()
        .map(|(id, perms)| {
            let perms = perms.iter().map(|p| p.to_string()).collect();
            (id.to_string(), perms)
        })
        .collect()
}

/// Start a test server and return a connected client.
async fn setup_test() -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
) {
    let scene = SceneGraph::new(800.0, 600.0);
    let service =
        HudSessionImpl::new(scene, "test-key").with_agent_permissions(restricted_test_agents());

    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(HudSessionServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    let client = connect_test_client_with_retry(addr.port()).await;

    (client, handle)
}

/// Start a connected test server whose scene clock can deterministically cross
/// a finite lease TTL. The caller receives the same durable runtime-to-session
/// publisher that the windowed compositor owns in production.
async fn setup_test_with_lease_expiry_clock(
    clock: tze_hud_scene::TestClock,
) -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
    Arc<tokio::sync::Mutex<crate::session::SharedState>>,
    LeaseExpirySender,
) {
    let scene = SceneGraph::new_with_clock(800.0, 600.0, Arc::new(clock));
    let service = HudSessionImpl::new(scene, "test-key");
    let state = Arc::clone(&service.state);
    let lease_expirations = service.lease_expirations.clone();

    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(HudSessionServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    let client = connect_test_client_with_retry(addr.port()).await;
    (client, handle, state, lease_expirations)
}

async fn connect_test_client_with_retry(port: u16) -> HudSessionClient<tonic::transport::Channel> {
    let endpoint = format!("http://[::1]:{port}");
    for attempt in 0..25 {
        if let Ok(client) = HudSessionClient::connect(endpoint.clone()).await {
            return client;
        }
        if attempt < 24 {
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        }
    }
    panic!("failed to connect test client to {endpoint} after retries");
}

/// Helper: create a bidirectional stream and perform handshake.
/// Returns the sender and the three ordered handshake messages:
/// SessionEstablished, SceneSnapshot, and current DegradationNotice.
async fn handshake(
    client: &mut HudSessionClient<tonic::transport::Channel>,
    agent_id: &str,
    psk: &str,
) -> (
    tokio::sync::mpsc::Sender<ClientMessage>,
    Vec<ServerMessage>,
    tonic::Streaming<ServerMessage>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    // Send SessionInit with canonical capability names (create_tiles, access_input_events)
    // and read_scene_topology so SCENE_TOPOLOGY subscription is granted.
    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: agent_id.to_string(),
            initial_subscriptions: vec!["SCENE_TOPOLOGY".to_string()],
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(crate::auth::psk_credential(psk.to_string())),
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();

    // Collect SessionEstablished, SceneSnapshot, and current degradation state.
    let mut messages = Vec::new();
    for _ in 0..3 {
        if let Some(msg) = response_stream.next().await {
            messages.push(msg.unwrap());
        }
    }

    (tx, messages, response_stream)
}

/// Helper that returns the shared state alongside the client for state-manipulation tests.
async fn setup_test_with_state() -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
    Arc<Mutex<SharedState>>,
) {
    setup_test_with_state_and_render_wake(tze_hud_scene::render_wake::RenderWakeNotifier::default())
        .await
}

async fn setup_test_with_state_and_render_wake(
    render_wake: tze_hud_scene::render_wake::RenderWakeNotifier,
) -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
    Arc<Mutex<SharedState>>,
) {
    let scene = SceneGraph::new(800.0, 600.0);
    let mut service =
        HudSessionImpl::new(scene, "test-key").with_agent_permissions(restricted_test_agents());
    service.render_wake = render_wake;
    let shared_state = service.state.clone();

    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(HudSessionServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let client = HudSessionClient::connect(format!("http://[::1]:{}", addr.port()))
        .await
        .unwrap();

    (client, handle, shared_state)
}

// ─── Live capability revocation tests (RFC 0001 §3.3, GAP-G3-4) ────────────

// ─── Widget publish tests (widget-system spec §Requirement: Widget Publishing via gRPC) ──

/// Helper: create a test service with a durable widget registered.
async fn setup_widget_service() -> HudSessionImpl {
    use tze_hud_scene::types::{
        ContentionPolicy, GeometryPolicy, RenderingPolicy, WidgetDefinition, WidgetInstance,
        WidgetParamType, WidgetParameterDeclaration, WidgetParameterValue, WidgetSvgLayer,
    };

    let scene = SceneGraph::new(800.0, 600.0);
    let service =
        HudSessionImpl::new(scene, "test-key").with_agent_permissions(restricted_test_agents());
    {
        let st = service.state.lock().await;
        let mut s = st.scene.lock().await;

        // Register a durable widget type "gauge"
        s.widget_registry.register_definition(WidgetDefinition {
            id: "gauge".to_string(),
            name: "Gauge".to_string(),
            description: "A simple gauge widget".to_string(),
            parameter_schema: vec![WidgetParameterDeclaration {
                name: "level".to_string(),
                param_type: WidgetParamType::F32,
                default_value: WidgetParameterValue::F32(0.0),
                constraints: None,
            }],
            layers: vec![WidgetSvgLayer {
                svg_file: "fill.svg".to_string(),
                bindings: vec![],
            }],
            default_geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.0,
                y_pct: 0.0,
                width_pct: 0.1,
                height_pct: 0.1,
            },
            default_rendering_policy: RenderingPolicy::default(),
            default_contention_policy: ContentionPolicy::LatestWins,
            max_publishers: WidgetDefinition::default_max_publishers(),
            ephemeral: false, // durable
            hover_behavior: None,
        });

        // Create a tab and widget instance
        let tab_id = s.create_tab("main", 0).unwrap();
        s.widget_registry.register_instance(WidgetInstance {
            id: SceneId::new(),
            widget_type_name: "gauge".to_string(),
            tab_id,
            geometry_override: None,
            contention_override: None,
            instance_name: "gauge".to_string(),
            current_params: std::collections::HashMap::new(),
        });
    }
    service
}

/// Helper: start a server with a widget service and connect.
async fn setup_widget_test() -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
) {
    let service = setup_widget_service().await;
    setup_widget_test_with_service(service).await
}

async fn setup_widget_test_with_service(
    service: HudSessionImpl,
) -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(HudSessionServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let client = HudSessionClient::connect(format!("http://[::1]:{}", addr.port()))
        .await
        .unwrap();

    (client, handle)
}

/// Helper: handshake with specific capabilities in SessionInit.
///
/// Widget capability checks use `session.capabilities` which is populated
/// from the SessionInit `requested_capabilities` list.
async fn handshake_with_capabilities(
    client: &mut HudSessionClient<tonic::transport::Channel>,
    agent_id: &str,
    psk: &str,
    extra_caps: &[&str],
) -> (
    tokio::sync::mpsc::Sender<ClientMessage>,
    Vec<ServerMessage>,
    tonic::Streaming<ServerMessage>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    let mut caps = vec![
        "create_tiles".to_string(),
        "access_input_events".to_string(),
        "read_scene_topology".to_string(),
    ];
    for c in extra_caps {
        caps.push(c.to_string());
    }

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: agent_id.to_string(),
            initial_subscriptions: vec![],
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(crate::auth::psk_credential(psk.to_string())),
        })),
    })
    .await
    .unwrap();

    let mut streaming = client.session(stream).await.unwrap().into_inner();

    // Collect the full ordered handshake baseline.
    let mut init_messages = Vec::new();
    for _ in 0..3 {
        if let Some(msg) = streaming.next().await {
            init_messages.push(msg.unwrap());
        }
    }

    (tx, init_messages, streaming)
}

// ─── Invariants 1 and 4 over the real gRPC session path ──────────────────────

use tze_hud_scene::Clock as _;

fn subtitle_publish(text: &str, ttl_ms: u64, present_at: u64, expires_at: u64) -> Publish {
    Publish {
        surface: "zone:subtitle".to_string(),
        content: Some(crate::proto::ZoneContent {
            payload: Some(crate::proto::zone_content::Payload::StreamText(
                text.to_string(),
            )),
        }),
        ttl_ms,
        present_at_us: present_at,
        expires_at_us: expires_at,
        ..Default::default()
    }
}

fn subtitle_texts(scene: &SceneGraph) -> Vec<String> {
    scene
        .zone_registry
        .active_publishes
        .get("subtitle")
        .into_iter()
        .flatten()
        .filter_map(|r| match &r.content {
            ZoneContent::StreamText(t) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

// ─── Lifecycle verbs (docs/api.md) ──────────────────────────────────────────

/// Send one client payload and return the next `RequestResult`.
async fn request(
    tx: &tokio::sync::mpsc::Sender<ClientMessage>,
    stream: &mut tonic::Streaming<ServerMessage>,
    sequence: u64,
    payload: ClientPayload,
) -> RequestResult {
    tx.send(ClientMessage {
        sequence,
        timestamp_wall_us: now_wall_us(),
        payload: Some(payload),
    })
    .await
    .unwrap();
    loop {
        if let Some(ServerPayload::RequestResult(r)) = next_server_msg(stream).await.payload {
            return r;
        }
    }
}

/// A connected agent on a scene with an active tab and the default zones.
async fn verb_agent(
    agent_id: &str,
) -> (
    tokio::sync::mpsc::Sender<ClientMessage>,
    tonic::Streaming<ServerMessage>,
    Arc<Mutex<SharedState>>,
    tokio::task::JoinHandle<()>,
    HudSessionClient<tonic::transport::Channel>,
) {
    let (mut client, server, state) = setup_test_with_state().await;
    {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        let tab = scene.create_tab("main", 0).expect("create tab");
        scene.active_tab = Some(tab);
        scene.zone_registry = tze_hud_scene::types::ZoneRegistry::with_defaults();
    }
    let (tx, _init, stream) = handshake(&mut client, agent_id, "test-key").await;
    (tx, stream, state, server, client)
}

fn claim(
    anchor: TileAnchor,
    size: TileSize,
    root: Option<crate::proto::NodeProto>,
) -> ClientPayload {
    ClientPayload::ClaimTile(ClaimTile {
        placement: Some(TilePlacement {
            anchor: anchor as i32,
            size: size as i32,
        }),
        ttl_ms: 60_000,
        root,
    })
}
