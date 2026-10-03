use super::*;
use crate::contract::bounded_copy;
use proptest::prelude::*;

fn envelope(
    operation: ProjectionOperation,
    projection_id: &str,
    request_id: &str,
) -> OperationEnvelope {
    OperationEnvelope {
        operation,
        projection_id: projection_id.to_string(),
        request_id: request_id.to_string(),
        client_timestamp_wall_us: 1,
    }
}

fn attach_request(projection_id: &str, request_id: &str) -> AttachRequest {
    AttachRequest {
        envelope: envelope(ProjectionOperation::Attach, projection_id, request_id),
        provider_kind: ProviderKind::Codex,
        display_name: "Codex Session".to_string(),
        workspace_hint: Some("mayor/rig".to_string()),
        repository_hint: None,
        icon_profile_hint: None,
        content_classification: ContentClassification::Private,
        hud_target: None,
        idempotency_key: Some("attach-once".to_string()),
    }
}

fn attach(authority: &mut ProjectionAuthority, projection_id: &str) -> String {
    authority
        .handle_attach(attach_request(projection_id, "attach-1"), "caller-a", 10)
        .owner_token
        .expect("attach must issue owner token")
}

fn output_request(
    projection_id: &str,
    owner_token: &str,
    request_id: &str,
) -> PublishOutputRequest {
    PublishOutputRequest {
        envelope: envelope(
            ProjectionOperation::PublishOutput,
            projection_id,
            request_id,
        ),
        owner_token: owner_token.to_string(),
        output_text: "hello projection".to_string(),
        output_kind: OutputKind::Assistant,
        content_classification: ContentClassification::Private,
        logical_unit_id: Some("unit-1".to_string()),
        coalesce_key: None,
        expects_reply: false,
    }
}

fn status_request(
    projection_id: &str,
    owner_token: &str,
    request_id: &str,
) -> PublishStatusRequest {
    PublishStatusRequest {
        envelope: envelope(
            ProjectionOperation::PublishStatus,
            projection_id,
            request_id,
        ),
        owner_token: owner_token.to_string(),
        lifecycle_state: ProjectionLifecycleState::Active,
        status_text: Some("working".to_string()),
    }
}

fn connection_metadata(grants: &[&str]) -> HudConnectionMetadata {
    HudConnectionMetadata {
        connection_id: "connection-1".to_string(),
        authenticated_session_id: "runtime-session-1".to_string(),
        granted_capabilities: grants.iter().map(|grant| (*grant).to_string()).collect(),
        connected_at_wall_us: 20,
        last_reconnect_wall_us: 20,
    }
}

fn advisory_lease(capabilities: &[&str], expires_at_wall_us: u64) -> AdvisoryLeaseIdentity {
    AdvisoryLeaseIdentity {
        lease_id: "lease-1".to_string(),
        capabilities: capabilities
            .iter()
            .map(|capability| (*capability).to_string())
            .collect(),
        acquired_at_wall_us: 21,
        expires_at_wall_us,
    }
}

fn portal_submission(input_id: &str, text: &str) -> PortalInputSubmission {
    PortalInputSubmission {
        input_id: input_id.to_string(),
        submission_text: text.to_string(),
        submitted_at_wall_us: 30,
        expires_at_wall_us: Some(1_000),
        content_classification: ContentClassification::Private,
    }
}

#[test]
fn schema_uses_required_wall_clock_and_owner_token_fields() {
    let attach_json = serde_json::to_value(attach_request("projection-a", "req-a")).unwrap();
    assert_eq!(attach_json["operation"], "attach");
    assert_eq!(attach_json["client_timestamp_wall_us"], 1);
    assert!(attach_json.get("owner_token").is_none());

    let publish_json =
        serde_json::to_value(output_request("projection-a", "owner-token", "req-b")).unwrap();
    assert_eq!(publish_json["operation"], "publish_output");
    assert_eq!(publish_json["owner_token"], "owner-token");
    assert_eq!(publish_json["content_classification"], "private");
}

#[test]
fn stable_error_code_set_is_append_only_wire_shape() {
    let codes: Vec<&str> = INITIAL_ERROR_CODES
        .iter()
        .map(|code| code.as_str())
        .collect();
    assert_eq!(
        codes,
        vec![
            "PROJECTION_NOT_FOUND",
            "PROJECTION_ALREADY_ATTACHED",
            "PROJECTION_UNAUTHORIZED",
            "PROJECTION_TOKEN_EXPIRED",
            "PROJECTION_INVALID_ARGUMENT",
            "PROJECTION_OUTPUT_TOO_LARGE",
            "PROJECTION_INPUT_TOO_LARGE",
            "PROJECTION_INPUT_QUEUE_FULL",
            "PROJECTION_RATE_LIMITED",
            "PROJECTION_STATE_CONFLICT",
            "PROJECTION_HUD_UNAVAILABLE",
            "PROJECTION_INTERNAL_ERROR",
        ]
    );
    assert_eq!(
        serde_json::to_string(&ProjectionErrorCode::ProjectionUnauthorized).unwrap(),
        "\"PROJECTION_UNAUTHORIZED\""
    );
}

#[test]
fn list_projections_is_caller_scoped_bounded_and_content_free() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        max_list_items: 2,
        ..ProjectionBounds::default()
    })
    .expect("bounded authority must be valid");

    let mut alpha = attach_request("alpha", "attach-alpha");
    alpha.display_name = "Alpha session".to_string();
    let alpha_token = authority
        .handle_attach(alpha, "resident-a", 10)
        .owner_token
        .expect("alpha attach must issue owner token");

    let mut beta = attach_request("beta", "attach-beta");
    beta.display_name = "Beta session".to_string();
    assert!(authority.handle_attach(beta, "resident-a", 11).accepted);

    let mut gamma = attach_request("gamma", "attach-gamma");
    gamma.display_name = "Gamma session".to_string();
    assert!(authority.handle_attach(gamma, "resident-a", 12).accepted);

    let mut foreign = attach_request("foreign", "attach-foreign");
    foreign.display_name = "Foreign session".to_string();
    assert!(authority.handle_attach(foreign, "resident-b", 13).accepted);

    let mut output = output_request("alpha", &alpha_token, "publish-alpha");
    output.output_text = "private transcript must never leak".to_string();
    assert!(
        authority
            .handle_publish_output(output, "resident-a", 14)
            .accepted
    );
    authority
        .enqueue_input(
            "alpha",
            "input-alpha",
            "private input must never leak".to_string(),
            15,
            1_000,
            None,
        )
        .expect("pending input must be accepted");

    let response = authority.handle_list_projections(
        ListProjectionsRequest {
            request_id: "list-resident-a".to_string(),
            client_timestamp_wall_us: 16,
        },
        "resident-a",
        16,
    );

    assert!(response.accepted);
    assert_eq!(response.projections.len(), 2, "hard list cap must apply");
    assert_eq!(
        response
            .projections
            .iter()
            .map(|projection| projection.projection_id.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha", "beta"],
        "entries must be deterministic and exclude both capped and foreign sessions"
    );

    let alpha = &response.projections[0];
    assert_eq!(alpha.display_name, "Alpha session");
    assert_eq!(alpha.lifecycle_state, ProjectionLifecycleState::Active);
    assert_eq!(alpha.unread_output_count, 1);
    assert_eq!(alpha.pending_input_count, 1);

    let payload = serde_json::to_string(&response).expect("list response must serialize");
    for forbidden in [
        "private transcript must never leak",
        "private input must never leak",
        "output_text",
        "submission_text",
        "owner_token",
        "resident-b",
    ] {
        assert!(
            !payload.contains(forbidden),
            "list payload must not expose {forbidden:?}: {payload}"
        );
    }
}

#[test]
fn list_bound_must_be_non_zero() {
    let error = ProjectionAuthority::new(ProjectionBounds {
        max_list_items: 0,
        ..ProjectionBounds::default()
    })
    .expect_err("a zero list cap would make the low-token contract unusable");
    assert!(
        matches!(error, ProjectionContractError::InvalidArgument(ref message) if message.contains("non-zero")),
        "list cap validation must fail closed"
    );
}

#[test]
fn poll_response_bound_must_cover_each_accepted_input_item() {
    let error = ProjectionAuthority::new(ProjectionBounds {
        max_poll_response_bytes: DEFAULT_MAX_PENDING_INPUT_BYTES_PER_ITEM - 1,
        ..ProjectionBounds::default()
    })
    .expect_err("an accepted input must fit in at least one bounded poll response");

    assert!(
        matches!(
            error,
            ProjectionContractError::InvalidArgument(ref message)
                if message.contains("max_poll_response_bytes")
                    && message.contains("max_pending_input_bytes_per_item")
        ),
        "the invalid poll/input-bound relationship must fail closed"
    );
}

#[test]
fn poll_response_bound_equal_to_input_item_bound_is_valid() {
    ProjectionAuthority::new(ProjectionBounds {
        max_poll_response_bytes: DEFAULT_MAX_PENDING_INPUT_BYTES_PER_ITEM,
        ..ProjectionBounds::default()
    })
    .expect("one maximum-sized accepted input must fit in one bounded poll response");
}

#[test]
fn attach_materializes_content_layer_projected_portal_and_reuses_idempotently() {
    let mut authority = ProjectionAuthority::default();
    let first = authority.handle_attach(attach_request("projection-a", "req-a"), "caller-a", 10);
    assert!(first.accepted);

    let state = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("attach creates portal state");
    assert_eq!(
        state.adapter_family,
        ProjectedPortalAdapterFamily::CooperativeProjection
    );
    assert_eq!(
        state.runtime_authority,
        ProjectedPortalRuntimeAuthority::ResidentSessionLease
    );
    assert_eq!(state.layer, ProjectedPortalLayer::Content);
    assert_eq!(state.presentation, ProjectedPortalPresentation::Expanded);
    assert_eq!(state.display_name.as_deref(), Some("Codex Session"));
    assert_eq!(state.workspace_hint.as_deref(), Some("mayor/rig"));
    assert!(state.interaction_enabled);

    authority.collapse_projected_portal("projection-a").unwrap();
    let collapsed = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("collapsed portal state remains materializable");
    assert_eq!(collapsed.portal_id, state.portal_id);
    assert_eq!(
        collapsed.presentation,
        ProjectedPortalPresentation::Collapsed
    );
    assert!(collapsed.visible_transcript.is_empty());
    assert!(!collapsed.interaction_enabled);

    let replay = authority.handle_attach(attach_request("projection-a", "req-b"), "caller-a", 11);
    assert!(replay.accepted);
    assert!(replay.owner_token.is_some());
    assert_eq!(
        authority
            .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
            .unwrap()
            .portal_id,
        state.portal_id
    );
}

#[test]
fn attach_hud_target_hint_is_persisted_and_surfaced_in_projected_portal_state() {
    let mut authority = ProjectionAuthority::default();
    let mut request = attach_request("projection-a", "req-a");
    request.hud_target = Some("wall-display".to_string());
    assert!(authority.handle_attach(request, "caller-a", 10).accepted);

    let state = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("attach creates portal state");
    assert_eq!(
        state.hud_target.as_deref(),
        Some("wall-display"),
        "hud_target attach hint must be persisted and surfaced, not silently dropped"
    );

    // Redaction gates hud_target identically to sibling identity hints.
    let redacted = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::default())
        .expect("redacted portal still materializes");
    assert!(
        redacted.hud_target.is_none(),
        "hud_target must be withheld from viewers who cannot see attach identity"
    );
}

#[test]
fn attach_without_hud_target_defaults_to_none() {
    let mut authority = ProjectionAuthority::default();
    // attach_request sets hud_target: None (the default, no routing hint).
    assert!(
        authority
            .handle_attach(attach_request("projection-a", "req-a"), "caller-a", 10)
            .accepted
    );

    let state = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("attach creates portal state");
    assert!(
        state.hud_target.is_none(),
        "absent hud_target must default to None, not a spurious value"
    );
}

#[test]
fn successful_attach_issues_high_entropy_token_and_stores_only_verifier() {
    let mut authority = ProjectionAuthority::default();
    let response = authority.handle_attach(attach_request("projection-a", "req-a"), "caller-a", 10);
    assert!(response.accepted);
    let owner_token = response.owner_token.expect("attach must return token once");
    assert_eq!(owner_token.len(), OWNER_TOKEN_ENTROPY_BITS / 4);
    assert!(owner_token.chars().all(|ch| ch.is_ascii_hexdigit()));

    let verifier = authority
        .owner_token_verifier_for_test("projection-a")
        .expect("session stores verifier");
    assert_ne!(verifier, owner_token);
    assert_eq!(verifier.len(), 64);
}

#[test]
fn idempotent_attach_replay_rotates_owner_token_and_invalidates_previous_token() {
    let mut authority = ProjectionAuthority::default();
    let first = authority.handle_attach(attach_request("projection-a", "req-a"), "caller-a", 10);
    assert!(first.accepted);
    let first_token = first.owner_token.expect("initial attach returns a token");

    let replay = authority.handle_attach(attach_request("projection-a", "req-b"), "caller-a", 11);
    assert!(replay.accepted);
    let replay_token = replay
        .owner_token
        .expect("matching replay returns a fresh token");
    assert_ne!(replay_token, first_token);

    let stale = authority.handle_publish_status(
        status_request("projection-a", &first_token, "req-stale"),
        "caller-a",
        12,
    );
    assert!(!stale.accepted);
    assert_eq!(
        stale.error_code,
        Some(ProjectionErrorCode::ProjectionUnauthorized)
    );

    let current = authority.handle_publish_status(
        status_request("projection-a", &replay_token, "req-current"),
        "caller-a",
        13,
    );
    assert!(current.accepted);
}

#[test]
fn repeated_idempotent_attach_replays_leave_exactly_one_current_verifier() {
    let mut authority = ProjectionAuthority::default();
    let first = authority.handle_attach(attach_request("projection-a", "req-a"), "caller-a", 10);
    let first_token = first.owner_token.expect("initial attach returns a token");

    let replay_one = authority.handle_attach(
        attach_request("projection-a", "req-replay-1"),
        "caller-a",
        11,
    );
    let replay_one_token = replay_one
        .owner_token
        .expect("first serialized replay returns a token");
    let replay_two = authority.handle_attach(
        attach_request("projection-a", "req-replay-2"),
        "caller-a",
        12,
    );
    let replay_two_token = replay_two
        .owner_token
        .expect("second serialized replay returns a token");

    assert_ne!(first_token, replay_one_token);
    assert_ne!(replay_one_token, replay_two_token);
    for (request_id, stale_token) in [
        ("req-first-stale", first_token),
        ("req-replay-one-stale", replay_one_token),
    ] {
        let stale = authority.handle_publish_status(
            status_request("projection-a", &stale_token, request_id),
            "caller-a",
            13,
        );
        assert!(!stale.accepted);
        assert_eq!(
            stale.error_code,
            Some(ProjectionErrorCode::ProjectionUnauthorized)
        );
    }
    assert!(
        authority
            .handle_publish_status(
                status_request("projection-a", &replay_two_token, "req-latest"),
                "caller-a",
                13,
            )
            .accepted
    );
}

#[test]
fn unrelated_attach_keys_cannot_rotate_or_mint_owner_tokens() {
    let mut authority = ProjectionAuthority::default();
    let first = authority.handle_attach(attach_request("projection-a", "req-a"), "caller-a", 10);
    let first_token = first.owner_token.expect("initial attach returns a token");
    let initial_verifier = authority
        .owner_token_verifier_for_test("projection-a")
        .expect("session stores verifier")
        .to_string();

    let mut conflicting = attach_request("projection-a", "req-c");
    conflicting.idempotency_key = Some("different-key".to_string());
    let conflict = authority.handle_attach(conflicting, "caller-b", 12);
    assert!(!conflict.accepted);
    assert_eq!(
        conflict.error_code,
        Some(ProjectionErrorCode::ProjectionAlreadyAttached)
    );
    assert!(conflict.owner_token.is_none());

    let mut missing_key = attach_request("projection-a", "req-d");
    missing_key.idempotency_key = None;
    let missing_key_conflict = authority.handle_attach(missing_key, "caller-b", 13);
    assert!(!missing_key_conflict.accepted);
    assert_eq!(
        missing_key_conflict.error_code,
        Some(ProjectionErrorCode::ProjectionAlreadyAttached)
    );
    assert!(missing_key_conflict.owner_token.is_none());
    assert_eq!(
        authority.owner_token_verifier_for_test("projection-a"),
        Some(initial_verifier.as_str())
    );
    assert!(
        authority
            .handle_publish_status(
                status_request("projection-a", &first_token, "req-original-still-current"),
                "caller-a",
                14,
            )
            .accepted
    );
}

#[test]
fn idempotent_attach_replay_preserves_original_token_expiry() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        owner_token_ttl_wall_us: 20,
        ..ProjectionBounds::default()
    })
    .unwrap();
    let first = authority.handle_attach(attach_request("projection-a", "req-a"), "caller-a", 10);
    let first_token = first.owner_token.expect("initial attach returns a token");

    let replay =
        authority.handle_attach(attach_request("projection-a", "req-replay"), "caller-a", 29);
    let replay_token = replay
        .owner_token
        .expect("matching replay before expiry returns a fresh token");
    assert_ne!(replay_token, first_token);
    assert!(
        authority
            .handle_publish_status(
                status_request("projection-a", &replay_token, "req-before-expiry"),
                "caller-a",
                29,
            )
            .accepted
    );

    let expired = authority.handle_publish_status(
        status_request("projection-a", &replay_token, "req-at-original-expiry"),
        "caller-a",
        30,
    );
    assert!(!expired.accepted);
    assert_eq!(
        expired.error_code,
        Some(ProjectionErrorCode::ProjectionTokenExpired)
    );
    assert!(!authority.has_projection("projection-a"));
}

