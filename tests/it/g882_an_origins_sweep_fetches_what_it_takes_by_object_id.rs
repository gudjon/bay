//! G882. An origin's sweep fetches what it takes by object id.
//!
//! `bay host serve` lists the hosted remote, then fetches the values it takes before it sets
//! them by compare-and-swap. A fetch that names each ref sends the remote a prefix per name and
//! matches every name against every ref the remote sends back, so its cost grows with the square
//! of the refs it takes: a first sweep of a store with tens of thousands of refs spent its whole
//! run matching names and brought no pack. The listing already holds every value, so the sweep
//! asks for those objects by id: its fetch names no ref.

// The origin's hooks are shell scripts, so these run on Unix only.
#![cfg(unix)]

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;

use crate::support;

use support::{TestBox, formats, result_json};

/// The branches, and as many records, the hosted remote holds before the first sweep.
const REFS: usize = 120;

/// `git update-ref --stdin` in the bare repository `bare`, reading `input`.
fn update_refs(test_box: &TestBox, bare: &Path, input: &str) {
    let mut child = test_box
        .git(test_box.path())
        .arg(format!("--git-dir={}", bare.display()))
        .args(["update-ref", "--stdin"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success(), "update-ref");
}

/// The value of `name` in the bare repository `bare`, `None` when it has none.
fn value(test_box: &TestBox, bare: &Path, name: &str) -> Option<String> {
    let out = test_box
        .git(test_box.path())
        .arg(format!("--git-dir={}", bare.display()))
        .args(["rev-parse", "--verify", "-q", name])
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8(out.stdout).unwrap().trim().to_owned())
}

#[test]
fn a_first_sweep_fetches_every_value_it_takes_by_object_id() {
    let test_box = TestBox::new();
    let hosted = test_box.bare_any("hosted.git");
    let tree = test_box.git_bare(&hosted, &["hash-object", "-w", "-t", "tree", "/dev/null"]);
    // Two values, so refs share objects as the refs of a store do.
    let values: Vec<String> = ["one", "two"]
        .iter()
        .map(|message| test_box.git_bare(&hosted, &["commit-tree", &tree, "-m", message]))
        .collect();
    let mut input = String::new();
    for i in 0..REFS {
        let value = &values[i % values.len()];
        let _ = writeln!(input, "create refs/heads/topic-{i} {value}");
        let _ = writeln!(input, "create refs/bay/test/record-{i} {value}");
    }
    update_refs(&test_box, &hosted, &input);

    let origin = test_box.path().join("origin.git");
    let (object_format, _) = formats();
    test_box.bay_ok(
        test_box.path(),
        &[
            "host",
            origin.to_str().unwrap(),
            "--object-format",
            &object_format,
            "--upstream",
            hosted.to_str().unwrap(),
        ],
    );

    // The first sweep takes every ref, with a trace of each packet Git sends and reads.
    let trace = test_box.path().join("packets.trace");
    let (code, served) = result_json(
        test_box
            .bay(test_box.path())
            .env("GIT_TRACE_PACKET", &trace)
            .args(["host", "serve", origin.to_str().unwrap(), "--once"]),
    );
    assert_eq!(code, 0, "{served}");
    assert_eq!(served["detail"]["mirrored"]["taken"], 2 * REFS, "{served}");
    assert_eq!(served["detail"]["mirrored"]["left"], 0, "{served}");
    for i in [0, REFS - 1] {
        for name in [
            format!("refs/heads/topic-{i}"),
            format!("refs/bay/test/record-{i}"),
        ] {
            assert_eq!(
                value(&test_box, &origin, &name),
                value(&test_box, &hosted, &name),
                "{name}"
            );
        }
    }

    // No call named a ref to the hosted remote: a fetch by name sends a prefix for each one.
    let packets = std::fs::read_to_string(&trace).unwrap();
    let named: Vec<&str> = packets
        .lines()
        .filter(|line| line.contains("ref-prefix"))
        .collect();
    assert!(
        named.is_empty(),
        "{} ref prefixes went to the hosted remote, first {:?}",
        named.len(),
        named.first()
    );
}
