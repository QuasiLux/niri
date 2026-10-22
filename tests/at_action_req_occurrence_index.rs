//! v40.51 -- AT_ACTION_REQ occurrence index + reply identity.
//!
//! Extends the v40.50 TOGGLE shape onto ACTION. The defect this file pins
//! is the same class: a tree with three sibling rows sharing role+name,
//! the producer resolved index 2, and the server re-matched by name and
//! took the FIRST match. ACTION had an extra trap: the trailing wire token
//! was already the AT-SPI action index, so a naive token-count peel (the
//! v40.50 round-1 defect) cannot be copied. Occurrence is present iff the
//! last TWO remainder tokens both parse as numbers.
//!
//! Tests 1, 3 and 5 FAIL on the pre-v40.51 baseline (first-match selection,
//! no identity fields on the reply) and PASS here. Test 2 pins backward
//! compatibility. Test 4 guards the substitution class. Tests 7, 8, 9, 10
//! are the peel-gate tests (7 is the producer-framing gate).

use niri::inject::atspi_ladder::{
    element_lookup_via_tree_dump_counted, reset_gap4_test_hooks, set_tree_dump_override, TreeNode,
};
use niri::inject::protocol::{encode_at_action_req, parse_frame, Reply, ResolvedTarget};

const ROW_NAME: &str = "Allow";
const ROW_ROLE: &str = "button";

/// set_tree_dump_override is PROCESS-GLOBAL, and cargo runs the tests in
/// one binary in parallel. Without this guard, one tests drop_tree can
/// yank the tree out from under another tests in-flight lookup.
static TREE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn install_tree() -> std::sync::MutexGuard<'static, ()> {
    let g = TREE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_tree_dump_override(Some(three_sibling_rows));
    g
}

fn drop_tree() {
    reset_gap4_test_hooks();
}

/// Three siblings with IDENTICAL role and name, so only the occurrence
/// index can tell them apart. ACTION-path role/name (button / Allow).
/// Coordinates match the v40.50 toggle fixture so the two opcodes can
/// be compared on the same geometry.
///
/// - index 0: the role aggregate row at (1968, 49)
/// - index 1: per-stream row at (1929, 169)
/// - index 2: per-stream row at (1929, 305) -- the scored target
fn three_sibling_rows(_path: Option<&str>) -> Vec<TreeNode> {
    let row = |x: f64, y: f64| TreeNode {
        role: ROW_ROLE.into(),
        name: ROW_NAME.into(),
        x,
        y,
        w: 36.0,
        h: 34.0,
        visible: true,
        n_actions: 1,
        action_name: "click".into(),
        children: vec![],
        ..Default::default()
    };
    vec![TreeNode {
        role: "panel".into(),
        name: "dialog".into(),
        x: 0.0,
        y: 0.0,
        w: 400.0,
        h: 400.0,
        visible: true,
        children: vec![row(1968.0, 49.0), row(1929.0, 169.0), row(1929.0, 305.0)],
        ..Default::default()
    }]
}