#[test]
fn attach_at_expiry_starts_fresh_session_with_fresh_ttl() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        owner_token_ttl_wall_us: 20,
        ..ProjectionBounds::default()
    })
    .unwrap();
    let first = authority.handle_attach(attach_request("projection-a", "req-a"), "caller-a", 10);
    let first_token = first.owner_token.expect("initial attach returns a token");
    assert!(
        authority
            .handle_publish_output(
                output_request("projection-a", &first_token, "req-old-output"),
                "caller-a",
                20,
            )
            .accepted
    );

    let fresh = authority.handle_attach(
        attach_request("projection-a", "req-after-expiry"),
        "caller-a",
        30,
    );
    assert!(fresh.accepted);
    let fresh_token = fresh
        .owner_token
        .expect("attach at the old deadline starts a fresh session");
    assert_ne!(fresh_token, first_token);
    assert!(
        authority
            .visible_transcript_window("projection-a")
            .expect("fresh session exists")
            .is_empty(),
        "expired session state must not survive a fresh attach"
    );
    assert!(
        authority
            .handle_publish_status(
                status_request("projection-a", &fresh_token, "req-fresh-current"),
                "caller-a",
                49,
            )
            .accepted,
        "fresh attach receives a full TTL from its own attach timestamp"
    );
}

#[test]
fn cross_projection_read_fails_closed() {
    let mut authority = ProjectionAuthority::default();
    let _token_a = attach(&mut authority, "projection-a");
    let token_b = attach(&mut authority, "projection-b");
    authority
        .enqueue_input(
            "projection-a",
            "input-1",
            "private operator text".to_string(),
            20,
            1_000,
            None,
        )
        .unwrap();

    let denied = authority.handle_get_pending_input(
        GetPendingInputRequest {
            envelope: envelope(
                ProjectionOperation::GetPendingInput,
                "projection-a",
                "req-read",
            ),
            owner_token: token_b,
            max_items: None,
            max_bytes: None,
        },
        "caller-b",
        30,
    );
    assert!(!denied.accepted);
    assert_eq!(
        denied.error_code,
        Some(ProjectionErrorCode::ProjectionUnauthorized)
    );
    assert!(denied.pending_input.is_empty());
}

#[test]
fn oversized_output_is_rejected_with_stable_code() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        max_output_bytes_per_call: 4,
        ..ProjectionBounds::default()
    })
    .unwrap();
    let owner_token = attach(&mut authority, "projection-a");
    let mut request = output_request("projection-a", &owner_token, "req-output");
    request.output_text = "too large".to_string();

    let response = authority.handle_publish_output(request, "caller-a", 20);
    assert!(!response.accepted);
    assert_eq!(
        response.error_code,
        Some(ProjectionErrorCode::ProjectionOutputTooLarge)
    );
}

/// hud-xwt6d: `OutputKind::Viewer` is reserved for the runtime's own
/// `submit_portal_input` echo path (§Viewer Reply Echo — "an adapter MUST
/// NOT ... forge a viewer turn through the output-publication contract").
/// Before this fix the rejection existed only in
/// `tze_hud_runtime::portal_projection_driver::parse_output_kind`, a string
/// check ahead of the authority call — any other caller of
/// `handle_publish_output` that constructs a `PublishOutputRequest` directly
/// had no guard at all. This drives the contract/authority layer directly (bypassing the
/// driver shim entirely) and asserts the forged publish is rejected and
/// leaves no trace in the retained transcript or unread accounting.
#[test]
fn viewer_kind_publish_is_rejected_at_contract_layer() {
    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");
    let mut request = output_request("projection-a", &owner_token, "req-output");
    request.output_kind = OutputKind::Viewer;

    let response = authority.handle_publish_output(request, "caller-a", 20);
    assert!(
        !response.accepted,
        "a forged viewer-kind publish must be rejected at the contract layer"
    );
    assert_eq!(
        response.error_code,
        Some(ProjectionErrorCode::ProjectionInvalidArgument)
    );

    let summary = authority.state_summary("projection-a").unwrap();
    assert_eq!(
        summary.retained_transcript_units, 0,
        "a rejected forged viewer turn must not land in the retained transcript"
    );
    assert_eq!(
        summary.unread_output_count, 0,
        "a rejected forged viewer turn must not increment unread accounting"
    );
}

#[test]
fn logical_unit_id_replay_is_idempotent() {
    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");
    let first = authority.handle_publish_output(
        output_request("projection-a", &owner_token, "req-output-1"),
        "caller-a",
        20,
    );
    let replay = authority.handle_publish_output(
        output_request("projection-a", &owner_token, "req-output-2"),
        "caller-a",
        21,
    );
    assert!(first.accepted);
    assert!(replay.accepted);
    assert!(replay.status_summary.contains("idempotently"));
}

#[test]
fn logical_unit_id_cache_is_bounded() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        max_seen_logical_units: 1,
        ..ProjectionBounds::default()
    })
    .unwrap();
    let owner_token = attach(&mut authority, "projection-a");

    let first = authority.handle_publish_output(
        output_request("projection-a", &owner_token, "req-output-1"),
        "caller-a",
        20,
    );
    let mut second_request = output_request("projection-a", &owner_token, "req-output-2");
    second_request.logical_unit_id = Some("unit-2".to_string());
    let second = authority.handle_publish_output(second_request, "caller-a", 21);
    let first_again = authority.handle_publish_output(
        output_request("projection-a", &owner_token, "req-output-3"),
        "caller-a",
        22,
    );

    assert!(first.accepted);
    assert!(second.accepted);
    assert!(first_again.accepted);
    assert!(!first_again.status_summary.contains("idempotently"));
}

#[test]
fn portal_composer_submission_is_transactional_bounded_inbox_feedback() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        max_pending_input_items: 1,
        max_pending_input_bytes_per_item: 4,
        ..ProjectionBounds::default()
    })
    .unwrap();
    let owner_token = attach(&mut authority, "projection-a");

    let oversized = authority.submit_portal_input(
        "projection-a",
        portal_submission("input-too-large", "12345"),
    );
    assert_eq!(oversized.feedback_state, PortalInputFeedbackState::Rejected);
    assert_eq!(
        oversized.error_code,
        Some(ProjectionErrorCode::ProjectionInputTooLarge)
    );
    assert_eq!(oversized.pending_input_count, 0);

    let accepted =
        authority.submit_portal_input("projection-a", portal_submission("input-1", "ok"));
    assert_eq!(accepted.feedback_state, PortalInputFeedbackState::Accepted);
    assert_eq!(accepted.pending_input_count, 1);
    assert_eq!(accepted.pending_input_bytes, 2);

    let full = authority.submit_portal_input("projection-a", portal_submission("input-2", "yo"));
    assert_eq!(full.feedback_state, PortalInputFeedbackState::Rejected);
    assert_eq!(
        full.error_code,
        Some(ProjectionErrorCode::ProjectionInputQueueFull)
    );
    assert_eq!(full.pending_input_count, 1);

    let state = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("portal state includes pending feedback");
    assert_eq!(state.pending_input_count, Some(1));
    assert_eq!(state.pending_input_bytes, Some(2));
    assert_eq!(
        state.last_input_feedback.as_ref().map(|f| f.feedback_state),
        Some(PortalInputFeedbackState::Rejected)
    );
    assert_eq!(
        state
            .last_input_feedback
            .as_ref()
            .map(|f| f.input_id.as_str()),
        Some("input-2")
    );

    let poll = authority.handle_get_pending_input(
        GetPendingInputRequest {
            envelope: envelope(
                ProjectionOperation::GetPendingInput,
                "projection-a",
                "req-poll",
            ),
            owner_token,
            max_items: None,
            max_bytes: None,
        },
        "caller-a",
        40,
    );
    assert!(poll.accepted);
    assert_eq!(poll.pending_input.len(), 1);
    assert_eq!(poll.pending_input[0].input_id, "input-1");
    assert_eq!(poll.pending_input[0].submission_text, "ok");
}

#[test]
fn accepted_portal_input_echoes_viewer_unit_into_input_history() {
    let mut authority = ProjectionAuthority::default();
    attach(&mut authority, "projection-a");

    let feedback =
        authority.submit_portal_input("projection-a", portal_submission("input-1", "ok"));
    assert_eq!(feedback.feedback_state, PortalInputFeedbackState::Accepted);

    assert_eq!(
        authority.next_due_projection_id().as_deref(),
        Some("projection-a"),
        "accepted HUD input must schedule a portal state render"
    );
    let update = authority
        .take_due_portal_update("projection-a", 31)
        .expect("projection exists")
        .expect("portal update must be drainable after viewer submit");
    // §Viewer Reply Echo two-pane split: the echo lands in the INPUT history, NOT
    // the OUTPUT transcript — so the drained OUTPUT window stays empty and its
    // byte count does not grow (no follow-tail scroll movement), while the echo
    // still drains WITHOUT raising unread (the viewer's own already-seen text).
    assert_eq!(
        update.unread_output_count, 0,
        "viewer echo must not raise unread (Viewer Reply Echo / ambient-attention doctrine)"
    );
    assert!(
        update.visible_transcript.is_empty(),
        "viewer echo must NOT enter the OUTPUT transcript"
    );
    assert_eq!(
        update.visible_transcript_bytes, 0,
        "a viewer echo must not grow OUTPUT transcript bytes (no follow-tail scroll move)"
    );
    let input_history = authority
        .input_history_window("projection-a")
        .expect("projection exists");
    assert_eq!(
        input_history.len(),
        1,
        "submit_portal_input echoes viewer text into the INPUT history"
    );
    assert_eq!(input_history[0].output_kind, OutputKind::Viewer);
    assert_eq!(input_history[0].output_text, "ok");
}

/// hud-hwk2m end-to-end: the jump-to-latest pill's ambient unread badge reaches a
/// BRIDGED portal via the wire (`SetTileUnreadCount` in the resident-gRPC render
/// batch), at parity with the in-process driver's direct `set_tile_unread_count`
/// call (#1088). Agent output raises the badge; a viewer echo (Viewer Reply Echo)
/// must NOT bump it — while still leaving the update drainable
/// (`portal_update_pending`), the very gotcha that #979 guarded on the append
/// side. This ties the count that flows into `ProjectedPortalState` to the count
/// the bridge adapter puts on the wire.
#[cfg(feature = "resident-grpc")]
#[test]
fn bridged_jump_to_latest_badge_reflects_unread_and_ignores_viewer_echo() {
    use crate::resident_grpc::{ResidentGrpcPortalAdapter, ResidentGrpcPortalConfig};
    use tze_hud_protocol::proto;

    // The count the bridged pill badge carries: the single `SetTileUnreadCount`
    // mutation the resident-gRPC adapter emits for `state`.
    fn badge_count(adapter: &ResidentGrpcPortalAdapter, state: &ProjectedPortalState) -> u32 {
        let batch = adapter
            .render_batch(state, 0)
            .expect("render_batch must succeed");
        batch
            .mutations
            .iter()
            .find_map(|m| match &m.mutation {
                Some(proto::mutation_proto::Mutation::SetTileUnreadCount(u)) => Some(u.count),
                _ => None,
            })
            .expect("a bridged render must emit a SetTileUnreadCount for the pill badge")
    }

    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");

    let mut adapter = ResidentGrpcPortalAdapter::new(ResidentGrpcPortalConfig::new(vec![9u8; 16]));
    adapter.record_created_tile(vec![9u8; 16]);

    // Two agent turns raise the ambient unread count to 2.
    for (rid, unit, ts) in [("req-a", "unit-a", 20), ("req-b", "unit-b", 21)] {
        assert!(
            authority
                .handle_publish_output(
                    output_request_keyed(
                        "projection-a",
                        &owner_token,
                        rid,
                        "agent turn",
                        Some(unit),
                        None,
                    ),
                    "caller-a",
                    ts,
                )
                .accepted
        );
    }
    let state = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("portal state materializes");
    assert_eq!(state.unread_output_count, Some(2));
    assert_eq!(
        badge_count(&adapter, &state),
        2,
        "the bridged pill badge must carry the agent-output unread count"
    );

    // A viewer echo appends the viewer's own already-seen text WITHOUT raising
    // unread (Viewer Reply Echo) — the badge must not grow.
    let feedback =
        authority.submit_portal_input("projection-a", portal_submission("input-1", "ok"));
    assert_eq!(feedback.feedback_state, PortalInputFeedbackState::Accepted);
    // ...but the echo must still be drainable: portal_update_pending schedules a
    // render so the viewer's own text is not stranded (the #979 no-bump gotcha).
    assert_eq!(
        authority.next_due_projection_id().as_deref(),
        Some("projection-a"),
        "a viewer echo must still schedule a portal render (portal_update_pending)"
    );
    let after_echo = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("portal state materializes");
    assert_eq!(
        after_echo.unread_output_count,
        Some(2),
        "a viewer echo must not bump the ambient unread count"
    );
    assert_eq!(
        badge_count(&adapter, &after_echo),
        2,
        "the bridged pill badge must not grow on a viewer echo (no unread bump)"
    );
}

#[test]
fn rate_limited_viewer_echo_is_drained_and_stays_unread_zero() {
    // Regression (gemini/codex review on #979): a viewer echo correctly does not
    // raise unread, but it must still be DRAINABLE. When the echo append is
    // rate-limited (the portal update slot is already spent), the session must
    // flag the update pending so take_due_portal_update's `unread==0 && !pending`
    // early return does not strand the viewer's own text.
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        max_portal_updates_per_second: 1,
        ..ProjectionBounds::default()
    })
    .unwrap();
    let owner_token = attach(&mut authority, "projection-a");

    // Spend the single rate slot with an agent publish at wall 20, then drain it
    // so the session returns to unread==0 && !pending.
    let publish = output_request("projection-a", &owner_token, "req-output-1");
    assert!(
        authority
            .handle_publish_output(publish, "caller-a", 20)
            .accepted
    );
    let drained = authority
        .take_due_portal_update("projection-a", 20)
        .unwrap()
        .expect("agent publish is immediately materializable");
    assert_eq!(drained.unread_output_count, 1);

    // Submit a viewer reply in the same rate window (portal_submission uses wall
    // 30); its echo append is therefore rate-limited.
    let feedback =
        authority.submit_portal_input("projection-a", portal_submission("input-1", "ok"));
    assert_eq!(feedback.feedback_state, PortalInputFeedbackState::Accepted);

    // The rate-limited viewer echo must still drain (not be stranded) and must
    // not raise unread. §Viewer Reply Echo two-pane split: it lands in the INPUT
    // history, never the OUTPUT transcript — so the drained OUTPUT window carries
    // only the earlier agent turn and no viewer unit.
    let update = authority
        .take_due_portal_update("projection-a", 30)
        .unwrap()
        .expect("rate-limited viewer echo must still be drainable, not stranded");
    assert_eq!(
        update.unread_output_count, 0,
        "viewer echo must not raise unread even when rate-limited"
    );
    assert!(
        !update
            .visible_transcript
            .iter()
            .any(|u| u.output_kind == OutputKind::Viewer),
        "the viewer echo must NOT appear in the OUTPUT transcript"
    );
    assert!(
        authority
            .input_history_window("projection-a")
            .expect("projection exists")
            .iter()
            .any(|u| u.output_kind == OutputKind::Viewer && u.output_text == "ok"),
        "the viewer echo must appear in the INPUT history"
    );
}

#[test]
fn acknowledged_portal_input_schedules_state_render_without_output() {
    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");
    let feedback =
        authority.submit_portal_input("projection-a", portal_submission("input-1", "ok"));
    assert_eq!(feedback.feedback_state, PortalInputFeedbackState::Accepted);
    let _ = authority
        .take_due_portal_update("projection-a", 31)
        .expect("projection exists")
        .expect("submit state update must be drainable");

    let poll = authority.handle_get_pending_input(
        GetPendingInputRequest {
            envelope: envelope(
                ProjectionOperation::GetPendingInput,
                "projection-a",
                "req-poll",
            ),
            owner_token: owner_token.clone(),
            max_items: None,
            max_bytes: None,
        },
        "caller-a",
        40,
    );
    assert!(poll.accepted);
    assert_eq!(poll.pending_input.len(), 1);

    let handled = authority.handle_acknowledge_input(
        AcknowledgeInputRequest {
            envelope: envelope(
                ProjectionOperation::AcknowledgeInput,
                "projection-a",
                "req-ack",
            ),
            owner_token,
            input_id: "input-1".to_string(),
            ack_state: InputAckState::Handled,
            ack_message: None,
            not_before_wall_us: None,
        },
        "caller-a",
        41,
    );
    assert!(handled.accepted);

    assert_eq!(
        authority.next_due_projection_id().as_deref(),
        Some("projection-a"),
        "handled acknowledgement must schedule a portal state render"
    );
    let update = authority
        .take_due_portal_update("projection-a", 42)
        .expect("projection exists")
        .expect("ack state update must be drainable");
    // No new output since the previous drain; the viewer echo never counted.
    assert_eq!(update.unread_output_count, 0);
    // §Viewer Reply Echo two-pane split: the echo lives in the INPUT history, not
    // the OUTPUT transcript (which stays empty — no agent output in this test);
    // the ack does not clear the echo.
    assert!(update.visible_transcript.is_empty());
    let input_history = authority
        .input_history_window("projection-a")
        .expect("projection exists");
    assert_eq!(input_history.len(), 1);
    assert_eq!(input_history[0].output_kind, OutputKind::Viewer);
}

#[test]
fn default_bounds_match_projection_spec_values() {
    let bounds = ProjectionBounds::default();
    assert_eq!(
        bounds.max_output_bytes_per_call,
        DEFAULT_MAX_OUTPUT_BYTES_PER_CALL
    );
    assert_eq!(bounds.max_status_text_bytes, DEFAULT_MAX_STATUS_TEXT_BYTES);
    assert_eq!(
        bounds.max_retained_transcript_bytes,
        DEFAULT_MAX_RETAINED_TRANSCRIPT_BYTES
    );
    assert_eq!(
        bounds.max_visible_transcript_bytes,
        DEFAULT_MAX_VISIBLE_TRANSCRIPT_BYTES
    );
    assert_eq!(
        bounds.max_pending_input_items,
        DEFAULT_MAX_PENDING_INPUT_ITEMS
    );
    assert_eq!(
        bounds.max_pending_input_bytes_per_item,
        DEFAULT_MAX_PENDING_INPUT_BYTES_PER_ITEM
    );
    assert_eq!(
        bounds.max_pending_input_total_bytes,
        DEFAULT_MAX_PENDING_INPUT_TOTAL_BYTES
    );
    assert_eq!(bounds.max_poll_items, DEFAULT_MAX_POLL_ITEMS);
    assert_eq!(
        bounds.max_poll_response_bytes,
        DEFAULT_MAX_POLL_RESPONSE_BYTES
    );
    assert_eq!(bounds.max_list_items, DEFAULT_MAX_LIST_ITEMS);
    assert_eq!(
        bounds.max_portal_updates_per_second,
        DEFAULT_MAX_PORTAL_UPDATES_PER_SECOND
    );
}

