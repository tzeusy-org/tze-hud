use super::*;

fn is_valid_widget_type_id(widget_type_id: &str) -> bool {
    let mut chars = widget_type_id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn is_valid_widget_svg_filename(svg_filename: &str) -> bool {
    !svg_filename.is_empty()
        && svg_filename.ends_with(".svg")
        && !svg_filename.contains('/')
        && !svg_filename.contains('\\')
}

fn validate_svg_payload(svg_bytes: &[u8]) -> Result<(), String> {
    let svg_text =
        std::str::from_utf8(svg_bytes).map_err(|e| format!("SVG payload is not UTF-8: {e}"))?;

    let mut reader = Reader::from_str(svg_text);
    reader.config_mut().trim_text(true);

    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) | Ok(Event::Empty(start)) => {
                if start.name().as_ref() == b"svg" {
                    return Ok(());
                }
                return Err("SVG root element must be <svg>".to_string());
            }
            Ok(Event::Decl(_) | Event::Comment(_) | Event::DocType(_) | Event::Text(_)) => {
                continue;
            }
            Ok(Event::Eof) => {
                return Err("SVG payload is empty or missing a root element".to_string());
            }
            Err(e) => return Err(format!("SVG XML parse error: {e}")),
            _ => continue,
        }
    }
}

async fn send_widget_asset_register_result(
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    result: WidgetAssetRegisterResult,
) {
    let seq = session.next_server_seq();
    let _ = tx
        .send(Ok(ServerMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ServerPayload::WidgetAssetRegisterResult(result)),
        }))
        .await;
}

fn make_widget_asset_error_result(
    request_sequence: u64,
    widget_type_id: &str,
    svg_filename: &str,
    error_code: &str,
    error_message: String,
) -> WidgetAssetRegisterResult {
    WidgetAssetRegisterResult {
        request_sequence,
        accepted: false,
        widget_type_id: widget_type_id.to_string(),
        svg_filename: svg_filename.to_string(),
        asset_handle: String::new(),
        was_deduplicated: false,
        error_code: error_code.to_string(),
        error_message,
    }
}

fn widget_asset_handle_from_hash(hash: [u8; 32]) -> String {
    format!(
        "widget-svg:{}",
        tze_hud_resource::ResourceId::from_bytes(hash).to_hex()
    )
}

fn register_widget_asset_in_scene(
    scene: &mut SceneGraph,
    register: &WidgetAssetRegister,
    svg_bytes: &[u8],
    asset_handle: &str,
) -> Result<(), RuntimeWidgetAssetError> {
    register_runtime_widget_svg_asset(
        scene,
        &register.widget_type_id,
        &register.svg_filename,
        svg_bytes,
        asset_handle,
        &HashMap::new(),
    )
}

