//! v40.50 — `AT_TOGGLE_REQ` occurrence index + reply identity.
//!
//! The defect this file pins (measured in `t_973ff960`, v40.49 G2): on a
//! pavucontrol tree with three sibling rows sharing `role`+`name`, the
//! cua-driver client resolved index 2 (bounds `1929 305 36 34`) and then
//! handed the server only the NAME. The server re-matched by accessible
//! name and took the FIRST match — index 0, the role-aggregate row that
//! `t_9f63629a` had already proven inert under both paths. The click landed
//! nowhere, and the wire reported rc=0 with a well-formed reply.
//!
//! The wire was not broken. It was **blind**: it could not say WHICH
//! occurrence, and it could not report which one it hit.
//!
//! Non-tautology: every assertion below pins WHICH element was selected
//! and WHAT the reply reports. Tests 1, 3 and 5 FAIL on the unmodified
//! v40.46 baseline (first-match selection, no identity fields on the
//! reply) and PASS here. Test 2 pins backward compatibility and must pass
//! on both. Test 4 guards the substitution class.

use niri::inject::atspi_ladder::{
    element_lookup_via_tree_dump_counted, reset_gap4_test_hooks, set_tree_dump_override, TreeNode,
};
use niri::inject::protocol::{
    encode_at_toggle_reply_resolved, encode_at_toggle_req, parse_frame, Reply, ToggleReply,
    ToggleState,
};

const ROW_NAME: &str = "Mute audio";

/// `set_tree_dump_override` is PROCESS-GLOBAL, and cargo runs the tests in
/// one binary in parallel. Without this guard, one test's `drop_tree()` can
/// yank the tree out from under another test's in-flight lookup, which shows
/// up as a flake that looks exactly like a real regression. Every test that
/// touches the tree holds this lock for its whole body.
static TREE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn install_tree() -> std::sync::MutexGuard<'static, ()> {
    let g = TREE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_tree_dump_override(Some(three_sibling_rows));
    g
}

fn drop_tree() {
    reset_gap4_test_hooks();
}

/// pavucontrol-shaped tree: THREE siblings with IDENTICAL `role` and
/// `name`, so only the occurrence index can tell them apart.
///
/// - index 0: the role aggregate row at (1968, 49)
/// - index 1: per-stream row #4852 at (1929, 169)
/// - index 2: per-stream row #7254 at (1929, 305) — the scored target
fn three_sibling_rows(_path: Option<&str>) -> Vec<TreeNode> {
    let row = |x: f64, y: f64| TreeNode {
        role: "toggle button".into(),
        name: ROW_NAME.into(),
        x,
        y,
        w: 36.0,
        h: 34.0,
        visible: true,
        children: vec![],
        ..Default::default()
    };
    vec![TreeNode {
        role: "panel".into(),
        name: "pavucontrol".into(),
        x: 0.0,
        y: 0.0,
        w: 400.0,
        h: 400.0,
        visible: true,
        children: vec![row(1968.0, 49.0), row(1929.0, 169.0), row(1929.0, 305.0)],
        ..Default::default()
    }]
}

/// v40.50 test 1 — `<nth>=2` selects the THIRD row, not the first.
///
/// FAILS on the v40.46 baseline: first-match selection returns the
/// aggregate row at (1968, 49), so `x` is 1968 and not 1929.
#[test]
fn at_toggle_req_with_index_selects_nth_match() {
    let _guard = install_tree();
    let found = element_lookup_via_tree_dump_counted(4, "toggle button", ROW_NAME, 2);
    drop_tree();

    let bounds = found
        .expect("index 2 must resolve")
        .expect("index 2 must exist among 3 candidates");
    assert_eq!(
        (bounds.rect.x, bounds.rect.y),
        (1929.0, 305.0),
        "nth=2 must select the per-stream row, NOT the index-0 role aggregate"
    );
}

