//! G76. Lineage pairs name reverts, supersedes and fixes (`DESIGN` § 4, § 5; `plans/wave2.md`
//! G76, corrected by the review dossier's K5).
//!
//! `bay status --lineage [<range>]` lists, for every first-parent commit of the range (trunk by
//! default) whose `Reverts:`, `Supersedes:` or `Fixes:` trailer names a change, one pair:
//! `{commit, change, relation, target_change, target_commit, off_line, publication}`, oldest
//! first. `target_commit` is the publication of the named change on the first-parent line of the
//! range's tip. A target that is not on that line is kept, with `target_commit` null and
//! `off_line` true: a pair is never dropped. `publication` says whether the naming commit is an
//! executor-signed bay publication, so a typed trailer on a commit outside bay is listed as one.
//! The query reads local refs only: it never fetches.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use serde_json::{Value, json};
use support::result_json;
use support::talk::{TalkCase, hosted_talk_landing};

/// Starts a change on `src/` as `agent`, writes `file`, and lands it. Returns (id, landing).
fn land(case: &TalkCase, args: &[&str], file: &str, text: &str) -> (String, String) {
    let mut start = vec!["start"];
    start.extend(args);
    start.extend(["--paths", "src/"]);
    let (code, started) = result_json(case.bay_as("agent").args(&start));
    assert_eq!(code, 0, "{started}");
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(file), text).unwrap();
    let (code, landed) = result_json(
        case.test_box
            .bay(&view)
            .env("BAY_ACTOR", "agent")
            .env("BAY_SESSION", "s-agent")
            .args(["merge", "--wait"]),
    );
    assert_eq!(code, 0, "{landed}");
    (id, trunk_tip(case))
}

fn trunk_tip(case: &TalkCase) -> String {
    case.host_tip("refs/heads/main").unwrap()
}

/// A commit in `a` with `parents` and the tree of the first, whose message is `message`: a
/// commit made outside bay, never signed by the executor.
fn commit(case: &TalkCase, parents: &[&str], message: &str) -> String {
    let tree = format!("{}^{{tree}}", parents[0]);
    let mut args = vec!["commit-tree", tree.as_str(), "-m", message];
    for parent in parents {
        args.extend(["-p", parent]);
    }
    case.test_box.git_ok(&case.a, &args)
}

fn lineage(case: &TalkCase, range: Option<&str>) -> Value {
    let mut args = vec!["status", "--lineage"];
    args.extend(range);
    let (code, result) = result_json(case.bay_as("agent").args(&args));
    assert_eq!(code, 0, "{result}");
    result
}

fn pairs(result: &Value) -> Vec<Value> {
    result["detail"]["pairs"].as_array().unwrap().clone()
}

#[test]
fn every_relation_is_a_pair_and_a_target_off_the_first_parent_line_is_kept() {
    let case = hosted_talk_landing();
    let seed = trunk_tip(&case);
    let (x, x_landing) = land(&case, &["CARD-x"], "src/x.rs", "fn x() {}\n");
    let (y, y_landing) = land(&case, &["CARD-y"], "src/y.rs", "fn y() {}\n");
    let (z, z_landing) = land(&case, &["CARD-z"], "src/z.rs", "fn z() {}\n");
    let (fix, fix_landing) = land(
        &case,
        &["CARD-fix", "--fixes", &x],
        "src/x.rs",
        "fn x() { 1 }\n",
    );
    let (code, undone) = result_json(case.bay_as("agent").args(["undo", &y, "--wait"]));
    assert_eq!(code, 0, "{undone}");
    let undo = undone["change"].as_str().unwrap().to_owned();
    let undo_landing = trunk_tip(&case);
    let (next, next_landing) = land(&case, &["CARD-z", "--new"], "src/z.rs", "fn z() { 2 }\n");
    case.test_box.git_ok(&case.a, &["fetch", "-q", "origin"]);

    let bay_pairs = vec![
        json!({"commit": fix_landing, "change": fix, "relation": "fixes",
            "target_change": x, "target_commit": x_landing, "off_line": false,
            "publication": true}),
        json!({"commit": undo_landing, "change": undo, "relation": "reverts",
            "target_change": y, "target_commit": y_landing, "off_line": false,
            "publication": true}),
        json!({"commit": next_landing, "change": next, "relation": "supersedes",
            "target_change": z, "target_commit": z_landing, "off_line": false,
            "publication": true}),
    ];
    // Trunk by default, oldest first.
    let all = lineage(&case, None);
    assert_eq!(pairs(&all), bay_pairs, "{all}");
    assert_eq!(all["detail"]["trunk"], next_landing.as_str(), "{all}");

    // A change landed behind a merge's second parent, and a change on no line at all, are each
    // named by a later first-parent commit made outside bay.
    let side_id = bay::ids::new_change_id().unwrap();
    let gone_id = bay::ids::new_change_id().unwrap();
    let side = commit(
        &case,
        &[&next_landing],
        &format!("side work\n\nChange-Id: {side_id}\n"),
    );
    let merge = commit(&case, &[&next_landing, &side], "merge side work");
    let reverts_side = commit(
        &case,
        &[&merge],
        &format!("revert side work\n\nReverts: {side_id}\nFixes: {gone_id}\n"),
    );
    let range = format!("{seed}..{reverts_side}");
    let ranged = lineage(&case, Some(&range));
    let mut expected = bay_pairs.clone();
    expected.extend([
        json!({"commit": reverts_side, "change": null, "relation": "reverts",
            "target_change": side_id, "target_commit": null, "off_line": true,
            "publication": false}),
        json!({"commit": reverts_side, "change": null, "relation": "fixes",
            "target_change": gone_id, "target_commit": null, "off_line": true,
            "publication": false}),
    ]);
    assert_eq!(pairs(&ranged), expected, "{ranged}");
    assert_eq!(ranged["detail"]["range"], range.as_str(), "{ranged}");

    // A range after the fix lists only what lands after it; its targets are still found on the
    // first-parent line of the range's tip.
    let after_fix = lineage(&case, Some(&format!("{fix_landing}..{next_landing}")));
    assert_eq!(pairs(&after_fix), bay_pairs[1..].to_vec(), "{after_fix}");
}

#[test]
fn lineage_reads_local_refs_only() {
    let case = hosted_talk_landing();
    let (x, _) = land(&case, &["CARD-x"], "src/x.rs", "fn x() {}\n");
    case.test_box.git_ok(&case.a, &["fetch", "-q", "origin"]);
    let before = lineage(&case, None);
    assert_eq!(pairs(&before), Vec::<Value>::new(), "{before}");

    // A fix lands while this clone's trunk tracking ref stays at the old trunk: lineage, which
    // never fetches, does not list it.
    let local_trunk = case
        .test_box
        .git_ok(&case.a, &["rev-parse", "refs/remotes/origin/main"]);
    land(
        &case,
        &["CARD-fix", "--fixes", &x],
        "src/x.rs",
        "fn x() { 1 }\n",
    );
    case.test_box.git_ok(
        &case.a,
        &["update-ref", "refs/remotes/origin/main", &local_trunk],
    );
    let still = lineage(&case, None);
    assert_eq!(pairs(&still), Vec::<Value>::new(), "{still}");
    assert_eq!(
        case.test_box
            .git_ok(&case.a, &["rev-parse", "refs/remotes/origin/main"]),
        local_trunk
    );
}
