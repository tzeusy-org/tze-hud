//! Exemplar dashboard tile agent scaffold — session and lease acquisition tests.
//!
//! Implements the test harness for `hud-i6yd.3` (Scaffold exemplar agent with
//! session and lease acquisition). Covers tasks.md §1 (Exemplar Agent Scaffold)
//! and §2 (Lease Acquisition), plus spec.md Requirement: Lease Request With AutoRenew
//! (scenarios 1 and 2).
//!
//! All tests are headless Layer 0: no display server or GPU required.
//!
//! Test scenarios:
//! 1. Session establishment produces `SessionEstablished` with valid `session_id`
//!    and namespace assignment.
//! 2. Lease request with ttl_ms=60000 and capabilities=[create_tiles, modify_own_tiles]
//!    returns `LeaseResponse { granted: true }` with a 16-byte UUIDv7 `lease_id`.
//! 3. Lease request containing a non-canonical capability name is denied with
//!    `LeaseResponse { granted: false }` (CONFIG_UNKNOWN_CAPABILITY).
//! 4. MutationBatch submitted with a random (unknown) lease_id is rejected with
//!    `MutationResult { accepted: false }` (MUTATION_REJECTED / LeaseNotFound).

use tokio_stream::StreamExt;
use tze_hud_protocol::proto::session::client_message::Payload as ClientPayload;
use tze_hud_protocol::proto::session::hud_session_client::HudSessionClient;
use tze_hud_protocol::proto::session::hud_session_server::HudSessionServer;
use tze_hud_protocol::proto::session::server_message::Payload as ServerPayload;
use tze_hud_protocol::proto::session::{ClientMessage, LeaseRequest, MutationBatch, SessionInit};
use tze_hud_protocol::session_server::HudSessionImpl;
use tze_hud_scene::graph::SceneGraph;

// ─── Test helpers ─────────────────────────────────────────────────────────────

/// Start an in-process HudSession server and return a connected gRPC client.
///
/// Uses an ephemeral TCP port on loopback so tests can run in parallel without
/// port conflicts. The returned `JoinHandle` must be kept alive for the duration
/// of the test.
async fn start_server() -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
) {
    let scene = SceneGraph::new(800.0, 600.0);
    let service = HudSessionImpl::new(scene, "test-psk");

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

    // Brief settle time for the server task to start listening.
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let client = HudSessionClient::connect(format!("http://[::1]:{}", addr.port()))
        .await
        .unwrap();

    (client, handle)
}

/// Wall-clock timestamp helper (µs since Unix epoch).
fn now_wall_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

/// Open a gRPC bidirectional stream, send `SessionInit`, and return
/// `(sender, SessionEstablished_payload, stream)`.
///
/// This helper consumes the mandatory `SceneSnapshot` that follows
/// `SessionEstablished`, so the caller receives a stream positioned at the
/// first message after the snapshot (ready for lease/mutation traffic).
async fn perform_handshake(
    client: &mut HudSessionClient<tonic::transport::Channel>,
    agent_id: &str,
    capabilities: Vec<String>,
) -> (
    tokio::sync::mpsc::Sender<ClientMessage>,
    tze_hud_protocol::proto::session::SessionEstablished,
    tonic::Streaming<tze_hud_protocol::proto::session::ServerMessage>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(32);
    let inbound = tokio_stream::wrappers::ReceiverStream::new(rx);

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: agent_id.to_string(),
            requested_capabilities: capabilities,
            initial_subscriptions: vec![],
            resume_token: vec![],
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(tze_hud_protocol::auth::psk_credential(
                "test-psk".to_string(),
            )),
        })),
    })
    .await
    .unwrap();

    let mut stream = client.session(inbound).await.unwrap().into_inner();

    // First message must be SessionEstablished.
    let first = stream.next().await.unwrap().unwrap();
    let established = match first.payload {
        Some(ServerPayload::SessionEstablished(e)) => e,
        other => panic!("Expected SessionEstablished, got: {other:?}"),
    };

    // Drain the mandatory SceneSnapshot that immediately follows.
    let second = stream.next().await.unwrap().unwrap();
    match second.payload {
        Some(ServerPayload::SceneSnapshot(_)) => {}
        other => panic!("Expected SceneSnapshot after SessionEstablished, got: {other:?}"),
    }

    let third = stream.next().await.unwrap().unwrap();
    match third.payload {
        Some(ServerPayload::DegradationNotice(_)) => {}
        other => panic!("Expected DegradationNotice after SceneSnapshot, got: {other:?}"),
    }

    (tx, established, stream)
}

/// Return the next server message.
async fn next_server_msg(
    stream: &mut tonic::Streaming<tze_hud_protocol::proto::session::ServerMessage>,
) -> tze_hud_protocol::proto::session::ServerMessage {
    stream.next().await.unwrap().unwrap()
}

// ─── Scenario 1: Session establishment ────────────────────────────────────────

