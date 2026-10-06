# Owner-qualified repair surface reconciliation

Source main `398c6f91de6e0d4883b10a41b6c63044e6192730`. Raw current references accompany this table; name-only hits are not caller proof. Out-of-line cfg(test) modules and declaration attributes are manually resolved.

## R: 6 original diagnostic groups

| Group | Current disposition / actual ownership |
|---|---|
| 1 | Refcount increment/decrement/read island removed; SceneGraph resource_ref_count is a distinct fixture owner. |
| 2 | RuntimeWidgetStore put_svg, asset_count, contains and PutOutcome are private cfg(test) persisted-startup/budget fixtures; total_bytes_used removed. open/reindex/enforce_budgets remain production. Current declaration: put_svg: crates/tze_hud_resource/src/runtime_widget_store.rs:121; asset_count: crates/tze_hud_resource/src/runtime_widget_store.rs:196; contains: crates/tze_hud_resource/src/dedup.rs:100; PutOutcome: crates/tze_hud_resource/src/runtime_widget_store.rs:43 |
| 3 | write_atomic is private cfg(test), called by retained persisted-asset fixtures only. Current declaration: write_atomic: crates/tze_hud_resource/src/runtime_widget_store.rs:349 |
| 4 | sync_parent_dir is private cfg(test), called by those same persisted fixtures only. Current declaration: sync_parent_dir: crates/tze_hud_resource/src/runtime_widget_store.rs:372 |
| 5 | ResourceStore dedup_index/abort_upload/in_flight_count use existing test-support/self-dev convention; eight external store_behavior gates are actual users. Production abort_all_uploads remains. Current declaration: dedup_index: crates/tze_hud_resource/src/upload.rs:175; abort_upload: crates/tze_hud_resource/src/upload.rs:446; in_flight_count: crates/tze_hud_resource/src/upload.rs:461 |
| 6 | validate_upload removed; actual complete_upload admission/decode/storage remains. |

## P: 20 original diagnostic groups

| Group | Current disposition / actual ownership |
|---|---|
| 1 | Obsolete authenticate_session_init removed; identify_session/evaluate_auth_credential remain live. |
| 2 | T7 exception, no production-caller claim: peer-class outbound portal converter. Current declaration: scene_portal_peer_class_to_proto: crates/tze_hud_protocol/src/convert.rs:1040 |
| 3 | T7 exception, no production-caller claim: lifecycle outbound portal converter. Current declaration: scene_portal_lifecycle_to_proto: crates/tze_hud_protocol/src/convert.rs:1084 |
| 4 | T7 exception, no production-caller claim: display-state outbound portal converter. Current declaration: scene_portal_display_state_to_proto: crates/tze_hud_protocol/src/convert.rs:1132 |
| 5 | T7 exception, no production-caller claim: part-kind outbound portal converter. Current declaration: scene_portal_part_kind_to_proto: crates/tze_hud_protocol/src/convert.rs:1171 |
| 6 | T7 exception, no production-caller claim: complete outbound portal converter and its fixture. Current declaration: scene_portal_surface_to_proto: crates/tze_hud_protocol/src/convert.rs:1243 |
| 7 | cfg(test) node-layout fixture converter; live inbound node-layout decoding retained. Current declaration: scene_node_layout_to_proto: crates/tze_hud_protocol/src/convert.rs:226 |
| 8 | cfg(test) ResourceId outbound fixture converter; actual resource replies construct raw IDs directly. Current declaration: resource_id_to_proto: crates/tze_hud_protocol/src/convert.rs:27 |
| 9 | existing dev-mode external fuzz/convert fixture, different owner from scene ResourceId and live image decoding. Current declaration: proto_to_resource_id: crates/tze_hud_protocol/src/convert.rs:37 |
| 10 | cfg(test) text-overflow outbound fixture; real inbound decoding retained. Current declaration: scene_text_overflow_to_proto: crates/tze_hud_protocol/src/convert.rs:456 |
| 11 | cfg(test) color-run outbound fixture; real inbound decoding retained. Current declaration: scene_color_runs_to_proto: crates/tze_hud_protocol/src/convert.rs:465 |
| 12 | cfg(test) input-mode outbound fixture; real inbound decoding retained. Current declaration: scene_input_mode_to_proto: crates/tze_hud_protocol/src/convert.rs:494 |
| 13 | cfg(test) scene_node_to_proto used by existing wire/session fixtures, never claimed a production serializer. Current declaration: scene_node_to_proto: crates/tze_hud_protocol/src/convert.rs:591 |
| 14 | cfg(test) nested outbound node-tree fixture; live inbound subtree decoder retained. Current declaration: scene_node_tree_to_proto: crates/tze_hud_protocol/src/convert.rs:699 |
| 15 | cfg(test) scene ID outbound fixture; live scene_id_to_bytes is a different helper. Current declaration: scene_id_to_proto: crates/tze_hud_protocol/src/convert.rs:12 |
| 16 | get_session/get_session_mut removed; session_count is existing dev-mode/test fixture only. Current declaration: session_count: crates/tze_hud_protocol/src/session.rs:246 |
| 17 | Unused allows_mutations removed; live lifecycle/permission checks retained. |
| 18 | inject_input_event cfg(test) actual channel fixtures; broadcast_frame_presented removed; emit_drag_repositioned_event existing dev-mode separate-integration fixture. Current declaration: inject_input_event: crates/tze_hud_protocol/src/session_server/service.rs:213; emit_drag_repositioned_event: crates/tze_hud_protocol/src/session_server/service.rs:320 |
| 19 | Unused subscription build_event_batch_message removed; live filtering retained. |
| 20 | evict_expired private cfg(test); actual consume validates/removes expiry with explicit now_ms. Current declaration: evict_expired: crates/tze_hud_protocol/src/token.rs:153 |

