//! Real resident streams crossing the runtime's existing pairing/Unpair seam.
use super::*;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tze_hud_protocol::proto::session::{
    self, ClientMessage, ResourceMetadata, ResourceUploadStart, ServerMessage, SessionInit,
    client_message::Payload as ClientPayload, hud_session_client::HudSessionClient,
    hud_session_server::HudSessionServer, server_message::Payload as ServerPayload,
};
use tze_hud_protocol::session_server::{HudSessionImpl, SessionDeps};
use tze_hud_resource::{
    ResidentLedger, ResidentLedgerLimits, ResourceId, ResourceStore, ResourceStoreConfig,
};
use tze_hud_scene::config::agents::PairingAuthorization;

const WAIT: std::time::Duration = std::time::Duration::from_secs(5);

struct Resident {
    tx: mpsc::Sender<ClientMessage>,
    rx: tonic::Streaming<ServerMessage>,
    sequence: u64,
}

impl Resident {
    async fn connect(endpoint: &str, agent: &str, credential: &str) -> Self {
        let mut client = HudSessionClient::connect(endpoint.to_owned())
            .await
            .unwrap();
        let (tx, rx) = mpsc::channel(16);
        tx.send(ClientMessage {
            sequence: 1,
            timestamp_wall_us: 0,
            payload: Some(ClientPayload::SessionInit(SessionInit {
                agent_id: agent.into(),
                auth_credential: Some(tze_hud_protocol::auth::psk_credential(credential)),
            })),
        })
        .await
        .unwrap();
        let rx = client
            .session(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();
        let mut resident = Self {
            tx,
            rx,
            sequence: 1,
        };
        assert!(matches!(
            resident.next().await,
            ServerPayload::SessionEstablished(_)
        ));
        assert!(matches!(
            resident.next().await,
            ServerPayload::DegradationNotice(_)
        ));
        resident
    }

    async fn send(&mut self, payload: ClientPayload) {
        self.sequence += 1;
        self.tx
            .send(ClientMessage {
                sequence: self.sequence,
                timestamp_wall_us: 0,
                payload: Some(payload),
            })
            .await
            .unwrap();
    }

    async fn next(&mut self) -> ServerPayload {
        tokio::time::timeout(WAIT, self.rx.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .payload
            .unwrap()
    }

    async fn publish_image(&mut self, bytes: &[u8], zone: &str, at: u64) -> session::RequestResult {
        self.send(ClientPayload::Publish(session::Publish {
            surface: format!("zone:{zone}"),
            content: Some(tze_hud_protocol::proto::ZoneContent {
                payload: Some(
                    tze_hud_protocol::proto::zone_content::Payload::StaticImageRef(
                        tze_hud_protocol::proto::StaticImageRef {
                            resource_id: ResourceId::from_content(bytes).as_bytes().to_vec(),
                        },
                    ),
                ),
            }),
            present_at_us: at,
            ..Default::default()
        }))
        .await;
        let ServerPayload::RequestResult(result) = self.next().await else {
            panic!("non-ephemeral Publish must return RequestResult")
        };
        result
    }

    async fn upload(&mut self, bytes: &[u8], kind: i32, width: u32, height: u32, inline: bool) {
        let hash = ResourceId::from_content(bytes);
        self.send(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: hash.as_bytes().to_vec(),
            resource_type: kind,
            total_size_bytes: bytes.len() as u64,
            metadata: Some(ResourceMetadata {
                width,
                height,
                ..Default::default()
            }),
            inline_data: if inline { bytes.to_vec() } else { Vec::new() },
        }))
        .await;
        if !inline {
            let ServerPayload::ResourceUploadAccepted(accepted) = self.next().await else {
                panic!("chunk upload must be accepted")
            };
            self.send(ClientPayload::ResourceUploadChunk(
                session::ResourceUploadChunk {
                    upload_id: accepted.upload_id.clone(),
                    chunk_index: 0,
                    data: bytes.to_vec(),
                },
            ))
            .await;
            self.send(ClientPayload::ResourceUploadComplete(
                session::ResourceUploadComplete {
                    upload_id: accepted.upload_id,
                },
            ))
            .await;
        }
    }

    async fn digest(&mut self, bytes: &[u8], kind: i32, width: u32, height: u32) -> ServerPayload {
        self.send(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: ResourceId::from_content(bytes).as_bytes().to_vec(),
            resource_type: kind,
            total_size_bytes: bytes.len() as u64,
            metadata: Some(ResourceMetadata {
                width,
                height,
                ..Default::default()
            }),
            inline_data: Vec::new(),
        }))
        .await;
        self.next().await
    }
}

