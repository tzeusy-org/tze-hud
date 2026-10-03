//! Public-API behavior of the asset store, pinned to docs/invariants.md §7
//! (budgets are hard caps) and §8 (errors are affordances for the model).
//!
//! Everything goes through `ResourceStore`'s public upload calls. Uploads use
//! raw RGBA8 so a resource's decoded size is exactly `w * h * 4`. Nothing here
//! reads a clock or sleeps.

use tze_hud_resource::{
    AgentBudget, CHUNK_SIZE_LIMIT, MAX_CONCURRENT_UPLOADS_PER_AGENT, ResourceError, ResourceId,
    ResourceStore, ResourceStoreConfig, ResourceType, UploadId, UploadStartRequest,
};

fn caps() -> Vec<String> {
    vec!["upload_resource".to_string()]
}

fn unlimited() -> AgentBudget {
    AgentBudget {
        texture_bytes_total_limit: 0,
        texture_bytes_total_used: 0,
    }
}

/// A solid `side x side` RGBA8 image; `seed` makes distinct content.
fn rgba(side: u32, seed: u8) -> Vec<u8> {
    vec![seed; (side * side * 4) as usize]
}

fn hash(data: &[u8]) -> [u8; 32] {
    *blake3::hash(data).as_bytes()
}

fn inline_req(
    agent: &str,
    id: u8,
    side: u32,
    data: Vec<u8>,
    budget: AgentBudget,
) -> UploadStartRequest {
    UploadStartRequest {
        agent_namespace: agent.into(),
        agent_capabilities: caps(),
        agent_budget: budget,
        upload_id: UploadId::from_bytes([id; 16]),
        resource_type: ResourceType::ImageRgba8,
        expected_hash: hash(&data),
        total_size: data.len(),
        inline_data: data,
        width: side,
        height: side,
    }
}

fn chunked_start(agent: &str, id: u8, side: u32, data: &[u8]) -> UploadStartRequest {
    UploadStartRequest {
        inline_data: Vec::new(),
        ..inline_req(agent, id, side, data.to_vec(), unlimited())
    }
}

async fn upload(
    store: &ResourceStore,
    agent: &str,
    id: u8,
    side: u32,
    seed: u8,
    budget: AgentBudget,
) -> Result<(), ResourceError> {
    store
        .handle_upload_start(inline_req(agent, id, side, rgba(side, seed), budget))
        .await
        .map(|stored| assert!(stored.is_some()))
}

fn stored(store: &ResourceStore, side: u32, seed: u8) -> bool {
    store
        .dedup_index()
        .contains(&ResourceId::from_bytes(hash(&rgba(side, seed))))
}

// ─── Invariant 7: hard caps ───────────────────────────────────────────────────

#[tokio::test]
async fn agent_texture_budget_rejects_whole_upload_and_stores_nothing() {
    let store = ResourceStore::new(ResourceStoreConfig::default());
    // 8x8 RGBA = 256 bytes; the agent has 100 bytes of headroom.
    let tight = AgentBudget {
        texture_bytes_total_limit: 1000,
        texture_bytes_total_used: 900,
    };

    let err = upload(&store, "a", 1, 8, 1, tight.clone())
        .await
        .unwrap_err();

    assert!(matches!(err, ResourceError::BudgetExceeded { .. }));
    assert!(
        !stored(&store, 8, 1),
        "rejected upload must leave no resource behind"
    );
    assert_eq!(store.dedup_index().total_decoded_bytes(), 0);

    // The cap is a cap, not a penalty: a fitting upload straight after succeeds.
    upload(&store, "a", 2, 2, 2, tight).await.unwrap(); // 16 bytes
    assert!(stored(&store, 2, 2));
}

