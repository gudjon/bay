//! G151. A release signed by any listed release key verifies, and by no other.
//!
//! `DESIGN` (the binary's lifecycle, `bay update`). `bay update` pins a list of release keys,
//! so a rollover carries the old and the new key for a stated transition, and a later release
//! drops the old one. The release is the local fixture (`BAY_TEST_RELEASE_URL`), and the list
//! is passed through `BAY_TEST_RELEASE_KEY`, one key per line. The real release keys are never
//! used.

#![cfg(unix)]

use crate::release_fixture;

use release_fixture::Fixture;

const NEWER: &str = "9.0.0";

/// A hardware (FIDO2) public key: a well-formed `sk-ssh-ed25519@openssh.com` line with no
/// device behind it, so it verifies nothing, but the list must accept it.
const SK_KEY: &str = "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29tAAAAIDgOPZsmzgIilFlX+FEXSGHrntiIAzexLc6I0f+I9/4qAAAAD3NzaDpiYXktcmVsZWFzZQ==";

/// The fixture with keys A and B, and a release signed by `signer` (`a` or `b`).
fn rollover(signer: &str) -> (Fixture, String, String) {
    let fixture = Fixture::new();
    let a = Fixture::keygen(fixture.path(), "a");
    let b = Fixture::keygen(fixture.path(), "b");
    let signer = if signer == "a" { &a } else { &b };
    fixture.release(NEWER, NEWER, Some(signer));
    fixture.list(&[(NEWER, false)]);
    (fixture, Fixture::public_line(&a), Fixture::public_line(&b))
}

fn update_with(fixture: &Fixture, keys: &[&str]) -> (i32, serde_json::Value) {
    fixture.update_with_keys(fixture.path(), &[], &keys.join("\n"))
}

#[test]
fn a_release_signed_by_the_new_key_verifies_while_both_are_listed() {
    let (fixture, a, b) = rollover("b");
    let (code, result) = update_with(&fixture, &[&a, &b]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["detail"]["updated"], true, "{result}");
}

#[test]
fn a_release_signed_by_the_new_key_is_refused_before_it_is_listed() {
    let (fixture, a, _) = rollover("b");
    let (code, result) = update_with(&fixture, &[&a]);
    assert_eq!(code, 20, "{result}");
    assert_eq!(result["reason"], "UPDATE_UNVERIFIED", "{result}");
    fixture.assert_untouched();
}

#[test]
fn a_release_signed_by_the_old_key_is_refused_once_it_is_dropped() {
    let (fixture, _, b) = rollover("a");
    let (code, result) = update_with(&fixture, &[&b]);
    assert_eq!(code, 20, "{result}");
    assert_eq!(result["reason"], "UPDATE_UNVERIFIED", "{result}");
    fixture.assert_untouched();
}

#[test]
fn a_hardware_key_in_the_list_leaves_the_other_keys_verifying() {
    let (fixture, _, b) = rollover("b");
    let (code, result) = update_with(&fixture, &[SK_KEY, &b]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["detail"]["updated"], true, "{result}");
}