/// v40.50 test 2 — an ABSENT index reproduces v40.46 behaviour exactly.
///
/// Passes before AND after by design; this pins the backward-compat claim
/// the brief makes about the optional trailing field.
#[test]
fn at_toggle_req_without_index_still_takes_first_match() {
    let _guard = install_tree();
    let found = element_lookup_via_tree_dump_counted(4, "toggle button", ROW_NAME, 0);
    drop_tree();

    let bounds = found
        .expect("index 0 must resolve")
        .expect("index 0 must exist");
    assert_eq!(
        (bounds.rect.x, bounds.rect.y),
        (1968.0, 49.0),
        "absent index must stay first-match, as v40.46 did"
    );
}

/// v40.50 test 3 — the reply reports the RESOLVED geometry, and after
/// `<nth>=2` those are the index-2 bounds, not the index-0 bounds.
///
/// FAILS on the v40.46 baseline: the reply carries no geometry at all.
#[test]
fn at_toggle_reply_reports_resolved_bounds() {
    let _guard = install_tree();
    let bounds = element_lookup_via_tree_dump_counted(4, "toggle button", ROW_NAME, 2)
        .expect("resolve")
        .expect("index 2 exists");
    drop_tree();

    let reply = ToggleReply::checked()
        .with_resolved(ToggleReply::resolved_from_bounds(&bounds, ROW_NAME));
    let wire = String::from_utf8(encode_at_toggle_reply_resolved(&reply)).expect("ascii wire");

    assert!(
        wire.contains("x=1929 y=305 w=36 h=34"),
        "reply must report the resolved index-2 geometry, got: {wire}"
    );
    assert!(
        !wire.contains("x=1968"),
        "reply must NOT report the index-0 aggregate geometry, got: {wire}"
    );
}

/// v40.50 test 4 — an out-of-range index is a DISTINCT error naming the
/// candidate count. It must NEVER silently fall back to the first match.
///
/// This is the guard on the defect class itself: a silent fall-back would
/// reproduce the original bug in a shape nobody would notice.
#[test]
fn at_toggle_index_out_of_range_errors_with_count() {
    let _guard = install_tree();
    let found = element_lookup_via_tree_dump_counted(4, "toggle button", ROW_NAME, 9);
    drop_tree();

    let candidates = found.err().expect("index 9 must NOT resolve");
    assert_eq!(candidates, 3, "the error must name the true candidate count");

    // And the error must serialize distinctly — not as a success state.
    let reply = ToggleReply::index_out_of_range(9, candidates);
    let wire = String::from_utf8(encode_at_toggle_reply_resolved(&reply)).expect("ascii wire");
    assert!(
        wire.starts_with("AT_TOGGLE_REPLY index_out_of_range"),
        "out-of-range must be a distinct wire outcome, got: {wire}"
    );
    assert!(
        wire.contains("index=9") && wire.contains("candidates=3"),
        "wire must name the requested index and the candidate count, got: {wire}"
    );
    assert!(
        !wire.contains("checked") && !wire.contains("unchecked"),
        "out-of-range must never read as a delivered toggle, got: {wire}"
    );
}

/// v40.50 test 5 — the real failure end-to-end, in the shape of
/// `t_973ff960`: identical `role`+`name` candidates where the FIRST is a
/// role aggregate and the nth is a per-stream row. The nth is dispatched
/// to, and the reply names it.
///
/// FAILS on the v40.46 baseline: index 2 dispatches to the aggregate.
#[test]
fn at_toggle_nth_dispatches_to_per_stream_row_not_role_aggregate() {
    let _guard = install_tree();
    // What the client resolved: nth=2.
    let client_nth = 2u32;
    let resolved = element_lookup_via_tree_dump_counted(
        4,
        "toggle button",
        ROW_NAME,
        client_nth,
    )
    .expect("resolve")
    .expect("index 2 exists");
    // What a mis-delivery would have produced: the first match. Read BEFORE
    // `drop_tree()` — the override is process-global, so a lookup after the
    // teardown sees no tree at all.
    let first = element_lookup_via_tree_dump_counted(4, "toggle button", ROW_NAME, 0)
        .expect("resolve")
        .expect("index 0 exists");
    drop_tree();

    // The scored target is the per-stream row #7254 at (1929, 305).
    assert_eq!(
        (resolved.rect.x, resolved.rect.y, resolved.rect.w, resolved.rect.h),
        (1929.0, 305.0, 36.0, 34.0),
        "the nth candidate must be the scored per-stream row"
    );
    assert_ne!(
        (resolved.rect.x, resolved.rect.y),
        (first.rect.x, first.rect.y),
        "guard: the aggregate row and the target row must differ, else this test is vacuous"
    );

    // The reply identifies the row that was actually acted on, so the
    // caller can VERIFY delivery instead of trusting rc=0.
    let reply = ToggleReply::unchecked()
        .with_resolved(ToggleReply::resolved_from_bounds(&resolved, ROW_NAME));
    let wire = String::from_utf8(encode_at_toggle_reply_resolved(&reply)).expect("ascii wire");
    assert!(
        wire.contains("path=atspi://4#toggle"),
        "reply must carry the atspi path of the row it acted on, got: {wire}"
    );
    assert_eq!(reply.state, ToggleState::Unchecked);
    assert!(reply.resolved.is_some(), "reply must carry resolved identity");
}