struct RasterRig {
    pairing: crate::pairing::tests::Rig,
    control: AgentControl,
    store: ResourceStore,
    ledger: ResidentLedger,
    clock: Arc<tze_hud_scene::clock::TestClock>,
    endpoint: String,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for RasterRig {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl RasterRig {
    async fn new(capacity: u64) -> Self {
        Self::with_caps(capacity, capacity).await
    }

    async fn with_caps(store_capacity: u64, ledger_capacity: u64) -> Self {
        let pairing = crate::pairing::tests::rig();
        let ledger = ResidentLedger::new(ResidentLedgerLimits {
            aggregate_bytes: ledger_capacity,
            resource_bytes: ledger_capacity,
            widget_source_bytes: 0,
            widget_raster_bytes: 0,
            font_bytes: 0,
        });
        let store = ResourceStore::new_with_resident_ledger(
            ResourceStoreConfig {
                max_total_texture_bytes: store_capacity as usize,
                ..Default::default()
            },
            ledger.clone(),
        );
        let clock = Arc::new(tze_hud_scene::clock::TestClock::new(10_000));
        let mut scene = SceneGraph::new_with_clock(800.0, 600.0, clock.clone());
        scene.zone_registry = tze_hud_scene::ZoneRegistry::with_defaults();
        let base = HudSessionImpl::new(scene, "");
        let shared = base.state.clone();
        shared.lock().await.resource_store = store.clone();
        let scene = shared.lock().await.scene.clone();
        let control = AgentControl {
            store: pairing.store.clone(),
            shared: shared.clone(),
            scene,
            portals: None,
            pairing: Some(pairing.pairing.clone()),
            render_wake: Default::default(),
            main_wake: Default::default(),
        };
        let service = HudSessionImpl::from_deps(SessionDeps::new(shared, pairing.agents.clone()));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(HudSessionServer::new(service))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        Self {
            pairing,
            control,
            store,
            ledger,
            clock,
            endpoint,
            server,
        }
    }

    fn pair(&self, agent: &str) -> String {
        self.pairing.pairing.open();
        let (code, _) = self.pairing.pairing.code().unwrap();
        let response = self
            .pairing
            .pairing
            .pair(&serde_json::to_vec(&serde_json::json!({"agent":agent,"code":code})).unwrap());
        assert_eq!(response.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        body["psk"].as_str().unwrap().to_owned()
    }

    fn authorization(&self, agent: &str, credential: &str) -> PairingAuthorization {
        self.pairing
            .agents
            .load()
            .resolve(credential, agent)
            .unwrap()
            .authorization
    }
}

fn granted(authorization: &PairingAuthorization, bytes: &[u8]) -> bool {
    authorization
        .with_live_grants(|g| g.contains(ResourceId::from_content(bytes).as_bytes()))
        .unwrap_or(false)
}

fn stored(payload: ServerPayload) -> session::ResourceStored {
    let ServerPayload::ResourceStored(stored) = payload else {
        panic!("expected ResourceStored")
    };
    stored
}

fn error(payload: ServerPayload, expected: session::ResourceErrorCode) {
    let ServerPayload::ResourceErrorResponse(error) = payload else {
        panic!("expected resource error")
    };
    assert_eq!(error.error_code, expected as i32);
}

#[tokio::test]
async fn resident_raster_first_grants_validate_and_keep_one_store_charge() {
    let rig = RasterRig::new(8).await;
    let key_a = rig.pair("raster-a");
    let key_b = rig.pair("raster-b");
    let auth_a = rig.authorization("raster-a", &key_a);
    let auth_b = rig.authorization("raster-b", &key_b);
    let mut a = Resident::connect(&rig.endpoint, "raster-a", &key_a).await;
    let mut b = Resident::connect(&rig.endpoint, "raster-b", &key_b).await;
    let pixels = [255, 0, 0, 255, 0, 255, 0, 255];
    let id = ResourceId::from_content(&pixels);
    a.upload(&pixels, 1, 2, 1, true).await;
    assert!(!stored(a.next().await).was_deduplicated);
    let allocation = rig
        .store
        .dedup_index()
        .get(&id)
        .unwrap()
        .raster
        .as_ref()
        .unwrap()
        .clone();
    assert_eq!(allocation.pixels.as_ref(), &pixels);
    assert_eq!((allocation.width, allocation.height), (2, 1));
    let charged = rig.ledger.snapshot();
    assert_eq!(charged.resource_bytes, 8);
    assert_eq!(charged.allocation_count, 1);

    // Another authenticated namespace cannot get its first grant by hash alone.
    assert!(matches!(
        b.digest(&pixels, 1, 2, 1).await,
        ServerPayload::ResourceUploadAccepted(_)
    ));
    assert!(!granted(&auth_b, &pixels));
    // A dedup hit still validates full bytes and exact type/dimensions.
    b.upload(&pixels, 1, 1, 2, true).await;
    error(
        b.next().await,
        session::ResourceErrorCode::ResourceDecodeError,
    );
    b.upload(&pixels, 2, 0, 0, true).await;
    error(
        b.next().await,
        session::ResourceErrorCode::ResourceDecodeError,
    );
    let mut corrupt = pixels;
    corrupt[0] = 0;
    b.send(ClientPayload::ResourceUploadStart(ResourceUploadStart {
        expected_hash: id.as_bytes().to_vec(),
        resource_type: 1,
        total_size_bytes: 8,
        metadata: Some(ResourceMetadata {
            width: 2,
            height: 1,
            ..Default::default()
        }),
        inline_data: corrupt.to_vec(),
    }))
    .await;
    error(
        b.next().await,
        session::ResourceErrorCode::ResourceHashMismatch,
    );
    assert!(!granted(&auth_b, &pixels));
    b.upload(&pixels, 1, 2, 1, false).await;
    assert!(stored(b.next().await).was_deduplicated);
    assert!(granted(&auth_a, &pixels) && granted(&auth_b, &pixels));
    assert!(Arc::ptr_eq(
        &allocation,
        rig.store
            .dedup_index()
            .get(&id)
            .unwrap()
            .raster
            .as_ref()
            .unwrap()
    ));
    assert_eq!(rig.ledger.snapshot(), charged);
    assert!(stored(b.digest(&pixels, 1, 2, 1).await).was_deduplicated);

    // Permission edits preserve both the pairing token and connected grants.
    rig.control
        .set_grant("raster-a", Grant::Zones, false)
        .await
        .unwrap();
    let edited = rig.authorization("raster-a", &key_a);
    assert_eq!(edited, auth_a);
    assert!(stored(a.digest(&pixels, 1, 2, 1).await).was_deduplicated);
    let mut fresh = Resident::connect(&rig.endpoint, "raster-a", &key_a).await;
    fresh.upload(&pixels, 1, 2, 1, true).await;
    error(
        fresh.next().await,
        session::ResourceErrorCode::ResourceCapabilityDenied,
    );

    // A new allocation is refused at capacity before record or grant appears.
    let other = [0, 0, 255, 255];
    a.upload(&other, 1, 1, 1, true).await;
    error(
        a.next().await,
        session::ResourceErrorCode::ResourceBudgetExceeded,
    );
    assert!(!granted(&auth_a, &other));
    assert!(
        rig.store
            .dedup_index()
            .get(&ResourceId::from_content(&other))
            .is_none()
    );
    assert_eq!(rig.store.dedup_index().len(), 1);
    assert_eq!(rig.ledger.snapshot(), charged);
    rig.control.unpair("raster-a").await.unwrap();
    assert!(auth_a.with_live_grants(|_| ()).is_none());
    assert!(granted(&auth_b, &pixels));
    assert!(stored(b.digest(&pixels, 1, 2, 1).await).was_deduplicated);
    assert_eq!(rig.ledger.snapshot(), charged);

    // PNG/JPEG keep decoded RGBA and share that exact store allocation on a
    // second authenticated first grant, just like the raw directional raster.
    for (kind, format) in [(2, image::ImageFormat::Png), (3, image::ImageFormat::Jpeg)] {
        let rig = RasterRig::new(8).await;
        let key_a = rig.pair("raster-a");
        let key_b = rig.pair("raster-b");
        let mut a = Resident::connect(&rig.endpoint, "raster-a", &key_a).await;
        let mut b = Resident::connect(&rig.endpoint, "raster-b", &key_b).await;
        let image = image::DynamicImage::ImageRgb8(
            image::RgbImage::from_raw(2, 1, vec![255, 0, 0, 0, 255, 0]).unwrap(),
        );
        let mut encoded = std::io::Cursor::new(Vec::new());
        image.write_to(&mut encoded, format).unwrap();
        let encoded = encoded.into_inner();
        let id = ResourceId::from_content(&encoded);
        a.upload(&encoded, kind, 0, 0, true).await;
        let first = stored(a.next().await);
        assert!(!first.was_deduplicated);
        let metadata = first.metadata.unwrap();
        assert_eq!((metadata.width, metadata.height), (2, 1));
        // Nonzero compressed declarations must match, even on an existing hash.
        b.upload(&encoded, kind, 1, 2, true).await;
        error(
            b.next().await,
            session::ResourceErrorCode::ResourceDecodeError,
        );
        assert!(!granted(&rig.authorization("raster-b", &key_b), &encoded));
        let raster = rig
            .store
            .dedup_index()
            .get(&id)
            .unwrap()
            .raster
            .as_ref()
            .unwrap()
            .clone();
        assert_eq!(
            (raster.width, raster.height, raster.pixels.len()),
            (2, 1, 8)
        );
        if kind == 2 {
            assert_eq!(raster.pixels.as_ref(), &pixels);
        }
        b.upload(&encoded, kind, 0, 0, false).await;
        assert!(stored(b.next().await).was_deduplicated);
        assert!(granted(&rig.authorization("raster-b", &key_b), &encoded));
        assert!(Arc::ptr_eq(
            &raster,
            rig.store
                .dedup_index()
                .get(&id)
                .unwrap()
                .raster
                .as_ref()
                .unwrap()
        ));
        assert_eq!(rig.ledger.snapshot().resource_bytes, 8);
        assert_eq!(rig.ledger.snapshot().allocation_count, 1);
        error(
            b.digest(&encoded, kind, 1, 2).await,
            session::ResourceErrorCode::ResourceDecodeError,
        );
    }
    // Separate caps exercise both the store check and actual ledger refusal.
    for (store_cap, ledger_cap) in [(3, 64), (64, 3)] {
        let rig = RasterRig::with_caps(store_cap, ledger_cap).await;
        let key = rig.pair("raster-a");
        let auth = rig.authorization("raster-a", &key);
        let mut a = Resident::connect(&rig.endpoint, "raster-a", &key).await;
        let bytes = [1, 2, 3, 255];
        a.upload(&bytes, 1, 1, 1, true).await;
        error(
            a.next().await,
            session::ResourceErrorCode::ResourceBudgetExceeded,
        );
        assert!(!granted(&auth, &bytes));
        assert!(
            rig.store
                .dedup_index()
                .get(&ResourceId::from_content(&bytes))
                .is_none()
        );
        let denied = rig.ledger.snapshot();
        assert_eq!((denied.resource_bytes, denied.allocation_count), (0, 0));
        assert_eq!(denied.class_denial_count, u64::from(ledger_cap < 4));
    }
}

#[tokio::test]
async fn runtime_unpair_blocks_old_resident_raster_commit_across_repair() {
    for replacement in [false, true] {
        for inline in [true, false] {
            for dedup in [true, false] {
                let rig = RasterRig::new(64).await;
                let key_a = rig.pair("raster-a");
                let key_b = rig.pair("raster-b");
                let auth_a = rig.authorization("raster-a", &key_a);
                let auth_b = rig.authorization("raster-b", &key_b);
                let mut a = Resident::connect(&rig.endpoint, "raster-a", &key_a).await;
                let mut b = Resident::connect(&rig.endpoint, "raster-b", &key_b).await;
                let peer_pixels = [255, 0, 0, 255];
                let new_pixels = [0, 255, 0, 255];
                b.upload(&peer_pixels, 1, 1, 1, true).await;
                stored(b.next().await);
                let peer_id = ResourceId::from_content(&peer_pixels);
                let peer_allocation = rig
                    .store
                    .dedup_index()
                    .get(&peer_id)
                    .unwrap()
                    .raster
                    .as_ref()
                    .unwrap()
                    .clone();
                let before = rig.ledger.snapshot();
                let target = if dedup { &peer_pixels } else { &new_pixels };
                let target_id = ResourceId::from_content(target);
                let mut stale_slots = Vec::new();
                if replacement {
                    for _ in 0..if inline { 4 } else { 3 } {
                        let ServerPayload::ResourceUploadAccepted(accepted) =
                            a.digest(target, 1, 1, 1).await
                        else {
                            panic!("old chunk slot")
                        };
                        stale_slots.push(accepted.upload_id);
                    }
                }
                let (entered, release) = rig.store.pause_before_raster_commit(target_id);
                a.upload(target, 1, 1, 1, inline).await;
                tokio::time::timeout(WAIT, entered.notified())
                    .await
                    .unwrap();
                let (new_key_a, unpair) = if replacement {
                    // Actual serialized Pair edit with the same test key, without
                    // closing the old stream: its denied completion is observable.
                    let record = rig.pairing.store.cached().agents["raster-a"].clone();
                    rig.pairing
                        .store
                        .apply(&tze_hud_config::agents_file::AgentChange::Pair {
                            agent: "raster-a".into(),
                            record,
                        })
                        .unwrap();
                    (key_a.clone(), None)
                } else {
                    let control = rig.control.clone();
                    let unpair = tokio::spawn(async move { control.unpair("raster-a").await });
                    tokio::time::timeout(WAIT, async {
                        while auth_a.with_live_grants(|_| ()).is_some() {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                    // Pair through the production HTTP implementation while the
                    // old upload remains parked, before waiting for its cleanup.
                    (rig.pair("raster-a"), Some(unpair))
                };
                let new_auth_a = rig.authorization("raster-a", &new_key_a);
                assert_ne!(new_auth_a, auth_a);
                release.notify_one();
                if let Some(unpair) = unpair {
                    tokio::time::timeout(WAIT, unpair)
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap();
                } else {
                    error(
                        a.next().await,
                        session::ResourceErrorCode::ResourceCapabilityDenied,
                    );
                    error(
                        a.digest(target, 1, 1, 1).await,
                        session::ResourceErrorCode::ResourceCapabilityDenied,
                    );
                    assert_eq!(
                        rig.store.in_flight_count("raster-a").await,
                        stale_slots.len()
                    );
                    a.send(ClientPayload::ResourceUploadChunk(
                        session::ResourceUploadChunk {
                            upload_id: stale_slots[0].clone(),
                            chunk_index: 0,
                            data: target.to_vec(),
                        },
                    ))
                    .await;
                    error(
                        a.next().await,
                        session::ResourceErrorCode::ResourceCapabilityDenied,
                    );
                    assert_eq!(
                        rig.store.in_flight_count("raster-a").await,
                        stale_slots.len() - 1
                    );
                }
                assert!(auth_a.with_live_grants(|_| ()).is_none());
                assert!(!granted(&new_auth_a, target));
                assert!(granted(&auth_b, &peer_pixels));
                assert_eq!(rig.ledger.snapshot(), before);
                assert_eq!(rig.store.dedup_index().len(), 1);
                assert!(Arc::ptr_eq(
                    &peer_allocation,
                    rig.store
                        .dedup_index()
                        .get(&peer_id)
                        .unwrap()
                        .raster
                        .as_ref()
                        .unwrap()
                ));
                if !dedup {
                    assert!(rig.store.dedup_index().get(&target_id).is_none());
                }
                assert!(stored(b.digest(&peer_pixels, 1, 1, 1).await).was_deduplicated);
                let mut repaired = Resident::connect(&rig.endpoint, "raster-a", &new_key_a).await;
                assert!(matches!(
                    repaired.digest(target, 1, 1, 1).await,
                    ServerPayload::ResourceUploadAccepted(_)
                ));
                assert!(!granted(&new_auth_a, target));
                assert_eq!(
                    rig.store.in_flight_count("raster-a").await,
                    1,
                    "fresh admission prunes only revoked raster slots"
                );
                repaired.upload(target, 1, 1, 1, true).await;
                assert_eq!(stored(repaired.next().await).was_deduplicated, dedup);
                assert!(granted(&new_auth_a, target));
                assert_eq!(
                    rig.ledger.snapshot().resource_bytes,
                    if dedup { 4 } else { 8 }
                );
            }
        }
    }
}

#[tokio::test]
async fn resident_raster_publish_admits_current_grants_before_enqueue_and_apply() {
    let rig = RasterRig::new(8).await;
    let key_a = rig.pair("raster-a");
    let key_b = rig.pair("raster-b");
    let mut a = Resident::connect(&rig.endpoint, "raster-a", &key_a).await;
    let mut b = Resident::connect(&rig.endpoint, "raster-b", &key_b).await;
    let pixels = [255, 0, 0, 255, 0, 255, 0, 255];
    a.upload(&pixels, 1, 2, 1, true).await;
    stored(a.next().await);
    let charged = rig.ledger.snapshot();
    let allocation = rig
        .store
        .validated_raster(&ResourceId::from_content(&pixels))
        .unwrap();
    let now = rig.control.scene.lock().await.now_wall_us();
    for at in [0, now + 10_000] {
        let denied = b.publish_image(&pixels, "ambient-background", at).await;
        assert!(!denied.ok);
        assert_eq!(denied.code, "INVALID_ARGUMENT");
        assert!(denied.hint.contains("resource"));
        let unknown = a.publish_image(&[7; 4], "ambient-background", at).await;
        assert!(!unknown.ok);
        assert_eq!(unknown.code, "INVALID_ARGUMENT");
        let wrong_zone = a.publish_image(&pixels, "subtitle", at).await;
        assert!(!wrong_zone.ok);
        assert_eq!(wrong_zone.code, "CONTENT_REJECTED");
    }
    {
        let scene = rig.control.scene.lock().await;
        assert!(scene.zone_registry.active_publishes.is_empty());
        assert!(scene.scheduled_publications.is_empty());
    }
    assert!(a.publish_image(&pixels, "ambient-background", 0).await.ok);
    {
        let scene = rig.control.scene.lock().await;
        let record = &scene.zone_registry.active_for_zone("ambient-background")[0];
        let source = record.resident_raster.as_ref().unwrap();
        let (retained, width, height) = source.pixels_if_authorized().unwrap();
        assert!(Arc::ptr_eq(&retained, &allocation.pixels));
        assert_eq!((width, height), (2, 1));
        let restored: tze_hud_scene::ZonePublishRecord =
            serde_json::from_str(&serde_json::to_string(record).unwrap()).unwrap();
        assert!(
            restored.resident_raster.is_none(),
            "serialized hashes confer no authority"
        );
    }
    a.send(ClientPayload::Clear(session::Clear {
        surface: "zone:ambient-background".into(),
        ..Default::default()
    }))
    .await;
    let ServerPayload::RequestResult(clear) = a.next().await else {
        panic!("clear result")
    };
    assert!(clear.ok);
    assert!(
        rig.control
            .scene
            .lock()
            .await
            .zone_registry
            .active_for_zone("ambient-background")
            .is_empty()
    );
    assert_eq!(rig.ledger.snapshot(), charged);
    assert!(
        a.publish_image(&pixels, "ambient-background", now + 10_000)
            .await
            .ok
    );
    {
        let mut scene = rig.control.scene.lock().await;
        assert_eq!(scene.scheduled_publications.len(), 1);
        rig.clock.advance_us(10_000);
        let due = scene.apply_due_publications();
        assert_eq!(due.len(), 1);
        assert!(due[0].result.is_ok());
        assert!(
            scene.zone_registry.active_for_zone("ambient-background")[0]
                .resident_raster
                .as_ref()
                .unwrap()
                .is_authorized()
        );
    }
    assert!(a.publish_image(&pixels, "pip", now + 20_000).await.ok);
    // Explicit same-key Pair leaves the old stream observable, but revokes its
    // accepted provenance before due application, without namespace Clear.
    let record = rig.pairing.store.cached().agents["raster-a"].clone();
    rig.pairing
        .store
        .apply(&tze_hud_config::agents_file::AgentChange::Pair {
            agent: "raster-a".into(),
            record,
        })
        .unwrap();
    {
        let mut scene = rig.control.scene.lock().await;
        rig.clock.advance_us(10_000);
        let due = scene.apply_due_publications();
        assert_eq!(due.len(), 1);
        assert!(due[0].result.is_err());
        assert!(scene.zone_registry.active_for_zone("pip").is_empty());
        assert!(
            !scene.zone_registry.active_for_zone("ambient-background")[0]
                .resident_raster
                .as_ref()
                .unwrap()
                .is_authorized()
        );
    }
    for at in [0, now + 30_000] {
        let denied = a.publish_image(&pixels, "ambient-background", at).await;
        assert!(!denied.ok);
        assert_eq!(denied.code, "INVALID_ARGUMENT");
    }
    b.upload(&pixels, 1, 2, 1, true).await;
    assert!(stored(b.next().await).was_deduplicated);
    assert!(b.publish_image(&pixels, "ambient-background", 0).await.ok);
    assert_eq!(rig.ledger.snapshot(), charged);
    assert!(Arc::ptr_eq(
        &allocation.pixels,
        &rig.store
            .validated_raster(&ResourceId::from_content(&pixels))
            .unwrap()
            .pixels
    ));
}
