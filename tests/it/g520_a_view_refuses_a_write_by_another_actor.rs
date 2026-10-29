//! G520 (B4, a fleet finding, 2026-10-28). A view refuses a write by another actor.
//!
//! At 20:54Z one seat committed in another seat's view, and the commit would have merged under
//! the view owner's change: a view was a plain folder, and nothing read who it was for. Now a
//! view records its actor (the one who made it, in its journal beside it), and a write there by
//! any other actor is refused `VIEW_NOT_YOURS`, naming the view's actor: `bay merge` (in the view,
//! or naming its change), `bay save` and `bay start <change>` refuse before they write, and the
//! agent hook denies an edit, or a `git commit` (also after a `cd` in the same line, or with
//! `git -C`), in the view. `bay start <change> --adopt` is the one way to take a view over: it
//! adopts the change as `bay start <intent>` does, records the new actor and names the one
//! before. A view an older bay made records no actor and refuses no one.
//!
//! With no `BAY_ACTOR` the actor is the user name, as bay attributes every other act: a person at
//! a shell is that name, refused in a seat's view and served in a view of their own. Sessions that
//! share one account and set no `BAY_ACTOR` are one actor to bay, so they are not told apart
//! (every other test of this suite runs so, as `agent`).
//!
//! The guards to mutate (each makes a case here fail):
//! - `src/view.rs` `refused_for`: an actor equal to the view's is the only one served (always
//!   `None`: the merge, save and start cases fail).
//! - `src/view.rs` `not_yours` and `actor_at`: a journal with no actor refuses no one (an
//!   unrecorded view refused: the older view case fails).
//! - `src/publish.rs` `submit_land`: the view's refusal before the freeze (removed: the merge
//!   case fails).
//! - `src/save.rs` `save_in`: the view's refusal in a view (removed: the save case fails).
//! - `src/start.rs` `resume`: the owner's check before a checkpoint (removed: the start case
//!   fails).
//! - `src/start.rs` `adopt_or_refuse`: the view's take-over (removed: the adopter's merge is
//!   refused).
//! - `src/agent_hook.rs` `pre_tool`: the view's judgement (removed: the hook cases fail).
//! - `src/deny.rs` `commit_dirs_in`: a literal `cd` moves the commit's directory (removed: the
//!   `cd … && git commit` case is allowed).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{Value, json};
use support::{TestBox, result_json, unchecked_files};

/// Who runs a command: a named actor (`BAY_ACTOR`), or a person at a shell (only `USER`).
#[derive(Clone, Copy)]
enum Who<'a> {
    Actor(&'a str),
    Person(&'a str),
}

impl Who<'_> {
    fn apply(self, cmd: &mut assert_cmd::Command) -> &mut assert_cmd::Command {
        match self {
            Who::Actor(name) => cmd.env("BAY_ACTOR", name),
            Who::Person(user) => cmd.env("USER", user),
        }
    }

    fn apply_process(self, cmd: &mut std::process::Command) -> &mut std::process::Command {
        match self {
            Who::Actor(name) => cmd.env("BAY_ACTOR", name),
            Who::Person(user) => cmd.env("USER", user),
        }
    }
}

fn setup() -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    (test_box, repo, remote)
}

fn bay(test_box: &TestBox, who: Who<'_>, dir: &Path, args: &[&str]) -> (i32, Value) {
    let mut cmd = test_box.bay(dir);
    who.apply(&mut cmd);
    result_json(cmd.args(args))
}

/// `bay start <intent> --paths src/` as `who`, with one new file written in its view.
fn start(test_box: &TestBox, repo: &Path, who: Who<'_>, intent: &str) -> (String, PathBuf) {
    let (code, started) = bay(test_box, who, repo, &["start", intent, "--paths", "src/"]);
    assert_eq!(code, 0, "{started}");
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(
        view.join(format!("src/{}.rs", intent.to_lowercase())),
        "fn x() {}\n",
    )
    .unwrap();
    (started["change"].as_str().unwrap().to_owned(), view)
}

fn assert_not_yours(code: i32, refused: &Value, view_actor: &str, actor: &str) {
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "VIEW_NOT_YOURS", "{refused}");
    assert_eq!(refused["detail"]["view_actor"], view_actor, "{refused}");
    assert_eq!(refused["detail"]["actor"], actor, "{refused}");
    assert!(
        refused["next_step"]
            .as_str()
            .is_some_and(|step| step.contains("--adopt")),
        "{refused}"
    );
}

