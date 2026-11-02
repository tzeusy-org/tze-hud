//! Local composer editing and transactional delivery into the portal hub.
use tze_hud_input::{
    ComposerDraft, ComposerDraftManager, ComposerOutcome, EditOutcome, MAX_DRAFT_BYTES,
};
use tze_hud_projection::{PortalHub, PortalKey, Publish};
use tze_hud_scene::SceneId;

fn focused() -> ComposerDraftManager {
    let mut drafts = ComposerDraftManager::new();
    drafts.on_focus_gained(SceneId::new(), false);
    drafts
}
fn hub() -> (PortalHub, PortalKey) {
    let mut hub = PortalHub::default();
    let key = PortalKey::new("composer-agent", "p");
    hub.publish(&key, Publish::default(), 0).unwrap();
    (hub, key)
}
fn submit(drafts: &mut ComposerDraftManager) -> String {
    let (handled, outcome) = drafts.route_key_down("Enter", "Enter", false, false, false);
    assert!(handled);
    submitted(outcome)
}
fn submitted(outcome: ComposerOutcome) -> String {
    match outcome {
        ComposerOutcome::Submitted(text) => text,
        other => panic!("expected submitted text, got {other:?}"),
    }
}

#[test]
fn local_echo_does_not_depend_on_adapter_round_trip() {
    let mut drafts = focused();
    let (hub, key) = hub();
    for ch in "hello".chars() {
        let outcome = drafts.route_character(&ch.to_string());
        assert_eq!(outcome, EditOutcome::Mutated);
        assert!(drafts.draft().unwrap().text().ends_with(ch));
        assert!(hub.get(&key).unwrap().inputs.is_empty());
    }
    assert_eq!(drafts.draft().unwrap().text(), "hello");
}

#[test]
fn word_backspace_removes_preceding_word_and_trailing_space() {
    let mut drafts = focused();
    drafts.route_character("the quick brown fox");
    for expected in ["the quick brown ", "the quick "] {
        let (handled, outcome) =
            drafts.route_key_down("Backspace", "Backspace", false, true, false);
        assert!(handled);
        assert_eq!(outcome, ComposerOutcome::Edited);
        let draft = drafts.draft().unwrap();
        assert_eq!(draft.text(), expected);
        assert_eq!(draft.cursor(), expected.len());
    }
}

#[test]
fn oversized_paste_does_not_forward_overflow_bytes() {
    let mut draft = ComposerDraft::new(8);
    draft.paste(&"é".repeat(100));
    assert_eq!(draft.text(), "éééé");
    let (mut hub, key) = hub();
    let submission = submitted(draft.submit());
    hub.submit_reply(&key, submission, 1).unwrap();
    let portal = hub.get(&key).unwrap();
    assert_eq!(portal.inputs.back().unwrap().text, "éééé");
    assert_eq!(portal.replies.back().unwrap().text, "éééé");
    assert!(draft.text().is_empty());
}

#[test]
fn submission_carries_exact_draft_text() {
    let mut drafts = focused();
    let (mut hub, key) = hub();
    let expected = "submit this exact 日本語 content";
    drafts.route_character(expected);
    let batch = submit(&mut drafts);
    hub.submit_reply(&key, batch, 1).unwrap();
    let portal = hub.get(&key).unwrap();
    assert_eq!(portal.inputs.back().unwrap().text, expected);
    assert_eq!(portal.replies.back().unwrap().text, expected);
    assert!(drafts.draft().unwrap().text().is_empty());
    assert_eq!(portal.unread, 0, "viewer echo is not unread output");
}

#[test]
fn draft_suspends_under_safe_mode() {
    let mut drafts = focused();
    drafts.route_character("text before safe mode");
    drafts.set_suspended(true);
    assert_eq!(drafts.route_character("blocked"), EditOutcome::Suspended);
    for key in ["Backspace", "Delete", "Enter", "Escape"] {
        let (_, outcome) = drafts.route_key_down(key, key, false, false, false);
        assert_eq!(outcome, ComposerOutcome::Unchanged);
        assert_eq!(drafts.draft().unwrap().text(), "text before safe mode");
    }
    assert_eq!(drafts.draft().unwrap().cursor(), 21);
    drafts.set_suspended(false);
    assert_eq!(submit(&mut drafts), "text before safe mode");
}

#[test]
fn safe_mode_gate_suppresses_submission_and_delivery() {
    let mut drafts = ComposerDraftManager::new();
    drafts.on_focus_gained(SceneId::new(), true);
    let (mut hub, key) = hub();
    assert_eq!(drafts.route_character("blocked"), EditOutcome::Suspended);
    let (_, outcome) = drafts.route_key_down("Enter", "Enter", false, false, false);
    assert_eq!(outcome, ComposerOutcome::Unchanged);
    if let ComposerOutcome::Submitted(submission) = outcome {
        hub.submit_reply(&key, submission, 1).unwrap();
    }
    assert!(hub.get(&key).unwrap().inputs.is_empty());
}

#[test]
fn submission_is_transactional_not_coalescible() {
    let mut drafts = focused();
    let (mut hub, key) = hub();
    for (at, text) in [(1, "first reply"), (2, "second reply")] {
        drafts.route_character(text);
        let batch = submit(&mut drafts);
        hub.submit_reply(&key, batch, at).unwrap();
    }
    assert_eq!(
        hub.get(&key)
            .unwrap()
            .inputs
            .iter()
            .map(|i| i.text.as_str())
            .collect::<Vec<_>>(),
        ["first reply", "second reply"]
    );
}

#[test]
fn navigation_and_delete_keystrokes_remain_local() {
    let mut drafts = focused();
    drafts.route_character("hello world");
    drafts.route_key_down("Backspace", "Backspace", false, true, false);
    drafts.route_key_down("Home", "Home", false, false, false);
    drafts.route_key_down("Delete", "Delete", false, false, false);
    drafts.route_character("H");
    assert_eq!(drafts.draft().unwrap().text(), "Hello ");
    let batch = submit(&mut drafts);
    assert_eq!(batch, "Hello ");
}

#[test]
fn draft_never_exceeds_max_bytes_on_direct_cap() {
    let mut draft = ComposerDraft::new(MAX_DRAFT_BYTES);
    assert_eq!(
        draft.paste(&"a".repeat(MAX_DRAFT_BYTES)),
        EditOutcome::Mutated
    );
    assert_eq!(draft.insert("x"), EditOutcome::AtCapacity);
    assert_eq!(draft.text().len(), MAX_DRAFT_BYTES);
}
