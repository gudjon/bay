//! A bay host under `[policy] signing = "approvals"` whose policy asks a person's approval for
//! `billing/`, for the approval G-tests of `bay waiting` and `bay approve` (G280, G281). Its
//! `tests` check passes every candidate but one that holds `src/fail`, which it fails. Two
//! clones beside the work repository: the enrolled approver's, and a stranger's whose approver
//! name and key the registry does not enrol.

use std::path::{Path, PathBuf};

use super::talk::fresh_key;
use super::{
    AGENT, REGISTRAR, TestBox, TestKeys, check_files, key_line, passing_result, result_json,
};

pub const APPROVER: &str = "approver-a";
pub const STRANGER: &str = "approver-b";
pub const CHECK: &str = "billing-approval";
const APPROVAL: &str = "\n[[check]]\nid = \"billing-approval\"\nenvironment = \"people\"\nkind = \"approval\"\nwhen_paths = [\"billing/\"]\n";
const APPROVALS_ONLY: &str = "\n[policy]\nsigning = \"approvals\"\n";

pub struct Trunk {
    pub test_box: TestBox,
    /// The work repository, where agents start and land changes; it sets no approver.
    pub repo: PathBuf,
    /// The approver's own clone, with `bay.approverName` and `bay.approverKey`.
    pub approver_box: PathBuf,
    /// A clone whose approver, `approver-b`, the registry does not enrol.
    pub stranger_box: PathBuf,
}

/// The trunk: seeded, its registry enrolling the agent, approver `approver-a` and the
/// registrar, and the two clones configured.
#[must_use]
pub fn trunk() -> Trunk {
    trunk_with("")
}

/// [`trunk`], with `tests` added to the `tests` check's own `[[check]]` entry (for example
/// `"waivable = true\n"`, N-9).
#[must_use]
pub fn trunk_with(tests: &str) -> Trunk {
    trunk_with_config(tests, "")
}

/// [`trunk_with`], with `config` as the trunk's `.bay/config.toml` (empty: none).
#[must_use]
pub fn trunk_with_config(tests: &str, config: &str) -> Trunk {
    trunk_enrolled_by(tests, config, |test_box, repo, keys, approver_key| {
        test_box.enroll_files(
            repo,
            keys,
            &[
                ("agent", key_line(AGENT, &keys.agent)),
                ("approver", key_line(APPROVER, approver_key)),
                ("registry", key_line(REGISTRAR, &keys.registry)),
            ],
        );
    })
}

/// [`trunk_with_config`], with the registry made by `enrol` (G299: `bay enrol`), given the
/// work repository, the test keys and approver `approver-a`'s key; it may enrol nothing.
#[must_use]
pub fn trunk_enrolled_by(
    tests: &str,
    config: &str,
    enrol: impl FnOnce(&TestBox, &Path, &TestKeys, &Path),
) -> Trunk {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let mut check = check_files(
        &format!("[ -e src/fail ] && {{ {failing}; exit 0; }}"),
        &passing_result(1),
        1,
        60,
    );
    for (path, text) in &mut check {
        if path == ".bay/policy.toml" {
            // The `tests` entry is the policy's last table so far, so these keys are its own.
            text.push_str(tests);
            text.push_str(APPROVAL);
            text.push_str(APPROVALS_ONLY);
        }
    }
    if !config.is_empty() {
        check.push((".bay/config.toml".to_owned(), config.to_owned()));
    }
    test_box.seed_with_check(
        &repo,
        &[
            ("src/a.rs", "fn a() {}\n"),
            ("billing/rates.txt", "rate 1\n"),
        ],
        &check,
    );
    let keys = test_box.keys();
    let approver_key = fresh_key(&test_box, APPROVER);
    let stranger_key = fresh_key(&test_box, STRANGER);
    enrol(&test_box, &repo, &keys, &approver_key);
    let approver_box = test_box.clone_of(&host, "approver-box");
    let stranger_box = test_box.clone_of(&host, "stranger-box");
    for (dir, name, key) in [
        (&approver_box, APPROVER, &approver_key),
        (&stranger_box, STRANGER, &stranger_key),
    ] {
        test_box.git_ok(dir, &["config", "bay.approverName", name]);
        test_box.git_ok(dir, &["config", "bay.approverKey", key.to_str().unwrap()]);
    }
    Trunk {
        test_box,
        repo,
        approver_box,
        stranger_box,
    }
}

impl Trunk {
    /// Starts a change under `intent` declaring `dirs` and writes each of `files` in its view.
    /// Returns the change id and its view.
    pub fn start(&self, intent: &str, dirs: &[&str], files: &[(&str, &str)]) -> (String, PathBuf) {
        let mut args = vec!["start", intent, "--paths"];
        args.extend(dirs);
        let started = self.test_box.bay_ok(&self.repo, &args);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        for (path, text) in files {
            let path = view.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        (id, view)
    }

    /// `bay merge --wait` in `view`.
    pub fn land(&self, view: &Path) -> (i32, serde_json::Value) {
        result_json(self.test_box.bay(view).args(["merge", "--wait"]))
    }

    /// A change on `billing/` and `src/` held `APPROVAL_PENDING`: its id, its view and the
    /// candidate commit the refusal names.
    pub fn held_change(&self) -> (String, PathBuf, String) {
        let (id, view) = self.start(
            "CARD-billing",
            &["billing/", "src/"],
            &[
                ("billing/discount.txt", "10%\n"),
                ("src/discount.rs", "fn discount() {}\nfn more() {}\n"),
            ],
        );
        let (code, waiting) = self.land(&view);
        assert_eq!(code, 10, "{waiting}");
        assert_eq!(waiting["reason"], "APPROVAL_PENDING", "{waiting}");
        let candidate = waiting["detail"]["approval"]["commit"]
            .as_str()
            .unwrap()
            .to_owned();
        (id, view, candidate)
    }

    /// `bay waiting` in `dir`, which always decides `ok`.
    pub fn waiting(&self, dir: &Path) -> serde_json::Value {
        let listed = self.test_box.bay_ok(dir, &["waiting"]);
        assert_eq!(listed["decision"], "ok", "{listed}");
        listed
    }

    /// `bay approve <change>` in `dir`, with `args` after it.
    pub fn approve(&self, dir: &Path, change: &str, args: &[&str]) -> (i32, serde_json::Value) {
        result_json(self.test_box.bay(dir).arg("approve").arg(change).args(args))
    }

    /// `bay reject <change> <args>` in `dir`.
    pub fn reject(&self, dir: &Path, change: &str, args: &[&str]) -> (i32, serde_json::Value) {
        result_json(self.test_box.bay(dir).arg("reject").arg(change).args(args))
    }

    /// The tip of the remote trunk, as the work repository last fetched it.
    pub fn trunk_tip(&self) -> String {
        self.test_box
            .git_ok(&self.repo, &["rev-parse", "refs/remotes/origin/main"])
    }
}
