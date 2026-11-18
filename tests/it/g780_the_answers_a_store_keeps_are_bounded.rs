//! G780 (a measurement, 2026-11-18: one store's answers reached 2.5 GB in 65,442 files, 25,771
//! written in a day, and one box held 5.7 GB of them across its clones). The answers a store
//! keeps stay within their bound: a write keeps its directory, one of 256, within its share,
//! removing the answers read least lately. Every directory starts full of old answers here, so
//! each one a listing writes in must give room.
//!
//! Guard to mutate: `src/facts.rs` `evict`: return at once (nothing is removed past the bound).

#![cfg(unix)]

use crate::support;

use std::time::{Duration, SystemTime};

use support::{TestBox, result_json, unchecked_files};

/// The bound the reads below run under: 4,096 bytes for each of the 256 directories.
const SHARE: u64 = 4096;

#[test]
fn a_write_makes_room_in_its_directory_by_the_answers_read_least_lately() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    for _ in 0..2 {
        let _ = test_box.start_and_edit(&repo);
    }
    let (code, swept) = result_json(test_box.bay(&repo).arg("status"));
    assert_eq!(code, 0, "{swept}");
    support::sweeps_ended(&repo);
    // GIVEN every directory of answers full to its share with answers last read a day ago
    let answers = repo.join(".git/bay/answers");
    let day_ago = SystemTime::now() - Duration::from_hours(24);
    for byte in 0..=255_u8 {
        let dir = answers.join(format!("{byte:02x}"));
        std::fs::create_dir_all(&dir).unwrap();
        for n in 0..2 {
            let file = dir.join(format!("{n:062}"));
            std::fs::write(&file, vec![b'o'; (SHARE / 2) as usize]).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&file)
                .unwrap()
                .set_modified(day_ago)
                .unwrap();
        }
    }

    // AND a change the store has not answered for yet
    let _ = test_box.start_and_edit(&repo);
    support::sweeps_ended(&repo);
    let before: std::collections::BTreeSet<std::path::PathBuf> = std::fs::read_dir(&answers)
        .unwrap()
        .flatten()
        .flat_map(|dir| std::fs::read_dir(dir.path()).unwrap().flatten())
        .map(|file| file.path())
        .collect();

    // WHEN listings keep their answers under that bound
    for _ in 0..2 {
        let (code, listed) = result_json(
            test_box
                .bay(&repo)
                .arg("status")
                .env("BAY_TEST_SAVED", "off")
                // Served by the last sweep: every answer is kept by this process, under the
                // bound it was given.
                .env("BAY_TEST_SWEPT_S", "600")
                .env("BAY_TEST_ANSWERS_MAX_BYTES", (SHARE * 256).to_string()),
        );
        assert_eq!(code, 0, "{listed}");
        support::sweeps_ended(&repo);
    }

    // THEN each directory a listing wrote a new answer in holds at most its share, or its one new answer,
    // and lost old answers to make the room
    let mut wrote_in = 0;
    let mut over = Vec::new();
    for dir in std::fs::read_dir(&answers).unwrap().flatten() {
        let files: Vec<(u64, bool)> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|file| {
                (
                    file.metadata().unwrap().len(),
                    !before.contains(&file.path()),
                )
            })
            .collect();
        if !files.iter().any(|(_, new)| *new) {
            continue;
        }
        wrote_in += 1;
        let bytes: u64 = files.iter().map(|(len, _)| len).sum();
        if bytes > SHARE && files.len() > 1 {
            over.push((dir.file_name(), files));
        }
    }
    assert!(wrote_in > 0, "the listings kept no answer");
    assert!(over.is_empty(), "directories over their share: {over:?}");
}