#[test]
fn default_and_oversized_item_poll_requests_return_32_small_fifo_inputs() {
    for (case, max_items) in [("default", None), ("oversized", Some(33))] {
        let mut authority = ProjectionAuthority::new(ProjectionBounds {
            // The retained queue must be larger than the poll cap to make the
            // default poll-item limit observable.
            max_pending_input_items: 33,
            ..ProjectionBounds::default()
        })
        .expect("test bounds are valid");
        let owner_token = attach(&mut authority, "projection-a");
        for index in 0..33 {
            authority
                .enqueue_input(
                    "projection-a",
                    &format!("input-{index:02}"),
                    format!("message-{index:02}"),
                    20 + index as u64,
                    10_000,
                    None,
                )
                .expect("small input fits the test queue");
        }

        let poll = authority.handle_get_pending_input(
            GetPendingInputRequest {
                envelope: envelope(
                    ProjectionOperation::GetPendingInput,
                    "projection-a",
                    &format!("req-poll-{case}"),
                ),
                owner_token,
                max_items,
                max_bytes: None,
            },
            "caller-a",
            100,
        );

        assert!(poll.accepted, "{case} request must be accepted");
        assert_eq!(
            poll.pending_input.len(),
            32,
            "{case} request must be clamped to the default item cap"
        );
        assert!(
            poll.pending_input
                .iter()
                .map(|item| item.submission_text.len())
                .sum::<usize>()
                < DEFAULT_MAX_POLL_RESPONSE_BYTES,
            "{case} test data must leave the byte cap nonbinding"
        );
        for (index, item) in poll.pending_input.iter().enumerate() {
            assert_eq!(item.input_id, format!("input-{index:02}"));
            assert_eq!(item.submission_text, format!("message-{index:02}"));
            assert_eq!(item.delivery_state, InputDeliveryState::Delivered);
        }
        assert_eq!(poll.pending_remaining_count, 1);
        assert_eq!(poll.pending_remaining_bytes, "message-32".len());
    }
}

#[test]
fn default_and_oversized_byte_poll_requests_keep_byte_backpressure() {
    for (case, max_bytes) in [
        ("default", None),
        ("oversized", Some(DEFAULT_MAX_POLL_RESPONSE_BYTES + 1)),
    ] {
        let mut authority = ProjectionAuthority::default();
        let owner_token = attach(&mut authority, "projection-a");
        for index in 0..5 {
            authority
                .enqueue_input(
                    "projection-a",
                    &format!("input-{index}"),
                    "x".repeat(DEFAULT_MAX_PENDING_INPUT_BYTES_PER_ITEM),
                    20 + index as u64,
                    10_000,
                    None,
                )
                .expect("input fits the default queue and per-item bounds");
        }

        let poll = authority.handle_get_pending_input(
            GetPendingInputRequest {
                envelope: envelope(
                    ProjectionOperation::GetPendingInput,
                    "projection-a",
                    &format!("req-poll-{case}"),
                ),
                owner_token,
                // Five items makes the item cap nonbinding; this test exercises
                // only the independent response-byte backpressure limit.
                max_items: Some(DEFAULT_MAX_POLL_ITEMS + 1),
                max_bytes,
            },
            "caller-a",
            100,
        );

        assert!(poll.accepted, "{case} request must be accepted");
        assert_eq!(poll.pending_input.len(), 4);
        assert_eq!(
            poll.pending_input
                .iter()
                .map(|item| item.submission_text.len())
                .sum::<usize>(),
            DEFAULT_MAX_POLL_RESPONSE_BYTES
        );
        for (index, item) in poll.pending_input.iter().enumerate() {
            assert_eq!(item.input_id, format!("input-{index}"));
            assert_eq!(item.delivery_state, InputDeliveryState::Delivered);
        }
        assert_eq!(poll.pending_remaining_count, 1);
        assert_eq!(
            poll.pending_remaining_bytes,
            DEFAULT_MAX_PENDING_INPUT_BYTES_PER_ITEM
        );
    }
}

#[test]
fn byte_bounded_polling_keeps_the_fifo_prefix_after_the_first_overflow() {
    for (case, max_bytes) in [
        ("default", None),
        ("oversized", Some(DEFAULT_MAX_POLL_RESPONSE_BYTES + 1)),
    ] {
        let mut authority = ProjectionAuthority::default();
        let owner_token = attach(&mut authority, "projection-a");
        for (index, payload_bytes) in [4_096, 4_096, 4_096, 4_095, 2, 1].into_iter().enumerate() {
            authority
                .enqueue_input(
                    "projection-a",
                    &format!("input-{index}"),
                    "x".repeat(payload_bytes),
                    20 + index as u64,
                    10_000,
                    None,
                )
                .expect("input fits the default queue and per-item bounds");
        }

        let poll = authority.handle_get_pending_input(
            GetPendingInputRequest {
                envelope: envelope(
                    ProjectionOperation::GetPendingInput,
                    "projection-a",
                    &format!("req-poll-fifo-{case}"),
                ),
                owner_token,
                // Exercise caller-side item-limit normalization without letting the
                // item cap bind; this isolates FIFO behavior under the byte cap.
                max_items: Some(DEFAULT_MAX_POLL_ITEMS + 1),
                max_bytes,
            },
            "caller-a",
            100,
        );

        assert!(poll.accepted, "{case} request must be accepted");
        assert_eq!(
            poll.pending_input
                .iter()
                .map(|item| item.input_id.as_str())
                .collect::<Vec<_>>(),
            vec!["input-0", "input-1", "input-2", "input-3"],
            "{case} request must stop at the first non-fitting eligible input"
        );
        assert_eq!(
            poll.pending_input
                .iter()
                .map(|item| item.submission_text.len())
                .sum::<usize>(),
            DEFAULT_MAX_POLL_RESPONSE_BYTES - 1
        );
        for item in &poll.pending_input {
            assert_eq!(item.delivery_state, InputDeliveryState::Delivered);
        }
        assert_eq!(poll.pending_remaining_count, 2);
        assert_eq!(poll.pending_remaining_bytes, 3);
    }
}

#[test]
fn collapsed_redacted_projection_preserves_geometry_and_suppresses_private_affordances() {
    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");
    let mut output = output_request("projection-a", &owner_token, "req-output");
    output.output_text = "private projected transcript".to_string();
    assert!(
        authority
            .handle_publish_output(output, "caller-a", 20)
            .accepted
    );
    let feedback =
        authority.submit_portal_input("projection-a", portal_submission("input-1", "help"));
    assert_eq!(feedback.feedback_state, PortalInputFeedbackState::Accepted);
    authority.collapse_projected_portal("projection-a").unwrap();

    let state = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::default())
        .expect("redacted portal still materializes");
    assert_eq!(state.presentation, ProjectedPortalPresentation::Collapsed);
    assert!(state.preserve_geometry);
    assert!(state.redacted);
    assert!(!state.interaction_enabled);
    assert_eq!(state.layer, ProjectedPortalLayer::Content);
    assert!(state.provider_kind.is_none());
    assert!(state.display_name.is_none());
    assert!(state.workspace_hint.is_none());
    assert!(state.hud_target.is_none());
    assert!(state.lifecycle_state.is_none());
    assert!(state.visible_transcript.is_empty());
    assert_eq!(state.unread_output_count, None);
    assert_eq!(state.pending_input_count, None);
    assert!(
        !serde_json::to_string(&state)
            .unwrap()
            .contains("private projected transcript")
    );
}

// ── hud-meqet: unread-output count reaches the render boundary un-nulled ──

/// `unread_output_count` is computed on the session (`ProjectionSession`) and
/// must flow through to `ProjectedPortalState` un-nulled whenever the viewer
/// policy reveals it — this is the authority-side half of the render
/// boundary; `resident_grpc.rs`'s `portal_markdown`/`unread_indicator_*` tests
/// cover the other half (actually drawing it). Before hud-meqet the value
/// made it this far correctly but was silently dropped by the render path, so
/// this test pins the authority contract explicitly.
#[test]
fn unread_output_count_flows_through_to_projected_portal_state_when_revealed() {
    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");
    let output = output_request("projection-a", &owner_token, "req-output");
    assert!(
        authority
            .handle_publish_output(output, "caller-a", 20)
            .accepted
    );

    let visible = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("portal state materializes");
    assert_eq!(
        visible.unread_output_count,
        Some(1),
        "unread_output_count must reach ProjectedPortalState un-nulled when \
         the viewer policy reveals it"
    );

    let redacted = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::default())
        .expect("redacted portal state still materializes");
    assert_eq!(
        redacted.unread_output_count, None,
        "unread count must stay redacted when reveal_unread is false"
    );
}

/// hud-g1ena.2 review regression: when a viewer's clearance filters some unread
/// turns out of `visible_transcript`, `visible_unread_output_count` must report
/// only the unread turns that viewer can actually see, while `unread_output_count`
/// stays the aggregate. The in-transcript unread divider is placed with the
/// clearance-corrected count, so a higher-classification unread turn hidden from
/// this viewer cannot push the divider onto an already-seen visible turn.
#[test]
fn visible_unread_output_count_excludes_clearance_filtered_unread_turns() {
    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");

    // Three unread agent turns: one Private (visible to a Private-cleared viewer)
    // and two Sensitive (filtered out for that viewer). Distinct logical unit ids
    // so each is a fresh append, not an idempotent duplicate.
    let private_turn = output_request_keyed(
        "projection-a",
        &owner_token,
        "req-private",
        "visible private turn",
        Some("unit-private"),
        None,
    );
    let mut sensitive_a = output_request_keyed(
        "projection-a",
        &owner_token,
        "req-sensitive-a",
        "hidden sensitive turn a",
        Some("unit-sensitive-a"),
        None,
    );
    sensitive_a.content_classification = ContentClassification::Sensitive;
    let mut sensitive_b = output_request_keyed(
        "projection-a",
        &owner_token,
        "req-sensitive-b",
        "hidden sensitive turn b",
        Some("unit-sensitive-b"),
        None,
    );
    sensitive_b.content_classification = ContentClassification::Sensitive;

    for (request, ts) in [(private_turn, 20), (sensitive_a, 21), (sensitive_b, 22)] {
        assert!(
            authority
                .handle_publish_output(request, "caller-a", ts)
                .accepted
        );
    }

    // A Private-cleared viewer that still reveals the transcript and unread count.
    let mut private_viewer = ProjectedPortalPolicy::permit_all();
    private_viewer.viewer_clearance = ContentClassification::Private;

    let visible = authority
        .projected_portal_state("projection-a", &private_viewer)
        .expect("portal state materializes");
    assert_eq!(
        visible.unread_output_count,
        Some(3),
        "the ambient count stays the aggregate across all clearances"
    );
    assert_eq!(
        visible.visible_unread_output_count,
        Some(1),
        "only the single Private unread turn is visible to a Private-cleared viewer"
    );
    assert_eq!(
        visible
            .visible_transcript
            .iter()
            .filter(|u| u.output_kind != OutputKind::Viewer)
            .count(),
        1,
        "the two Sensitive turns must be filtered out of the visible transcript"
    );

    // A fully-cleared viewer sees every unread turn: the two counts converge.
    let cleared = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("portal state materializes");
    assert_eq!(cleared.unread_output_count, Some(3));
    assert_eq!(
        cleared.visible_unread_output_count,
        Some(3),
        "with full clearance the visible unread count equals the aggregate"
    );
}

// ── hud-jip0k: expects_reply (Question) signal round-trip ────────────────

/// `PublishOutputRequest.expects_reply` must round-trip through
/// `handle_publish_output` into the retained `TranscriptUnit.expects_reply`,
/// and from there into `ProjectedPortalState.visible_transcript` — the shape
/// the resident gRPC adapter renders from. Omitted/`false` is the exact
/// pre-existing behavior (backward-compat default).
#[test]
fn expects_reply_round_trips_from_publish_output_to_projected_portal_state() {
    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");

    // Default/omitted case first — must match pre-existing behavior exactly.
    // Distinct `logical_unit_id`s (via output_request_keyed) so each publish
    // is a fresh append, not an idempotent duplicate of the other.
    let unset = output_request_keyed(
        "projection-a",
        &owner_token,
        "req-unset",
        "plain output",
        Some("unit-unset"),
        None,
    );
    assert!(!unset.expects_reply, "expects_reply must default to false");
    assert!(
        authority
            .handle_publish_output(unset, "caller-a", 20)
            .accepted
    );
    let after_unset = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("portal state materializes");
    assert_eq!(
        after_unset
            .visible_transcript
            .last()
            .map(|u| u.expects_reply),
        Some(false),
        "an unset expects_reply publish must round-trip as false"
    );

    // Opt-in case: a fresh request with expects_reply explicitly set.
    let mut question = output_request_keyed(
        "projection-a",
        &owner_token,
        "req-question",
        "which option do you prefer?",
        Some("unit-question"),
        None,
    );
    question.expects_reply = true;
    assert!(
        authority
            .handle_publish_output(question, "caller-a", 21)
            .accepted
    );
    let after_question = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("portal state materializes");
    assert_eq!(
        after_question.visible_transcript.len(),
        2,
        "the opt-in publish must append a fresh unit, not dedupe"
    );
    assert_eq!(
        after_question
            .visible_transcript
            .last()
            .map(|u| u.expects_reply),
        Some(true),
        "expects_reply == true must round-trip to the last visible transcript unit"
    );
}

// ── hud-o7h1r: viewer reply echoed into portal transcript ─────────────────

/// A successful `submit_portal_input` appends a `Viewer`-kind transcript unit
/// carrying the submitted text and the submission timestamp, making the
/// conversation visible on both sides of the portal surface.
#[test]
fn submit_portal_input_echoes_viewer_text_into_input_history() {
    let mut authority = ProjectionAuthority::default();
    attach(&mut authority, "projection-a");

    let submission = PortalInputSubmission {
        input_id: "input-viewer-1".to_string(),
        submission_text: "hello from the viewer".to_string(),
        submitted_at_wall_us: 50,
        expires_at_wall_us: Some(1_000),
        content_classification: ContentClassification::Private,
    };
    let feedback = authority.submit_portal_input("projection-a", submission);
    assert_eq!(
        feedback.feedback_state,
        PortalInputFeedbackState::Accepted,
        "submission must be accepted"
    );

    // §Viewer Reply Echo two-pane split: the echo lands in the INPUT history and
    // never in the OUTPUT transcript.
    assert!(
        authority
            .visible_transcript_window("projection-a")
            .expect("projection must exist")
            .is_empty(),
        "viewer echo must NOT enter the OUTPUT transcript"
    );
    let input_history = authority
        .input_history_window("projection-a")
        .expect("projection must exist");
    assert_eq!(
        input_history.len(),
        1,
        "accepted submit must echo exactly one viewer unit into the INPUT history"
    );
    let unit = &input_history[0];
    assert_eq!(
        unit.output_kind,
        OutputKind::Viewer,
        "output_kind must be Viewer"
    );
    assert_eq!(
        unit.output_text, "hello from the viewer",
        "viewer unit must carry the submitted text"
    );
    assert_eq!(
        unit.appended_at_wall_us, 50,
        "viewer unit timestamp must match submitted_at_wall_us"
    );
}

/// §Viewer Reply Echo two-pane split: the INPUT history is a separately-bounded
/// newest-fit window — the oldest viewer turns evict once the cap is exceeded,
/// exactly as the OUTPUT transcript is bounded, so INPUT growth never has to
/// borrow OUTPUT's budget.
#[test]
fn input_history_is_bounded_newest_fit_evicting_oldest() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        // The INPUT-history cap reuses `max_visible_transcript_bytes`; a small cap
        // forces eviction after a few 5-byte turns.
        max_visible_transcript_bytes: 10,
        ..ProjectionBounds::default()
    })
    .unwrap();
    attach(&mut authority, "projection-a");

    for (id, text) in [("i1", "aaaaa"), ("i2", "bbbbb"), ("i3", "ccccc")] {
        assert_eq!(
            authority
                .submit_portal_input("projection-a", portal_submission(id, text))
                .feedback_state,
            PortalInputFeedbackState::Accepted,
        );
    }

    let history = authority
        .input_history_window("projection-a")
        .expect("projection exists");
    assert_eq!(
        history
            .iter()
            .map(|u| u.output_text.as_str())
            .collect::<Vec<_>>(),
        vec!["bbbbb", "ccccc"],
        "INPUT history keeps a bounded newest-fit window, evicting the oldest turn"
    );
}

/// §Viewer Reply Echo two-pane split + reconnect parity (#1098): both
/// separately-bounded streams (OUTPUT transcript + INPUT history) live in the
/// projection session, so an ungraceful drop + reconnect-within-grace preserves
/// both — the viewer's replies are not lost across a session bounce, and they
/// still never leak into the OUTPUT transcript.
#[test]
fn input_history_and_output_transcript_survive_reconnect() {
    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");
    authority
        .record_hud_connection("projection-a", connection_metadata(&["modify_own_tiles"]))
        .unwrap();

    // Agent output → OUTPUT transcript; viewer reply → INPUT history.
    assert!(
        authority
            .handle_publish_output(
                output_request("projection-a", &owner_token, "req-1"),
                "caller-a",
                20,
            )
            .accepted
    );
    assert_eq!(
        authority
            .submit_portal_input("projection-a", portal_submission("input-1", "hi"))
            .feedback_state,
        PortalInputFeedbackState::Accepted,
    );

    // Ungraceful drop, then reconnect within grace (the session is not dropped).
    authority.mark_hud_disconnected("projection-a", 30).unwrap();
    authority
        .record_hud_connection(
            "projection-a",
            HudConnectionMetadata {
                connection_id: "connection-2".to_string(),
                authenticated_session_id: "runtime-session-2".to_string(),
                granted_capabilities: vec!["modify_own_tiles".to_string()],
                connected_at_wall_us: 40,
                last_reconnect_wall_us: 40,
            },
        )
        .unwrap();

    let state = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("portal state materializes");
    assert!(
        state
            .visible_transcript
            .iter()
            .any(|u| u.output_kind == OutputKind::Assistant),
        "OUTPUT transcript survives reconnect"
    );
    assert!(
        state
            .input_history
            .iter()
            .any(|u| u.output_kind == OutputKind::Viewer && u.output_text == "hi"),
        "INPUT history survives reconnect"
    );
    assert!(
        !state
            .visible_transcript
            .iter()
            .any(|u| u.output_kind == OutputKind::Viewer),
        "viewer echo never leaks into the OUTPUT transcript across reconnect"
    );
}

