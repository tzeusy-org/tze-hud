use super::*;

#[tokio::test]
async fn test_recreated_portal_tile_adopts_orphaned_durable_override() {
    // hud-08nls: after a runtime restart, an adapter republishes a portal and
    // the runtime creates its member tile with a FRESH SceneId. The durable
    // per-member override (hud-8vejp) loaded from disk is keyed by the DEAD id.
    // persist_created_tile_entries must re-home that override onto the recreated
    // tile (not orphan it) and re-lock its viewer geometry, so the portal keeps
    // its resized/moved geometry across restart even though the tile id changed.
    let (_client, _server, shared_state) = setup_test_with_state().await;
    let path = std::env::temp_dir().join(format!(
        "tze_hud_element_store_reconcile_{}.toml",
        SceneId::new()
    ));
    let _ = std::fs::remove_file(&path);

    let override_policy = GeometryPolicy::Relative {
        x_pct: 0.25,
        y_pct: 0.6,
        width_pct: 0.4,
        height_pct: 0.3,
    };
    let dead_id = SceneId::new();
    let member_z_order = 7u32;

    let recreated_id;
    {
        let mut st = shared_state.lock().await;
        st.element_store = tze_hud_scene::element_store::ElementStore::default();
        st.element_store_path = Some(path.clone());

        // The store as loaded from disk after restart: an orphaned Tile entry
        // carrying the durable override, keyed by the pre-restart (now dead) id.
        st.element_store.entries.insert(
            dead_id,
            tze_hud_scene::element_store::ElementStoreEntry {
                element_type: tze_hud_scene::element_store::ElementType::Tile,
                namespace: "agent.portal".to_string(),
                created_at: 100,
                last_published_at: 100,
                z_order: member_z_order,
                unseen_restarts: 0,
                geometry_override: Some(override_policy),
            },
        );

        // The adapter republish recreates the member tile with a fresh id.
        let mut scene = st.scene.lock().await;
        let tab_id = scene.create_tab("main", 0).expect("create tab");
        let lease = scene.grant_lease("agent.portal", 60_000);
        recreated_id = scene
            .create_tile(
                tab_id,
                "agent.portal",
                lease,
                Rect::new(0.0, 0.0, 100.0, 80.0),
                member_z_order,
            )
            .expect("create recreated portal tile");
        assert_ne!(recreated_id, dead_id, "the recreated tile has a fresh id");
    }

    let persist_request = {
        let mut st = shared_state.lock().await;
        persist_created_tile_entries(&mut st, &[recreated_id]).await
    };
    persist_element_store(persist_request).await;

    {
        let st = shared_state.lock().await;
        let entry = st
            .element_store
            .entries
            .get(&recreated_id)
            .expect("recreated tile has an element-store entry");
        assert_eq!(
            entry.geometry_override,
            Some(override_policy),
            "durable override is re-applied to the recreated tile, not orphaned"
        );
        assert!(
            !st.element_store.entries.contains_key(&dead_id),
            "the orphaned dead-id entry is consumed"
        );
        assert!(
            st.scene
                .lock()
                .await
                .is_viewer_geometry_locked(recreated_id),
            "viewer geometry is re-locked so an adapter republish cannot reposition it"
        );
    }

    // The re-homed override also survives to disk.
    let persisted = load_element_store_for_test(&path).expect("reload persisted store");
    assert_eq!(
        persisted
            .entries
            .get(&recreated_id)
            .and_then(|e| e.geometry_override),
        Some(override_policy),
        "the reconciled override is persisted under the new id"
    );
    assert!(!persisted.entries.contains_key(&dead_id));

    let _ = std::fs::remove_file(path);
}
