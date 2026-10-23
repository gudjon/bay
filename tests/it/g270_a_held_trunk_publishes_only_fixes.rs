//! G270 (N-8): a held trunk publishes only fixes.
//!
//! A team sometimes must hold its trunk: an incident, an infrastructure migration, a lock
//! outside the repository. A `[[hold]]` whose `target` is the trunk's own name holds it until
//! its `end`; the hold adapter (`[plugin.hold] command`) holds it while it answers `held`. While trunk
//! is held, a landing that carries neither `Fixes:` nor `Reverts:` publishes nothing: it stays
//! queued with `MAIN_HELD`, naming the hold, and trunk does not move. A `--fixes` change lands
//! through the hold. Once the hold clears, the queued change lands. An adapter that answers
//! anything but its JSON is never read as clear: unknown is held, and the reason names the
//! fault. `bay status` names the hold under `detail.hold`.
//!
//! The guards to mutate, each of which turns a case red:
//!
//! - `hold_gate` in `src/publish.rs`, its `let Some(hold) = crate::hold::held(..) else`: return
//!   `Ok(None)` always, and the held change lands during the freeze;
//! - `crate::hold::passes` in `src/hold.rs`: answer `false`, and the `--fixes` change is held;
//! - `crate::hold::held`, `Err(fault) => Some(Hold { .. })`: answer `None` on a fault, and the
//!   change lands on the adapter's garbage;
//! - `Config::validate_targets` in `src/config.rs`, `freeze.target != trunk &&`: drop it, and a
//!   freeze of the trunk is refused as naming no `[[target]]`, so bay cannot read the config.

// Each case runs shell scripts (the hold adapter), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::talk::from_now;
use support::{TestBox, result_json, unchecked_files, write_executable};

const CONFIG: &str = "main = \"main\"\n";

struct Trunk {
    test_box: TestBox,
    repo: PathBuf,
}

/// A trunk under an unchecked policy and `config`, with `adapter` (an executable script) at
/// `scripts/bay-hold` when given.
fn trunk(test_box: TestBox, config: &str, adapter: Option<&str>) -> Trunk {
    let repo = test_box.repo_any("repo");
    // An adapter is committed executable: it is written so before the seed adds it.
    if let Some(script) = adapter {
        let full = repo.join("scripts/bay-hold");
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        write_executable(&full, script);
    }
    let mut files = unchecked_files();
    files.push((".bay/config.toml".to_owned(), config.to_owned()));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    Trunk { test_box, repo }
}

impl Trunk {
    /// Starts a change writing `path` (`--fixes <fixes>` when given). Returns (id, view).
    fn start(&self, path: &str, fixes: Option<&str>) -> (String, PathBuf) {
        let intent = format!("CARD-{}", path.replace(['/', '.'], "-"));
        let mut args = vec!["start", intent.as_str(), "--paths", "src/"];
        if let Some(fixed) = fixes {
            args.extend(["--fixes", fixed]);
        }
        let started = self.test_box.bay_ok(&self.repo, &args);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(path), format!("// {path}\n")).unwrap();
        (id, view)
    }

    /// `bay merge --wait` in `view`, which exits 0 for a landing and for a queued change alike.
    fn land(&self, view: &Path) -> Value {
        let (code, result) = result_json(self.test_box.bay(view).args(["merge", "--wait"]));
        assert_eq!(code, 0, "{result}");
        result
    }

    /// The trunk's tip on the remote.
    fn tip(&self) -> String {
        let listed = self
            .test_box
            .git_ok(&self.repo, &["ls-remote", "origin", "refs/heads/main"]);
        listed
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned()
    }

    fn hold(&self) -> Value {
        self.test_box.bay_ok(&self.repo, &["status"])["detail"]["hold"].clone()
    }
}

/// Asserts `result` is the change waiting on a held trunk, and returns its `detail.hold`.
fn held(result: &Value) -> Value {
    assert_eq!(result["decision"], "queued", "{result}");
    assert_eq!(result["reason"], "MAIN_HELD", "{result}");
    assert_eq!(result["allowed"], serde_json::json!(["wait"]), "{result}");
    assert_eq!(result["retry_when"], "after_person", "{result}");
    result["detail"]["hold"].clone()
}