#[tokio::test]
async fn runtime_wide_texture_cap_is_shared_across_agents() {
    let store = ResourceStore::new(ResourceStoreConfig {
        max_total_texture_bytes: 300,
        ..ResourceStoreConfig::default()
    });

    upload(&store, "a", 1, 8, 1, unlimited()).await.unwrap(); // 256 bytes
    // A different agent with an unlimited personal budget still hits the shared ceiling.
    let err = upload(&store, "b", 1, 4, 2, unlimited()).await.unwrap_err(); // +64 > 300

    assert!(matches!(err, ResourceError::BudgetExceeded { .. }));
    assert!(
        stored(&store, 8, 1),
        "earlier resource is untouched by the rejection"
    );
    assert!(!stored(&store, 4, 2));
    assert_eq!(store.dedup_index().total_decoded_bytes(), 256);
}

#[tokio::test]
async fn resource_count_cap_rejects_the_extra_resource() {
    let store = ResourceStore::new(ResourceStoreConfig {
        max_concurrent_resources: 2,
        ..ResourceStoreConfig::default()
    });
    upload(&store, "a", 1, 1, 1, unlimited()).await.unwrap();
    upload(&store, "a", 2, 1, 2, unlimited()).await.unwrap();

    let err = upload(&store, "a", 3, 1, 3, unlimited()).await.unwrap_err();

    assert!(matches!(err, ResourceError::BudgetExceeded { .. }));
    assert_eq!(store.dedup_index().len(), 2);
    // Re-uploading something already stored is a dedup hit, not a new resource.
    let again = store
        .handle_upload_start(inline_req("a", 4, 1, rgba(1, 1), unlimited()))
        .await
        .unwrap()
        .unwrap();
    assert!(again.was_deduplicated);
}

#[tokio::test]
async fn per_resource_size_cap_rejects_chunked_upload_at_start() {
    let store = ResourceStore::new(ResourceStoreConfig {
        max_resource_bytes: 100,
        ..ResourceStoreConfig::default()
    });
    let data = rgba(8, 1); // 256 bytes declared

    let err = store
        .handle_upload_start(chunked_start("a", 1, 8, &data))
        .await
        .unwrap_err();

    assert!(matches!(err, ResourceError::SizeExceeded { .. }));
    assert_eq!(
        store.in_flight_count("a").await,
        0,
        "no upload slot is consumed"
    );
}

#[tokio::test]
async fn upload_slot_cap_is_per_agent_and_freed_by_abort() {
    let store = ResourceStore::new(ResourceStoreConfig::default());
    let data = rgba(2, 1);
    for id in 0..MAX_CONCURRENT_UPLOADS_PER_AGENT as u8 {
        assert!(
            store
                .handle_upload_start(chunked_start("a", id, 2, &data))
                .await
                .unwrap()
                .is_none()
        );
    }

    let err = store
        .handle_upload_start(chunked_start("a", 99, 2, &data))
        .await
        .unwrap_err();
    assert_eq!(err, ResourceError::TooManyUploads);
    // Another agent is unaffected by a's slots.
    store
        .handle_upload_start(chunked_start("b", 99, 2, &data))
        .await
        .unwrap();

    store.abort_upload("a", UploadId::from_bytes([0; 16])).await;
    store
        .handle_upload_start(chunked_start("a", 99, 2, &data))
        .await
        .expect("an aborted upload frees its slot");
}

#[tokio::test]
async fn chunked_upload_rejected_at_complete_frees_its_slot_and_stores_nothing() {
    let store = ResourceStore::new(ResourceStoreConfig::default());
    let data = rgba(8, 1);
    let tight = AgentBudget {
        texture_bytes_total_limit: 100,
        texture_bytes_total_used: 0,
    };
    let id = UploadId::from_bytes([1; 16]);
    store
        .handle_upload_start(chunked_start("a", 1, 8, &data))
        .await
        .unwrap();
    store
        .handle_upload_chunk("a", id, 0, data.clone())
        .await
        .unwrap();

    let err = store
        .handle_upload_complete("a", id, &caps(), &tight)
        .await
        .unwrap_err();

    assert!(matches!(err, ResourceError::BudgetExceeded { .. }));
    assert_eq!(store.in_flight_count("a").await, 0);
    assert!(!stored(&store, 8, 1));
}