## S: 23 original diagnostic groups

| Group | Current disposition / actual ownership |
|---|---|
| 1 | MAX_TILES_PER_LEASE removed. suspend/resume test-support real MCP/T7/lease fixtures; expire_lease cfg(test); real runtime safe-mode uses bulk suspend/resume operations. Current declaration: suspend_lease: crates/tze_hud_scene/src/graph/leases.rs:206; resume_lease: crates/tze_hud_scene/src/graph/leases.rs:218; expire_lease: crates/tze_hud_scene/src/graph/leases.rs:374 |
| 2 | clear_tile_font_scale cfg(test) existing round-trip fixture; live set/query retained. Current declaration: clear_tile_font_scale: crates/tze_hud_scene/src/graph/overlay.rs:376 |
| 3 | Hover/pressed/focused/drag helper methods are existing test-support hit/render fixtures; live production local feedback writes remain different methods. Current declaration: update_hover_state: crates/tze_hud_scene/src/graph/queries.rs:338; update_pressed_state: crates/tze_hud_scene/src/graph/queries.rs:371; update_focused_state: crates/tze_hud_scene/src/graph/queries.rs:379; set_drag_active: crates/tze_hud_scene/src/graph/queries.rs:400; clear_drag_active: crates/tze_hud_scene/src/graph/queries.rs:406 |
| 4 | SceneGraph.resource_ref_count cfg(test) scene ownership/eviction fixtures, distinct from removed ResourceRecord counter. Current declaration: resource_ref_count: crates/tze_hud_scene/src/graph/resources.rs:28 |
| 5 | SceneGraph.from_json test-support lifecycle/snapshot fixtures; live serialized snapshot shape unchanged. Current declaration: from_json: crates/tze_hud_scene/src/graph/snapshot.rs:13 |
| 6 | Lease-wrapped tab fixture methods cfg(test); live manage_tabs boundary remains. Current declaration: create_tab_with_lease: crates/tze_hud_scene/src/graph/tabs.rs:23; switch_active_tab_with_lease: crates/tze_hud_scene/src/graph/tabs.rs:95 |
| 7 | create_tile_checked test-support lease fixtures; update_node_content test-support pixel/T7 fixtures; live mutation path retained. Current declaration: create_tile_checked: crates/tze_hud_scene/src/graph/tiles.rs:29; update_node_content: crates/tze_hud_scene/src/graph/tiles.rs:662 |
| 8 | unregister_zone cfg(test); breakpoints and no-lease publish wrappers test-support; production lease-aware publish paths retained. Current declaration: unregister_zone: crates/tze_hud_scene/src/graph/zone_ops.rs:110; publish_to_zone_with_breakpoints: crates/tze_hud_scene/src/graph/zone_ops.rs:408; publish_to_widget: crates/tze_hud_scene/src/graph/zone_ops.rs:458 |
| 9 | MutationResult.budget_warning retains current return/fixture contract. Production producers call is_lease_budget_warning; only graph/tests.rs:3026,3132 read the field. No production reader is asserted. |
| 10 | TRANSACTION_VALIDATION_BUDGET_US cfg(test)/test-support for actual vertical_slice budget_assertions.rs; constant declaration attributes are manually qualified. |
| 11 | DurationUs and after_wall/after_mono island removed; real WallUs/MonoUs remain. |
| 12 | Dead Schedule helpers/model removed; live present_at scheduling remains BatchTimingHints/protobuf fields. |
| 13 | Dead scene TimingHints helpers/model removed; generated protocol TimingHints remains a different owner. |
| 14 | HitResult.node_hit_ids test-support existing hit-test fixture. Current declaration: node_hit_ids: crates/tze_hud_scene/src/types.rs:1066 |
| 15 | Removed only unconstructed CapsError MaxTilesPerLeaseExceeded/MaxNodesPerTileExceeded variants. |
| 16 | Removed only LeaseError LeaseNotActive/BudgetExceeded variants. Live ValidationError, ResourceError and BudgetError same-spelling variants remain. |
| 17 | geometry_policy_to_absolute_rect test-support separate movable-elements integration fixture; live geometry_policy_to_proto retained in protocol events. Current declaration: geometry_policy_to_absolute_rect: crates/tze_hud_scene/src/types.rs:1583 |
| 18 | Unused unenforced MAX_ACTION_LABEL_LEN removed; renderer/schema behavior unchanged. |
| 19 | WidgetRegistry.get_instance/active_for_widget test-support widget/cleanup fixtures. Out-of-line graph test modules are cfg(test), including new widget_tests/test_helpers owners. Current declaration: get_instance: crates/tze_hud_scene/src/types.rs:2518; active_for_widget: crates/tze_hud_scene/src/types.rs:2542 |
| 20 | SceneGraphSnapshot.verify_checksum/from_json test-support fixtures. to_json remains live via actual graph_snap.to_json at crates/tze_hud_protocol/src/session_server/mod.rs:280; unrelated JSON methods are not evidence. Current declaration: verify_checksum: crates/tze_hud_scene/src/types.rs:2829; to_json: crates/tze_hud_scene/src/types.rs:2838; from_json: crates/tze_hud_scene/src/graph/snapshot.rs:13 |
| 21 | ZoneRegistry zones_accepting/all_zones/get_occupancy test-support ontology fixtures. WidgetRegistry.get_occupancy is a different live graph/zone_ops.rs reader. Current declaration: zones_accepting: crates/tze_hud_scene/src/types.rs:3092; all_zones: crates/tze_hud_scene/src/types.rs:3101; get_occupancy: crates/tze_hud_scene/src/types.rs:2570 |
| 22 | PortalPartKind.is_text_bearing private cfg(test); actual T7 types/variants remain. Current declaration: is_text_bearing: crates/tze_hud_scene/src/types.rs:387 |
| 23 | BatchRejected.primary_code test-support existing atomicity/mutation fixtures; real rejection structure unchanged. Current declaration: primary_code: crates/tze_hud_scene/src/validation.rs:588 |

