//! G304: the plain sentence of `bay approve` says who adds the change next, and a path in a
//! sentence is never rewritten inside.
//!
//! An approval or a waiver is given, and the change is not on main yet: `say`
//! never reads "nothing else is needed" while `detail.next` names a step someone still owes.
//! `bay approve` writes who adds the change (`lands_by`: `executor`, `here`, `author`, or
//! `author_repairs` after a no), and `say` is derived from it. A fault's message that names a
//! path holding a Git word, such as `refs/bay/registry`, says "bay's records" for the path
//! whole, never a path that does not exist (the unit test `plain_puts_each_unplain_word_plainly`).
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/result.rs` `ok_say`: drop the `answered_say(detail)` branch (an approval with no
//!   executor says "Done; nothing else is needed.").
//! - `src/result.rs` `answered_say`: the `(_, false)` sentence back to "Done; nothing else is
//!   needed." (the approval with no executor names nobody who adds the change).
//! - `src/approve.rs` `approve_in`: `"lands_by": lands_by,` dropped from the approval's detail
//!   (the approval and the no carry no `lands_by`).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::approval::{trunk, trunk_with};

#[test]
fn an_approval_with_no_executor_says_its_author_adds_it() {
    let trunk = trunk();
    let (id, _view, _candidate) = trunk.held_change();
    let (code, approved) = trunk.approve(&trunk.approver_box, &id, &[]);
    assert_eq!(code, 0, "{approved}");
    assert_eq!(approved["detail"]["lands_by"], "author", "{approved}");
    let say = approved["say"].as_str().unwrap_or_default();
    assert!(!say.contains("nothing else is needed"), "{approved}");
    assert!(say.starts_with("Approved"), "{approved}");
    assert!(say.contains("its author saves it again"), "{approved}");
}

#[test]
fn a_no_says_the_author_fixes_it() {
    let trunk = trunk();
    let (id, _view, _candidate) = trunk.held_change();
    let (code, rejected) = trunk.approve(&trunk.approver_box, &id, &["--reject"]);
    assert_eq!(code, 0, "{rejected}");
    assert_eq!(
        rejected["detail"]["lands_by"], "author_repairs",
        "{rejected}"
    );
    let say = rejected["say"].as_str().unwrap_or_default();
    assert!(say.contains("its author fixes it"), "{rejected}");
}

#[test]
fn a_waiver_says_the_change_is_added_once_saved_again() {
    let trunk = trunk_with("waivable = true\n");
    // Clear the box's shared producer name, as on people's machines, so the approver reads the
    // author's evidence (G297).
    std::fs::write(&trunk.test_box.global_config, support::UNSANDBOXED_CHOICE).unwrap();
    let (id, view) = trunk.start(
        "CARD-fail",
        &["src/"],
        &[("src/fail", "1\n"), ("src/b.rs", "fn b() {}\n")],
    );
    let (code, blocked) = trunk.land(&view);
    assert_eq!(code, 10, "{blocked}");
    let (code, waived) = trunk.approve(
        &trunk.approver_box,
        &id,
        &["--waive", "tests", "--why", "flaky runner"],
    );
    assert_eq!(code, 0, "{waived}");
    assert_eq!(waived["detail"]["lands_by"], "author", "{waived}");
    let say = waived["say"].as_str().unwrap_or_default();
    assert!(!say.contains("nothing else is needed"), "{waived}");
    assert!(say.starts_with("Waived"), "{waived}");
    assert!(say.contains("not passed"), "{waived}");
}
