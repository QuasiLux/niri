//! v40.71 — production `AT_TOGGLE_REPLY` must carry resolved identity in the
//! layout cua-driver v40.50 already parses.
//!
//! Live G2 (`t_865ad02b`) delivered 3/3 (pactl Mute bit moved) and still
//! exited rc=4 `UNVERIFIED_TARGET` with `reply bounds None path None`.
//!
//! Root cause (this card): `NiriInjectHarness::at_toggle_req` already
//! attaches `ToggleReply.resolved` (v40.50). The **production encoder**
//! then emitted the ACTION kv form
//! `AT_TOGGLE_REPLY <state> x=<x> y=<y> w=<w> h=<h> path=<path>`.
//! cua-driver `parse_toggle_reply_identity_toks` requires the v40.50
//! positional form
//! `AT_TOGGLE_REPLY <wid> <name> <state> <x> <y> <w> <h> <path>`
//! (first token a u64 window id; a state token; four i32 bounds). The kv
//! form's first token is the state word, so the client treats identity as
//! missing → `NoReplyIdentity` → `UNVERIFIED_TARGET`.
//!
//! This test exercises `Reply::AtToggleReply.to_bytes` (the same encoder
//! `src/inject/socket.rs` writes on a real socket). It does **not** go
//! through `DispatchHarness::at_toggle_req` (the in-process fallback that
//! already attached identity and would prove nothing).
//!
//! Non-tautology: on unmodified `bfa8645e` the encoder emits `x=` / `path=`
//! and this test FAILS. After the v40.71 encoder change it PASSES.
//!
//! ACTION is out of scope: `Reply::at_action_ok` already emits `x=` `y=`
//! `w=` `h=` `path=` and cua-driver already parses that.

use niri::inject::protocol::{
    encode_at_toggle_reply_resolved, ResolvedTarget, ToggleReply, ToggleState,
};

#[test]
fn at_toggle_reply_wire_is_positional_identity_v40_71() {
    let _ = std::hint::black_box(niri::inject::protocol::AT_REPLY_RESOLVED_IDENTITY_V40_71);
    let resolved = ResolvedTarget {
        name: "Mute audio".into(),
        x: 1929.0,
        y: 425.0,
        w: 36.0,
        h: 34.0,
        path: "atspi://4#toggle_button:Mute_audio".into(),
    };
    let reply = ToggleReply::unchecked().with_resolved(resolved);
    let wire = String::from_utf8(encode_at_toggle_reply_resolved(&reply)).expect("ascii wire");

    assert_eq!(
        wire,
        "AT_TOGGLE_REPLY 4 Mute audio unchecked 1929 425 36 34 atspi://4#toggle_button:Mute_audio\n",
        "production encoder must emit the cua-driver v40.50 positional identity layout, got: {wire:?}"
    );
    assert!(
        !wire.contains("x="),
        "must not emit ACTION-shaped kv tokens (cua-driver cannot parse them on TOGGLE), got: {wire:?}"
    );
    assert!(
        !wire.contains("path="),
        "must not emit ACTION-shaped kv tokens (cua-driver cannot parse them on TOGGLE), got: {wire:?}"
    );

    // Mirror cua-driver `parse_toggle_reply_identity_toks`: first token is
    // a window id, a state token exists, four i32 bounds follow it.
    let rest = wire
        .strip_prefix("AT_TOGGLE_REPLY ")
        .expect("opcode")
        .trim();
    let toks: Vec<&str> = rest.split_whitespace().collect();
    assert!(
        toks[0].parse::<u64>().is_ok(),
        "first token must be window id so cua-driver does not take the legacy state-only arm, got {:?}",
        toks[0]
    );
    let state_at = toks
        .iter()
        .position(|t| *t == "unchecked")
        .expect("state token");
    assert!(
        toks.len() >= state_at + 5,
        "state must be followed by x y w h path, got {toks:?}"
    );
    let bounds: [i32; 4] = [
        toks[state_at + 1].parse().expect("x"),
        toks[state_at + 2].parse().expect("y"),
        toks[state_at + 3].parse().expect("w"),
        toks[state_at + 4].parse().expect("h"),
    ];
    assert_eq!(bounds, [1929, 425, 36, 34]);
    assert_eq!(reply.state, ToggleState::Unchecked);
    assert!(reply.resolved.is_some());
}