// ─── Invariant 8: errors are affordances ──────────────────────────────────────

/// Each rejection reachable through the store carries a stable wire code, and
/// its message leads with that code and names the offending quantity.
#[tokio::test]
async fn rejections_carry_stable_wire_code_and_actionable_detail() {
    let store = ResourceStore::new(ResourceStoreConfig {
        max_resource_bytes: 1000,
        max_total_texture_bytes: 1000,
        ..ResourceStoreConfig::default()
    });
    let data = rgba(2, 1);

    let mut no_caps = inline_req("a", 1, 2, data.clone(), unlimited());
    no_caps.agent_capabilities.clear();
    let mut bad_hash = inline_req("a", 2, 2, data.clone(), unlimited());
    bad_hash.expected_hash = [0xAB; 32];
    let mut wrong_dims = inline_req("a", 3, 2, data.clone(), unlimited());
    wrong_dims.width = 3; // 3x2 needs 24 bytes, not 16
    let oversize = inline_req("a", 4, 20, rgba(20, 1), unlimited()); // 1600 > 1000
    let over_budget = inline_req(
        "a",
        5,
        2,
        data.clone(),
        AgentBudget {
            texture_bytes_total_limit: 10,
            texture_bytes_total_used: 0,
        },
    );

    let cases = [
        (no_caps, "RESOURCE_CAPABILITY_DENIED", "upload_resource"),
        (bad_hash, "RESOURCE_HASH_MISMATCH", "expected"),
        (wrong_dims, "RESOURCE_DECODE_ERROR", "RGBA8"),
        (oversize, "RESOURCE_SIZE_EXCEEDED", "1000"),
        (over_budget, "RESOURCE_BUDGET_EXCEEDED", "10"),
    ];
    for (req, code, names) in cases {
        let err = store.handle_upload_start(req).await.unwrap_err();
        assert_eq!(err.wire_code(), code);
        let msg = err.to_string();
        assert!(msg.starts_with(code), "{msg:?} should lead with {code}");
        assert!(msg.contains(names), "{msg:?} should mention {names:?}");
    }
    assert_eq!(store.dedup_index().len(), 0, "no rejection stored anything");
}

#[tokio::test]
async fn chunk_protocol_errors_have_stable_codes() {
    let store = ResourceStore::new(ResourceStoreConfig::default());
    let data = rgba(2, 1);
    let id = UploadId::from_bytes([1; 16]);
    store
        .handle_upload_start(chunked_start("a", 1, 2, &data))
        .await
        .unwrap();

    let out_of_order = store
        .handle_upload_chunk("a", id, 1, data.clone())
        .await
        .unwrap_err();
    let unknown = store
        .handle_upload_chunk("a", UploadId::from_bytes([9; 16]), 0, data.clone())
        .await
        .unwrap_err();
    let oversized = store
        .handle_upload_chunk("a", id, 0, vec![0; CHUNK_SIZE_LIMIT + 1])
        .await
        .unwrap_err();
    let too_many = {
        for n in 2..=MAX_CONCURRENT_UPLOADS_PER_AGENT as u8 {
            store
                .handle_upload_start(chunked_start("a", n, 2, &data))
                .await
                .unwrap();
        }
        store
            .handle_upload_start(chunked_start("a", 50, 2, &data))
            .await
            .unwrap_err()
    };

    for err in [&out_of_order, &unknown, &oversized] {
        assert_eq!(err.wire_code(), "RESOURCE_INVALID_CHUNK");
    }
    // The out-of-order message tells the sender which index to send.
    assert!(out_of_order.to_string().contains("expected chunk index 0"));
    assert_eq!(too_many.wire_code(), "RESOURCE_TOO_MANY_UPLOADS");
}