fn freeze(end: &str) -> String {
    format!(
        "{CONFIG}\n[[hold]]\ntarget = \"main\"\nend = \"{end}\"\nreason = \"database migration\"\n"
    )
}

#[test]
fn a_trunk_freeze_holds_a_plain_landing_and_lets_a_fix_through() {
    let trunk = trunk(TestBox::new(), CONFIG, None);
    let (broke, view) = trunk.start("src/broke.rs", None);
    let landed = trunk.land(&view);
    assert_eq!(landed["decision"], "merged", "{landed}");

    let end = from_now(3600);
    trunk.test_box.commit_config(&trunk.repo, &freeze(&end));
    let before = trunk.tip();

    // A plain change waits: queued, naming the freeze, and trunk does not move.
    let (_, plain) = trunk.start("src/plain.rs", None);
    let waiting = trunk.land(&plain);
    let hold = held(&waiting);
    assert_eq!(hold["source"], "window", "{waiting}");
    assert_eq!(hold["reason"], "database migration", "{waiting}");
    assert_eq!(hold["end"], end.as_str(), "{waiting}");
    assert_eq!(trunk.tip(), before, "a held trunk takes no plain landing");
    let shown = trunk.hold();
    assert_eq!(shown["source"], "window", "{shown}");
    assert_eq!(shown["end"], end.as_str(), "{shown}");

    // Waiting again changes nothing while the freeze is in force, and counts toward no limit.
    let again = trunk.land(&plain);
    held(&again);
    assert_eq!(trunk.tip(), before);

    // A fix lands through the freeze.
    let (_, fix) = trunk.start("src/fix.rs", Some(&broke));
    let fixed = trunk.land(&fix);
    assert_eq!(fixed["decision"], "merged", "{fixed}");
    assert_ne!(trunk.tip(), before, "the fix moved trunk");

    // Once the freeze has ended, the queued change lands.
    let ended = bay::clock::rfc3339_of(bay::live::unix_now() - 60);
    trunk.test_box.commit_config(&trunk.repo, &freeze(&ended));
    assert_eq!(trunk.hold(), Value::Null, "an ended freeze holds nothing");
    let cleared = trunk.land(&plain);
    assert_eq!(cleared["decision"], "merged", "{cleared}");
    assert_eq!(
        trunk.tip(),
        cleared["detail"]["commit"].as_str().unwrap(),
        "{cleared}"
    );
}

#[test]
fn the_hold_adapter_holds_trunk_and_an_unreadable_answer_is_held() {
    // The answer lives outside the repository, so the test changes it with no commit; the
    // adapter itself is read from trunk, and asked with `status`.
    let test_box = TestBox::new();
    let answer = test_box.path().join("hold-answer");
    let say = |text: &str| std::fs::write(&answer, text).unwrap();
    say("{\"held\": true, \"reason\": \"incident 7\"}\n");
    let script = format!(
        "#!/bin/sh\n[ \"$1\" = status ] || exit 2\ncat '{}'\n",
        answer.display()
    );
    let config =
        format!("{CONFIG}\n[plugin.hold]\ncommand = \"scripts/bay-hold\"\ntimeout_s = 10\n");
    let trunk = trunk(test_box, &config, Some(&script));
    let before = trunk.tip();

    // Held: the change waits, naming the adapter's reason.
    let (_, view) = trunk.start("src/plain.rs", None);
    let waiting = trunk.land(&view);
    let hold = held(&waiting);
    assert_eq!(hold["source"], "plugin", "{waiting}");
    assert_eq!(hold["reason"], "incident 7", "{waiting}");
    assert_eq!(trunk.tip(), before);
    assert_eq!(trunk.hold()["reason"], "incident 7");

    // An answer bay cannot read is held, never clear, and the reason names the fault.
    say("all clear, probably\n");
    let garbage = trunk.land(&view);
    let hold = held(&garbage);
    assert_eq!(hold["source"], "plugin", "{garbage}");
    let reason = hold["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("printed no JSON answer"), "{garbage}");
    assert_eq!(trunk.tip(), before);

    // Not held: the change lands.
    say("{\"held\": false, \"reason\": \"\"}\n");
    assert_eq!(trunk.hold(), Value::Null);
    let landed = trunk.land(&view);
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert_eq!(trunk.tip(), landed["detail"]["commit"].as_str().unwrap());
}