/// hud-g1ena.1: the projected portal state surfaces the viewer's most recent
/// submitted reply's delivery state so the render layer can present the ambient
/// per-turn delivery cue. The value is derived from the authority's existing
/// runtime-owned `pending_input` bookkeeping (no new adapter round trip): it starts
/// `Pending` on submit, advances to `Delivered` once the owner takes delivery via
/// `get_pending_input`, tracks the NEWEST submission when several are outstanding,
/// and is withheld (`None`) from a viewer whose clearance redacts the transcript.
#[test]
fn latest_viewer_delivery_state_flows_into_projected_portal_state() {
    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");

    // No submission yet → nothing to acknowledge.
    let initial = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("portal state materializes");
    assert_eq!(
        initial.latest_viewer_delivery_state, None,
        "no viewer submission yet → no delivery cue state"
    );

    // Submit a viewer reply → the runtime tracks it as Pending.
    assert_eq!(
        authority
            .submit_portal_input("projection-a", portal_submission("input-1", "first reply"))
            .feedback_state,
        PortalInputFeedbackState::Accepted,
    );
    let pending = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("portal state materializes");
    assert_eq!(
        pending.latest_viewer_delivery_state,
        Some(InputDeliveryState::Pending),
        "a freshly submitted reply is in-flight (Pending)"
    );

    // The owner takes delivery → the same reply advances to Delivered.
    let poll = authority.handle_get_pending_input(
        GetPendingInputRequest {
            envelope: envelope(
                ProjectionOperation::GetPendingInput,
                "projection-a",
                "req-poll",
            ),
            owner_token,
            max_items: None,
            max_bytes: None,
        },
        "caller-a",
        200,
    );
    assert!(poll.accepted, "owner poll must succeed: {poll:?}");
    let delivered = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("portal state materializes");
    assert_eq!(
        delivered.latest_viewer_delivery_state,
        Some(InputDeliveryState::Delivered),
        "once the owner takes delivery the cue state advances to Delivered"
    );

    // A newer submission is what the cue reflects (the tail of pending_input),
    // even while the older one stays Delivered.
    assert_eq!(
        authority
            .submit_portal_input("projection-a", portal_submission("input-2", "second reply"))
            .feedback_state,
        PortalInputFeedbackState::Accepted,
    );
    let newest = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("portal state materializes");
    assert_eq!(
        newest.latest_viewer_delivery_state,
        Some(InputDeliveryState::Pending),
        "the cue reflects the NEWEST viewer submission"
    );

    // Redaction: a viewer whose clearance withholds the transcript gets no cue
    // state — the delivery cue redacts together with the echoed turn.
    let mut no_transcript = ProjectedPortalPolicy::permit_all();
    no_transcript.reveal_transcript = false;
    let redacted = authority
        .projected_portal_state("projection-a", &no_transcript)
        .expect("portal state materializes");
    assert_eq!(
        redacted.latest_viewer_delivery_state, None,
        "a viewer who cannot see the transcript gets no delivery cue state"
    );
}

/// hud-1idbl (fail-closed privacy): the delivery cue must gate on the back()
/// item's OWN per-turn clearance, not merely on projection-level
/// `transcript_visible`. A viewer turn whose classification exceeds the viewer's
/// clearance is dropped from `visible_transcript` by the per-turn
/// `policy.permits(unit.content_classification)` filter even when the projection
/// itself is visible; if the cue still rendered → sending / ✓✓ / ✕ it would leak
/// the presence + delivery status of hidden content. The session here is Private
/// and the viewer clears Private (so the projection is visible and the transcript
/// window is shown), but the submitted reply is Sensitive (above clearance) — the
/// cue state MUST be withheld.
#[test]
fn latest_viewer_delivery_state_redacts_above_clearance_back_item() {
    let mut authority = ProjectionAuthority::default();
    attach(&mut authority, "projection-a");

    // Submit a Sensitive-classified viewer reply (above the viewer's clearance).
    assert_eq!(
        authority
            .submit_portal_input(
                "projection-a",
                PortalInputSubmission {
                    input_id: "input-sensitive".to_string(),
                    submission_text: "above-clearance reply".to_string(),
                    submitted_at_wall_us: 30,
                    expires_at_wall_us: Some(1_000),
                    content_classification: ContentClassification::Sensitive,
                },
            )
            .feedback_state,
        PortalInputFeedbackState::Accepted,
    );

    // Viewer clears Private: the Private session projection IS visible (so
    // `transcript_visible` is true), but the Sensitive back() item is redacted by
    // the per-turn clearance filter.
    let mut restricted = ProjectedPortalPolicy::permit_all();
    restricted.viewer_clearance = ContentClassification::Private;

    let state = authority
        .projected_portal_state("projection-a", &restricted)
        .expect("portal state materializes");
    assert!(
        state
            .visible_transcript
            .iter()
            .all(|unit| unit.content_classification <= ContentClassification::Private),
        "precondition: the Sensitive viewer turn is redacted from the visible transcript"
    );
    assert_eq!(
        state.latest_viewer_delivery_state, None,
        "hud-1idbl: an above-clearance back() item must withhold the delivery cue \
         EVEN WHEN the projection transcript is otherwise visible"
    );

    // Control: raising clearance to Sensitive reveals the same turn AND its cue.
    let mut cleared = ProjectedPortalPolicy::permit_all();
    cleared.viewer_clearance = ContentClassification::Sensitive;
    let revealed = authority
        .projected_portal_state("projection-a", &cleared)
        .expect("portal state materializes");
    assert_eq!(
        revealed.latest_viewer_delivery_state,
        Some(InputDeliveryState::Pending),
        "a viewer who clears the turn sees its delivery cue"
    );
}

/// hud-ny8rm (liveness): an owner poll that flips a viewer turn
/// Pending/Deferred -> Delivered changes the delivery cue (→ sending becomes
/// ✓✓ delivered), so it MUST schedule a portal repaint. The driver only
/// re-materializes portals from due coalescer entries; without the scheduled
/// update the cue would stay "sending" until an unrelated publish/ack forced a
/// repaint. Mirrors the `handle_acknowledge_input` cadence_append path.
#[test]
fn owner_poll_delivery_transition_schedules_portal_repaint() {
    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");

    // Submit a reply, then drain every pending portal update so the portal is at a
    // clean idle baseline — isolating the effect of the delivery transition from
    // attach/submit scheduling.
    assert_eq!(
        authority
            .submit_portal_input("projection-a", portal_submission("input-1", "reply"))
            .feedback_state,
        PortalInputFeedbackState::Accepted,
    );
    while let Some(id) = authority.next_due_projection_id() {
        let _ = authority.take_due_portal_update(&id, 40);
    }
    assert!(
        authority.next_due_projection_id().is_none(),
        "baseline must be idle before the owner poll"
    );

    // The owner poll transitions the reply Pending -> Delivered.
    let poll = authority.handle_get_pending_input(
        GetPendingInputRequest {
            envelope: envelope(
                ProjectionOperation::GetPendingInput,
                "projection-a",
                "req-poll",
            ),
            owner_token,
            max_items: None,
            max_bytes: None,
        },
        "caller-a",
        41,
    );
    assert!(poll.accepted, "owner poll must succeed: {poll:?}");
    assert_eq!(
        poll.pending_input.len(),
        1,
        "the poll delivers the pending reply"
    );

    assert_eq!(
        authority.next_due_projection_id().as_deref(),
        Some("projection-a"),
        "hud-ny8rm: a poll-driven Pending->Delivered transition must schedule a \
         portal repaint so the delivery cue advances from → sending to ✓✓ delivered"
    );
}

/// A poll that returns NO newly-delivered items (nothing pending) must NOT
/// schedule a spurious repaint — the repaint is gated on an actual delivery
/// transition (hud-ny8rm).
#[test]
fn owner_poll_without_delivery_transition_schedules_no_repaint() {
    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");

    // Drain attach-induced updates to an idle baseline. No input was ever
    // submitted, so the poll finds nothing to deliver.
    while let Some(id) = authority.next_due_projection_id() {
        let _ = authority.take_due_portal_update(&id, 40);
    }
    assert!(authority.next_due_projection_id().is_none());

    let poll = authority.handle_get_pending_input(
        GetPendingInputRequest {
            envelope: envelope(
                ProjectionOperation::GetPendingInput,
                "projection-a",
                "req-poll-empty",
            ),
            owner_token,
            max_items: None,
            max_bytes: None,
        },
        "caller-a",
        41,
    );
    assert!(poll.accepted);
    assert!(poll.pending_input.is_empty(), "no pending input to deliver");
    assert!(
        authority.next_due_projection_id().is_none(),
        "a poll with no delivery transition must not schedule a repaint"
    );
}

/// A rejected `submit_portal_input` (e.g. timestamp overflow) must NOT append
/// a viewer unit to the transcript.
#[test]
fn submit_portal_input_rejected_does_not_append_transcript_unit() {
    let mut authority = ProjectionAuthority::default();
    attach(&mut authority, "projection-a");

    let feedback = authority.submit_portal_input(
        "projection-a",
        PortalInputSubmission {
            input_id: "input-rejected".to_string(),
            submission_text: "help".to_string(),
            submitted_at_wall_us: u64::MAX,
            expires_at_wall_us: None,
            content_classification: ContentClassification::Private,
        },
    );
    assert_eq!(feedback.feedback_state, PortalInputFeedbackState::Rejected);

    let transcript = authority
        .visible_transcript_window("projection-a")
        .expect("projection must exist");
    assert!(
        transcript.is_empty(),
        "rejected submit must not add a viewer unit to the transcript"
    );
}

#[test]
fn portal_submission_default_ttl_overflow_is_rejected_not_panicked() {
    let mut authority = ProjectionAuthority::default();
    attach(&mut authority, "projection-a");

    let feedback = authority.submit_portal_input(
        "projection-a",
        PortalInputSubmission {
            input_id: "input-overflow".to_string(),
            submission_text: "help".to_string(),
            submitted_at_wall_us: u64::MAX,
            expires_at_wall_us: None,
            content_classification: ContentClassification::Private,
        },
    );

    assert_eq!(feedback.feedback_state, PortalInputFeedbackState::Rejected);
    assert_eq!(
        feedback.error_code,
        Some(ProjectionErrorCode::ProjectionInvalidArgument)
    );
    assert_eq!(feedback.pending_input_count, 0);
}

#[test]
fn projection_private_state_is_memory_only_and_purged_on_detach_cleanup_and_expiry() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        owner_token_ttl_wall_us: 30,
        ..ProjectionBounds::default()
    })
    .unwrap();
    let owner_token = attach(&mut authority, "projection-a");
    authority
        .record_hud_connection(
            "projection-a",
            connection_metadata(&["create_tiles", "modify_own_tiles"]),
        )
        .unwrap();
    authority
        .record_advisory_lease("projection-a", advisory_lease(&["create_tiles"], 100), 22)
        .unwrap();
    authority
        .enqueue_input(
            "projection-a",
            "input-1",
            "operator private text".to_string(),
            23,
            100,
            None,
        )
        .unwrap();
    let mut output = output_request("projection-a", &owner_token, "req-output");
    output.output_text = "private transcript text".to_string();
    assert!(
        authority
            .handle_publish_output(output, "caller-a", 24)
            .accepted
    );

    let summary = authority.state_summary("projection-a").unwrap();
    assert!(summary.has_hud_connection);
    assert!(summary.has_advisory_lease);
    assert_eq!(summary.pending_input_count, 1);
    assert!(summary.retained_transcript_bytes > 0);
    assert_eq!(
        authority.visible_transcript_window("projection-a").unwrap()[0].output_text,
        "private transcript text"
    );

    let detached = authority.handle_detach(
        DetachRequest {
            envelope: envelope(ProjectionOperation::Detach, "projection-a", "req-detach"),
            owner_token: owner_token.clone(),
            reason: "done".to_string(),
        },
        "caller-a",
        25,
    );
    assert!(detached.accepted);
    assert!(!authority.has_projection("projection-a"));

    let mut restarted = ProjectionAuthority::default();
    assert!(!restarted.has_projection("projection-a"));
    let fresh = attach(&mut restarted, "projection-a");
    assert_ne!(fresh, owner_token);

    let expired_token = attach(&mut restarted, "projection-expiring");
    assert!(restarted.has_projection("projection-expiring"));
    let expired = restarted.handle_publish_status(
        PublishStatusRequest {
            envelope: envelope(
                ProjectionOperation::PublishStatus,
                "projection-expiring",
                "req-expired",
            ),
            owner_token: expired_token,
            lifecycle_state: ProjectionLifecycleState::Active,
            status_text: None,
        },
        "caller-a",
        DEFAULT_OWNER_TOKEN_TTL_WALL_US + 20,
    );
    assert!(!expired.accepted);
    assert_eq!(
        expired.error_code,
        Some(ProjectionErrorCode::ProjectionTokenExpired)
    );
    assert!(!restarted.has_projection("projection-expiring"));
}

#[test]
fn stale_or_overbroad_lease_identity_cannot_authorize_republish() {
    let mut authority = ProjectionAuthority::default();
    attach(&mut authority, "projection-a");
    authority
        .record_hud_connection(
            "projection-a",
            connection_metadata(&["create_tiles", "modify_own_tiles"]),
        )
        .unwrap();
    authority
        .record_advisory_lease("projection-a", advisory_lease(&["create_tiles"], 100), 22)
        .unwrap();

    assert_eq!(
        authority.authorize_portal_republish(
            "projection-a",
            "lease-1",
            &[String::from("create_tiles")],
            30
        ),
        Ok(())
    );
    assert_eq!(
        authority.authorize_portal_republish(
            "projection-a",
            "lease-1",
            &[String::from("upload_resource")],
            31
        ),
        Err(ProjectionErrorCode::ProjectionUnauthorized)
    );
    assert_eq!(
        authority.authorize_portal_republish(
            "projection-a",
            "lease-1",
            &[String::from("create_tiles")],
            101
        ),
        Err(ProjectionErrorCode::ProjectionTokenExpired)
    );
    assert!(
        !authority
            .state_summary("projection-a")
            .unwrap()
            .has_advisory_lease
    );

    authority
        .record_advisory_lease("projection-a", advisory_lease(&["create_tiles"], 200), 120)
        .unwrap();
    authority
        .mark_hud_disconnected("projection-a", 130)
        .unwrap();
    assert_eq!(
        authority.authorize_portal_republish(
            "projection-a",
            "lease-1",
            &[String::from("create_tiles")],
            131
        ),
        Err(ProjectionErrorCode::ProjectionHudUnavailable)
    );
}

#[test]
fn same_session_reconnect_preserves_advisory_lease() {
    let mut authority = ProjectionAuthority::default();
    attach(&mut authority, "projection-a");
    authority
        .record_hud_connection(
            "projection-a",
            connection_metadata(&["create_tiles", "modify_own_tiles"]),
        )
        .unwrap();
    authority
        .record_advisory_lease("projection-a", advisory_lease(&["create_tiles"], 100), 22)
        .unwrap();

    let mut reconnected = connection_metadata(&["create_tiles", "modify_own_tiles"]);
    reconnected.connection_id = "connection-2".to_string();
    reconnected.connected_at_wall_us = 40;
    reconnected.last_reconnect_wall_us = 40;
    authority
        .record_hud_connection("projection-a", reconnected)
        .unwrap();

    let summary = authority.state_summary("projection-a").unwrap();
    assert_eq!(summary.reconnect.reconnect_count, 1);
    assert_eq!(summary.reconnect.last_reconnect_wall_us, Some(40));
    assert!(summary.has_advisory_lease);
    assert_eq!(
        authority.authorize_portal_republish(
            "projection-a",
            "lease-1",
            &[String::from("create_tiles")],
            41
        ),
        Ok(())
    );
}

#[test]
fn authenticated_session_takeover_drops_advisory_lease() {
    let mut authority = ProjectionAuthority::default();
    attach(&mut authority, "projection-a");
    authority
        .record_hud_connection(
            "projection-a",
            connection_metadata(&["create_tiles", "modify_own_tiles"]),
        )
        .unwrap();
    authority
        .record_advisory_lease("projection-a", advisory_lease(&["create_tiles"], 100), 22)
        .unwrap();

    let mut takeover = connection_metadata(&["create_tiles", "modify_own_tiles"]);
    // Keep the transport identity stable: the authenticated-session identity is
    // the takeover boundary, independently of a transport rotation.
    takeover.authenticated_session_id = "runtime-session-2".to_string();
    takeover.connected_at_wall_us = 40;
    takeover.last_reconnect_wall_us = 40;
    authority
        .record_hud_connection("projection-a", takeover)
        .unwrap();

    let summary = authority.state_summary("projection-a").unwrap();
    assert_eq!(summary.reconnect.reconnect_count, 1);
    assert_eq!(summary.reconnect.last_reconnect_wall_us, Some(40));
    assert!(!summary.has_advisory_lease);
    assert_eq!(
        authority.authorize_portal_republish(
            "projection-a",
            "lease-1",
            &[String::from("create_tiles")],
            41
        ),
        Err(ProjectionErrorCode::ProjectionUnauthorized)
    );
}

