//! Subscription and topology handlers for the session server (SS-7g).
//!
//! - `handle_subscription_change`: apply a subscription add/remove diff (RFC 0010 §7.2).
//! - `handle_list_elements_request`: scene topology query (requires read_scene_topology).

use super::*;

pub(super) async fn handle_subscription_change(
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    change: SubscriptionChange,
) {
    // Merge plain subscriptions and filtered subscriptions into a combined add list.
    // `subscribe` contains category-only adds (use default prefix).
    // `subscribe_filter` contains category + optional finer-grained prefix (RFC 0010 §7.2).
    // Use a HashSet to deduplicate in O(n) rather than O(n²).
    let mut seen: std::collections::HashSet<&str> =
        change.subscribe.iter().map(String::as_str).collect();
    let mut add: Vec<String> = change.subscribe.clone();
    for entry in &change.subscribe_filter {
        if seen.insert(entry.category.as_str()) {
            add.push(entry.category.clone());
        }
    }

    // Apply capability-filtered subscription change (RFC 0005 §7.3).
    // Mandatory subscriptions (DEGRADATION_NOTICES, LEASE_CHANGES) cannot be removed.
    // Additions without the required capability are placed in denied_subscriptions.
    // New subscription set takes effect immediately after the ack is sent.
    let result = subscriptions::apply_subscription_change(
        &session.subscriptions,
        &add,
        &change.unsubscribe,
        &session.capabilities,
    );

    // Update per-category subscription filters to match the new active set.
    //
    // Semantics:
    // - Plain `subscribe` for a category implies default behavior (no stored filter),
    //   so any existing filter for that category is cleared when the subscription is active.
    // - `subscribe_filter` with a non-empty filter_prefix stores/updates the filter
    //   for that category, but only if the subscription is active (not denied).
    // - `subscribe_filter` with an empty filter_prefix explicitly resets to default:
    //   any stored filter for that category is removed.
    // - Unsubscribed categories always have their filters removed.

    // Clear filters for categories in plain `subscribe` that are now active.
    for cat in &change.subscribe {
        if result.active.contains(cat) {
            session.subscription_filters.remove(cat.as_str());
        }
    }

    // Apply filtered subscriptions: store, update, or clear filter per entry.
    for entry in &change.subscribe_filter {
        if result.active.contains(&entry.category) {
            if entry.filter_prefix.is_empty() {
                // Empty prefix for an active subscription resets to default behavior.
                session.subscription_filters.remove(entry.category.as_str());
            } else {
                session
                    .subscription_filters
                    .insert(entry.category.clone(), entry.filter_prefix.clone());
            }
        }
    }

    // Remove filters for unsubscribed categories.
    for cat in &change.unsubscribe {
        session.subscription_filters.remove(cat.as_str());
    }

    // Update session's active subscription set
    session.subscriptions = result.active.clone();

    let seq = session.next_server_seq();
    let _ = tx
        .send(Ok(ServerMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ServerPayload::SubscriptionChangeResult(
                SubscriptionChangeResult {
                    active_subscriptions: result.active,
                    denied_subscriptions: result.denied,
                },
            )),
        }))
        .await;
}

