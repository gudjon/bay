//! G242: an update steps through the newest release it can verify (`DESIGN`, the binary's
//! lifecycle; after the 0.1.4-0.1.5 key rollover).
//!
//! After a release-key rollover the newest release is signed by a key the installed binary
//! does not trust yet. An unpinned `bay update` then installs the newest older release that
//! verifies and is newer than the installed one, which trusts the new key, and says that
//! another update reaches the newest. Only an untrusted signature steps: a release whose bytes
//! do not match its signed sums is refused, and nothing is installed.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::release_fixture;

use std::process::Command;

use release_fixture::{Fixture, run, target};

const STEP: &str = "9.0.0";
const NEWEST: &str = "9.1.0";

#[test]
fn a_newest_release_under_a_new_key_is_reached_through_the_newest_one_that_verifies() {
    let fixture = Fixture::new();
    let rolled = Fixture::keygen(fixture.path(), "rolled");
    fixture.release(STEP, STEP, None);
    fixture.release(NEWEST, NEWEST, Some(&rolled));
    fixture.list(&[(NEWEST, true), (STEP, true)]);

    let (code, result) = fixture.update(fixture.path(), &[]);
    assert_eq!(code, 0, "{result}");
    let detail = &result["detail"];
    assert_eq!(detail["target"], STEP, "{result}");
    assert_eq!(detail["newest"], NEWEST, "{result}");
    assert_eq!(detail["stepped"], true, "{result}");
    assert_eq!(detail["updated"], true, "{result}");
    assert!(
        detail["next"]
            .as_str()
            .is_some_and(|next| next.contains("bay update")),
        "{result}"
    );
    let version = run(Command::new(fixture.bin()).arg("--version"));
    assert_eq!(version.trim(), format!("bay {STEP}"));
}

#[test]
fn a_release_whose_bytes_do_not_match_is_refused_never_stepped_past() {
    let fixture = Fixture::new();
    fixture.release(STEP, STEP, None);
    let dir = fixture.release(NEWEST, NEWEST, None);
    fixture.list(&[(NEWEST, true), (STEP, true)]);
    // The newest is signed by the trusted key, but its archive is not the signed one.
    std::fs::write(
        dir.join(format!("bay-{}.tar.gz", target())),
        b"not the signed archive",
    )
    .unwrap();

    let (code, result) = fixture.update(fixture.path(), &[]);
    assert_eq!(code, 20, "{result}");
    assert_eq!(result["reason"], "UPDATE_UNVERIFIED", "{result}");
    assert_eq!(result["detail"]["target"], NEWEST, "{result}");
    fixture.assert_untouched();
}

#[test]
fn with_nothing_to_step_to_the_refusal_stands() {
    let fixture = Fixture::new();
    let rolled = Fixture::keygen(fixture.path(), "rolled");
    fixture.release(NEWEST, NEWEST, Some(&rolled));
    fixture.list(&[(NEWEST, true)]);
    let (code, result) = fixture.update(fixture.path(), &[]);
    assert_eq!(code, 20, "{result}");
    assert_eq!(result["reason"], "UPDATE_UNVERIFIED", "{result}");
    fixture.assert_untouched();
}