#[test]
fn heartbeat_requires_live_connection_and_is_monotonic() {
    let mut authority = ProjectionAuthority::default();
    attach(&mut authority, "projection-a");

    assert_eq!(
        authority.record_heartbeat("projection-a", 25),
        Err(ProjectionErrorCode::ProjectionHudUnavailable)
    );

    authority
        .record_hud_connection("projection-a", connection_metadata(&["create_tiles"]))
        .unwrap();
    authority.record_heartbeat("projection-a", 30).unwrap();
    assert_eq!(
        authority
            .state_summary("projection-a")
            .unwrap()
            .reconnect
            .last_heartbeat_wall_us,
        Some(30)
    );
    assert_eq!(
        authority.record_heartbeat("projection-a", 29),
        Err(ProjectionErrorCode::ProjectionStateConflict)
    );

    authority.mark_hud_disconnected("projection-a", 40).unwrap();
    assert_eq!(
        authority.record_heartbeat("projection-a", 41),
        Err(ProjectionErrorCode::ProjectionHudUnavailable)
    );
}

#[test]
fn agent_liveness_gap_degrades_portal_without_escalating_attention() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        agent_liveness_degraded_after_wall_us: 1_000_000,
        ..ProjectionBounds::default()
    })
    .unwrap();
    attach(&mut authority, "projection-a");

    assert!(
        authority
            .sweep_agent_liveness_degradation(1_000_009)
            .is_empty(),
        "the configured threshold is a lower bound, not an early warning"
    );
    assert_eq!(
        authority.sweep_agent_liveness_degradation(1_000_010),
        vec!["projection-a".to_string()],
        "an idle attached projection must enter Degraded at the threshold"
    );

    let summary = authority.state_summary("projection-a").unwrap();
    assert_eq!(summary.lifecycle_state, ProjectionLifecycleState::Degraded);
    assert_eq!(summary.reconnect.last_heartbeat_wall_us, Some(10));
    let state = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .unwrap();
    assert!(
        state.connection_degraded,
        "agent-side liveness loss must reuse the token-styled stale treatment"
    );
    assert_eq!(
        state.attention,
        ProjectedPortalAttention::Ambient,
        "going stale is ambient state, never an attention escalation"
    );
    assert!(
        authority
            .sweep_agent_liveness_degradation(2_000_000)
            .is_empty(),
        "the transition is one-shot while the same session stays degraded"
    );
}

#[test]
fn agent_liveness_deadline_exposes_the_first_required_idle_sweep() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        agent_liveness_degraded_after_wall_us: 1_000_000,
        ..ProjectionBounds::default()
    })
    .unwrap();
    attach(&mut authority, "projection-a");

    assert_eq!(
        authority.next_agent_liveness_deadline_wall_us(),
        Some(1_000_010),
        "the event loop must wake exactly when the attached projection becomes stale"
    );

    authority.sweep_agent_liveness_degradation(1_000_010);
    assert_eq!(
        authority.next_agent_liveness_deadline_wall_us(),
        None,
        "an already-degraded session must not leave a periodic liveness wake behind"
    );
}

#[test]
fn agent_liveness_authenticated_projection_operations_refresh_and_recover() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        agent_liveness_degraded_after_wall_us: 1_000_000,
        ..ProjectionBounds::default()
    })
    .unwrap();
    let owner_token = attach(&mut authority, "projection-a");
    authority
        .enqueue_input(
            "projection-a",
            "input-1",
            "operator input".to_string(),
            20,
            10_000_000,
            None,
        )
        .unwrap();

    assert_eq!(
        authority.sweep_agent_liveness_degradation(1_000_010),
        vec!["projection-a".to_string()]
    );

    let denied = authority.handle_publish_output(
        output_request("projection-a", "wrong-owner-token", "req-denied"),
        "caller-a",
        1_000_019,
    );
    assert!(!denied.accepted);
    assert_eq!(
        authority
            .state_summary("projection-a")
            .unwrap()
            .reconnect
            .last_heartbeat_wall_us,
        Some(10),
        "a failed token check must not refresh liveness"
    );

    let poll = authority.handle_get_pending_input(
        GetPendingInputRequest {
            envelope: envelope(
                ProjectionOperation::GetPendingInput,
                "projection-a",
                "req-poll-recover",
            ),
            owner_token: owner_token.clone(),
            max_items: Some(1),
            max_bytes: None,
        },
        "caller-a",
        1_000_020,
    );
    assert!(poll.accepted);
    assert_eq!(
        authority
            .state_summary("projection-a")
            .unwrap()
            .lifecycle_state,
        ProjectionLifecycleState::Active,
        "an authenticated poll recovers a liveness-degraded session"
    );
    assert_eq!(
        authority
            .state_summary("projection-a")
            .unwrap()
            .reconnect
            .last_heartbeat_wall_us,
        Some(1_000_020)
    );

    assert_eq!(
        authority.sweep_agent_liveness_degradation(2_000_020),
        vec!["projection-a".to_string()]
    );
    let ack = authority.handle_acknowledge_input(
        AcknowledgeInputRequest {
            envelope: envelope(
                ProjectionOperation::AcknowledgeInput,
                "projection-a",
                "req-ack-recover",
            ),
            owner_token: owner_token.clone(),
            input_id: "input-1".to_string(),
            ack_state: InputAckState::Handled,
            ack_message: None,
            not_before_wall_us: None,
        },
        "caller-a",
        2_000_030,
    );
    assert!(ack.accepted);
    assert_eq!(
        authority
            .state_summary("projection-a")
            .unwrap()
            .reconnect
            .last_heartbeat_wall_us,
        Some(2_000_030),
        "an authenticated acknowledgement counts as liveness"
    );

    assert_eq!(
        authority.sweep_agent_liveness_degradation(3_000_030),
        vec!["projection-a".to_string()]
    );
    let status = authority.handle_publish_status(
        status_request("projection-a", &owner_token, "req-status-recover"),
        "caller-a",
        3_000_040,
    );
    assert!(status.accepted);
    assert_eq!(
        authority
            .state_summary("projection-a")
            .unwrap()
            .reconnect
            .last_heartbeat_wall_us,
        Some(3_000_040),
        "an authenticated status update counts as liveness"
    );

    assert_eq!(
        authority.sweep_agent_liveness_degradation(4_000_040),
        vec!["projection-a".to_string()]
    );
    let publish = authority.handle_publish_output(
        output_request("projection-a", &owner_token, "req-publish-recover"),
        "caller-a",
        4_000_050,
    );
    assert!(publish.accepted);
    let recovered = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .unwrap();
    assert_eq!(
        recovered.lifecycle_state,
        Some(ProjectionLifecycleState::Active)
    );
    assert!(!recovered.connection_degraded);
    assert_eq!(
        authority
            .state_summary("projection-a")
            .unwrap()
            .reconnect
            .last_heartbeat_wall_us,
        Some(4_000_050),
        "an authenticated output publish counts as liveness"
    );
}

#[test]
fn agent_liveness_threshold_is_bounded_and_has_a_safe_default() {
    let defaults = ProjectionBounds::default();
    assert!(
        defaults.agent_liveness_degraded_after_wall_us >= MIN_AGENT_LIVENESS_DEGRADED_AFTER_WALL_US
    );
    assert!(
        defaults.agent_liveness_degraded_after_wall_us <= MAX_AGENT_LIVENESS_DEGRADED_AFTER_WALL_US
    );
    assert!(
        ProjectionAuthority::new(ProjectionBounds {
            agent_liveness_degraded_after_wall_us: 0,
            ..defaults.clone()
        })
        .is_err()
    );
    assert!(
        ProjectionAuthority::new(ProjectionBounds {
            agent_liveness_degraded_after_wall_us: MAX_AGENT_LIVENESS_DEGRADED_AFTER_WALL_US
                .saturating_add(1),
            ..defaults
        })
        .is_err()
    );
}

#[test]
fn reconnect_preserves_transcript_inbox_ack_state_and_requires_new_lease() {
    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");
    let mut output = output_request("projection-a", &owner_token, "req-output");
    output.output_text = "retained across HUD reconnect".to_string();
    assert!(
        authority
            .handle_publish_output(output, "caller-a", 20)
            .accepted
    );
    authority
        .enqueue_input(
            "projection-a",
            "input-1",
            "operator input survives reconnect".to_string(),
            21,
            1_000,
            None,
        )
        .unwrap();
    let delivered = authority.handle_get_pending_input(
        GetPendingInputRequest {
            envelope: envelope(
                ProjectionOperation::GetPendingInput,
                "projection-a",
                "req-poll",
            ),
            owner_token: owner_token.clone(),
            max_items: Some(1),
            max_bytes: None,
        },
        "caller-a",
        22,
    );
    assert!(delivered.accepted);
    authority
        .record_hud_connection("projection-a", connection_metadata(&["create_tiles"]))
        .unwrap();
    authority
        .record_advisory_lease("projection-a", advisory_lease(&["create_tiles"], 100), 23)
        .unwrap();

    authority.mark_hud_disconnected("projection-a", 30).unwrap();
    let mut reconnected = connection_metadata(&["create_tiles"]);
    reconnected.connection_id = "connection-after-drop".to_string();
    reconnected.authenticated_session_id = "runtime-session-after-drop".to_string();
    reconnected.connected_at_wall_us = 40;
    reconnected.last_reconnect_wall_us = 40;
    authority
        .record_hud_connection("projection-a", reconnected)
        .unwrap();

    let summary = authority.state_summary("projection-a").unwrap();
    assert_eq!(summary.retained_transcript_units, 1);
    assert_eq!(summary.pending_input_count, 1);
    assert!(!summary.has_advisory_lease);
    assert_eq!(
        authority.visible_transcript_window("projection-a").unwrap()[0].output_text,
        "retained across HUD reconnect"
    );
    assert_eq!(
        authority.authorize_portal_republish(
            "projection-a",
            "lease-1",
            &[String::from("create_tiles")],
            41
        ),
        Err(ProjectionErrorCode::ProjectionUnauthorized)
    );

    let handled_after_reconnect = authority.handle_acknowledge_input(
        AcknowledgeInputRequest {
            envelope: envelope(
                ProjectionOperation::AcknowledgeInput,
                "projection-a",
                "req-ack-after-reconnect",
            ),
            owner_token,
            input_id: "input-1".to_string(),
            ack_state: InputAckState::Handled,
            ack_message: None,
            not_before_wall_us: None,
        },
        "caller-a",
        42,
    );
    assert!(handled_after_reconnect.accepted);
    assert_eq!(
        authority
            .state_summary("projection-a")
            .unwrap()
            .pending_input_count,
        0
    );
}

#[test]
fn owner_degraded_lifecycle_is_not_overwritten_by_connection_or_output() {
    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");
    let degraded = authority.handle_publish_status(
        PublishStatusRequest {
            envelope: envelope(
                ProjectionOperation::PublishStatus,
                "projection-a",
                "req-status",
            ),
            owner_token: owner_token.clone(),
            lifecycle_state: ProjectionLifecycleState::Degraded,
            status_text: Some("HUD projection is degraded".to_string()),
        },
        "caller-a",
        20,
    );
    assert!(degraded.accepted);

    authority
        .record_hud_connection("projection-a", connection_metadata(&["create_tiles"]))
        .unwrap();
    assert_eq!(
        authority
            .state_summary("projection-a")
            .unwrap()
            .lifecycle_state,
        ProjectionLifecycleState::Degraded
    );

    let published = authority.handle_publish_output(
        output_request("projection-a", &owner_token, "req-output"),
        "caller-a",
        21,
    );
    assert!(published.accepted);
    assert_eq!(
        authority
            .state_summary("projection-a")
            .unwrap()
            .lifecycle_state,
        ProjectionLifecycleState::Degraded
    );
}

/// Regression (PR #945 review, hud-y8h3m): a status-only `publish_status`
/// must schedule a portal refresh so the drain loop re-materialises the
/// viewer-facing lifecycle/status. Without the cadence-coalescer wiring the new
/// state would stay invisible until some unrelated publish/input made the portal
/// due — `publish_status` would not be end-to-end for status-only updates.
#[test]
fn status_only_publish_marks_portal_due_for_refresh() {
    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");

    // Drain any attach-induced pending update so we have a clean idle baseline,
    // isolating the effect of the status publish from attach scheduling.
    while let Some(id) = authority.next_due_projection_id() {
        let _ = authority.take_due_portal_update(&id, 10);
    }
    assert!(
        authority.next_due_projection_id().is_none(),
        "baseline must be idle before the status publish"
    );

    // A status-only publish (no transcript output) must make the portal due.
    let accepted = authority.handle_publish_status(
        PublishStatusRequest {
            envelope: envelope(
                ProjectionOperation::PublishStatus,
                "projection-a",
                "req-status-due",
            ),
            owner_token: owner_token.clone(),
            lifecycle_state: ProjectionLifecycleState::Degraded,
            status_text: Some("blocked on input".to_string()),
        },
        "caller-a",
        20,
    );
    assert!(accepted.accepted);
    assert_eq!(
        authority.next_due_projection_id().as_deref(),
        Some("projection-a"),
        "a status-only publish must mark the portal due so the viewer is refreshed"
    );

    // Drain it back to idle, then confirm a DENIED status (bad owner token) does
    // not schedule a refresh.
    while let Some(id) = authority.next_due_projection_id() {
        let _ = authority.take_due_portal_update(&id, 21);
    }
    let denied = authority.handle_publish_status(
        PublishStatusRequest {
            envelope: envelope(
                ProjectionOperation::PublishStatus,
                "projection-a",
                "req-status-denied",
            ),
            owner_token: "not-the-real-token".to_string(),
            lifecycle_state: ProjectionLifecycleState::Active,
            status_text: None,
        },
        "caller-a",
        22,
    );
    assert!(!denied.accepted);
    assert!(
        authority.next_due_projection_id().is_none(),
        "a denied status publish must not schedule a portal refresh"
    );
}

#[test]
fn acknowledgement_and_detach_cleanup_update_projected_portal_state() {
    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");
    let accepted =
        authority.submit_portal_input("projection-a", portal_submission("input-1", "ok"));
    assert_eq!(accepted.feedback_state, PortalInputFeedbackState::Accepted);
    assert_eq!(
        authority
            .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
            .unwrap()
            .pending_input_count,
        Some(1)
    );

    let poll = authority.handle_get_pending_input(
        GetPendingInputRequest {
            envelope: envelope(
                ProjectionOperation::GetPendingInput,
                "projection-a",
                "req-poll",
            ),
            owner_token: owner_token.clone(),
            max_items: None,
            max_bytes: None,
        },
        "caller-a",
        40,
    );
    assert!(poll.accepted);
    let handled = authority.handle_acknowledge_input(
        AcknowledgeInputRequest {
            envelope: envelope(
                ProjectionOperation::AcknowledgeInput,
                "projection-a",
                "req-ack",
            ),
            owner_token: owner_token.clone(),
            input_id: "input-1".to_string(),
            ack_state: InputAckState::Handled,
            ack_message: None,
            not_before_wall_us: None,
        },
        "caller-a",
        41,
    );
    assert!(handled.accepted);
    let state = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .unwrap();
    assert_eq!(state.pending_input_count, Some(0));
    assert_eq!(state.pending_input_bytes, Some(0));

    let detached = authority.handle_detach(
        DetachRequest {
            envelope: envelope(ProjectionOperation::Detach, "projection-a", "req-detach"),
            owner_token,
            reason: "session complete".to_string(),
        },
        "caller-a",
        42,
    );
    assert!(detached.accepted);
    assert!(
        authority
            .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
            .is_none()
    );
}

#[test]
fn projected_portal_contract_has_no_terminal_or_process_authority() {
    let mut authority = ProjectionAuthority::default();
    attach(&mut authority, "projection-a");
    let state = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("portal state exists");

    assert_eq!(
        state.adapter_family,
        ProjectedPortalAdapterFamily::CooperativeProjection
    );
    assert_eq!(
        state.runtime_authority,
        ProjectedPortalRuntimeAuthority::ResidentSessionLease
    );
    let wire = serde_json::to_string(&state).unwrap();
    for forbidden in ["pty", "tmux", "terminal", "stdin", "process"] {
        assert!(
            !wire.contains(forbidden),
            "projected portal state must not expose {forbidden} authority"
        );
    }
}

#[test]
fn provider_kind_does_not_change_projection_semantics() {
    for (index, provider_kind) in [
        ProviderKind::Codex,
        ProviderKind::Claude,
        ProviderKind::Opencode,
        ProviderKind::Other,
    ]
    .into_iter()
    .enumerate()
    {
        let projection_id = format!("projection-provider-{index}");
        let mut authority = ProjectionAuthority::default();
        let attach = authority.handle_attach(
            AttachRequest {
                provider_kind,
                display_name: format!("Provider {index}"),
                ..attach_request(&projection_id, "req-attach")
            },
            "caller-a",
            10,
        );
        assert!(attach.accepted);
        let owner_token = attach.owner_token.expect("attach returns owner token");

        assert!(
            authority
                .handle_publish_output(
                    output_request(&projection_id, &owner_token, "req-output"),
                    "caller-a",
                    20,
                )
                .accepted
        );
        let feedback =
            authority.submit_portal_input(&projection_id, portal_submission("input-1", "ok"));
        assert_eq!(feedback.feedback_state, PortalInputFeedbackState::Accepted);
        let poll = authority.handle_get_pending_input(
            GetPendingInputRequest {
                envelope: envelope(
                    ProjectionOperation::GetPendingInput,
                    &projection_id,
                    "req-poll",
                ),
                owner_token: owner_token.clone(),
                max_items: None,
                max_bytes: None,
            },
            "caller-a",
            30,
        );
        assert!(poll.accepted);
        assert_eq!(poll.pending_input.len(), 1);
        let detached = authority.handle_detach(
            DetachRequest {
                envelope: envelope(ProjectionOperation::Detach, &projection_id, "req-detach"),
                owner_token,
                reason: "done".to_string(),
            },
            "caller-a",
            40,
        );
        assert!(detached.accepted);
        assert!(!authority.has_projection(&projection_id));
    }
}

