//! G607 (a fleet finding, 2026-10-31: a records read asked each producer's note at all 20
//! fanouts a notes tree may hold it, 260 names a tree on a box with 13 producers, about 20 ms
//! a tree). A records read asks for a fanout directory only under one its notes tree holds, and
//! for a note only at the fanouts its directories reach. It reads the records `git notes show`
//! gives, at whatever fanout.
//!
//! Guards to mutate, in `src/evidence.rs` `fanouts`: count every level asked as held, and a read
//! asks each note at all 20 fanouts; stop after the first level, and a note two directories deep
//! is never read.

// The landing runs a shell check adapter, and the case puts a shell wrapper for `git` on PATH,
// so it runs on Unix only.
#![cfg(unix)]

use crate::g196_evidence_reads_spawn_a_constant_number_of_git_processes::{fanned_note, landed};

use std::collections::BTreeMap;

use bay::evidence::EVIDENCE_PREFIX;

#[test]
fn a_records_read_asks_for_a_note_only_where_its_notes_tree_has_directories() {
    let (test_box, repo, id, tree) = landed();
    fanned_note(&test_box, &repo, &tree, "fanned-1", 1);
    fanned_note(&test_box, &repo, &tree, "fanned-2", 2);

    // How many fanout directories of `tree` each producer's notes tree holds, as Git finds them.
    let refs = test_box.git_ok(
        &repo,
        &["for-each-ref", "--format=%(refname)", EVIDENCE_PREFIX],
    );
    let mut held = BTreeMap::new();
    for name in refs.lines() {
        let mut levels = 0;
        while levels + 1 < tree.len() / 2 {
            let dir: Vec<&str> = (0..=levels)
                .map(|level| &tree[level * 2..level * 2 + 2])
                .collect();
            let found = test_box
                .git(&repo)
                .args(["rev-parse", "--verify", "-q"])
                .arg(format!("{name}:{}", dir.join("/")))
                .output()
                .unwrap();
            if !found.status.success() {
                break;
            }
            levels += 1;
        }
        held.insert(name.to_owned(), levels);
    }
    // G736: a read names each producer's notes by its notes tree, so a name read back is mapped
    // to the ref whose tree it is.
    let by_tree: BTreeMap<String, String> = held
        .keys()
        .map(|name| {
            let tree = test_box.git_ok(&repo, &["rev-parse", &format!("{name}^{{tree}}")]);
            (tree, name.clone())
        })
        .collect();
    assert_eq!(
        held.get(&format!("{EVIDENCE_PREFIX}fanned-2")),
        Some(&2),
        "{held:?}"
    );

    // Each `cat-file` keeps the names it is given, one file a process.
    let asked = test_box.path().join("asked");
    std::fs::create_dir_all(&asked).unwrap();
    test_box.wrap_git(&format!(
        "for arg in \"$@\"; do\n  if [ \"$arg\" = cat-file ]; then\n    tee '{asked}/'$$ | \"$real\" \"$@\"\n    exit $?\n  fi\ndone",
        asked = asked.display(),
    ));
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let records = status["detail"]["changes"][0]["evidence"]["records"]
        .as_array()
        .map_or(0, Vec::len);
    assert_eq!(records, 3, "the status lost a record: {status}");

    // The deepest name about `tree` each producer was asked: a directory names its levels, and
    // a note is one level under the directories above it.
    let mut deepest: BTreeMap<String, usize> = BTreeMap::new();
    for file in std::fs::read_dir(&asked).unwrap() {
        let text = std::fs::read_to_string(file.unwrap().path()).unwrap_or_default();
        for line in text.lines() {
            let Some((name, path)) = line.split_once(':') else {
                continue;
            };
            let name = by_tree.get(name).map_or(name, String::as_str);
            let joined = path.replace('/', "");
            if !name.starts_with(EVIDENCE_PREFIX) || joined.is_empty() || !tree.starts_with(&joined)
            {
                continue;
            }
            let slashes = path.matches('/').count();
            let level = if joined == tree { slashes } else { slashes + 1 };
            let deepest = deepest.entry(name.to_owned()).or_insert(0);
            *deepest = (*deepest).max(level);
        }
    }
    for (name, &levels) in &held {
        let asked = deepest.get(name).copied();
        assert!(
            asked.is_some_and(|asked| asked <= levels + 1),
            "{name} holds {levels} fanout directories of {tree}, and a read asked {asked:?} deep: {deepest:?}"
        );
    }
}