/// The change record without its receipts, and the reasons of those receipts. A refused merge
/// appends a receipt of the refusal to the chain, and nothing else.
fn record(test_box: &TestBox, repo: &Path, id: &str) -> (Value, Vec<String>) {
    let text = test_box.git_ok(
        repo,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    let mut record: Value = serde_json::from_str(&text).unwrap();
    let receipts = record
        .as_object_mut()
        .unwrap()
        .remove("receipts")
        .unwrap_or_default();
    let reasons = receipts
        .as_array()
        .into_iter()
        .flatten()
        .map(|receipt| receipt["reason"].as_str().unwrap_or_default().to_owned())
        .collect();
    (record, reasons)
}

/// The value of the `Actor` trailer on the remote's main.
fn actor_trailer(test_box: &TestBox, remote: &Path) -> String {
    test_box.git_bare(
        remote,
        &[
            "log",
            "-1",
            "--format=%(trailers:key=Actor,valueonly)",
            "main",
        ],
    )
}

#[test]
fn a_merge_save_or_start_in_another_actors_view_is_refused_and_its_actor_merges() {
    let (test_box, repo, remote) = setup();
    let trunk = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let (id, view) = start(&test_box, &repo, Who::Actor("seat-a"), "CARD-1");
    let (before, _) = record(&test_box, &repo, &id);

    // In the view, and naming its change from the main checkout: nothing is submitted.
    let (code, refused) = bay(&test_box, Who::Actor("seat-b"), &view, &["merge", "--wait"]);
    assert_not_yours(code, &refused, "seat-a", "seat-b");
    assert_eq!(refused["change"], id.as_str(), "{refused}");
    let (code, refused) = bay(&test_box, Who::Actor("seat-b"), &repo, &["merge", &id]);
    assert_not_yours(code, &refused, "seat-a", "seat-b");
    // A save in the view, and a pick-up of its change, refuse the same way.
    let (code, refused) = bay(&test_box, Who::Actor("seat-b"), &view, &["save", "CARD-2"]);
    assert_not_yours(code, &refused, "seat-a", "seat-b");
    let (code, refused) = bay(&test_box, Who::Actor("seat-b"), &repo, &["start", &id]);
    assert_not_yours(code, &refused, "seat-a", "seat-b");
    // Nothing was frozen, queued or taken over: the chain holds only receipts of the refusals.
    let (after, reasons) = record(&test_box, &repo, &id);
    assert_eq!(after, before, "nothing was written");
    assert!(
        reasons.iter().all(|reason| reason == "VIEW_NOT_YOURS"),
        "{reasons:?}"
    );
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), trunk);

    // The view's own actor merges it.
    let (code, merged) = bay(&test_box, Who::Actor("seat-a"), &view, &["merge", "--wait"]);
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
    assert_eq!(actor_trailer(&test_box, &remote), "seat-a");
}

#[test]
fn adopt_takes_the_view_over_and_names_the_actor_before() {
    let (test_box, repo, remote) = setup();
    let (id, view) = start(&test_box, &repo, Who::Actor("seat-a"), "CARD-1");

    // seat-a goes idle; after the takeover window seat-b takes its change and its view over.
    let (code, adopted) = bay(
        &test_box,
        Who::Actor("seat-b"),
        &repo,
        &["start", &id, "--adopt"],
    );
    assert_eq!(code, 0, "{adopted}");
    assert_eq!(adopted["change"], id.as_str(), "{adopted}");
    assert_eq!(adopted["detail"]["adopted"], true, "{adopted}");
    assert_eq!(
        adopted["detail"]["adopted_from"]["name"], "seat-a",
        "{adopted}"
    );
    assert_eq!(
        adopted["detail"]["view_adopted_from"], "seat-a",
        "{adopted}"
    );
    assert_eq!(adopted["detail"]["actor"]["name"], "seat-b", "{adopted}");
    assert_eq!(
        PathBuf::from(adopted["detail"]["view"].as_str().unwrap()),
        view,
        "{adopted}"
    );

    // The actor before is now the other one: told its change was adopted, never merged as it.
    let (code, told) = bay(&test_box, Who::Actor("seat-a"), &view, &["merge"]);
    assert_eq!(code, 10, "{told}");
    assert_eq!(told["detail"]["adopted_by"]["name"], "seat-b", "{told}");

    // The new actor merges the view, seat-a's work in it too, under its own name.
    let (code, merged) = bay(&test_box, Who::Actor("seat-b"), &view, &["merge", "--wait"]);
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
    assert_eq!(actor_trailer(&test_box, &remote), "seat-b");
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:src/card-1.rs"]),
        "fn x() {}"
    );

    // `--adopt` names a change, never an intent.
    let (code, refused) = bay(
        &test_box,
        Who::Actor("seat-b"),
        &repo,
        &["start", "CARD-9", "--adopt"],
    );
    assert_eq!(code, 20, "{refused}");
    assert!(refused.to_string().contains("--adopt"), "{refused}");
}

/// The journal beside the view of change `id`, as JSON.
fn journal(view: &Path, id: &str) -> PathBuf {
    view.parent().unwrap().join(format!("{id}.journal"))
}

#[test]
fn a_view_an_older_bay_made_refuses_no_one() {
    let (test_box, repo, _remote) = setup();
    let (id, view) = start(&test_box, &repo, Who::Actor("seat-a"), "CARD-1");
    // An older bay's journal: no actor in it.
    let file = journal(&view, &id);
    let mut text: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(
        text["actor"], "seat-a",
        "the journal records the view's actor: {text}"
    );
    text.as_object_mut().unwrap().remove("actor");
    std::fs::write(&file, text.to_string()).unwrap();

    let written = view.join("src/b.rs");
    let (exit, stderr) = hook(
        &test_box,
        Who::Actor("seat-b"),
        &repo,
        &write_tool("s-b", &repo, &written),
    );
    assert_eq!(exit, Some(0), "{stderr}");
    let (code, merged) = bay(&test_box, Who::Actor("seat-b"), &view, &["merge", "--wait"]);
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
}