#[test]
fn ready_portal_update_can_be_taken_without_spending_another_rate_slot() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        max_portal_updates_per_second: 1,
        ..ProjectionBounds::default()
    })
    .unwrap();
    let owner_token = attach(&mut authority, "projection-a");

    let mut first = output_request("projection-a", &owner_token, "req-output-1");
    first.output_text = "first".to_string();
    first.logical_unit_id = Some("unit-first".to_string());
    let first_response = authority.handle_publish_output(first, "caller-a", 20);
    assert!(first_response.accepted);
    assert!(first_response.portal_update_ready);

    let immediate = authority
        .take_due_portal_update("projection-a", 20)
        .unwrap()
        .expect("ready publish should be immediately materializable");
    assert_eq!(immediate.unread_output_count, 1);
    assert_eq!(immediate.coalesced_output_count, 0);
    assert!(
        authority
            .take_due_portal_update("projection-a", 20)
            .unwrap()
            .is_none()
    );

    let mut second = output_request("projection-a", &owner_token, "req-output-2");
    second.output_text = "second".to_string();
    second.logical_unit_id = Some("unit-second".to_string());
    let second_response = authority.handle_publish_output(second, "caller-a", 20);
    assert!(second_response.accepted);
    assert!(!second_response.portal_update_ready);
    assert!(
        authority
            .take_due_portal_update("projection-a", 20)
            .unwrap()
            .is_none()
    );

    let coalesced = authority
        .take_due_portal_update("projection-a", PORTAL_UPDATE_RATE_WINDOW_WALL_US + 20)
        .unwrap()
        .expect("coalesced publish should become due in the next rate window");
    assert_eq!(coalesced.unread_output_count, 1);
    assert_eq!(coalesced.coalesced_output_count, 1);
    assert_eq!(
        coalesced
            .visible_transcript
            .last()
            .expect("visible transcript includes second publish")
            .output_text,
        "second"
    );
}

#[test]
fn transcript_pruning_and_portal_update_rate_coalescing_are_bounded() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        max_retained_transcript_bytes: 12,
        max_visible_transcript_bytes: 8,
        max_portal_updates_per_second: 1,
        ..ProjectionBounds::default()
    })
    .unwrap();
    let owner_token = attach(&mut authority, "projection-a");

    let mut first = output_request("projection-a", &owner_token, "req-output-1");
    first.output_text = "aaaa".to_string();
    first.logical_unit_id = Some("unit-a".to_string());
    let first_response = authority.handle_publish_output(first, "caller-a", 20);
    assert!(first_response.accepted);
    assert!(first_response.portal_update_ready);

    let mut second = output_request("projection-a", &owner_token, "req-output-2");
    second.output_text = "bbbb".to_string();
    second.logical_unit_id = Some("unit-b".to_string());
    second.coalesce_key = Some("status-line".to_string());
    let second_response = authority.handle_publish_output(second, "caller-a", 20);
    assert!(second_response.accepted);
    assert!(!second_response.portal_update_ready);
    assert_eq!(second_response.coalesced_output_count, 1);

    let mut third = output_request("projection-a", &owner_token, "req-output-3");
    third.output_text = "cccc".to_string();
    third.logical_unit_id = Some("unit-c".to_string());
    third.coalesce_key = Some("status-line".to_string());
    let third_response = authority.handle_publish_output(third, "caller-a", 20);
    assert!(third_response.accepted);
    assert!(!third_response.portal_update_ready);
    assert_eq!(third_response.coalesced_output_count, 2);

    let retained = authority.visible_transcript_window("projection-a").unwrap();
    assert_eq!(
        retained
            .iter()
            .filter(|unit| unit.coalesce_key.as_deref() == Some("status-line"))
            .count(),
        1
    );
    assert_eq!(
        retained
            .iter()
            .find(|unit| unit.coalesce_key.as_deref() == Some("status-line"))
            .unwrap()
            .output_text,
        "cccc"
    );

    let mut fourth = output_request("projection-a", &owner_token, "req-output-4");
    fourth.output_text = "dddd".to_string();
    fourth.logical_unit_id = Some("unit-d".to_string());
    assert!(
        authority
            .handle_publish_output(fourth, "caller-a", 20)
            .accepted
    );

    let mut fifth = output_request("projection-a", &owner_token, "req-output-5");
    fifth.output_text = "eeee".to_string();
    fifth.logical_unit_id = Some("unit-e".to_string());
    let fifth_response =
        authority.handle_publish_output(fifth, "caller-a", PORTAL_UPDATE_RATE_WINDOW_WALL_US + 21);
    assert!(fifth_response.accepted);
    assert!(fifth_response.portal_update_ready);
    assert_eq!(fifth_response.coalesced_output_count, 3);

    let summary = authority.state_summary("projection-a").unwrap();
    assert!(summary.retained_transcript_bytes <= 12);
    assert!(summary.visible_transcript_bytes <= 8);
    assert_eq!(summary.retained_transcript_units, 3);

    let update = authority
        .take_due_portal_update("projection-a", (PORTAL_UPDATE_RATE_WINDOW_WALL_US * 2) + 22)
        .unwrap()
        .expect("update should be due in the next rate window");
    assert!(update.visible_transcript_bytes <= 8);
    assert_eq!(update.coalesced_output_count, 3);
    assert_eq!(update.unread_output_count, 5);
    assert_eq!(
        authority
            .state_summary("projection-a")
            .unwrap()
            .unread_output_count,
        0
    );
}

#[test]
fn pending_input_bounds_and_acknowledgement_state_conflicts_are_enforced() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        max_pending_input_items: 1,
        ..ProjectionBounds::default()
    })
    .unwrap();
    let owner_token = attach(&mut authority, "projection-a");
    authority
        .enqueue_input(
            "projection-a",
            "input-1",
            "first".to_string(),
            20,
            1_000,
            None,
        )
        .unwrap();
    assert_eq!(
        authority.enqueue_input(
            "projection-a",
            "input-2",
            "second".to_string(),
            21,
            1_000,
            None
        ),
        Err(ProjectionErrorCode::ProjectionInputQueueFull)
    );

    let poll = authority.handle_get_pending_input(
        GetPendingInputRequest {
            envelope: envelope(
                ProjectionOperation::GetPendingInput,
                "projection-a",
                "req-poll",
            ),
            owner_token: owner_token.clone(),
            max_items: Some(8),
            max_bytes: None,
        },
        "caller-a",
        30,
    );
    assert!(poll.accepted);
    assert_eq!(
        poll.pending_input[0].delivery_state,
        InputDeliveryState::Delivered
    );

    let handled = authority.handle_acknowledge_input(
        AcknowledgeInputRequest {
            envelope: envelope(
                ProjectionOperation::AcknowledgeInput,
                "projection-a",
                "req-ack-1",
            ),
            owner_token: owner_token.clone(),
            input_id: "input-1".to_string(),
            ack_state: InputAckState::Handled,
            ack_message: None,
            not_before_wall_us: None,
        },
        "caller-a",
        31,
    );
    assert!(handled.accepted);

    let replay = authority.handle_acknowledge_input(
        AcknowledgeInputRequest {
            envelope: envelope(
                ProjectionOperation::AcknowledgeInput,
                "projection-a",
                "req-ack-2",
            ),
            owner_token: owner_token.clone(),
            input_id: "input-1".to_string(),
            ack_state: InputAckState::Handled,
            ack_message: None,
            not_before_wall_us: None,
        },
        "caller-a",
        32,
    );
    assert!(replay.accepted);

    let conflict = authority.handle_acknowledge_input(
        AcknowledgeInputRequest {
            envelope: envelope(
                ProjectionOperation::AcknowledgeInput,
                "projection-a",
                "req-ack-3",
            ),
            owner_token,
            input_id: "input-1".to_string(),
            ack_state: InputAckState::Rejected,
            ack_message: None,
            not_before_wall_us: None,
        },
        "caller-a",
        33,
    );
    assert!(!conflict.accepted);
    assert_eq!(
        conflict.error_code,
        Some(ProjectionErrorCode::ProjectionStateConflict)
    );
}

#[test]
fn pending_input_expiry_sweep_schedules_idle_portal_state_update() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds::default()).unwrap();
    attach(&mut authority, "projection-expiry");
    authority
        .enqueue_input(
            "projection-expiry",
            "input-expiry",
            "expires while parked".to_string(),
            20,
            100,
            None,
        )
        .unwrap();

    assert_eq!(authority.next_pending_input_expiry_wall_us(), Some(100));
    assert!(authority.sweep_pending_input_expiry(99).is_empty());
    assert_eq!(
        authority.sweep_pending_input_expiry(100),
        vec!["projection-expiry".to_string()]
    );
    assert_eq!(authority.next_pending_input_expiry_wall_us(), None);
    let summary = authority.state_summary("projection-expiry").unwrap();
    assert_eq!(summary.pending_input_count, 0);
    assert_eq!(summary.pending_input_bytes, 0);
    assert_eq!(authority.coalescer_pending_portal_count(), 1);
}

#[test]
fn deferred_input_redelivers_after_not_before_and_expires_before_delivery() {
    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");
    authority
        .enqueue_input(
            "projection-a",
            "input-1",
            "defer me".to_string(),
            20,
            100,
            None,
        )
        .unwrap();
    authority
        .enqueue_input(
            "projection-a",
            "input-2",
            "expire me".to_string(),
            21,
            45,
            None,
        )
        .unwrap();

    let first_poll = authority.handle_get_pending_input(
        GetPendingInputRequest {
            envelope: envelope(
                ProjectionOperation::GetPendingInput,
                "projection-a",
                "req-poll-1",
            ),
            owner_token: owner_token.clone(),
            max_items: Some(1),
            max_bytes: None,
        },
        "caller-a",
        30,
    );
    assert_eq!(first_poll.pending_input[0].input_id, "input-1");

    let deferred = authority.handle_acknowledge_input(
        AcknowledgeInputRequest {
            envelope: envelope(
                ProjectionOperation::AcknowledgeInput,
                "projection-a",
                "req-defer",
            ),
            owner_token: owner_token.clone(),
            input_id: "input-1".to_string(),
            ack_state: InputAckState::Deferred,
            ack_message: None,
            not_before_wall_us: Some(60),
        },
        "caller-a",
        31,
    );
    assert!(deferred.accepted);

    let hidden = authority.handle_get_pending_input(
        GetPendingInputRequest {
            envelope: envelope(
                ProjectionOperation::GetPendingInput,
                "projection-a",
                "req-poll-2",
            ),
            owner_token: owner_token.clone(),
            max_items: Some(8),
            max_bytes: None,
        },
        "caller-a",
        50,
    );
    assert!(hidden.pending_input.is_empty());

    let redelivered = authority.handle_get_pending_input(
        GetPendingInputRequest {
            envelope: envelope(
                ProjectionOperation::GetPendingInput,
                "projection-a",
                "req-poll-3",
            ),
            owner_token: owner_token.clone(),
            max_items: Some(8),
            max_bytes: None,
        },
        "caller-a",
        61,
    );
    assert_eq!(redelivered.pending_input.len(), 1);
    assert_eq!(redelivered.pending_input[0].input_id, "input-1");
    assert_eq!(
        redelivered.pending_input[0].delivery_state,
        InputDeliveryState::Delivered
    );

    let expired_ack = authority.handle_acknowledge_input(
        AcknowledgeInputRequest {
            envelope: envelope(
                ProjectionOperation::AcknowledgeInput,
                "projection-a",
                "req-expired-ack",
            ),
            owner_token,
            input_id: "input-2".to_string(),
            ack_state: InputAckState::Handled,
            ack_message: None,
            not_before_wall_us: None,
        },
        "caller-a",
        62,
    );
    assert!(!expired_ack.accepted);
    assert_eq!(
        expired_ack.error_code,
        Some(ProjectionErrorCode::ProjectionStateConflict)
    );
}

#[test]
fn terminal_pending_input_is_pruned_without_losing_ack_replay() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        max_pending_input_items: 1,
        ..ProjectionBounds::default()
    })
    .unwrap();
    let owner_token = attach(&mut authority, "projection-a");
    authority
        .enqueue_input(
            "projection-a",
            "input-1",
            "first".to_string(),
            20,
            1_000,
            None,
        )
        .unwrap();

    let poll = authority.handle_get_pending_input(
        GetPendingInputRequest {
            envelope: envelope(
                ProjectionOperation::GetPendingInput,
                "projection-a",
                "req-poll",
            ),
            owner_token: owner_token.clone(),
            max_items: Some(8),
            max_bytes: None,
        },
        "caller-a",
        30,
    );
    assert!(poll.accepted);

    let handled = authority.handle_acknowledge_input(
        AcknowledgeInputRequest {
            envelope: envelope(
                ProjectionOperation::AcknowledgeInput,
                "projection-a",
                "req-ack-1",
            ),
            owner_token: owner_token.clone(),
            input_id: "input-1".to_string(),
            ack_state: InputAckState::Handled,
            ack_message: None,
            not_before_wall_us: None,
        },
        "caller-a",
        31,
    );
    assert!(handled.accepted);

    authority
        .enqueue_input(
            "projection-a",
            "input-2",
            "second".to_string(),
            32,
            1_000,
            None,
        )
        .unwrap();

    let replay = authority.handle_acknowledge_input(
        AcknowledgeInputRequest {
            envelope: envelope(
                ProjectionOperation::AcknowledgeInput,
                "projection-a",
                "req-ack-2",
            ),
            owner_token,
            input_id: "input-1".to_string(),
            ack_state: InputAckState::Handled,
            ack_message: None,
            not_before_wall_us: None,
        },
        "caller-a",
        33,
    );
    assert!(replay.accepted);
    assert!(replay.status_summary.contains("idempotently"));
}

#[test]
fn not_before_is_rejected_for_terminal_acknowledgements() {
    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");
    authority
        .enqueue_input(
            "projection-a",
            "input-1",
            "first".to_string(),
            20,
            1_000,
            None,
        )
        .unwrap();

    let response = authority.handle_acknowledge_input(
        AcknowledgeInputRequest {
            envelope: envelope(
                ProjectionOperation::AcknowledgeInput,
                "projection-a",
                "req-ack",
            ),
            owner_token,
            input_id: "input-1".to_string(),
            ack_state: InputAckState::Handled,
            ack_message: None,
            not_before_wall_us: Some(50),
        },
        "caller-a",
        30,
    );

    assert!(!response.accepted);
    assert_eq!(
        response.error_code,
        Some(ProjectionErrorCode::ProjectionInvalidArgument)
    );
}

#[test]
fn bounded_copy_preserves_utf8_boundaries() {
    assert_eq!(bounded_copy("hello".to_string(), 10), "hello");
    assert_eq!(bounded_copy("éclair".to_string(), 1), "");
    assert_eq!(bounded_copy("aéclair".to_string(), 2), "a");
}

#[test]
fn token_expiry_fails_with_stable_code() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        owner_token_ttl_wall_us: 5,
        ..ProjectionBounds::default()
    })
    .unwrap();
    let owner_token = attach(&mut authority, "projection-a");
    let response = authority.handle_publish_status(
        PublishStatusRequest {
            envelope: envelope(
                ProjectionOperation::PublishStatus,
                "projection-a",
                "req-status",
            ),
            owner_token,
            lifecycle_state: ProjectionLifecycleState::Active,
            status_text: None,
        },
        "caller-a",
        20,
    );
    assert!(!response.accepted);
    assert_eq!(
        response.error_code,
        Some(ProjectionErrorCode::ProjectionTokenExpired)
    );
    assert!(!authority.has_projection("projection-a"));
}

proptest! {
    #[test]
    fn pending_input_polling_is_fifo_and_bounded(
        item_count in 1usize..24,
        requested_items in 1usize..12,
        requested_bytes in 1usize..96,
    ) {
        let mut authority = ProjectionAuthority::new(ProjectionBounds {
            max_pending_input_items: 32,
            max_pending_input_bytes_per_item: 128,
            max_pending_input_total_bytes: 4096,
            max_poll_items: 16,
            max_poll_response_bytes: 128,
            ..ProjectionBounds::default()
        })
        .unwrap();
        let owner_token = attach(&mut authority, "projection-a");
        for index in 0..item_count {
            authority
                .enqueue_input(
                    "projection-a",
                    &format!("input-{index}"),
                    format!("msg-{index:02}"),
                    20 + index as u64,
                    10_000,
                    None,
                )
                .unwrap();
        }

        let poll = authority.handle_get_pending_input(
            GetPendingInputRequest {
                envelope: envelope(
                    ProjectionOperation::GetPendingInput,
                    "projection-a",
                    "req-poll",
                ),
                owner_token,
                max_items: Some(requested_items),
                max_bytes: Some(requested_bytes),
            },
            "caller-a",
            100,
        );

        prop_assert!(poll.accepted);
        prop_assert!(poll.pending_input.len() <= requested_items.min(16));
        let returned_bytes: usize = poll
            .pending_input
            .iter()
            .map(|item| item.submission_text.len())
            .sum();
        prop_assert!(returned_bytes <= requested_bytes.min(128));
        for (index, item) in poll.pending_input.iter().enumerate() {
            prop_assert_eq!(&item.input_id, &format!("input-{index}"));
            prop_assert_eq!(item.delivery_state, InputDeliveryState::Delivered);
            prop_assert_eq!(item.delivered_at_wall_us, Some(100));
        }
        prop_assert_eq!(
            poll.pending_remaining_count + poll.pending_input.len(),
            item_count
        );
    }

    #[test]
    fn lifecycle_state_machine_never_reuses_stale_connection_or_lease(
        actions in prop::collection::vec(0u8..6, 1..32),
    ) {
        let mut authority = ProjectionAuthority::default();
        let owner_token = attach(&mut authority, "projection-a");
        let mut projection_exists = true;
        let mut has_connection = false;
        let mut lease_expires_at = None;
        let mut now = 20u64;

        for action in actions {
            now += 10;
            match action {
                0 => {
                    let mut metadata = connection_metadata(&["create_tiles"]);
                    metadata.connection_id = format!("connection-{now}");
                    metadata.authenticated_session_id = format!("runtime-session-{now}");
                    metadata.connected_at_wall_us = now;
                    metadata.last_reconnect_wall_us = now;
                    prop_assert_eq!(
                        authority.record_hud_connection("projection-a", metadata),
                        Ok(())
                    );
                    has_connection = true;
                    lease_expires_at = None;
                }
                1 => {
                    let result = authority.record_heartbeat("projection-a", now);
                    if has_connection {
                        prop_assert_eq!(result, Ok(()));
                    } else {
                        prop_assert_eq!(result, Err(ProjectionErrorCode::ProjectionHudUnavailable));
                    }
                }
                2 => {
                    let result = authority.record_advisory_lease(
                        "projection-a",
                        advisory_lease(&["create_tiles"], now + 100),
                        now,
                    );
                    if has_connection {
                        prop_assert_eq!(result, Ok(()));
                        lease_expires_at = Some(now + 100);
                    } else {
                        prop_assert_eq!(result, Err(ProjectionErrorCode::ProjectionHudUnavailable));
                    }
                }
                3 => {
                    prop_assert_eq!(
                        authority.mark_hud_disconnected("projection-a", now),
                        Ok(())
                    );
                    has_connection = false;
                    lease_expires_at = None;
                }
                4 => {
                    let result = authority.authorize_portal_republish(
                        "projection-a",
                        "lease-1",
                        &[String::from("create_tiles")],
                        now,
                    );
                    if has_connection && lease_expires_at.is_some_and(|expires_at| now < expires_at) {
                        prop_assert_eq!(result, Ok(()));
                    } else {
                        prop_assert!(result.is_err());
                        if has_connection && lease_expires_at.is_some_and(|expires_at| now >= expires_at) {
                            prop_assert_eq!(result, Err(ProjectionErrorCode::ProjectionTokenExpired));
                            lease_expires_at = None;
                        }
                    }
                }
                _ => {
                    let response = authority.handle_detach(
                        DetachRequest {
                            envelope: envelope(
                                ProjectionOperation::Detach,
                                "projection-a",
                                "req-detach",
                            ),
                            owner_token: owner_token.clone(),
                            reason: "property lifecycle detach".to_string(),
                        },
                        "caller-a",
                        now,
                    );
                    prop_assert!(response.accepted);
                    projection_exists = false;
                }
            }

            if !projection_exists {
                prop_assert!(authority.state_summary("projection-a").is_none());
                break;
            }

            let summary = authority.state_summary("projection-a").unwrap();
            prop_assert_eq!(summary.has_hud_connection, has_connection);
            if !has_connection {
                prop_assert!(!summary.has_advisory_lease);
            }
        }
    }
}