pub(super) async fn handle_list_elements_request(
    state: &Arc<Mutex<SharedState>>,
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    request_sequence: u64,
    request: ListElementsRequest,
) {
    if !capability_set_covers(&session.capabilities, "read_scene_topology") {
        let seq = session.next_server_seq();
        let _ = tx
            .send(Ok(ServerMessage {
                sequence: seq,
                timestamp_wall_us: now_wall_us(),
                payload: Some(ServerPayload::RequestResult(super::verbs::fail(
                    request_sequence,
                    "NOT_ALLOWED",
                    format!("needs \"tiles\" in [agents.{}] allow", session.agent_name),
                ))),
            }))
            .await;
        return;
    }

    let namespace_filter = request.namespace_filter.unwrap_or_default();
    let element_type_filter = request.element_type.unwrap_or_default();
    let parsed_type_filter = if element_type_filter.trim().is_empty() {
        None
    } else {
        match parse_element_type_filter(&element_type_filter) {
            Some(element_type) => Some(element_type),
            None => {
                let seq = session.next_server_seq();
                let _ = tx
                    .send(Ok(ServerMessage {
                        sequence: seq,
                        timestamp_wall_us: now_wall_us(),
                        payload: Some(ServerPayload::RequestResult(super::verbs::fail(
                            request_sequence,
                            "INVALID_ARGUMENT",
                            format!(
                                "element_type {element_type_filter:?} is not tile, zone, or widget"
                            ),
                        ))),
                    }))
                    .await;
                return;
            }
        }
    };

    let (scene_handle, mut entries): (Arc<Mutex<SceneGraph>>, Vec<(SceneId, ElementStoreEntry)>) = {
        let st = state.lock().await;
        (
            st.scene.clone(),
            st.element_store
                .entries
                .iter()
                .map(|(id, entry)| (*id, entry.clone()))
                .collect(),
        )
    };
    entries.sort_by_key(|(id, entry)| (entry.created_at, id.to_bytes_le()));

    let scene = scene_handle.lock().await;
    let mut elements = Vec::new();
    for (element_id, entry) in entries {
        if let Some(filter) = parsed_type_filter {
            if entry.element_type != filter {
                continue;
            }
        }
        if !namespace_filter.is_empty() && !entry.namespace.starts_with(&namespace_filter) {
            continue;
        }

        let zero_policy = GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.0,
            height_pct: 0.0,
        };
        let current_geometry = match entry.element_type {
            ElementType::Tile => {
                let agent_policy = scene.tiles.get(&element_id).map(|tile| {
                    rect_to_relative_geometry_policy(
                        tile.bounds,
                        scene.display_area.width,
                        scene.display_area.height,
                    )
                });
                resolve_geometry_override_chain(entry.geometry_override, agent_policy, None, None)
                    .unwrap_or(zero_policy)
            }
            ElementType::Zone => scene
                .zone_registry
                .resolve_geometry_policy_for_zone(
                    &entry.namespace,
                    entry.geometry_override.as_ref(),
                    None,
                )
                .or(entry.geometry_override)
                .unwrap_or(zero_policy),
            ElementType::Widget => scene
                .widget_registry
                .resolve_geometry_policy_for_instance(
                    &entry.namespace,
                    entry.geometry_override.as_ref(),
                )
                .or(entry.geometry_override)
                .unwrap_or(zero_policy),
        };

        elements.push(ElementInfo {
            element_id: scene_id_to_bytes(element_id),
            element_type: element_type_wire_name(entry.element_type).to_string(),
            namespace: entry.namespace,
            current_geometry: Some(convert::geometry_policy_to_proto(&current_geometry)),
            has_user_override: entry.geometry_override.is_some(),
            created_at_ms: entry.created_at,
            last_published_at_ms: entry.last_published_at,
        });
    }

    let seq = session.next_server_seq();
    let _ = tx
        .send(Ok(ServerMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ServerPayload::ListElementsResponse(ListElementsResponse {
                elements,
            })),
        }))
        .await;
}

// ─── Helpers used only by this module (migrated from mod.rs, SS-9) ──────────

fn element_type_wire_name(element_type: ElementType) -> &'static str {
    match element_type {
        ElementType::Tile => "tile",
        ElementType::Zone => "zone",
        ElementType::Widget => "widget",
    }
}

fn parse_element_type_filter(filter: &str) -> Option<ElementType> {
    match filter.trim().to_ascii_lowercase().as_str() {
        "tile" => Some(ElementType::Tile),
        "zone" => Some(ElementType::Zone),
        "widget" => Some(ElementType::Widget),
        _ => None,
    }
}
