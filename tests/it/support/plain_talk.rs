//! A plain Git remote, as a forge is (no bay hook judges a push), and three clones, for the talk
//! G-tests of the sender side (G460-G469): `agent` on `a`, `agent-b` on `b`, `agent-c` on `c`.
//! The policy on main turns talk on (`[talk] enabled = true`), as 0.6.0 requires.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{TestBox, UNCHECKED_POLICY, result_json};

/// The policy's `[talk]` table, talk on.
pub const TALK_ON: &str = "\n[talk]\nenabled = true\n";
/// The `[policy]` table of a repository that signs approvals only (N-21, G445).
pub const APPROVALS: &str = "\n[policy]\nsigning = \"approvals\"\n";

pub struct Plain {
    pub test_box: TestBox,
    pub remote: PathBuf,
    pub a: PathBuf,
    pub b: PathBuf,
    pub c: PathBuf,
}

impl Plain {
    /// The remote, seeded on `a` with an unchecked policy followed by `tables`, and clones `b`
    /// and `c`.
    #[must_use]
    pub fn new(tables: &str) -> Self {
        let test_box = TestBox::new();
        let remote = test_box.bare_any("remote.git");
        let a = test_box.repo_without_remote_any("a");
        test_box.add_origin(&a, &remote);
        test_box.seed_with_check(
            &a,
            &[("src/a.rs", "fn a() {}\n")],
            &[(
                ".bay/policy.toml".to_owned(),
                format!("{UNCHECKED_POLICY}{tables}"),
            )],
        );
        let b = test_box.clone_of(&remote, "b");
        let c = test_box.clone_of(&remote, "c");
        Self {
            test_box,
            remote,
            a,
            b,
            c,
        }
    }

    /// Talk on, under `signing = "approvals"`: no key registry anywhere.
    #[must_use]
    pub fn approvals() -> Self {
        Self::new(&format!("{TALK_ON}{APPROVALS}"))
    }

    /// The clone each actor runs on.
    #[must_use]
    pub fn clone_of(&self, actor: &str) -> &Path {
        match actor {
            "agent" => &self.a,
            "agent-b" => &self.b,
            "agent-c" => &self.c,
            other => panic!("no clone for {other}"),
        }
    }

    /// `bay <args>` as `actor`, in session `s-<actor>`: the exit code and the result.
    pub fn bay(&self, actor: &str, args: &[&str]) -> (i32, Value) {
        self.bay_in(actor, &format!("s-{actor}"), args)
    }

    /// [`Self::bay`] in session `session`.
    pub fn bay_in(&self, actor: &str, session: &str, args: &[&str]) -> (i32, Value) {
        result_json(
            self.test_box
                .bay(self.clone_of(actor))
                .env("BAY_ACTOR", actor)
                .env("BAY_SESSION", session)
                .args(args),
        )
    }

    /// `bay talk <args>` as `actor`.
    pub fn talk(&self, actor: &str, args: &[&str]) -> (i32, Value) {
        let mut all = vec!["talk"];
        all.extend(args);
        self.bay(actor, &all)
    }

    /// The tip of `name` on the remote, if it exists.
    #[must_use]
    pub fn remote_tip(&self, name: &str) -> Option<String> {
        let out = self
            .test_box
            .git(self.test_box.path())
            .args([
                &format!("--git-dir={}", self.remote.display()),
                "rev-parse",
                "--verify",
                "-q",
                name,
            ])
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8(out.stdout).unwrap().trim().to_owned())
    }

    /// The talk refs the remote holds.
    #[must_use]
    pub fn remote_talks(&self) -> Vec<String> {
        self.test_box
            .git_bare(
                &self.remote,
                &["for-each-ref", "--format=%(refname)", "refs/bay/talk/"],
            )
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The body of the talk at `name` on the remote.
    #[must_use]
    pub fn body(&self, name: &str) -> Value {
        let tip = self
            .remote_tip(name)
            .unwrap_or_else(|| panic!("the remote holds no {name}"));
        let root = self
            .test_box
            .git_bare(&self.remote, &["rev-list", "--max-parents=0", &tip]);
        serde_json::from_str(
            &self
                .test_box
                .git_bare(&self.remote, &["show", &format!("{root}:talk.json")]),
        )
        .unwrap()
    }
}