// ── AdapterGeometryBatch::coalesce (§6b.4) ───────────────────────────────

fn make_snapshot(sequence: u64, gesture_active: bool) -> AdapterGeometrySnapshot {
    AdapterGeometrySnapshot {
        rect: AdapterPortalRect {
            x_px: 10,
            y_px: 20,
            width_px: 400,
            height_px: 300,
        },
        gesture_active,
        sequence,
    }
}

#[test]
fn coalesce_first_snapshot_is_stored() {
    let mut batch = AdapterGeometryBatch::default();
    assert!(batch.is_empty(), "batch must start empty");

    batch.coalesce(make_snapshot(1, true));

    assert!(!batch.is_empty(), "batch must not be empty after coalesce");
    let latest = batch.latest.expect("latest must be Some after coalesce");
    assert_eq!(latest.sequence, 1, "latest sequence must be 1");
    assert!(latest.gesture_active, "gesture_active must match snapshot");
}

#[test]
fn coalesce_newer_snapshot_replaces_older() {
    let mut batch = AdapterGeometryBatch::default();
    batch.coalesce(make_snapshot(1, true));
    batch.coalesce(make_snapshot(2, false)); // newer

    let latest = batch.latest.expect("latest must be Some");
    assert_eq!(
        latest.sequence, 2,
        "newer snapshot (seq=2) must replace older (seq=1)"
    );
    assert!(
        !latest.gesture_active,
        "gesture_active from newer snapshot must win"
    );
}

#[test]
fn coalesce_older_snapshot_does_not_replace_newer() {
    let mut batch = AdapterGeometryBatch::default();
    batch.coalesce(make_snapshot(5, false));
    batch.coalesce(make_snapshot(3, true)); // older — must be dropped

    let latest = batch.latest.expect("latest must be Some");
    assert_eq!(
        latest.sequence, 5,
        "older snapshot (seq=3) must NOT displace newer (seq=5)"
    );
    assert!(
        !latest.gesture_active,
        "gesture_active from older snapshot must be ignored"
    );
}

#[test]
fn coalesce_equal_sequence_does_not_replace() {
    let mut batch = AdapterGeometryBatch::default();
    batch.coalesce(make_snapshot(7, false));

    // Same sequence with different geometry — first write wins (not strictly
    // required by spec, but the coalescer MUST NOT drop the existing entry
    // for an equal sequence in a way that loses a confirmed snapshot).
    let mut snap_same_seq = make_snapshot(7, true); // gesture_active differs
    snap_same_seq.rect = AdapterPortalRect {
        x_px: 99,
        y_px: 99,
        width_px: 100,
        height_px: 100,
    };
    batch.coalesce(snap_same_seq);

    let latest = batch.latest.expect("latest must be Some");
    assert_eq!(
        latest.sequence, 7,
        "sequence must remain 7 after equal-sequence coalesce attempt"
    );
    assert!(
        !latest.gesture_active,
        "first write (gesture_active=false) must be retained for equal sequence"
    );
}

proptest! {
    /// For any sequence of (sequence, gesture_active) pairs,
    /// `latest.sequence` must equal the maximum sequence seen.
    #[test]
    fn coalesce_latest_wins_monotone(pairs in proptest::collection::vec((0u64..100u64, proptest::bool::ANY), 1..20usize)) {
        let mut batch = AdapterGeometryBatch::default();
        let mut max_seq = 0u64;
        for (seq, gesture_active) in &pairs {
            if *seq > max_seq { max_seq = *seq; }
            batch.coalesce(make_snapshot(*seq, *gesture_active));
        }
        let latest = batch.latest.expect("batch must be non-empty after coalescing snapshots");
        prop_assert_eq!(
            latest.sequence, max_seq,
            "latest sequence must equal maximum sequence seen"
        );
    }
}

// ── ProjectionAuthority geometry batch flow (§6b.4) ─────────────────────
//
// Covers push_geometry_snapshot → projected_portal_state → consume_geometry_batch
// so that callers cannot accidentally re-deliver geometry indefinitely or
// fail to surface it in ProjectedPortalState.

#[test]
fn geometry_batch_not_surfaced_before_first_push() {
    let mut authority = ProjectionAuthority::default();
    attach(&mut authority, "p-geo");
    // No snapshot has been pushed — geometry_batch must be None.
    let state = authority
        .projected_portal_state("p-geo", &ProjectedPortalPolicy::permit_all())
        .expect("session must exist");
    assert!(
        state.geometry_batch.is_none(),
        "geometry_batch must be None before any push"
    );
}

#[test]
fn geometry_batch_surfaced_after_push_and_cleared_after_consume() {
    let mut authority = ProjectionAuthority::default();
    attach(&mut authority, "p-geo");

    let snap = make_snapshot(1, false);
    let accepted = authority.push_geometry_snapshot("p-geo", snap);
    assert!(accepted, "push must return true for a new snapshot");

    // After push: projected_portal_state must include the batch.
    let state = authority
        .projected_portal_state("p-geo", &ProjectedPortalPolicy::permit_all())
        .expect("session must exist");
    let batch = state
        .geometry_batch
        .expect("geometry_batch must be Some after push");
    let latest = batch.latest.expect("batch.latest must be Some");
    assert_eq!(
        latest.sequence, 1,
        "surfaced sequence must match pushed snapshot"
    );

    // After consume: projected_portal_state must return None for geometry_batch.
    authority.consume_geometry_batch("p-geo");
    let state2 = authority
        .projected_portal_state("p-geo", &ProjectedPortalPolicy::permit_all())
        .expect("session must still exist");
    assert!(
        state2.geometry_batch.is_none(),
        "geometry_batch must be None after consume"
    );
}

#[test]
fn geometry_batch_not_re_delivered_without_new_push() {
    // Verifies that a caller that calls projected_portal_state twice after one
    // consume does NOT receive stale geometry on the second call.
    let mut authority = ProjectionAuthority::default();
    attach(&mut authority, "p-geo");

    authority.push_geometry_snapshot("p-geo", make_snapshot(3, true));
    // First read + consume.
    let _ = authority.projected_portal_state("p-geo", &ProjectedPortalPolicy::permit_all());
    authority.consume_geometry_batch("p-geo");

    // Second read without a new push — must be empty.
    let state = authority
        .projected_portal_state("p-geo", &ProjectedPortalPolicy::permit_all())
        .expect("session must exist");
    assert!(
        state.geometry_batch.is_none(),
        "geometry_batch must remain None after consume with no new push"
    );
}

#[test]
fn push_geometry_snapshot_rejects_unknown_session() {
    let mut authority = ProjectionAuthority::default();
    let accepted = authority.push_geometry_snapshot("does-not-exist", make_snapshot(1, false));
    assert!(
        !accepted,
        "push must return false for an unknown projection_id"
    );
}

// ── Regression tests for hud-endkj defect fixes ──────────────────────────
//
// Defect 1: coalesce-key in-place update path passed a stale sequence to
// record_append, causing the coalescer to drop it after the first drain.
// Final-state for a coalesce-key would never be presented until an unrelated
// fresh append arrived.
//
// Defect 2: next_due_projection_id returns a rate-limited portal whose
// take_due_portal_update returns Ok(None) without consuming the coalescer
// entry, allowing a naive driver to busy-spin.  portal_next_due_at_us
// provides the wait hint that breaks the spin.

/// Regression: coalesce-key final-state must appear via next_due_projection_id
/// after the first drain, even when no new append follows.
///
/// Scenario:
/// 1. First append (portal_update_ready = true) → coalescer entry seq=0 accepted.
/// 2. take_due_portal_update → drains coalescer (last_drained_sequence=0).
/// 3. Second append (same coalesce_key, rate-limited) → previously bugged
///    code passed seq=0 → stale guard dropped it → portal vanished from
///    next_due_projection_id forever.
///
/// After fix: append_transcript_unit bumps next_transcript_sequence on the
/// in-place path, returning seq=1, which clears the stale guard.
#[test]
fn coalesce_key_final_state_reappears_after_drain_without_fresh_append() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        max_portal_updates_per_second: 1,
        ..ProjectionBounds::default()
    })
    .unwrap();
    let owner_token = attach(&mut authority, "proj-coalesce");
    let portal_id = "proj-coalesce";

    // First append — rate window is open.
    let mut first = output_request(portal_id, &owner_token, "req-1");
    first.output_text = "initial".to_string();
    first.logical_unit_id = Some("unit-1".to_string());
    first.coalesce_key = Some("status".to_string());
    let r1 = authority.handle_publish_output(first, "caller", 10);
    assert!(r1.accepted, "first append must be accepted");
    assert!(
        r1.portal_update_ready,
        "first append must be immediately ready"
    );

    // Oracle confirms a pending update.
    assert_eq!(
        authority.next_due_projection_id().as_deref(),
        Some(portal_id),
        "portal must be ready after first append"
    );

    // Drain the coalescer — last_drained_sequence is now recorded.
    let update = authority
        .take_due_portal_update(portal_id, 10)
        .expect("no error")
        .expect("first update must be present");
    assert_eq!(update.unread_output_count, 1);

    // Oracle must return None after draining (nothing pending).
    assert!(
        authority.next_due_projection_id().is_none(),
        "oracle must be idle after drain"
    );

    // Second append — rate window is closed (same timestamp 10).
    // Uses the same coalesce_key → in-place update path.
    let mut second = output_request(portal_id, &owner_token, "req-2");
    second.output_text = "final-value".to_string();
    second.logical_unit_id = Some("unit-2".to_string());
    second.coalesce_key = Some("status".to_string());
    let r2 = authority.handle_publish_output(second, "caller", 10);
    assert!(r2.accepted, "second append must be accepted");
    assert!(
        !r2.portal_update_ready,
        "second append must be rate-limited (same window)"
    );

    // The coalescer must have a pending entry for this portal.
    // Previously this was dropped as stale — verify the fix.
    assert_eq!(
        authority.next_due_projection_id().as_deref(),
        Some(portal_id),
        "portal must re-appear in oracle after coalesce-key in-place update (hud-endkj defect 1)"
    );

    // And drain at the next window to confirm final value is delivered.
    let update2 = authority
        .take_due_portal_update(portal_id, 10 + PORTAL_UPDATE_RATE_WINDOW_WALL_US)
        .expect("no error")
        .expect("second update must be due in next window");
    let last_text = update2
        .visible_transcript
        .iter()
        .find(|u| u.coalesce_key.as_deref() == Some("status"))
        .map(|u| u.output_text.as_str())
        .unwrap_or("");
    assert_eq!(
        last_text, "final-value",
        "coalesced final value must be delivered (hud-endkj defect 1)"
    );
}

/// Regression: multiple coalesce-key in-place updates within the same rate
/// window must ALL produce strictly-increasing coalescer sequences, so each
/// successive in-place update remains visible via next_due_projection_id.
#[test]
fn coalesce_key_successive_in_place_updates_stay_in_oracle() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        max_portal_updates_per_second: 1,
        ..ProjectionBounds::default()
    })
    .unwrap();
    let owner_token = attach(&mut authority, "proj-multi");
    let portal_id = "proj-multi";

    // Seed with a first ready append.
    let mut seed = output_request(portal_id, &owner_token, "req-seed");
    seed.output_text = "seed".to_string();
    seed.logical_unit_id = Some("unit-seed".to_string());
    seed.coalesce_key = Some("progress".to_string());
    authority.handle_publish_output(seed, "caller", 5);
    authority
        .take_due_portal_update(portal_id, 5)
        .expect("no error")
        .expect("seed must be drained");

    // Three successive in-place updates within the same rate window.
    for i in 1u64..=3u64 {
        let mut req = output_request(portal_id, &owner_token, &format!("req-{i}"));
        req.output_text = format!("progress-{i}");
        req.logical_unit_id = None;
        req.coalesce_key = Some("progress".to_string());
        let r = authority.handle_publish_output(req, "caller", 5);
        assert!(r.accepted);
        // After each in-place update the oracle must show this portal.
        assert_eq!(
            authority.next_due_projection_id().as_deref(),
            Some(portal_id),
            "oracle must show portal after in-place update {i} (hud-endkj defect 1)"
        );
        // Peek back into the oracle without consuming (internal rotation).
        // Re-insert by querying once — this is fine; the VecDeque rotates.
    }

    // Final drain should deliver the last in-place value.
    let final_update = authority
        .take_due_portal_update(portal_id, 5 + PORTAL_UPDATE_RATE_WINDOW_WALL_US)
        .expect("no error")
        .expect("final update must be present");
    let last_text = final_update
        .visible_transcript
        .iter()
        .find(|u| u.coalesce_key.as_deref() == Some("progress"))
        .map(|u| u.output_text.as_str())
        .unwrap_or("");
    assert_eq!(
        last_text, "progress-3",
        "latest coalesced value must be delivered"
    );
}

/// Regression (defect 2): portal_next_due_at_us returns Some when the portal
/// is rate-limited, giving a wait hint to avoid busy-spinning.
#[test]
fn portal_next_due_at_us_returns_wait_hint_when_rate_limited() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        max_portal_updates_per_second: 1,
        ..ProjectionBounds::default()
    })
    .unwrap();
    let owner_token = attach(&mut authority, "proj-rate");
    let portal_id = "proj-rate";

    // First append (rate window open at t=100).
    let mut first = output_request(portal_id, &owner_token, "req-1");
    first.output_text = "first".to_string();
    first.logical_unit_id = Some("unit-1".to_string());
    authority.handle_publish_output(first, "caller", 100);

    // Drain the first update.
    authority
        .take_due_portal_update(portal_id, 100)
        .expect("no error")
        .expect("first update must be present");

    // Second append at the same timestamp → rate-limited, coalescer entry pending.
    let mut second = output_request(portal_id, &owner_token, "req-2");
    second.output_text = "second".to_string();
    second.logical_unit_id = Some("unit-2".to_string());
    authority.handle_publish_output(second, "caller", 100);

    // next_due_projection_id returns the portal (coalescer has a pending entry).
    assert_eq!(
        authority.next_due_projection_id().as_deref(),
        Some(portal_id),
        "oracle must show portal when coalescer entry is pending"
    );

    // take_due_portal_update returns Ok(None) (rate window not elapsed).
    let result = authority.take_due_portal_update(portal_id, 100);
    assert!(
        matches!(result, Ok(None)),
        "take_due must return Ok(None) when rate window has not elapsed"
    );

    // portal_next_due_at_us must return Some to prevent busy-spin.
    let next_due = authority.portal_next_due_at_us(portal_id, 100);
    assert!(
        next_due.is_some(),
        "portal_next_due_at_us must return Some when portal is rate-limited (hud-endkj defect 2)"
    );
    let next_due_us = next_due.unwrap();
    assert!(
        next_due_us > 100,
        "next_due must be in the future; got {next_due_us}"
    );
    assert_eq!(
        next_due_us,
        100 + PORTAL_UPDATE_RATE_WINDOW_WALL_US,
        "next_due must equal rate window start + window duration"
    );

    // After the rate window elapses, portal_next_due_at_us returns None.
    let after_window = authority.portal_next_due_at_us(portal_id, next_due_us);
    assert!(
        after_window.is_none(),
        "portal_next_due_at_us must return None after rate window elapses"
    );

    // And take_due_portal_update now succeeds.
    let update = authority
        .take_due_portal_update(portal_id, next_due_us)
        .expect("no error")
        .expect("update must be present after rate window elapses");
    assert_eq!(update.unread_output_count, 1);
}

/// portal_next_due_at_us returns None for an unknown projection.
#[test]
fn portal_next_due_at_us_returns_none_for_unknown_projection() {
    let authority = ProjectionAuthority::default();
    assert!(
        authority
            .portal_next_due_at_us("does-not-exist", 1_000_000)
            .is_none(),
        "must return None for unknown projection"
    );
}

/// portal_next_due_at_us returns None immediately after a successful drain
/// (no pending coalescer entry).
#[test]
fn portal_next_due_at_us_returns_none_when_no_pending_entry() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        max_portal_updates_per_second: 1,
        ..ProjectionBounds::default()
    })
    .unwrap();
    let owner_token = attach(&mut authority, "proj-idle");
    let portal_id = "proj-idle";

    let mut req = output_request(portal_id, &owner_token, "req-1");
    req.logical_unit_id = Some("unit-1".to_string());
    authority.handle_publish_output(req, "caller", 10);
    authority
        .take_due_portal_update(portal_id, 10)
        .expect("no error")
        .expect("must drain");

    // No pending coalescer entry → returns None.
    assert!(
        authority.portal_next_due_at_us(portal_id, 10).is_none(),
        "portal_next_due_at_us must return None when no coalescer entry is pending"
    );
}