/// v40.50 — wire roundtrip: `AT_TOGGLE_REQ <wid> <name> [<nth>]` parses,
/// and an absent `<nth>` stays absent.
#[test]
fn at_toggle_req_wire_roundtrips_optional_index() {
    let with = b"AT_TOGGLE_REQ 4 toggle button Mute audio 2\n";
    let (frame, rest) = parse_frame(with).expect("parse");
    assert!(rest.is_empty());
    match frame {
        niri::inject::Frame::AtToggleReq { wid, name, index } => {
            assert_eq!(wid, 4);
            assert_eq!(name, "Mute audio");
            assert_eq!(index, Some(2), "trailing numeric must peel as the occurrence index");
        }
        other => panic!("expected Frame::AtToggleReq, got {other:?}"),
    }

    let without = b"AT_TOGGLE_REQ 4 toggle button Mute audio\n";
    let (frame, rest) = parse_frame(without).expect("parse");
    assert!(rest.is_empty());
    match frame {
        niri::inject::Frame::AtToggleReq { name, index, .. } => {
            assert_eq!(name, "Mute audio");
            assert_eq!(index, None, "absent index must stay None (v40.46 behaviour)");
        }
        other => panic!("expected Frame::AtToggleReq, got {other:?}"),
    }
}

/// v40.50 — a numeric NAME with only two remainder tokens must not be
/// stolen as an occurrence index (`AT_TOGGLE_REQ 4 123`).
#[test]
fn at_toggle_req_does_not_steal_numeric_name_as_index() {
    let (frame, rest) = parse_frame(b"AT_TOGGLE_REQ 4 123\n").expect("parse");
    assert!(rest.is_empty());
    match frame {
        niri::inject::Frame::AtToggleReq { name, index, .. } => {
            assert_eq!(name, "123", "a bare numeric name must survive as the NAME");
            assert_eq!(index, None);
        }
        other => panic!("expected Frame::AtToggleReq, got {other:?}"),
    }
}

/// v40.50b — THE DEFECT THIS FILE WAS MISSING. The live client
/// `qlux-cua-driver` fb3b43d `encode_at_toggle_req_line` emits
/// `AT_TOGGLE_REQ <wid> <name_tok> <n>` for every name that is not exactly
/// two words — a TWO-token remainder. The old peel gate was
/// `rest.len() > 2`, so it never fired, the tokens fell into the legacy
/// 3-tuple arm, and the occurrence was taken as the NAME.
///
/// Parsing the client's real wire must yield the name and the occurrence,
/// never the name replaced by a number.
#[test]
fn at_toggle_req_two_token_client_wire_keeps_name_and_index() {
    // What cua-driver fb3b43d actually puts on the wire for
    // name="Allow", nth=Some(2) — asserted literally so a future change
    // to the producer cannot silently invalidate this test.
    let (frame, rest) = parse_frame(b"AT_TOGGLE_REQ 7 Allow 2\n").expect("parse");
    assert!(rest.is_empty(), "no trailing bytes");
    match frame {
        niri::inject::Frame::AtToggleReq { wid, name, index } => {
            assert_eq!(wid, 7);
            assert_eq!(
                name, "Allow",
                "the NAME must survive; the occurrence must not become the name"
            );
            assert_eq!(
                index,
                Some(2),
                "a two-token remainder must peel its trailing numeric as the occurrence"
            );
        }
        other => panic!("expected Frame::AtToggleReq, got {other:?}"),
    }
}