/// GIVEN a tonic gRPC client connecting to HudSession,
/// WHEN the agent sends SessionInit with agent_id="dashboard-agent",
/// THEN the server responds with SessionEstablished carrying:
///   - a non-empty session_id (16-byte UUIDv7 bytes),
///   - namespace == "dashboard-agent",
///   - granted_capabilities containing both requested capabilities,
///   - a non-empty resume_token for future reconnection.
///
/// spec.md §Requirement: Lease Request With AutoRenew (precondition: session must exist)
/// tasks.md §1.2: send SessionInit, receive SessionEstablished, verify session_id and namespace
#[tokio::test]
async fn exemplar_session_establishment_produces_session_established() {
    let (mut client, _server) = start_server().await;

    let (_tx, established, _stream) = perform_handshake(
        &mut client,
        "dashboard-agent",
        vec!["create_tiles".to_string(), "modify_own_tiles".to_string()],
    )
    .await;

    // session_id must be a non-empty 16-byte blob (UUIDv7).
    assert!(
        !established.session_id.is_empty(),
        "session_id must be non-empty"
    );
    assert_eq!(
        established.session_id.len(),
        16,
        "session_id must be exactly 16 bytes (UUIDv7)"
    );

    // Namespace must equal the agent_id used in SessionInit.
    assert_eq!(
        established.namespace, "dashboard-agent",
        "namespace must be assigned from agent_id"
    );

    // Both requested capabilities must be granted.
    assert!(
        established
            .granted_capabilities
            .contains(&"create_tiles".to_string()),
        "create_tiles must be in granted_capabilities"
    );
    assert!(
        established
            .granted_capabilities
            .contains(&"modify_own_tiles".to_string()),
        "modify_own_tiles must be in granted_capabilities"
    );

    // Resume token must be present for reconnection support.
    assert!(
        !established.resume_token.is_empty(),
        "resume_token must be non-empty for reconnect support"
    );

    // Mandatory subscriptions always active (RFC 0005 §7.3).
    assert!(
        established
            .active_subscriptions
            .contains(&"LEASE_CHANGES".to_string()),
        "LEASE_CHANGES must always be in active_subscriptions"
    );
    assert!(
        established
            .active_subscriptions
            .contains(&"DEGRADATION_NOTICES".to_string()),
        "DEGRADATION_NOTICES must always be in active_subscriptions"
    );
}

// ─── Scenario 2: Lease grant with valid capabilities ─────────────────────────

/// GIVEN a successfully established session for "dashboard-agent",
/// WHEN the agent sends LeaseRequest { ttl_ms: 60000, capabilities: [create_tiles,
///   modify_own_tiles], lease_priority: 2 },
/// THEN the server responds with LeaseResponse { granted: true } and a 16-byte
///   UUIDv7 lease_id that the agent MUST store for subsequent MutationBatch calls.
///
/// spec.md §Requirement: Lease Request With AutoRenew — Scenario: Lease granted
/// with requested parameters.
/// tasks.md §2.1–2.2: implement LeaseRequest, verify granted=true, store LeaseId.
#[tokio::test]
async fn exemplar_lease_grant_returns_granted_true_and_uuidv7_lease_id() {
    let (mut client, _server) = start_server().await;

    let (tx, _established, mut stream) = perform_handshake(
        &mut client,
        "dashboard-agent-lease",
        vec!["create_tiles".to_string(), "modify_own_tiles".to_string()],
    )
    .await;

    // Send the spec-mandated LeaseRequest.
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::LeaseRequest(LeaseRequest {
            ttl_ms: 60_000,
            capabilities: vec!["create_tiles".to_string(), "modify_own_tiles".to_string()],
            lease_priority: 2,
        })),
    })
    .await
    .unwrap();

    let resp_msg = next_server_msg(&mut stream).await;

    let lease_id_bytes = match resp_msg.payload {
        Some(ServerPayload::LeaseResponse(resp)) => {
            // Granted flag must be true.
            assert!(resp.granted, "LeaseResponse must have granted=true");

            // lease_id must be exactly 16 bytes (UUIDv7 as per SceneId spec).
            assert_eq!(
                resp.lease_id.len(),
                16,
                "lease_id must be 16 bytes (UUIDv7 SceneId)"
            );

            // Granted TTL must equal or exceed the requested TTL.
            assert_eq!(
                resp.granted_ttl_ms, 60_000,
                "granted_ttl_ms must match requested ttl_ms=60000"
            );

            // Both requested capabilities must be in the grant.
            assert!(
                resp.granted_capabilities
                    .contains(&"create_tiles".to_string()),
                "create_tiles must be in granted_capabilities"
            );
            assert!(
                resp.granted_capabilities
                    .contains(&"modify_own_tiles".to_string()),
                "modify_own_tiles must be in granted_capabilities"
            );

            // Priority 2 is the agent-owned default.
            assert_eq!(resp.granted_priority, 2, "granted_priority must be 2");

            resp.lease_id
        }
        other => panic!("Expected LeaseResponse, got: {other:?}"),
    };

    // The stored lease_id must be parseable as a UUID (16 raw bytes, any version).
    assert_eq!(
        lease_id_bytes.len(),
        16,
        "stored lease_id must be 16 bytes suitable for SceneId operations"
    );
}