/// v40.51 test 1 -- <nth>=2 selects the THIRD row, not the first.
///
/// FAILS on the pre-v40.51 baseline: first-match selection returns the
/// aggregate row at (1968, 49), so x is 1968 and not 1929.
#[test]
fn at_action_req_with_nth_selects_nth_match() {
    let _guard = install_tree();
    let found = element_lookup_via_tree_dump_counted(4, ROW_ROLE, ROW_NAME, 2);
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

/// v40.51 test 2 -- an ABSENT nth reproduces v40.46 behaviour exactly.
///
/// Passes before AND after by design; this pins the backward-compat claim
/// the brief makes about the optional trailing field.
#[test]
fn at_action_req_without_nth_still_takes_first_match() {
    let _guard = install_tree();
    let found = element_lookup_via_tree_dump_counted(4, ROW_ROLE, ROW_NAME, 0);
    drop_tree();

    let bounds = found
        .expect("index 0 must resolve")
        .expect("index 0 must exist");
    assert_eq!(
        (bounds.rect.x, bounds.rect.y),
        (1968.0, 49.0),
        "absent nth must stay first-match, as v40.46 did"
    );
}

/// v40.51 test 3 -- the reply reports the RESOLVED geometry, and after
/// <nth>=2 those are the index-2 bounds, not the index-0 bounds.
///
/// FAILS on the pre-v40.51 baseline: the reply carries no geometry at all.
#[test]
fn at_action_reply_reports_resolved_bounds() {
    let _guard = install_tree();
    let bounds = element_lookup_via_tree_dump_counted(4, ROW_ROLE, ROW_NAME, 2)
        .expect("resolve")
        .expect("index 2 exists");
    drop_tree();

    let reply = Reply::at_action_ok(
        false,
        Some(ResolvedTarget::from_bounds(&bounds, ROW_NAME)),
    );
    let wire = String::from_utf8(reply.to_bytes()).expect("ascii wire");

    assert!(
        wire.contains("x=1929 y=305 w=36 h=34"),
        "reply must report the resolved index-2 geometry, got: {wire}"
    );
    assert!(
        !wire.contains("x=1968"),
        "reply must NOT report the index-0 aggregate geometry, got: {wire}"
    );
}

/// v40.51 test 4 -- an out-of-range nth is a DISTINCT error naming the
/// candidate count. It must NEVER silently fall back to the first match.
#[test]
fn at_action_nth_out_of_range_errors_with_count() {
    let _guard = install_tree();
    let found = element_lookup_via_tree_dump_counted(4, ROW_ROLE, ROW_NAME, 9);
    drop_tree();

    let candidates = found.err().expect("index 9 must NOT resolve");
    assert_eq!(candidates, 3, "the error must name the true candidate count");

    let reply = Reply::at_action_occurrence_out_of_range(9, candidates);
    let wire = String::from_utf8(reply.to_bytes()).expect("ascii wire");
    assert_eq!(
        wire.trim_end(),
        "AT_ACTION_REPLY ERR INDEX_OUT_OF_RANGE index=9 candidates=3",
        "out-of-range must be a distinct wire outcome, got: {wire}"
    );
    assert!(
        !wire.contains("OK"),
        "out-of-range must never read as a delivered action, got: {wire}"
    );
}

/// v40.51 test 5 -- end-to-end: nth dispatches to the per-stream row, the
/// reply names ITS path, and an explicit assert_ne! guard that the aggregate
/// row differs (so the test cannot be vacuous).
///
/// FAILS on the pre-v40.51 baseline: index 2 dispatches to the aggregate.
#[test]
fn at_action_nth_dispatches_to_per_stream_row_not_role_aggregate() {
    let _guard = install_tree();
    let client_nth = 2u32;
    let resolved = element_lookup_via_tree_dump_counted(4, ROW_ROLE, ROW_NAME, client_nth)
        .expect("resolve")
        .expect("index 2 exists");
    let first = element_lookup_via_tree_dump_counted(4, ROW_ROLE, ROW_NAME, 0)
        .expect("resolve")
        .expect("index 0 exists");
    drop_tree();

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

    let reply = Reply::at_action_ok(
        false,
        Some(ResolvedTarget::from_bounds(&resolved, ROW_NAME)),
    );
    let wire = String::from_utf8(reply.to_bytes()).expect("ascii wire");
    assert!(
        wire.contains("path=atspi://4#button:Allow"),
        "reply must carry the atspi path of the row it acted on, got: {wire}"
    );
    match reply {
        Reply::AtActionReply { resolved, ok, .. } => {
            assert!(ok);
            assert!(resolved.is_some(), "reply must carry resolved identity");
        }
        other => panic!("expected AtActionReply, got {other:?}"),
    }
}

/// v40.51 test 6 -- wire roundtrip: AT_ACTION_REQ with and without the
/// occurrence. Absent nth stays None.
#[test]
fn at_action_req_wire_roundtrips_optional_nth() {
    let with = b"AT_ACTION_REQ 4 button Equals 0 2\n";
    let (frame, rest) = parse_frame(with).expect("parse");
    assert!(rest.is_empty());
    match frame {
        niri::inject::Frame::AtActionReq { wid, role, name, index, nth } => {
            assert_eq!(wid, 4);
            assert_eq!(role, "button");
            assert_eq!(name, "Equals");
            assert_eq!(index, 0, "action index must stay 0");
            assert_eq!(nth, Some(2), "trailing numeric pair must peel as nth");
        }
        other => panic!("expected Frame::AtActionReq, got {other:?}"),
    }

    let without = b"AT_ACTION_REQ 4 button Equals 0\n";
    let (frame, rest) = parse_frame(without).expect("parse");
    assert!(rest.is_empty());
    match frame {
        niri::inject::Frame::AtActionReq { name, index, nth, .. } => {
            assert_eq!(name, "Equals");
            assert_eq!(index, 0);
            assert_eq!(nth, None, "absent nth must stay None");
        }
        other => panic!("expected Frame::AtActionReq, got {other:?}"),
    }
}

/// v40.51 test 7 -- THE PRODUCER-FRAMING GATE. Assert the literal bytes
/// AT_ACTION_REQ 7 button Allow 0 2 keep the NAME Allow, the ACTION index 0,
/// AND nth=Some(2). This is the test that would have caught the v40.50
/// round-1 defect (name replaced by the number) for this opcode.
///
/// Justified by the producer qlux-cua-driver fb3b43d7 src/wire.rs::encode_at_action_req_line
/// which emits AT_ACTION_REQ <wid> <role_tok> <name_tok> <index> and, with
/// this cards selector, appends <nth>. A two-token remainder after name is
/// [action_index, nth]; a gate on rest.len() > 2 never fires.
#[test]
fn at_action_req_two_token_client_wire_keeps_name_and_index() {
    let (frame, rest) = parse_frame(b"AT_ACTION_REQ 7 button Allow 0 2\n").expect("parse");
    assert!(rest.is_empty(), "no trailing bytes");
    match frame {
        niri::inject::Frame::AtActionReq { wid, role, name, index, nth } => {
            assert_eq!(wid, 7);
            assert_eq!(role, "button");
            assert_eq!(
                name, "Allow",
                "the NAME must survive; the occurrence must not become the name"
            );
            assert_eq!(index, 0, "the ACTION index must stay 0, not steal 2");
            assert_eq!(nth, Some(2), "the trailing numeric must peel as nth");
        }
        other => panic!("expected Frame::AtActionReq, got {other:?}"),
    }
}

/// v40.51 test 8 -- the legacy two-token form must NOT be stolen.
/// AT_ACTION_REQ 4 button Mute click 0 is the exact shape at tests/inject.rs:4865.
/// Without this, the new gate breaks 4 pre-existing tests.
#[test]
fn at_action_req_legacy_two_token_form_is_not_stolen() {
    let (frame, rest) = parse_frame(b"AT_ACTION_REQ 4 button Mute click 0\n").expect("parse");
    assert!(rest.is_empty());
    match frame {
        niri::inject::Frame::AtActionReq { name, index, nth, .. } => {
            assert_eq!(name, "Mute");
            assert_eq!(index, 0);
            assert_eq!(nth, None, "click is not numeric; this is legacy, not occurrence");
        }
        other => panic!("expected Frame::AtActionReq, got {other:?}"),
    }
}

/// v40.51 test 9 -- no-steal at the boundary. A numeric NAME is never
/// reinterpreted as an occurrence; a one-token remainder has nothing to peel.
#[test]
fn at_action_req_numeric_name_is_never_stolen() {
    let (frame, rest) = parse_frame(b"AT_ACTION_REQ 4 button 123 0\n").expect("parse");
    assert!(rest.is_empty());
    match frame {
        niri::inject::Frame::AtActionReq { name, index, nth, .. } => {
            assert_eq!(name, "123", "a numeric NAME must survive as the NAME");
            assert_eq!(index, 0);
            assert_eq!(nth, None);
        }
        other => panic!("expected Frame::AtActionReq, got {other:?}"),
    }

    let (frame, rest) = parse_frame(b"AT_ACTION_REQ 4 button Equals 0\n").expect("parse");
    assert!(rest.is_empty());
    match frame {
        niri::inject::Frame::AtActionReq { name, index, nth, .. } => {
            assert_eq!(name, "Equals");
            assert_eq!(index, 0);
            assert_eq!(
                nth, None,
                "a one-token remainder has nothing to peel and must stay None"
            );
        }
        other => panic!("expected Frame::AtActionReq, got {other:?}"),
    }
}

/// v40.51 test 10 -- encode round-trip with its index.
#[test]
fn at_action_req_encode_roundtrips_single_token_name_with_nth() {
    let req = niri::inject::Frame::AtActionReq {
        wid: 7,
        role: "button".to_string(),
        name: "Allow".to_string(),
        index: 0,
        nth: Some(2),
    };
    let bytes = encode_at_action_req(&req);
    let line = String::from_utf8(bytes.clone()).expect("ascii wire");
    assert_eq!(
        line, "AT_ACTION_REQ 7 button Allow 0 2\n",
        "single-token name + action index + occurrence must encode as this literal"
    );

    let (parsed, rest) = parse_frame(&bytes).expect("parse");
    assert!(rest.is_empty(), "no trailing bytes");
    match parsed {
        niri::inject::Frame::AtActionReq { wid, role, name, index, nth } => {
            assert_eq!(wid, 7);
            assert_eq!(role, "button");
            assert_eq!(name, "Allow", "round-trip must preserve the name");
            assert_eq!(index, 0, "round-trip must preserve the action index");
            assert_eq!(nth, Some(2), "round-trip must preserve the occurrence");
        }
        other => panic!("expected Frame::AtActionReq, got {other:?}"),
    }
}

/// v40.51 test 11 -- legacy reply wire unchanged when resolved is None.
#[test]
fn at_action_reply_without_resolved_keeps_legacy_wire() {
    assert_eq!(
        String::from_utf8(Reply::at_action_ok(false, None).to_bytes()).unwrap(),
        "AT_ACTION_REPLY OK\n",
        "absent resolution must keep the v40.15.3 OK wire byte-for-byte"
    );
    assert_eq!(
        String::from_utf8(Reply::at_action_ok(true, None).to_bytes()).unwrap(),
        "AT_ACTION_REPLY OK FORCE_REDRAW\n",
        "absent resolution must keep the FORCE_REDRAW wire byte-for-byte"
    );
    let err = Reply::AtActionReply {
        ok: false,
        error: Some("ACTION_NOT_FOUND".into()),
        force_redraw: false,
        resolved: None,
    };
    assert_eq!(
        String::from_utf8(err.to_bytes()).unwrap(),
        "AT_ACTION_REPLY ERR ACTION_NOT_FOUND\n",
        "error replies stay the legacy ERR wire, resolved None"
    );
}
