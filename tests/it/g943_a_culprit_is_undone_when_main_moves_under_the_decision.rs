//! G943 (BACKLOG 358, a load measurement, 2026-11-27: two runners landed batches on a red main
//! (G938); the bisect named the culprit 450 s after the red, and no runner ever undid it). Two
//! causes, one test each:
//!
//! - A runner decided on main as it read it at its pass's start; on a busy line another
//!   runner's landing moved main before nearly every decision, and a decision main moved under
//!   waited for the next pass, which found it moved too. The pass now decides again on main as
//!   the remote holds it then.
//! - A pass took heads for its whole bound (300 s) before it told and undid main's reds and
//!   planned its post pieces, once. Between its visits it now does both, once every
//!   `RED_EVERY`.
//!
//! Guards to mutate, in `src/red_undo.rs`: `undone` waiting for the next pass when main moved
//! (`break` for the `Decided::Moved` return), `undo` deciding once (`DECISIONS` 1). In
//! `src/executor.rs`: the `between` call in `pass`'s head loop (remove it).

#![cfg(unix)]

use crate::g244d_a_red_post_check_names_its_culprit_by_bisection::{Trunk, trunk_with};
use crate::support::result_json;

use std::path::PathBuf;

use serde_json::Value;

/// The wake adapter: appends each request to `$HOME/woken`. When `$HOME/remote` names the
/// remote, it appends main's commit there to `$HOME/mains`. When `$HOME/move-main` names a
/// clone, it lands a commit on main from it, outside bay, as another runner's landing does
/// within a pass, once. bay gives an adapter only PATH and HOME.
const WAKE: &str = "#!/bin/sh\n\
cat >> \"$HOME/woken\"\n\
echo >> \"$HOME/woken\"\n\
if [ -f \"$HOME/remote\" ]; then\n\
  git ls-remote \"$(cat \"$HOME/remote\")\" refs/heads/main | cut -f1 >> \"$HOME/mains\"\n\
fi\n\
if [ -f \"$HOME/move-main\" ]; then\n\
  mover=$(cat \"$HOME/move-main\")\n\
  rm \"$HOME/move-main\"\n\
  cd \"$mover\" || exit 1\n\
  git fetch -q origin main >/dev/null 2>&1 && git checkout -q FETCH_HEAD >/dev/null 2>&1 || exit 1\n\
  echo moved > moved.md\n\
  git add moved.md >/dev/null 2>&1 || exit 1\n\
  git -c user.name=mover -c user.email=mover@example.invalid commit -q -m moved >/dev/null 2>&1 || exit 1\n\
  git push -q origin HEAD:main >/dev/null 2>&1 || exit 1\n\
fi\n\
printf '{\"schema\":\"bay.wake-result/1\",\"result\":\"woken\",\"reason\":null}'\n";

/// Every `red_owner` and `red_undo` note in a pass's result, for a failure's message.
fn red_notes(value: &Value, out: &mut Vec<Value>) {
    match value {
        Value::Object(map) => {
            if map.contains_key("red_undo") || map.contains_key("red_owner") {
                out.push(value.clone());
            }
            map.values().for_each(|inner| red_notes(inner, out));
        }
        Value::Array(items) => items.iter().for_each(|inner| red_notes(inner, out)),
        _ => {}
    }
}

#[test]
fn a_culprit_is_undone_in_the_pass_that_finds_it_due_when_main_moved_under_it() {
    // GIVEN a red the bisection pins on the fourth landing, undone at once, and a clone that
    // lands on main while the next pass tells the culprit's session
    let trunk = trunk_with(
        "[runner]\nbisect_post = true\n\n[plugin.wake]\ncommand = \"scripts/bay-wake\"\n\n[red]\nundo_after_s = 0\n",
        &[("scripts/bay-wake", WAKE)],
    );
    let landings = trunk.red_over_four();
    let culprit = landings[3].0.as_str();
    let report = trunk.executor_pass().expect("the executor bisects");
    assert_eq!(report[0]["outcome"], "culprit", "{report}");
    let mover = trunk.test_box.clone_of(&trunk.remote, "mover");
    let marker = trunk.test_box.home.join("move-main");
    std::fs::write(&marker, mover.to_str().unwrap()).unwrap();

    // WHEN one pass tells it, and main moves before the pass decides
    let ran = trunk
        .test_box
        .bay_ok(&trunk.repo, &["runner", "run", "--once"]);
    let mut notes = Vec::new();
    red_notes(&ran, &mut notes);

    // THEN main moved under the pass, and that pass opened the culprit's undo
    assert!(!marker.exists(), "the wake never ran: {notes:?}");
    let tip = trunk
        .test_box
        .git_bare(&trunk.remote, &["log", "-1", "--format=%s", "main"]);
    assert_eq!(tip.trim(), "moved", "{notes:?}");
    let red = trunk.red();
    assert_eq!(red[0]["culprit"]["change"], culprit, "{red}\n{notes:?}");
    assert!(
        red[0]["undo"]["change"].is_string(),
        "the pass opened no undo of {culprit}: {red}\n{notes:?}"
    );
}

/// Submits a change that writes `path`, outside `src/`, for a runner to land.
fn submit(trunk: &Trunk, path: &str) {
    let started = trunk
        .test_box
        .bay_ok(&trunk.repo, &["start", path, "--paths", "docs"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::create_dir_all(view.join("docs")).unwrap();
    std::fs::write(view.join(path), format!("{path}\n")).unwrap();
    let (code, queued) = result_json(trunk.test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
}

/// Main's commit on the remote.
fn main_tip(trunk: &Trunk) -> String {
    trunk
        .test_box
        .git_bare(&trunk.remote, &["rev-parse", "main"])
        .trim()
        .to_owned()
}

#[test]
fn a_pass_tells_a_culprit_between_the_heads_it_takes() {
    // GIVEN a red the bisection pins on the fourth landing, and two changes in line
    let trunk = trunk_with(
        "[runner]\nbisect_post = true\n\n[plugin.wake]\ncommand = \"scripts/bay-wake\"\n",
        &[("scripts/bay-wake", WAKE)],
    );
    let landings = trunk.red_over_four();
    let report = trunk.executor_pass().expect("the executor bisects");
    assert_eq!(report[0]["outcome"], "culprit", "{report}");
    let home = trunk.test_box.home.clone();
    std::fs::write(home.join("remote"), trunk.remote.to_str().unwrap()).unwrap();
    submit(&trunk, "docs/a.md");
    submit(&trunk, "docs/b.md");
    let before = main_tip(&trunk);

    // WHEN one pass takes both heads, and does its red work at once after each
    let (code, ran) = result_json(
        trunk
            .test_box
            .bay(&trunk.repo)
            .env("BAY_TEST_RED_EVERY_S", "0")
            .args(["runner", "run", "--once"]),
    );
    assert_eq!(code, 0, "{ran}");

    // THEN both landed, and the culprit's session was told between them: main held the first
    // and not yet the second
    let after = main_tip(&trunk);
    let mains = std::fs::read_to_string(home.join("mains")).unwrap_or_default();
    let told: Vec<&str> = mains.lines().collect();
    assert_eq!(
        told.len(),
        1,
        "one tell, of {}: {mains:?}\n{ran}",
        landings[3].0
    );
    assert_ne!(told[0], before, "told before any head landed: {ran}");
    assert_ne!(told[0], after, "told only after both heads landed: {ran}");
    let ahead = trunk.test_box.git_bare(
        &trunk.remote,
        &["rev-list", "--count", &format!("{before}..{after}")],
    );
    assert_eq!(ahead.trim(), "2", "one landing each: {ran}");
}