/// v40.50b — the other direction: `encode_at_toggle_req` must emit the
/// two-token shape AND the parser must read it back intact. This is the
/// round-trip contract `tests/inject.rs:5406` documents; before the fix it
/// held only for `index: None` and for two-word names.
#[test]
fn at_toggle_req_encode_roundtrips_single_token_name_with_index() {
    let req = niri::inject::Frame::AtToggleReq {
        wid: 7,
        name: "Allow".to_string(),
        index: Some(2),
    };
    let bytes = encode_at_toggle_req(&req);
    let line = String::from_utf8(bytes.clone()).expect("ascii wire");
    assert_eq!(
        line, "AT_TOGGLE_REQ 7 Allow 2\n",
        "single-token name + occurrence must encode as the two-token shape"
    );

    let (parsed, rest) = parse_frame(&bytes).expect("parse");
    assert!(rest.is_empty(), "no trailing bytes");
    match parsed {
        niri::inject::Frame::AtToggleReq { wid, name, index } => {
            assert_eq!(wid, 7);
            assert_eq!(name, "Allow", "round-trip must preserve the name");
            assert_eq!(index, Some(2), "round-trip must preserve the index");
        }
        other => panic!("expected Frame::AtToggleReq, got {other:?}"),
    }
}

/// v40.50b — the no-steal invariant at the NEW boundary. A single-token
/// remainder still has nothing to peel, so a numeric NAME survives as the
/// NAME even when it parses as a `u32`. This is the property that stops a
/// future "just remove the guard" edit from stealing an element's name.
///
/// Also pins the genuinely ambiguous shape: `<name> <n>` where the NAME is
/// itself a bare number (`AT_TOGGLE_REQ 4 7 2`) is now read as name=7,
/// index=Some(2). That is the documented consequence of lowering the gate,
/// and it is safe because the live client never emits a purely-numeric
/// name — it emits a resolved accessible name, never a number.
#[test]
fn at_toggle_req_single_token_numeric_name_is_never_stolen() {
    let (frame, rest) = parse_frame(b"AT_TOGGLE_REQ 4 123\n").expect("parse");
    assert!(rest.is_empty(), "no trailing bytes");
    match frame {
        niri::inject::Frame::AtToggleReq { name, index, .. } => {
            assert_eq!(name, "123", "a bare numeric name must survive as the NAME");
            assert_eq!(
                index, None,
                "a one-token remainder has nothing to peel and must stay None"
            );
        }
        other => panic!("expected Frame::AtToggleReq, got {other:?}"),
    }

    // The 2-token form with a numeric name is unambiguously <name> <n>.
    let (frame, rest) = parse_frame(b"AT_TOGGLE_REQ 4 7 2\n").expect("parse");
    assert!(rest.is_empty(), "no trailing bytes");
    match frame {
        niri::inject::Frame::AtToggleReq { name, index, .. } => {
            assert_eq!(name, "7");
            assert_eq!(index, Some(2));
        }
        other => panic!("expected Frame::AtToggleReq, got {other:?}"),
    }
}

/// v40.50 — `ToggleReply` gained a `resolved` field; `Reply::AtToggleReply`
/// must serialize `None` resolution as the OLD three-token wire, byte for
/// byte, so an old peer is unaffected.
#[test]
fn at_toggle_reply_without_resolved_keeps_legacy_wire() {
    let wire = Reply::AtToggleReply {
        state: ToggleState::NoAction,
        resolved: None,
    }
    .to_bytes();
    assert_eq!(
        String::from_utf8(wire).unwrap(),
        "AT_TOGGLE_REPLY no_action\n",
        "absent resolution must keep the v40.46 wire byte-for-byte"
    );
}