// ─── Scenario 3: Lease request with non-canonical capability is denied ────────

/// GIVEN a successfully established session,
/// WHEN the agent sends a LeaseRequest containing a non-canonical capability name
///   (e.g. "create_tile" — legacy singular form rejected since RFC 0005 Round 14),
/// THEN the server responds with LeaseResponse { granted: false } and a non-empty
///   deny_code of "CONFIG_UNKNOWN_CAPABILITY".
///
/// spec.md §Requirement: Lease Request With AutoRenew — Scenario: Tile creation
/// requires active lease (precondition: only valid capabilities may be requested).
/// tasks.md §2.3: add test — lease request without required capabilities is denied.
#[tokio::test]
async fn exemplar_lease_request_with_invalid_capability_is_denied() {
    let (mut client, _server) = start_server().await;

    // The session itself only needs create_tiles to be established.
    let (tx, _established, mut stream) = perform_handshake(
        &mut client,
        "bad-caps-agent",
        vec!["create_tiles".to_string()],
    )
    .await;

    // Request a lease with a non-canonical (legacy singular) capability name.
    // "create_tile" (singular) was superseded by "create_tiles" (plural) in
    // RFC 0005 Round 14. The server must reject this with CONFIG_UNKNOWN_CAPABILITY.
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::LeaseRequest(LeaseRequest {
            ttl_ms: 60_000,
            capabilities: vec!["create_tile".to_string()], // non-canonical: singular form
            lease_priority: 2,
        })),
    })
    .await
    .unwrap();

    let resp_msg = next_server_msg(&mut stream).await;

    match resp_msg.payload {
        Some(ServerPayload::LeaseResponse(resp)) => {
            assert!(
                !resp.granted,
                "LeaseResponse must NOT be granted for non-canonical capability"
            );
            assert_eq!(
                resp.deny_code, "CONFIG_UNKNOWN_CAPABILITY",
                "deny_code must be CONFIG_UNKNOWN_CAPABILITY for unknown capability names, \
                 got: {:?}",
                resp.deny_code
            );
            assert!(
                !resp.deny_reason.is_empty(),
                "deny_reason must be non-empty"
            );
        }
        other => {
            panic!("Expected LeaseResponse(denied) for non-canonical capability, got: {other:?}")
        }
    }
}

// ─── Scenario 4: MutationBatch without ACTIVE lease is rejected ───────────────

/// GIVEN a successfully established session with NO prior lease acquisition,
/// WHEN the agent submits a MutationBatch referencing a random (unknown) lease_id,
/// THEN the server responds with MutationResult { accepted: false } and a non-empty
///   error_code (MUTATION_REJECTED or INVALID_ARGUMENT), indicating the lease was
///   not found or not active.
///
/// spec.md §Requirement: Lease Request With AutoRenew — Scenario: Tile creation
/// requires active lease.
/// tasks.md §2 (implicit): MutationBatch without a prior ACTIVE lease is rejected.
#[tokio::test]
async fn exemplar_mutation_without_active_lease_is_rejected() {
    let (mut client, _server) = start_server().await;

    let (tx, _established, mut stream) = perform_handshake(
        &mut client,
        "no-lease-agent",
        vec!["create_tiles".to_string()],
    )
    .await;

    // Use a random 16-byte UUID as the lease_id. Since no lease has been acquired,
    // the server must not find this in its active lease registry and must reject
    // the batch. (bytes_to_scene_id requires exactly 16 bytes; using a valid-length
    // ID exercises the lease-not-found path rather than the INVALID_ARGUMENT path.)
    let random_lease_id: Vec<u8> = uuid::Uuid::now_v7().as_bytes().to_vec();
    let batch_id: Vec<u8> = uuid::Uuid::now_v7().as_bytes().to_vec();

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: batch_id.clone(),
            lease_id: random_lease_id,
            mutations: vec![],
            timing: None,
        })),
    })
    .await
    .unwrap();

    // Drain any interleaved lease state changes (none expected here since no lease
    // was acquired, but drain defensively for robustness).
    let result_msg = next_server_msg(&mut stream).await;

    match result_msg.payload {
        Some(ServerPayload::MutationResult(result)) => {
            assert!(
                !result.accepted,
                "MutationResult must NOT be accepted when no active lease exists"
            );
            // batch_id must always be echoed back (RFC 0005 §3.2).
            assert_eq!(
                result.batch_id, batch_id,
                "MutationResult.batch_id must echo the client-provided batch_id"
            );
            // Error code must be populated.
            assert!(
                !result.error_code.is_empty(),
                "error_code must be non-empty on rejection"
            );
        }
        other => panic!("Expected MutationResult(rejected) when no active lease, got: {other:?}"),
    }
}
