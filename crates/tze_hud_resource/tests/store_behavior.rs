//! Public-API behavior of the asset store, pinned to docs/invariants.md §7
//! (budgets are hard caps) and §8 (errors are affordances for the model).
//!
//! Everything goes through `ResourceStore`'s public upload calls. Uploads use
//! raw RGBA8 or compact encoded fixtures; decoded accounting is `w * h * 4`.
//! Nothing here reads a clock or sleeps.

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

    // Finite public completions exercise the shared count cap. The start
    // barrier does not force the private check/insert interleaving, so even
    // a passing vector cannot establish a global count transaction.
    for namespaces in [["a", "a"], ["a", "b"]] {
        let ledger =
            tze_hud_resource::ResidentLedger::new(tze_hud_resource::ResidentLedgerLimits {
                aggregate_bytes: 128,
                resource_bytes: 128,
                widget_source_bytes: 0,
                widget_raster_bytes: 0,
                font_bytes: 0,
            });
        let shared = ResourceStore::new_with_resident_ledger(
            ResourceStoreConfig {
                max_concurrent_resources: 2,
                ..ResourceStoreConfig::default()
            },
            ledger.clone(),
        );
        let retained = shared
            .handle_upload_start(inline_req("retained", 1, 1, rgba(1, 1), unlimited()))
            .await
            .unwrap()
            .unwrap();
        let runtime = tokio::runtime::Handle::current();
        let outcomes = std::thread::scope(|scope| {
            let start = std::sync::Arc::new(std::sync::Barrier::new(2));
            let workers: Vec<_> = namespaces
                .into_iter()
                .zip([2_u8, 3])
                .map(|(namespace, seed)| {
                    let store = shared.clone();
                    let runtime = runtime.clone();
                    let start = start.clone();
                    scope.spawn(move || {
                        let data = rgba(1, seed);
                        let expected = ResourceId::from_bytes(hash(&data));
                        start.wait();
                        let result = runtime
                            .block_on(store.handle_upload_start(inline_req(
                                namespace,
                                seed,
                                1,
                                data,
                                unlimited(),
                            )))
                            .map(|stored| stored.expect("inline completion"));
                        (namespace, seed, expected, result)
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().expect("upload thread completed"))
                .collect::<Vec<_>>()
        });
        println!("count namespaces={namespaces:?} joined={outcomes:?}");
        assert_eq!(
            outcomes
                .iter()
                .filter(|(_, _, _, result)| result.is_ok())
                .count(),
            1,
            "one remaining count slot must admit exactly one distinct completion: {outcomes:?}"
        );
        for (_, _, expected, result) in &outcomes {
            match result {
                Ok(stored) => {
                    assert_eq!(stored.resource_id, *expected);
                    assert!(!stored.was_deduplicated);
                    assert!(shared.dedup_index().contains(expected));
                }
                Err(error) => {
                    assert!(matches!(error, ResourceError::BudgetExceeded { .. }));
                    assert_eq!(error.wire_code(), "RESOURCE_BUDGET_EXCEEDED");
                    assert!(!shared.dedup_index().contains(expected));
                }
            }
        }
        assert_eq!(shared.dedup_index().len(), 2);
        assert!(shared.dedup_index().contains(&retained.resource_id));
        assert_eq!(shared.dedup_index().total_decoded_bytes(), 8);
        let full = ledger.snapshot();
        assert_eq!(full.resource_bytes, 8);
        assert_eq!(full.aggregate_bytes, 8);
        assert_eq!(full.allocation_count, 2);

        // Both real threads re-upload the retained hash at the full cap.
        // All public replies must keep its ID and charge no physical bytes.
        let repeated = std::thread::scope(|scope| {
            let start = std::sync::Arc::new(std::sync::Barrier::new(2));
            let workers: Vec<_> = namespaces
                .into_iter()
                .zip([10_u8, 11])
                .map(|(namespace, request)| {
                    let store = shared.clone();
                    let runtime = runtime.clone();
                    let start = start.clone();
                    scope.spawn(move || {
                        start.wait();
                        let result = runtime
                            .block_on(store.handle_upload_start(inline_req(
                                namespace,
                                request,
                                1,
                                rgba(1, 1),
                                unlimited(),
                            )))
                            .map(|stored| stored.expect("dedup completion"));
                        (namespace, request, result)
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().expect("dedup thread completed"))
                .collect::<Vec<_>>()
        });
        println!("dedup namespaces={namespaces:?} joined={repeated:?}");
        for (_, _, result) in repeated {
            let stored = result.unwrap();
            assert_eq!(stored.resource_id, retained.resource_id);
            assert!(stored.was_deduplicated);
        }
        assert_eq!(ledger.snapshot(), full);
        assert_eq!(shared.dedup_index().len(), 2);

        // Rejected chunked completion removes only its owned upload slot.
        let data = rgba(1, 4);
        for id in [20, 21] {
            assert!(
                shared
                    .handle_upload_start(chunked_start("a", id, 1, &data))
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        let owned = UploadId::from_bytes([20; 16]);
        shared
            .handle_upload_chunk("a", owned, 0, data.clone())
            .await
            .unwrap();
        let error = shared
            .handle_upload_complete("a", owned, &caps(), &unlimited())
            .await
            .unwrap_err();
        println!("chunked rejected upload={owned:?} error={error:?}");
        assert_eq!(error.wire_code(), "RESOURCE_BUDGET_EXCEEDED");
        assert_eq!(shared.in_flight_count("a").await, 1);
        assert!(
            !shared
                .dedup_index()
                .contains(&ResourceId::from_bytes(hash(&data)))
        );
        assert_eq!(ledger.snapshot(), full);
        assert!(shared.dedup_index().contains(&retained.resource_id));
        for id in [22, 23, 24] {
            assert!(
                shared
                    .handle_upload_start(chunked_start("a", id, 1, &data))
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        assert!(matches!(
            shared
                .handle_upload_start(chunked_start("a", 25, 1, &data))
                .await,
            Err(ResourceError::TooManyUploads)
        ));
        for id in [22, 23, 24] {
            shared
                .abort_upload("a", UploadId::from_bytes([id; 16]))
                .await;
        }
        assert_eq!(shared.in_flight_count("a").await, 1);
        // The peer upload remains usable, rather than merely counted.
        let peer = UploadId::from_bytes([21; 16]);
        shared
            .handle_upload_chunk("a", peer, 0, data)
            .await
            .unwrap();
        let peer_error = shared
            .handle_upload_complete("a", peer, &caps(), &unlimited())
            .await
            .unwrap_err();
        assert_eq!(peer_error.wire_code(), "RESOURCE_BUDGET_EXCEEDED");
        assert_eq!(shared.in_flight_count("a").await, 0);
        assert_eq!(ledger.snapshot(), full);
    }
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

    use image::{ExtendedColorType, ImageEncoder};
    use tze_hud_resource::{ResidentLedger, ResidentLedgerLimits};

    fn encoded(resource_type: ResourceType, color: ExtendedColorType, pixels: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();
        match resource_type {
            ResourceType::ImagePng => image::codecs::png::PngEncoder::new(&mut data)
                .write_image(pixels, 2, 2, color)
                .unwrap(),
            ResourceType::ImageJpeg => image::codecs::jpeg::JpegEncoder::new(&mut data)
                .write_image(pixels, 2, 2, color)
                .unwrap(),
            _ => unreachable!("encoded fixtures are PNG/JPEG"),
        }
        data
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(crc & 1));
            }
        }
        !crc
    }

    // Keep real decoder-readable headers, then damage only the raster. Cap
    // rejection must win over that damage without materializing a huge image.
    fn malformed_raster(
        mut data: Vec<u8>,
        resource_type: ResourceType,
        width: u32,
        height: u32,
    ) -> Vec<u8> {
        match resource_type {
            ResourceType::ImagePng => {
                data[16..20].copy_from_slice(&width.to_be_bytes());
                data[20..24].copy_from_slice(&height.to_be_bytes());
                let crc = crc32(&data[12..29]);
                data[29..33].copy_from_slice(&crc.to_be_bytes());
                let mut offset = 8;
                while offset + 12 <= data.len() {
                    let length =
                        u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
                    let end = offset + 8 + length;
                    if &data[offset + 4..offset + 8] == b"IDAT" {
                        data[offset + 8..end].fill(0); // invalid zlib stream
                        let crc = crc32(&data[offset + 4..end]);
                        data[end..end + 4].copy_from_slice(&crc.to_be_bytes());
                        return data;
                    }
                    offset = end + 4;
                }
            }
            ResourceType::ImageJpeg => {
                let mut offset = 2; // skip SOI
                while offset + 4 <= data.len() {
                    assert_eq!(data[offset], 0xFF);
                    let marker = data[offset + 1];
                    let length = usize::from(u16::from_be_bytes(
                        data[offset + 2..offset + 4].try_into().unwrap(),
                    ));
                    // Baseline SOF carries the dimensions.
                    if marker == 0xC0 {
                        data[offset + 5..offset + 7]
                            .copy_from_slice(&u16::try_from(height).unwrap().to_be_bytes());
                        data[offset + 7..offset + 9]
                            .copy_from_slice(&u16::try_from(width).unwrap().to_be_bytes());
                    }
                    // SOS stays decoder-readable, but its scan references an
                    // absent Huffman table. Entropy errors alone are tolerated
                    // by the JPEG decoder's existing permissive mode.
                    if marker == 0xDA {
                        data[offset + 6] = 0x33; // first component's DC/AC table 3
                        data.truncate(offset + 2 + length);
                        data.extend_from_slice(&[0xFF, 0x02]); // unknown entropy marker
                        return data;
                    }
                    offset += 2 + length;
                }
            }
            _ => unreachable!("encoded fixtures are PNG/JPEG"),
        }
        panic!("encoded fixture must contain a raster");
    }

    async fn upload_encoded(
        store: &ResourceStore,
        resource_type: ResourceType,
        data: Vec<u8>,
        chunked: bool,
    ) -> Result<tze_hud_resource::ResourceStored, ResourceError> {
        let mut req = inline_req("encoded", 1, 0, data.clone(), unlimited());
        req.resource_type = resource_type;
        if !chunked {
            return store
                .handle_upload_start(req)
                .await
                .map(|result| result.expect("inline upload completes immediately"));
        }
        let id = req.upload_id;
        req.inline_data.clear();
        assert!(store.handle_upload_start(req).await.unwrap().is_none());
        assert_eq!(store.in_flight_count("encoded").await, 1);
        store
            .handle_upload_chunk("encoded", id, 0, data)
            .await
            .unwrap();
        let result = store
            .handle_upload_complete("encoded", id, &caps(), &unlimited())
            .await;
        assert_eq!(store.in_flight_count("encoded").await, 0);
        result
    }

    let valid_png = encoded(ResourceType::ImagePng, ExtendedColorType::Rgba8, &[42; 16]);
    let valid_jpeg = encoded(ResourceType::ImageJpeg, ExtendedColorType::Rgb8, &[42; 12]);
    for (resource_type, valid) in [
        (ResourceType::ImagePng, &valid_png),
        (ResourceType::ImageJpeg, &valid_jpeg),
    ] {
        for chunked in [false, true] {
            for (width, height, cap, code, quantity) in [
                (8193, 2, 16, "RESOURCE_SIZE_EXCEEDED", "8192"),
                (2, 8193, 16, "RESOURCE_SIZE_EXCEEDED", "8192"),
                (2, 2, 15, "RESOURCE_SIZE_EXCEEDED", "15"),
                (2, 2, 16, "RESOURCE_DECODE_ERROR", "IMAGE_"),
            ] {
                let ledger = ResidentLedger::new(ResidentLedgerLimits {
                    aggregate_bytes: 64,
                    resource_bytes: 64,
                    widget_source_bytes: 0,
                    widget_raster_bytes: 0,
                    font_bytes: 0,
                });
                let store = ResourceStore::new_with_resident_ledger(
                    ResourceStoreConfig {
                        max_decoded_texture_bytes: cap,
                        ..ResourceStoreConfig::default()
                    },
                    ledger.clone(),
                );
                let before = ledger.snapshot();
                let data = malformed_raster(valid.clone(), resource_type, width, height);
                let err = upload_encoded(&store, resource_type, data, chunked)
                    .await
                    .expect_err(&format!(
                        "{resource_type} {width}x{height}, cap={cap}, chunked={chunked}, expected={code}"
                    ));
                assert_eq!(err.wire_code(), code, "{resource_type}, chunked={chunked}");
                let message = err.to_string();
                assert!(message.starts_with(code), "{message}");
                assert!(message.contains(quantity), "{message}");
                assert_eq!(store.dedup_index().len(), 0);
                assert_eq!(store.dedup_index().total_decoded_bytes(), 0);
                assert_eq!(
                    ledger.snapshot(),
                    before,
                    "rejection must not debit storage"
                );
            }
        }
    }

    // Equal RGBA8 caps accept intact rasters, including 16-bit PNG whose
    // native allocation exceeds its 16-byte RGBA8 accounting charge.
    let valid_png16 = encoded(ResourceType::ImagePng, ExtendedColorType::Rgba16, &[42; 32]);
    for (resource_type, data) in [
        (ResourceType::ImagePng, valid_png),
        (ResourceType::ImageJpeg, valid_jpeg),
        (ResourceType::ImagePng, valid_png16),
    ] {
        for chunked in [false, true] {
            let ledger = ResidentLedger::new(ResidentLedgerLimits {
                aggregate_bytes: 16,
                resource_bytes: 16,
                widget_source_bytes: 0,
                widget_raster_bytes: 0,
                font_bytes: 0,
            });
            let store = ResourceStore::new_with_resident_ledger(
                ResourceStoreConfig {
                    max_decoded_texture_bytes: 16,
                    ..ResourceStoreConfig::default()
                },
                ledger.clone(),
            );
            let result = upload_encoded(&store, resource_type, data.clone(), chunked)
                .await
                .unwrap();
            assert_eq!(result.resource_id, ResourceId::from_bytes(hash(&data)));
            assert_eq!(result.decoded_bytes, 16);
            assert!(!result.was_deduplicated);
            let record = store.dedup_index().get(&result.resource_id).unwrap();
            assert_eq!(record.resource_type, resource_type);
            assert_eq!(store.dedup_index().total_decoded_bytes(), 16);
            let charged = ledger.snapshot();
            assert_eq!(charged.resource_bytes, 16);
            assert_eq!(charged.allocation_count, 1);
            let mut repeat = inline_req("encoded", 2, 0, data.clone(), unlimited());
            repeat.resource_type = resource_type;
            let repeated = store.handle_upload_start(repeat).await.unwrap().unwrap();
            assert_eq!(repeated.resource_id, result.resource_id);
            assert!(repeated.was_deduplicated);
            assert_eq!(ledger.snapshot(), charged, "dedup must not debit twice");
        }
    }
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
