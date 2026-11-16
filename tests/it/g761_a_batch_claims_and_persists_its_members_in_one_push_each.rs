//! G761 (a fleet measurement, 2026-11-16: a batch of twenty made 41 pushes, 160 s, and 57
//! fetches, 91 s, to land, while its pre-merge check took 37 s at the median: each member was
//! claimed by a push of its own and persisted as checked by another). A batch claims all its
//! joining members in one atomic push, and persists all its members as checked in one more;
//! when a lease in either is lost, it goes member by member as before, and the batch lands.
//!
//! Guards to mutate, in `src/publish.rs`: `join_or_cut` (join each member with its own push:
//! the claims take a push each) and `persist_batch` (persist each member with its own push).

#![cfg(unix)]

use crate::support;

use support::PRODUCER;
use support::queued::{Check, host, ran_for};

const CONFIG: &str = "[runner]\nlease_s = 60\nheartbeat_s = 30\nbatch = { min = 2, max = 8, max_failure_rate = 1.0 }\n";

/// The pushes in `log` (one line per push's arguments) that move the chain of any of `changes`.
fn moving(log: &str, changes: &[&str]) -> usize {
    log.lines()
        .filter(|line| {
            changes
                .iter()
                .any(|change| line.contains(&format!("refs/bay/changes/{change}")))
        })
        .count()
}

#[test]
fn a_batch_of_four_claims_and_persists_its_members_in_two_pushes() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let one = held.queue("one", "src");
    let others: Vec<String> = ["two", "three", "four"]
        .iter()
        .map(|name| held.queue(name, &format!("lib{name}")))
        .collect();
    let pushes = held.test_box.path().join("pushes");
    held.test_box.wrap_git(&format!(
        "case \" $* \" in\n  *\" push \"*) echo \"$*\" >> '{}' ;;\nesac",
        pushes.display()
    ));

    // WHEN a runner lands the batch of four
    let runner = held.executor("exec", PRODUCER);
    let pass = held.pass(&runner);

    // THEN all four merged in one batch
    let batch = &ran_for(&pass, &one)["detail"]["batch"];
    assert_eq!(batch["size"], 4, "{pass}");
    let on_main = held.test_box.trunk_change_ids(&held.remote);
    for id in others.iter().chain(std::iter::once(&one)) {
        assert!(on_main.contains(id), "{id} not on main: {pass}");
    }
    // AND the three members after the head were claimed and persisted in two pushes, not six
    let log = std::fs::read_to_string(&pushes).unwrap_or_default();
    let members: Vec<&str> = others.iter().map(String::as_str).collect();
    let before_publish: String = log
        .lines()
        .take_while(|line| !line.contains("refs/heads/main"))
        .fold(String::new(), |mut kept, line| {
            kept.push_str(line);
            kept.push('\n');
            kept
        });
    let count = moving(&before_publish, &members);
    eprintln!("G761 pushes moving a member's chain before the publish: {count}\n{log}");
    assert!(
        count <= 2,
        "{count} pushes claimed and persisted three members:\n{log}"
    );
}

/// G761: when a member's chain moved before the batch's one atomic persist push, the push
/// loses that lease and moves nothing; the batch persists member by member, as before, and
/// lands every checked member.
#[test]
fn a_lost_lease_in_the_batch_push_persists_member_by_member_and_the_batch_lands() {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use support::{EXECUTOR, result_json};

    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let one = held.queue("one", "src");
    let started = held
        .test_box
        .bay_ok(&held.repo, &["start", "two", "--paths", "lib/"]);
    let two = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::create_dir_all(view.join("lib")).unwrap();
    std::fs::write(view.join("lib/two.rs"), "fn two() {}\n").unwrap();
    let (code, submitted) = result_json(held.test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");
    let opened = bay::repo::Repo::open(&held.repo).unwrap();
    let signer = bay::signing::Signer {
        key: held.keys.executor.clone(),
        name: EXECUTOR.to_owned(),
    };
    bay::queue::join(&opened, Some(&signer), &format!("{two}.1"), &two).unwrap();

    // GIVEN a runner whose batch of one and two is checked, held before its one persist push
    let runner = held.executor("exec", PRODUCER);
    let release = held.test_box.path().join("release-runner");
    let pass = held
        .test_box
        .bay_process(&runner)
        .env("BAY_TEST_PAUSE_AT", "before-batch-persist-push")
        .env("BAY_TEST_PAUSE_FILE", &release)
        .args(["--json", "runner", "run", "--once"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let paused = PathBuf::from(format!("{}.paused", release.display()));
    let deadline = Instant::now() + Duration::from_secs(120);
    while !paused.exists() {
        assert!(
            Instant::now() < deadline,
            "the batch never came to its persist push"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // WHEN two's chain moves meanwhile (its author sends a newer revision), so a lease is lost
    std::fs::write(view.join("lib/later.rs"), "fn later() {}\n").unwrap();
    let (code, revised) = result_json(held.test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{revised}");
    std::fs::write(&release, "go").unwrap();
    let pass = support::queued::finished(pass);

    // THEN the batch still lands both members as checked; the newer edits are not on main yet
    let on_main = held.test_box.trunk_change_ids(&held.remote);
    assert!(
        on_main.contains(&one) && on_main.contains(&two),
        "the batch did not land after its lost lease: {on_main:?} {pass}"
    );
    let tree = held
        .test_box
        .git_bare(&held.remote, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(!tree.contains("lib/later.rs"), "{tree}");
}