/// Handle a WidgetAssetRegister from the client (session-protocol spec
/// §Requirement: Widget Asset Registration via Session Stream).
///
/// Implements metadata-first dedup preflight:
/// - known hash => accepted dedup hit without payload transfer
/// - unknown hash => payload required, checksum/hash verified, SVG validated
pub(super) async fn handle_widget_asset_register(
    state: &Arc<Mutex<SharedState>>,
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    request_sequence: u64,
    register: WidgetAssetRegister,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
) {
    let required_cap = "register_widget_asset".to_string();
    if !super::capability_set_covers(&session.capabilities, &required_cap) {
        send_widget_asset_register_result(
            session,
            tx,
            make_widget_asset_error_result(
                request_sequence,
                &register.widget_type_id,
                &register.svg_filename,
                "WIDGET_ASSET_CAPABILITY_MISSING",
                format!("Missing capability: {required_cap}"),
            ),
        )
        .await;
        return;
    }

    if !is_valid_widget_type_id(&register.widget_type_id)
        || !is_valid_widget_svg_filename(&register.svg_filename)
    {
        send_widget_asset_register_result(
            session,
            tx,
            make_widget_asset_error_result(
                request_sequence,
                &register.widget_type_id,
                &register.svg_filename,
                "WIDGET_ASSET_TYPE_INVALID",
                "widget_type_id or svg_filename failed validation".to_string(),
            ),
        )
        .await;
        return;
    }

    let expected_hash: [u8; 32] = match register.content_hash_blake3.as_slice().try_into() {
        Ok(hash) => hash,
        Err(_) => {
            send_widget_asset_register_result(
                session,
                tx,
                make_widget_asset_error_result(
                    request_sequence,
                    &register.widget_type_id,
                    &register.svg_filename,
                    "WIDGET_ASSET_HASH_MISMATCH",
                    "content_hash_blake3 must be exactly 32 bytes".to_string(),
                ),
            )
            .await;
            return;
        }
    };

    let (existing_record, durable_dedup_hit) = {
        let st = state.lock().await;
        let existing = st.widget_asset_store.by_hash.get(&expected_hash).cloned();
        let durable_hit = if existing.is_none() {
            st.runtime_widget_store
                .as_ref()
                .map(|store| {
                    store.contains(tze_hud_resource::ResourceId::from_bytes(expected_hash))
                })
                .unwrap_or(false)
        } else {
            false
        };
        (existing, durable_hit)
    };
    if let Some(existing) = existing_record {
        let st = state.lock().await;
        let runtime_register_result = {
            let mut scene = st.scene.lock().await;
            register_widget_asset_in_scene(
                &mut scene,
                &register,
                &existing.bytes,
                &existing.asset_handle,
            )
        };
        if let Err(err) = runtime_register_result {
            let error_result = make_widget_asset_error_result(
                request_sequence,
                &register.widget_type_id,
                &register.svg_filename,
                err.wire_code(),
                err.to_string(),
            );
            drop(st);
            send_widget_asset_register_result(session, tx, error_result).await;
            return;
        }

        let asset_handle = existing.asset_handle;
        drop(st);
        render_wake.notify();
        send_widget_asset_register_result(
            session,
            tx,
            WidgetAssetRegisterResult {
                request_sequence,
                accepted: true,
                widget_type_id: register.widget_type_id.clone(),
                svg_filename: register.svg_filename.clone(),
                asset_handle,
                was_deduplicated: true,
                error_code: String::new(),
                error_message: String::new(),
            },
        )
        .await;
        return;
    }

    if durable_dedup_hit {
        send_widget_asset_register_result(
            session,
            tx,
            WidgetAssetRegisterResult {
                request_sequence,
                accepted: true,
                widget_type_id: register.widget_type_id.clone(),
                svg_filename: register.svg_filename.clone(),
                asset_handle: widget_asset_handle_from_hash(expected_hash),
                was_deduplicated: true,
                error_code: String::new(),
                error_message: String::new(),
            },
        )
        .await;
        return;
    }

    if register.inline_svg_bytes.is_empty() {
        send_widget_asset_register_result(
            session,
            tx,
            make_widget_asset_error_result(
                request_sequence,
                &register.widget_type_id,
                &register.svg_filename,
                "WIDGET_ASSET_HASH_MISMATCH",
                "unknown content hash requires payload bytes".to_string(),
            ),
        )
        .await;
        return;
    }

    if register.total_size_bytes != register.inline_svg_bytes.len() as u64 {
        send_widget_asset_register_result(
            session,
            tx,
            make_widget_asset_error_result(
                request_sequence,
                &register.widget_type_id,
                &register.svg_filename,
                "WIDGET_ASSET_CHECKSUM_MISMATCH",
                format!(
                    "declared total_size_bytes={} does not match payload length={}",
                    register.total_size_bytes,
                    register.inline_svg_bytes.len()
                ),
            ),
        )
        .await;
        return;
    }

    if register.transport_crc32c != 0 {
        let computed_crc = crc32c::crc32c(&register.inline_svg_bytes);
        if computed_crc != register.transport_crc32c {
            send_widget_asset_register_result(
                session,
                tx,
                make_widget_asset_error_result(
                    request_sequence,
                    &register.widget_type_id,
                    &register.svg_filename,
                    "WIDGET_ASSET_CHECKSUM_MISMATCH",
                    format!(
                        "transport_crc32c mismatch: declared={}, computed={computed_crc}",
                        register.transport_crc32c
                    ),
                ),
            )
            .await;
            return;
        }
    }

    let computed_hash = *blake3::hash(&register.inline_svg_bytes).as_bytes();
    if computed_hash != expected_hash {
        send_widget_asset_register_result(
            session,
            tx,
            make_widget_asset_error_result(
                request_sequence,
                &register.widget_type_id,
                &register.svg_filename,
                "WIDGET_ASSET_HASH_MISMATCH",
                "payload BLAKE3 hash does not match content_hash_blake3".to_string(),
            ),
        )
        .await;
        return;
    }

    if let Err(detail) = validate_svg_payload(&register.inline_svg_bytes) {
        send_widget_asset_register_result(
            session,
            tx,
            make_widget_asset_error_result(
                request_sequence,
                &register.widget_type_id,
                &register.svg_filename,
                "WIDGET_ASSET_INVALID_SVG",
                detail,
            ),
        )
        .await;
        return;
    }

    let payload_len = register.inline_svg_bytes.len() as u64;
    let mut st = state.lock().await;
    let asset_handle = widget_asset_handle_from_hash(expected_hash);

    // Re-check dedup after lock acquisition to avoid races between workers.
    if let Some(existing) = st.widget_asset_store.by_hash.get(&expected_hash).cloned() {
        let runtime_register_result = {
            let mut scene = st.scene.lock().await;
            register_widget_asset_in_scene(
                &mut scene,
                &register,
                &existing.bytes,
                &existing.asset_handle,
            )
        };
        if let Err(err) = runtime_register_result {
            let error_result = make_widget_asset_error_result(
                request_sequence,
                &register.widget_type_id,
                &register.svg_filename,
                err.wire_code(),
                err.to_string(),
            );
            drop(st);
            send_widget_asset_register_result(session, tx, error_result).await;
            return;
        }
        let dedup_result = WidgetAssetRegisterResult {
            request_sequence,
            accepted: true,
            widget_type_id: register.widget_type_id.clone(),
            svg_filename: register.svg_filename.clone(),
            asset_handle: existing.asset_handle,
            was_deduplicated: true,
            error_code: String::new(),
            error_message: String::new(),
        };
        drop(st);
        render_wake.notify();
        send_widget_asset_register_result(session, tx, dedup_result).await;
        return;
    }

    if let Some(store) = st.runtime_widget_store.as_ref() {
        let resource_id = tze_hud_resource::ResourceId::from_bytes(expected_hash);
        if store.contains(resource_id) {
            let dedup_result = WidgetAssetRegisterResult {
                request_sequence,
                accepted: true,
                widget_type_id: register.widget_type_id.clone(),
                svg_filename: register.svg_filename.clone(),
                asset_handle,
                was_deduplicated: true,
                error_code: String::new(),
                error_message: String::new(),
            };
            drop(st);
            send_widget_asset_register_result(session, tx, dedup_result).await;
            return;
        }
    }

    if let Some(store) = st.runtime_widget_store.as_mut() {
        let put_outcome = match store.put_svg(&session.namespace, &register.inline_svg_bytes) {
            Ok(outcome) => outcome,
            Err(RuntimeWidgetStoreError::TotalBudgetExceeded { .. })
            | Err(RuntimeWidgetStoreError::AgentBudgetExceeded { .. }) => {
                let budget_error = make_widget_asset_error_result(
                    request_sequence,
                    &register.widget_type_id,
                    &register.svg_filename,
                    "WIDGET_ASSET_BUDGET_EXCEEDED",
                    "runtime widget asset store budget exceeded".to_string(),
                );
                drop(st);
                send_widget_asset_register_result(session, tx, budget_error).await;
                return;
            }
            Err(err) => {
                let store_error = make_widget_asset_error_result(
                    request_sequence,
                    &register.widget_type_id,
                    &register.svg_filename,
                    "WIDGET_ASSET_STORE_IO_ERROR",
                    format!("runtime widget asset store write failed: {err}"),
                );
                drop(st);
                send_widget_asset_register_result(session, tx, store_error).await;
                return;
            }
        };

        let runtime_register_result = {
            let mut scene = st.scene.lock().await;
            register_widget_asset_in_scene(
                &mut scene,
                &register,
                &register.inline_svg_bytes,
                &asset_handle,
            )
        };
        if let Err(err) = runtime_register_result {
            let error_result = make_widget_asset_error_result(
                request_sequence,
                &register.widget_type_id,
                &register.svg_filename,
                err.wire_code(),
                err.to_string(),
            );
            drop(st);
            send_widget_asset_register_result(session, tx, error_result).await;
            return;
        }

        let was_deduplicated = matches!(put_outcome, DurablePutOutcome::Deduplicated { .. });
        drop(st);
        render_wake.notify();
        send_widget_asset_register_result(
            session,
            tx,
            WidgetAssetRegisterResult {
                request_sequence,
                accepted: true,
                widget_type_id: register.widget_type_id,
                svg_filename: register.svg_filename,
                asset_handle,
                was_deduplicated,
                error_code: String::new(),
                error_message: String::new(),
            },
        )
        .await;
        return;
    }

    let used_by_ns = st
        .widget_asset_store
        .per_namespace_bytes
        .get(&session.namespace)
        .copied()
        .unwrap_or(0);

    if st
        .widget_asset_store
        .total_bytes
        .saturating_add(payload_len)
        > st.widget_asset_store.max_total_bytes
        || used_by_ns.saturating_add(payload_len) > st.widget_asset_store.max_namespace_bytes
    {
        let budget_error = make_widget_asset_error_result(
            request_sequence,
            &register.widget_type_id,
            &register.svg_filename,
            "WIDGET_ASSET_BUDGET_EXCEEDED",
            "runtime widget asset store budget exceeded".to_string(),
        );
        drop(st);
        send_widget_asset_register_result(session, tx, budget_error).await;
        return;
    }

    let resident_allocation_id = format!(
        "widget-source:grpc:{}",
        tze_hud_resource::ResourceId::from_bytes(expected_hash).to_hex()
    );
    let resident_reserved = match st
        .widget_asset_store
        .reserve_resident_payload(&resident_allocation_id, payload_len)
    {
        Ok(reserved) => reserved,
        Err(_) => {
            let budget_error = make_widget_asset_error_result(
                request_sequence,
                &register.widget_type_id,
                &register.svg_filename,
                "WIDGET_ASSET_BUDGET_EXCEEDED",
                "runtime resident widget-source budget exceeded".to_string(),
            );
            drop(st);
            send_widget_asset_register_result(session, tx, budget_error).await;
            return;
        }
    };

    let runtime_register_result = {
        let mut scene = st.scene.lock().await;
        register_widget_asset_in_scene(
            &mut scene,
            &register,
            &register.inline_svg_bytes,
            &asset_handle,
        )
    };
    if let Err(err) = runtime_register_result {
        if resident_reserved {
            st.widget_asset_store
                .release_resident_payload(&resident_allocation_id);
        }
        let error_result = make_widget_asset_error_result(
            request_sequence,
            &register.widget_type_id,
            &register.svg_filename,
            err.wire_code(),
            err.to_string(),
        );
        drop(st);
        send_widget_asset_register_result(session, tx, error_result).await;
        return;
    }
    let previous = st.widget_asset_store.by_hash.insert(
        expected_hash,
        crate::session::WidgetAssetRecord {
            asset_handle: asset_handle.clone(),
            widget_type_id: register.widget_type_id.clone(),
            svg_filename: register.svg_filename.clone(),
            owner_namespace: session.namespace.clone(),
            bytes: register.inline_svg_bytes,
        },
    );
    if previous.is_some() {
        if resident_reserved {
            st.widget_asset_store
                .release_resident_payload(&resident_allocation_id);
        }
        // Should be unreachable due to dedup checks above; return a stable error anyway.
        let duplicate_error = make_widget_asset_error_result(
            request_sequence,
            &register.widget_type_id,
            &register.svg_filename,
            "WIDGET_ASSET_STORE_IO_ERROR",
            "duplicate hash insertion race while updating store".to_string(),
        );
        drop(st);
        render_wake.notify();
        send_widget_asset_register_result(session, tx, duplicate_error).await;
        return;
    }
    st.widget_asset_store.total_bytes = st
        .widget_asset_store
        .total_bytes
        .saturating_add(payload_len);
    let entry = st
        .widget_asset_store
        .per_namespace_bytes
        .entry(session.namespace.clone())
        .or_insert(0);
    *entry = entry.saturating_add(payload_len);
    drop(st);
    render_wake.notify();

    send_widget_asset_register_result(
        session,
        tx,
        WidgetAssetRegisterResult {
            request_sequence,
            accepted: true,
            widget_type_id: register.widget_type_id,
            svg_filename: register.svg_filename,
            asset_handle,
            was_deduplicated: false,
            error_code: String::new(),
            error_message: String::new(),
        },
    )
    .await;
}
