//! G462 (bay 0.6.0, T13; the owner's probe, 2026-10-28). `bay talk --to-change <id>` names the
//! actor that merged a change. A target that is neither a change id nor a commit this
//! repository holds, and a well-formed change id that no change has (here or on the remote),
//! never will be on main: each is `TALK_REFUSED`, its `detail.why` naming what is missing, never
//! `CHANGE_NOT_MERGED` ("still waiting"), whose next step waits forever. A change that exists and
//! is not on main yet stays `CHANGE_NOT_MERGED`, retry after a wait.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/talk/lander.rs` `resolve`: the "neither a change id nor a commit" refusal back to
//!   `not_landed` (`CHANGE_NOT_MERGED`).
//! - `src/talk/lander.rs` `change_exists`: always `true` (the unknown change id waits again), or
//!   always `false` (the pending change is refused).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::plain_talk::Plain;

#[test]
fn a_target_that_names_nothing_is_refused_and_a_pending_change_waits() {
    let case = Plain::approvals();
    let (code, missing) = case.talk("agent-b", &["--to-change", "c-missing", "Why this?"]);
    assert_eq!(code, 10, "{missing}");
    assert_eq!(missing["reason"], "TALK_REFUSED", "{missing}");
    assert_ne!(missing["retry_when"], "after_wait", "{missing}");
    let why = missing["detail"]["why"].as_str().unwrap();
    assert!(
        why.contains("neither a change id nor a commit"),
        "{missing}"
    );
    assert_eq!(missing["detail"]["to_change"], "c-missing", "{missing}");

    let (code, unknown) = case.talk("agent-b", &["--to-change", "abcdefgh", "Why this?"]);
    assert_eq!(code, 10, "{unknown}");
    assert_eq!(unknown["reason"], "TALK_REFUSED", "{unknown}");
    assert_ne!(unknown["retry_when"], "after_wait", "{unknown}");
    let why = unknown["detail"]["why"].as_str().unwrap();
    assert!(why.contains("no change abcdefgh"), "{unknown}");

    // A change agent started and has not merged: it exists on the remote, so it waits.
    let (pending, _view) = case.test_box.start_and_edit(&case.a);
    let (code, waits) = case.talk("agent-b", &["--to-change", &pending, "Why this?"]);
    assert_eq!(code, 10, "{waits}");
    assert_eq!(waits["reason"], "CHANGE_NOT_MERGED", "{waits}");
    assert_eq!(waits["retry_when"], "after_wait", "{waits}");
    assert_eq!(case.remote_talks(), Vec::<String>::new());
}
