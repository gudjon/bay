//! G267. A check reads the base and history, but never writes it.
//!
//! Every `bay.check.request/1` names the accepted base the candidate was merged onto (`base`),
//! the change it lands (`change`: its id, intent, actor and revision) and that change's own
//! commit messages (`messages`, oldest first, capped, with `messages_truncated`). A check whose
//! `[[check]]` entry declares `inputs = ["history"]` also gets `git`: a temporary bare repository
//! of its own run whose alternates name the repository's object directory, so
//! `GIT_DIR=<git.dir> git log <base>` works inside the adapter. What the adapter writes through
//! it stays in its scratch, and the object directory is bound read-only in the sandbox, so a
//! write aimed straight at the store fails and the object never appears in the repository.
//! Without `history` the request has no `git`.
//!
//! The guards to mutate: in `src/check/sandbox.rs`, `bwrap_args` binds each `read_only`
//! directory with `"--ro-bind"` (make it `"--bind"`, and the sandboxed case finds the object it
//! wrote in the real store); in `src/check.rs`, `request` inserts `"git"` only from
//! `place.history`, and `about` inserts `"base"`, `"change"` and `"messages"` (drop any, and
//! the request assertions fail). The read-only part needs the check sandbox; on a machine
//! without one it is skipped, unless `BAY_REQUIRE_SANDBOX` is set.

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// What the adapter does inside its sandbox before it passes: with a `git` in its request, it
/// runs `git log` on the base through `git.dir`, writes one object through `git.dir` (its own
/// scratch) and tries to write another straight into `git.object_dir` (the real store). Each
/// answer, or `none` without `git`, becomes an argument of the relay. Git's errors go nowhere:
/// bay reads the check's stderr for a path outside its sandbox.
const IN_THE_SANDBOX: &str = r#"gitdir=$(printf '%s' "$req" | sed -n 's/.*"git":{"dir":"\([^"]*\)".*/\1/p')
objdir=$(printf '%s' "$req" | sed -n 's/.*"object_dir":"\([^"]*\)".*/\1/p')
base=$(printf '%s' "$req" | sed -n 's/.*"base":{"commit":"\([0-9a-f]*\)".*/\1/p')
logged=none; own=none; real=none
if [ -n "$gitdir" ]; then
  logged=$(GIT_DIR="$gitdir" git log --format=%H -1 "$base" 2>/dev/null) || logged=failed
  own=$(printf 'g267 through the history repository\n' | GIT_DIR="$gitdir" git hash-object -w --stdin 2>/dev/null) || own=failed
  real=$(printf 'g267 straight into the store\n' | GIT_DIR="$gitdir" GIT_OBJECT_DIRECTORY="$objdir" git hash-object -w --stdin 2>/dev/null) || real=refused
fi
set -- "$logged" "$own" "$real""#;

/// One landing: the box, the repository, the remote, the change id, the trunk commit it was
/// merged onto, the request the adapter was handed and the adapter's three answers.
struct Landed {
    test_box: TestBox,
    repo: PathBuf,
    remote: PathBuf,
    change: String,
    trunk: String,
    request: Value,
    answers: Vec<String>,
}

/// Lands one change whose check runs [`IN_THE_SANDBOX`], with `extra` appended to its
/// `[[check]]` entry.
fn land(extra: &str) -> Landed {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let seen = test_box.path().join("request.json");
    let found = test_box.path().join("found.txt");
    // The relay runs as the test, outside the sandbox: it keeps the request and the answers.
    let relay = test_box.relay(&format!(
        "printf '%s' \"$req\" > '{}'; printf '%s\\n%s\\n%s\\n' \"$1\" \"$2\" \"$3\" > '{}'",
        seen.display(),
        found.display()
    ));
    let mut files = check_files(
        &format!("{IN_THE_SANDBOX}\n{relay}"),
        &passing_result(1),
        1,
        60,
    );
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(extra);
        }
    }
    let trunk = test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (change, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "landed", "{landed}");
    let request: Value = serde_json::from_str(&std::fs::read_to_string(&seen).unwrap()).unwrap();
    let answers = std::fs::read_to_string(&found)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    Landed {
        test_box,
        repo,
        remote,
        change,
        trunk,
        request,
        answers,
    }
}

/// Whether the object store of the repository whose git directory is `git_dir` holds `object`.
fn holds(test_box: &TestBox, git_dir: &Path, object: &str) -> bool {
    test_box
        .git(test_box.path())
        .arg(format!("--git-dir={}", git_dir.display()))
        .args(["cat-file", "-e", object])
        .output()
        .unwrap()
        .status
        .success()
}