#[test]
fn with_no_bay_actor_the_user_name_is_the_actor() {
    let (test_box, repo, remote) = setup();
    // A person at a shell is their user name: their view is theirs alone.
    let (_, own) = start(&test_box, &repo, Who::Person("alice"), "CARD-1");
    let (code, refused) = bay(&test_box, Who::Actor("seat-b"), &own, &["merge"]);
    assert_not_yours(code, &refused, "alice", "seat-b");
    assert_eq!(refused["detail"]["actor_source"], "env", "{refused}");
    // And a seat's view is not theirs.
    let (_, seat) = start(&test_box, &repo, Who::Actor("seat-a"), "CARD-2");
    let (code, refused) = bay(&test_box, Who::Person("alice"), &seat, &["merge"]);
    assert_not_yours(code, &refused, "seat-a", "alice");
    assert_eq!(
        refused["detail"]["actor_source"], "user-default",
        "{refused}"
    );
    // A second shell of the same person is the same actor.
    let (code, merged) = bay(&test_box, Who::Person("alice"), &own, &["merge", "--wait"]);
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
    assert_eq!(actor_trailer(&test_box, &remote), "alice");
}

/// `bay hook tool.before` as `who`, with `payload` on stdin; returns (exit, stderr).
fn hook(test_box: &TestBox, who: Who<'_>, cwd: &Path, payload: &Value) -> (Option<i32>, String) {
    let mut cmd = test_box.bay_process(cwd);
    who.apply_process(&mut cmd);
    let mut child = cmd
        .args(["hook", "tool.before"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A `PreToolUse` payload of `tool` with `input`, in the recorded shape (fixture 03).
fn pre(session: &str, cwd: &Path, tool: &str, input: &Value) -> Value {
    json!({
        "session_id": session,
        "hook_event_name": "PreToolUse",
        "tool_name": tool,
        "tool_input": input,
        "cwd": cwd,
    })
}

fn write_tool(session: &str, cwd: &Path, file: &Path) -> Value {
    pre(
        session,
        cwd,
        "Write",
        &json!({"file_path": file, "content": "x"}),
    )
}

fn bash(session: &str, cwd: &Path, command: &str) -> Value {
    pre(session, cwd, "Bash", &json!({"command": command}))
}

#[test]
fn the_hook_denies_an_edit_or_a_git_commit_in_another_actors_view() {
    let (test_box, repo, _remote) = setup();
    let (id, view) = start(&test_box, &repo, Who::Actor("seat-a"), "CARD-1");
    let file = view.join("src/b.rs");
    let denied = |who: Who<'_>, cwd: &Path, payload: &Value| {
        let (exit, stderr) = hook(&test_box, who, cwd, payload);
        assert_eq!(exit, Some(2), "{payload}: {stderr}");
        assert!(stderr.contains("VIEW_NOT_YOURS"), "{stderr}");
        assert!(stderr.contains("seat-a"), "{stderr}");
        assert!(
            stderr.contains(&format!("bay start {id} --adopt")),
            "{stderr}"
        );
    };
    let allowed = |who: Who<'_>, cwd: &Path, payload: &Value| {
        let (exit, stderr) = hook(&test_box, who, cwd, payload);
        assert_eq!(exit, Some(0), "{payload}: {stderr}");
    };

    // An edit of a file in the view, from the main checkout or from the view itself.
    let b = Who::Actor("seat-b");
    denied(b, &repo, &write_tool("s-b", &repo, &file));
    denied(b, &view, &write_tool("s-b", &view, Path::new("src/b.rs")));
    // A commit in the view: run there, after a `cd` to it, or with `git -C`.
    let view_text = view.display().to_string();
    denied(b, &view, &bash("s-b", &view, "git commit -qm x"));
    denied(
        b,
        &repo,
        &bash(
            "s-b",
            &repo,
            &format!("cd {view_text} && git add -A && git commit -qm x"),
        ),
    );
    denied(
        b,
        &repo,
        &bash("s-b", &repo, &format!("git -C {view_text} commit -qm x")),
    );
    denied(
        b,
        &repo,
        &bash(
            "s-b",
            &repo,
            &format!("bash -c 'cd {view_text}; git commit -qm x'"),
        ),
    );
    // A person at a shell's session is no other.
    denied(
        Who::Person("alice"),
        &repo,
        &write_tool("s-p", &repo, &file),
    );

    // Reading in the view, and a commit elsewhere, are not judged.
    allowed(b, &view, &bash("s-b", &view, "git status"));
    allowed(b, &repo, &bash("s-b", &repo, "git commit -qm x"));
    // The view's own actor is served.
    let a = Who::Actor("seat-a");
    allowed(a, &repo, &write_tool("s-a", &repo, &file));
    allowed(a, &view, &bash("s-a", &view, "git commit -qm x"));
}