// ── portal-disconnect-resume-ux §2/§3: connection_degraded signal ────────────

/// §3: a session marked HUD-unavailable exposes `connection_degraded = true`
/// on its projected state (computed from the lifecycle transition the authority
/// already owns — no new timer authority introduced).
#[test]
fn projected_portal_state_sets_connection_degraded_when_hud_unavailable() {
    let mut authority = ProjectionAuthority::default();
    authority.handle_attach(attach_request("projection-a", "req-a"), "caller-a", 10);

    let live = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("attach creates portal state");
    assert!(
        !live.connection_degraded,
        "a freshly attached portal is not degraded"
    );

    authority.mark_hud_disconnected("projection-a", 30).unwrap();

    let degraded = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("portal state remains materializable when degraded");
    assert!(
        degraded.connection_degraded,
        "§3: HUD-unavailable session must set connection_degraded"
    );
}

/// §2: `connection_degraded` is computed independently of viewer redaction, so a
/// restricted viewer (default fail-closed policy → lifecycle_state None, redacted
/// true) still learns the portal is disconnected without any content leak.
#[test]
fn connection_degraded_survives_redaction() {
    let mut authority = ProjectionAuthority::default();
    authority.handle_attach(attach_request("projection-a", "req-a"), "caller-a", 10);
    authority.mark_hud_disconnected("projection-a", 30).unwrap();

    // Fail-closed default policy: redacts identity/lifecycle/transcript.
    let redacted = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::default())
        .expect("portal state materializes under restrictive policy");

    assert!(
        redacted.redacted,
        "restrictive policy must redact the portal"
    );
    assert!(
        redacted.lifecycle_state.is_none(),
        "lifecycle spelling must be redaction-gated to None"
    );
    assert!(
        redacted.connection_degraded,
        "§2: connection_degraded must survive redaction (geometry-only signal)"
    );
}

/// §2: a degraded portal is forced non-interactive even under an input-permitting
/// policy — the surface must not present an active-stream affordance.
#[test]
fn degraded_state_forces_interaction_disabled() {
    let mut authority = ProjectionAuthority::default();
    authority.handle_attach(attach_request("projection-a", "req-a"), "caller-a", 10);

    let live = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("attach creates portal state");
    assert!(
        live.interaction_enabled,
        "a live expanded portal with permissive policy is interactive"
    );

    authority.mark_hud_disconnected("projection-a", 30).unwrap();
    let degraded = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("portal state remains materializable when degraded");
    assert!(
        !degraded.interaction_enabled,
        "§2: degraded portal must be non-interactive even with permissive policy"
    );
}

/// §2/§3 regression: the stale/degraded latch must stay set until the HUD
/// genuinely reconnects. Owner publishes are accepted while the HUD is gone and
/// promote the lifecycle `HudUnavailable -> Active`; if `connection_degraded`
/// were keyed off `lifecycle_state`, that owner traffic would silently clear the
/// stale treatment (un-dim, drop the disconnect marker, re-enable input) in the
/// orphan/grace window even though `authorize_portal_republish` still fails. The
/// latch is keyed off `hud_connection`/`last_disconnect_wall_us` so only a real
/// `record_hud_connection` clears it.
#[test]
fn connection_degraded_latches_until_real_reconnect_not_owner_traffic() {
    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");

    authority.mark_hud_disconnected("projection-a", 30).unwrap();
    assert!(
        authority
            .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
            .unwrap()
            .connection_degraded,
        "portal is degraded immediately after HUD disconnect"
    );

    // Owner publishes more output while the HUD is still gone. This promotes the
    // lifecycle back to Active but must NOT clear the connection-degraded latch.
    let accepted = authority.handle_publish_output(
        output_request("projection-a", &owner_token, "pub-after-disconnect"),
        "caller-a",
        40,
    );
    assert!(
        accepted.accepted,
        "owner publish is accepted even while the HUD is disconnected"
    );
    let still_degraded = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("portal state remains materializable while disconnected");
    assert!(
        still_degraded.connection_degraded,
        "§3: owner traffic must NOT clear the stale latch while the HUD is gone"
    );
    assert!(
        !still_degraded.interaction_enabled,
        "§2: input stays disabled until the HUD actually reconnects"
    );

    // A genuine HUD reconnect clears the latch.
    authority
        .record_hud_connection("projection-a", connection_metadata(&[]))
        .unwrap();
    let recovered = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("portal state materializes after reconnect");
    assert!(
        !recovered.connection_degraded,
        "real reconnect clears the connection-degraded latch"
    );
}

// ── portal-disconnect-resume-ux §4: reconnect and resume presentation ─────────
//
// §2/§3 (disconnect + stale degradation) landed in #878. This block locks the
// §4 reconnect-resume contract through the projected-state path the compositor
// actually consumes (`projected_portal_state`), not just the audit summary. The
// production behavior is already structural: the `connection_degraded` latch is
// derived from the HUD connection bookkeeping, the retained window is never
// cleared on disconnect, and the coalesce-key / logical_unit_id machinery is
// reconnect-agnostic. These tests pin that the spec scenarios hold end-to-end so
// a future change to disconnect/resume cannot silently regress resume continuity.

/// Build an owner publish with explicit identity keys, since the shared
/// `output_request` fixture hardcodes `logical_unit_id = "unit-1"` and
/// `coalesce_key = None`. Used by the §4 continuity tests.
fn output_request_keyed(
    projection_id: &str,
    owner_token: &str,
    request_id: &str,
    output_text: &str,
    logical_unit_id: Option<&str>,
    coalesce_key: Option<&str>,
) -> PublishOutputRequest {
    PublishOutputRequest {
        envelope: envelope(
            ProjectionOperation::PublishOutput,
            projection_id,
            request_id,
        ),
        owner_token: owner_token.to_string(),
        output_text: output_text.to_string(),
        output_kind: OutputKind::Assistant,
        content_classification: ContentClassification::Private,
        logical_unit_id: logical_unit_id.map(str::to_string),
        coalesce_key: coalesce_key.map(str::to_string),
        expects_reply: false,
    }
}

/// §4.1: a reconnect before grace expiry resumes from the authority-preserved
/// retained window AND clears the degraded/stale treatment in the same projected
/// frame the compositor consumes — the surface goes degraded → live, the
/// committed transcript unit is still materialized, and input is re-enabled.
#[test]
fn reconnect_resumes_from_retained_window_and_clears_stale_treatment() {
    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");

    // Commit a transcript unit, then connect and disconnect the HUD.
    authority
        .handle_publish_output(
            output_request_keyed(
                "projection-a",
                &owner_token,
                "req-pre",
                "committed before drop",
                Some("unit-pre"),
                None,
            ),
            "caller-a",
            20,
        )
        .accepted
        .then_some(())
        .expect("pre-disconnect publish accepted");
    authority
        .record_hud_connection("projection-a", connection_metadata(&["create_tiles"]))
        .unwrap();
    authority.mark_hud_disconnected("projection-a", 30).unwrap();

    let degraded = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("degraded portal state materializes");
    assert!(
        degraded.connection_degraded,
        "portal is degraded while the HUD is gone"
    );
    assert!(
        !degraded.interaction_enabled,
        "input is disabled while degraded"
    );
    // The retained window is preserved across the disconnect (no committed loss).
    assert_eq!(
        degraded.visible_transcript.len(),
        1,
        "§4.1: committed unit is retained through the disconnect"
    );

    // Genuine reconnect before grace.
    let mut reconnected = connection_metadata(&["create_tiles"]);
    reconnected.connection_id = "connection-resumed".to_string();
    reconnected.authenticated_session_id = "runtime-session-resumed".to_string();
    reconnected.connected_at_wall_us = 40;
    reconnected.last_reconnect_wall_us = 40;
    authority
        .record_hud_connection("projection-a", reconnected)
        .unwrap();

    let resumed = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("resumed portal state materializes");
    assert!(
        !resumed.connection_degraded,
        "§4.1: reconnect clears the degraded/stale treatment"
    );
    assert!(
        resumed.interaction_enabled,
        "§4.1: live presentation re-enables input after resume"
    );
    assert_eq!(
        resumed.visible_transcript.len(),
        1,
        "§4.1: resume preserves the already-committed retained unit"
    );
    assert_eq!(
        resumed.visible_transcript[0].output_text, "committed before drop",
        "§4.1: resume restores the exact retained content, not a fresh window"
    );
}

/// §4.2: a logical unit that was in progress at disconnect is continued after
/// reconnect by republishing it under the same `coalesce_key`. The authority
/// updates that unit in place via the existing coalesce-key path — it does NOT
/// render the continuation as a duplicate transcript unit. The rate limit is
/// pinned to 1/window so the continuation deterministically coalesces.
#[test]
fn reconnect_continues_in_progress_unit_in_place_via_coalesce_key() {
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        max_portal_updates_per_second: 1,
        ..ProjectionBounds::default()
    })
    .unwrap();
    let owner_token = attach(&mut authority, "projection-a");

    // First publish of the in-progress unit consumes the one allowed update in
    // the rate window, so the post-reconnect continuation is forced to coalesce.
    authority
        .handle_publish_output(
            output_request_keyed(
                "projection-a",
                &owner_token,
                "req-partial",
                "streaming so f",
                Some("unit-stream-a"),
                Some("coalesce-stream"),
            ),
            "caller-a",
            20,
        )
        .accepted
        .then_some(())
        .expect("partial publish accepted");
    authority
        .record_hud_connection("projection-a", connection_metadata(&["create_tiles"]))
        .unwrap();
    authority.mark_hud_disconnected("projection-a", 30).unwrap();
    authority
        .record_hud_connection("projection-a", {
            let mut m = connection_metadata(&["create_tiles"]);
            m.connection_id = "connection-resumed".to_string();
            m.authenticated_session_id = "runtime-session-resumed".to_string();
            m.connected_at_wall_us = 40;
            m.last_reconnect_wall_us = 40;
            m
        })
        .unwrap();

    // Continuation after reconnect: same coalesce_key, fresh logical_unit_id,
    // still inside the same 1s rate window (server ts 50 < 20 + 1_000_000).
    let continued = authority.handle_publish_output(
        output_request_keyed(
            "projection-a",
            &owner_token,
            "req-continue",
            "streaming so far complete",
            Some("unit-stream-b"),
            Some("coalesce-stream"),
        ),
        "caller-a",
        50,
    );
    assert!(continued.accepted, "continuation publish accepted");

    let window = authority
        .visible_transcript_window("projection-a")
        .expect("portal has a transcript window");
    assert_eq!(
        window.len(),
        1,
        "§4.2: coalesce-key continuation updates in place, not as a duplicate unit"
    );
    assert_eq!(
        window[0].output_text, "streaming so far complete",
        "§4.2: the in-place update carries the continued content"
    );
}

/// §4.2 (replay): a publish that replays an already-seen `logical_unit_id`
/// during/after reconnect is accepted idempotently — it does not append or
/// mutate the transcript. Resume does not redefine `logical_unit_id` semantics.
#[test]
fn reconnect_replayed_logical_unit_id_stays_idempotent() {
    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");

    authority
        .handle_publish_output(
            output_request_keyed(
                "projection-a",
                &owner_token,
                "req-orig",
                "original committed unit",
                Some("unit-replayed"),
                None,
            ),
            "caller-a",
            20,
        )
        .accepted
        .then_some(())
        .expect("original publish accepted");
    authority
        .record_hud_connection("projection-a", connection_metadata(&["create_tiles"]))
        .unwrap();
    authority.mark_hud_disconnected("projection-a", 30).unwrap();
    authority
        .record_hud_connection("projection-a", {
            let mut m = connection_metadata(&["create_tiles"]);
            m.connection_id = "connection-resumed".to_string();
            m.authenticated_session_id = "runtime-session-resumed".to_string();
            m.connected_at_wall_us = 40;
            m.last_reconnect_wall_us = 40;
            m
        })
        .unwrap();

    let before = authority
        .visible_transcript_window("projection-a")
        .expect("window exists before replay");
    assert_eq!(before.len(), 1);

    // Replay the same logical_unit_id with DIFFERENT text after reconnect: the
    // authority must drop it idempotently without appending or mutating.
    let replay = authority.handle_publish_output(
        output_request_keyed(
            "projection-a",
            &owner_token,
            "req-replay",
            "MUTATED replay text that must be ignored",
            Some("unit-replayed"),
            None,
        ),
        "caller-a",
        50,
    );
    assert!(replay.accepted, "replay accepted at the protocol level");
    assert!(
        replay.status_summary.contains("idempotently"),
        "§4.2: replayed logical_unit_id is accepted idempotently"
    );

    let after = authority
        .visible_transcript_window("projection-a")
        .expect("window exists after replay");
    assert_eq!(
        after.len(),
        1,
        "§4.2: idempotent replay does not append a duplicate unit"
    );
    assert_eq!(
        after[0].output_text, "original committed unit",
        "§4.2: idempotent replay does not mutate the retained unit in place"
    );
}

/// §4.4: when retained history exceeds the visible viewport, resume materializes
/// only the bounded visible window into scene nodes — it does not reconstruct
/// full transcript history. Bounds are tuned so retained > visible.
#[test]
fn reconnect_materializes_only_bounded_visible_window() {
    // Each unit's byte_len is dominated by output_text. Use a small visible
    // budget so only the most recent unit fits the visible window while the
    // retained window keeps several.
    let mut authority = ProjectionAuthority::new(ProjectionBounds {
        max_retained_transcript_bytes: 4096,
        max_visible_transcript_bytes: 32,
        ..ProjectionBounds::default()
    })
    .unwrap();
    let owner_token = attach(&mut authority, "projection-a");

    for i in 0..5u32 {
        authority
            .handle_publish_output(
                output_request_keyed(
                    "projection-a",
                    &owner_token,
                    &format!("req-{i}"),
                    "0123456789abcdef0123", // 20 bytes of text
                    Some(&format!("unit-{i}")),
                    None,
                ),
                "caller-a",
                20 + u64::from(i),
            )
            .accepted
            .then_some(())
            .expect("publish accepted");
    }
    authority
        .record_hud_connection("projection-a", connection_metadata(&["create_tiles"]))
        .unwrap();
    authority.mark_hud_disconnected("projection-a", 40).unwrap();
    authority
        .record_hud_connection("projection-a", {
            let mut m = connection_metadata(&["create_tiles"]);
            m.connection_id = "connection-resumed".to_string();
            m.authenticated_session_id = "runtime-session-resumed".to_string();
            m.connected_at_wall_us = 50;
            m.last_reconnect_wall_us = 50;
            m
        })
        .unwrap();

    let summary = authority.state_summary("projection-a").unwrap();
    let resumed = authority
        .projected_portal_state("projection-a", &ProjectedPortalPolicy::permit_all())
        .expect("resumed portal state materializes");

    assert!(
        summary.retained_transcript_units > resumed.visible_transcript.len(),
        "§4.4: retained history ({}) must exceed the materialized visible window ({})",
        summary.retained_transcript_units,
        resumed.visible_transcript.len()
    );
    let materialized_bytes: usize = resumed
        .visible_transcript
        .iter()
        .map(TranscriptUnit::byte_len)
        .sum();
    assert!(
        materialized_bytes <= 32,
        "§4.4: only the bounded visible window is materialized into scene nodes"
    );
}

/// §4.7: every frame of the stale → live transition respects the current
/// viewer's redaction policy. A viewer permitted identity/lifecycle but NOT
/// transcript never sees transcript content materialized at any transition
/// frame: degraded (stale), resumed (live), or live after a post-resume publish.
#[test]
fn stale_to_live_transition_respects_redaction_every_frame() {
    let mut authority = ProjectionAuthority::default();
    let owner_token = attach(&mut authority, "projection-a");

    // Viewer policy: reveal identity + lifecycle, redact transcript.
    let transcript_restricted = ProjectedPortalPolicy {
        reveal_transcript: false,
        ..ProjectedPortalPolicy::permit_all()
    };

    let assert_no_transcript = |authority: &ProjectionAuthority, frame: &str| {
        let state = authority
            .projected_portal_state("projection-a", &transcript_restricted)
            .unwrap_or_else(|| panic!("portal state materializes at frame: {frame}"));
        assert!(
            state.visible_transcript.is_empty(),
            "§4.7: no transcript content may materialize for a restricted viewer at frame: {frame}"
        );
    };

    authority
        .handle_publish_output(
            output_request_keyed(
                "projection-a",
                &owner_token,
                "req-secret",
                "secret transcript content",
                Some("unit-secret"),
                None,
            ),
            "caller-a",
            20,
        )
        .accepted
        .then_some(())
        .expect("publish accepted");
    authority
        .record_hud_connection("projection-a", connection_metadata(&["create_tiles"]))
        .unwrap();

    // Frame 1: live before drop.
    assert_no_transcript(&authority, "live-before-drop");

    // Frame 2: degraded/stale.
    authority.mark_hud_disconnected("projection-a", 30).unwrap();
    let degraded = authority
        .projected_portal_state("projection-a", &transcript_restricted)
        .expect("degraded state materializes");
    assert!(
        degraded.connection_degraded,
        "the disconnect geometry signal still reaches the restricted viewer"
    );
    assert_no_transcript(&authority, "degraded");

    // Frame 3: resumed/live after reconnect.
    authority
        .record_hud_connection("projection-a", {
            let mut m = connection_metadata(&["create_tiles"]);
            m.connection_id = "connection-resumed".to_string();
            m.authenticated_session_id = "runtime-session-resumed".to_string();
            m.connected_at_wall_us = 40;
            m.last_reconnect_wall_us = 40;
            m
        })
        .unwrap();
    assert_no_transcript(&authority, "resumed");

    // Frame 4: a post-resume publish must not flash content for the restricted viewer.
    authority
        .handle_publish_output(
            output_request_keyed(
                "projection-a",
                &owner_token,
                "req-post-resume",
                "more secret content after resume",
                Some("unit-secret-2"),
                None,
            ),
            "caller-a",
            50,
        )
        .accepted
        .then_some(())
        .expect("post-resume publish accepted");
    assert_no_transcript(&authority, "post-resume-publish");
}