/// The request names the base, the change and its messages, whether or not it reads history.
fn assert_names_the_base_and_the_change(landed: &Landed) {
    let Landed {
        test_box,
        repo,
        remote,
        change,
        trunk,
        request,
        ..
    } = landed;
    let published = test_box.git_bare(remote, &["rev-parse", "main"]);
    // The base is the trunk commit the candidate was merged onto, which the landing sits on.
    assert_eq!(request["base"]["commit"], trunk.as_str(), "{request}");
    assert_eq!(
        test_box.git_bare(remote, &["rev-parse", "main^"]),
        trunk.as_str()
    );
    assert_eq!(
        request["base"]["tree"],
        test_box.git_ok(repo, &["rev-parse", &format!("{trunk}^{{tree}}")]),
        "{request}"
    );
    // The change and its actor, as the landing's trailers name them.
    assert_eq!(request["change"]["id"], change.as_str(), "{request}");
    let actor = test_box.git_bare(
        remote,
        &[
            "log",
            "-1",
            "--format=%(trailers:key=Actor,valueonly)",
            &published,
        ],
    );
    assert!(!actor.is_empty(), "the landing names no actor");
    assert_eq!(request["change"]["actor"], actor.as_str(), "{request}");
    // The change's own draft commits, oldest first, each with its message as Git holds it.
    let messages = request["messages"].as_array().expect("messages");
    assert!(!messages.is_empty(), "{request}");
    assert_eq!(request["messages_truncated"], false, "{request}");
    assert_eq!(
        messages.last().unwrap()["commit"],
        request["change"]["revision"],
        "{request}"
    );
    for message in messages {
        let commit = message["commit"].as_str().unwrap();
        assert_eq!(
            message["message"].as_str().unwrap().trim_end(),
            test_box.git_ok(repo, &["log", "-1", "--format=%B", commit]),
            "{request}"
        );
    }
    // bay freezes a view into a draft commit of that message.
    assert!(
        matches!(
            messages.last().unwrap()["message"].as_str().unwrap().trim(),
            "draft" | "checkpoint"
        ),
        "{request}"
    );
}

#[test]
fn a_history_check_reads_the_base_and_history_and_never_writes_the_store() {
    let landed = land("inputs = [\"history\"]\n");
    assert_names_the_base_and_the_change(&landed);
    let Landed {
        test_box,
        repo,
        remote,
        trunk,
        request,
        answers,
        ..
    } = &landed;
    assert!(request["git"]["dir"].is_string(), "{request}");
    assert!(request["git"]["object_dir"].is_string(), "{request}");
    assert_eq!(answers.len(), 3, "{answers:?}");
    // Inside the adapter, `git log` on the base through `git.dir` reads the repository's history.
    assert_eq!(answers[0], trunk.as_str(), "{answers:?}\n{request}");
    // What the adapter writes through `git.dir` stays in its own scratch.
    let git_dir = repo.join(".git");
    assert!(holds(test_box, &git_dir, trunk), "the store cannot be read");
    assert!(holds(test_box, remote, trunk), "the remote cannot be read");
    let own = &answers[1];
    assert!(
        own.len() >= 40 && own.chars().all(|c| c.is_ascii_hexdigit()),
        "the history repository took no write of the check's own: {answers:?}"
    );
    assert!(
        !holds(test_box, &git_dir, own),
        "an object the check wrote reached the repository"
    );
    assert!(!holds(test_box, remote, own));
    let Some(sandbox) = support::sandbox() else {
        eprintln!("no check sandbox on this machine; G267's read-only store cannot run");
        return;
    };
    // A write aimed straight at the store fails, and its object is not in the repository.
    let content = test_box.path().join("straight.txt");
    std::fs::write(&content, "g267 straight into the store\n").unwrap();
    let object = test_box.git_ok(repo, &["hash-object", content.to_str().unwrap()]);
    assert_eq!(
        answers[2], "refused",
        "the check wrote into the store under {sandbox}: {answers:?}"
    );
    assert!(
        !holds(test_box, &git_dir, &object),
        "the check's write reached the store under {sandbox}"
    );
}

#[test]
fn a_check_without_history_gets_the_base_and_the_change_but_no_git() {
    let landed = land("");
    assert_names_the_base_and_the_change(&landed);
    let Landed {
        request, answers, ..
    } = &landed;
    assert!(request.get("git").is_none(), "{request}");
    assert_eq!(answers, &["none", "none", "none"], "{answers:?}");
}
