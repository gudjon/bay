//! G948 (a fleet finding, 2026-11-27): a post check's `TMPDIR`, in its scratch under bay's state
//! directory, was 98 bytes on the fleet's boxes, and a Unix socket a test bound in a directory of
//! it took 110: past the 107 bytes of an `AF_UNIX` path, so `listen` failed with EINVAL on every box
//! that ran the test. Under `bwrap` the check's scratch `tmp` is bound at a short path,
//! `/tmp/bay`, and `TMPDIR` names it, so a socket name of up to 98 bytes binds there.
//! `sandbox-exec` and an unsandboxed box keep the scratch's own path.
//!
//! Guard to mutate, in `src/check/sandbox.rs`: `tmp_dir` returns the scratch's own `tmp` under
//! `bwrap` too (the socket does not bind, and the merge is refused).

// Each case runs shell scripts (the adapter), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_check_under_bwrap_binds_a_unix_socket_in_its_tmpdir() {
    // GIVEN a box whose checks run under bwrap, and a check that passes only when it binds a
    // Unix socket with a 60-byte name in its TMPDIR, which no path in its deep scratch fits.
    if support::sandbox() != Some("bwrap") {
        eprintln!("no bwrap sandbox on this machine; G948 cannot run");
        return;
    }
    if !Path::new("/usr/bin/python3").exists() {
        eprintln!("no /usr/bin/python3 on this machine; G948 cannot run");
        return;
    }
    let before = r#"if /usr/bin/python3 -c 'import os, socket; socket.socket(socket.AF_UNIX).bind(os.path.join(os.environ["TMPDIR"], "s" * 60))'; then bound=yes; else bound=no; fi"#;
    let pass = passing_result(1);
    let fail = pass.replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let result = format!("if [ \"$bound\" = yes ]; then {pass}; else {fail}; fi");
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files(before, &result, 1, 60),
    );
    let (_, view) = test_box.start_and_edit(&repo);

    // WHEN the change is merged.
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));

    // THEN the socket bound, the check passed, and the change merged.
    assert_eq!(code, 0, "the check could not bind its socket: {landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
}
