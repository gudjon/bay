//! G286. `bay executor install` writes a service.
//!
//! In a repository set up for bay, `bay executor install` writes the service definition that
//! keeps `bay -C <repository> executor run` running, with the running bay as its program: a
//! launchd agent plist in `~/Library/LaunchAgents/` on macOS (`KeepAlive`), a systemd user unit
//! in `~/.config/systemd/user/` on Linux (`Restart=always`). Both log to
//! `<common dir>/bay/executor.log`. The result names the one command that loads it
//! (`launchctl load ...`, `systemctl --user enable --now ...`); bay never runs it. `--print`
//! writes nothing and shows the file. A file already there is refused, unless `--force`.
//!
//! The guards to mutate: in `src/service.rs`, `install_in` returns before writing when
//! `request.print` (drop that return, and `--print` writes the file); it refuses when
//! `exists && !request.force` (make it `false`, and the second install replaces the file);
//! `service` writes `KeepAlive` and `Restart=always` and the `-C <work_root> executor run`
//! arguments (change any, and the content assertions fail); `load_command` names
//! `launchctl load` and `systemctl --user enable --now`.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, result_json, unchecked_files};

/// A repository set up for bay: its main work tree and its common dir.
fn set_up(test_box: &TestBox) -> (PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let common = PathBuf::from(test_box.git_ok(
        &repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ));
    // The service runs in the main work tree: the directory that holds the common dir.
    (common.parent().unwrap().to_path_buf(), common)
}

/// The one file in `dir`, if there is exactly one.
fn only_file(dir: &Path) -> Option<PathBuf> {
    let files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .map(|entry| entry.unwrap().path())
        .collect();
    (files.len() == 1).then(|| files[0].clone())
}

/// Where this platform's service files go under `home`.
fn service_dir(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library").join("LaunchAgents")
    } else {
        home.join(".config").join("systemd").join("user")
    }
}

fn install(test_box: &TestBox, repo: &Path, args: &[&str]) -> (i32, serde_json::Value) {
    result_json(test_box.bay(repo).args(["executor", "install"]).args(args))
}

#[test]
fn print_shows_the_service_and_writes_nothing() {
    let test_box = TestBox::new();
    let (repo, _) = set_up(&test_box);
    let (code, shown) = install(&test_box, &repo, &["--print"]);
    assert_eq!(code, 0, "{shown}");
    assert_eq!(shown["detail"]["written"], false, "{shown}");
    assert!(
        shown["detail"]["content"]
            .as_str()
            .is_some_and(|content| content.contains("executor")),
        "{shown}"
    );
    assert!(
        !service_dir(&test_box.home).exists(),
        "--print wrote a service file"
    );
}

#[test]
fn install_writes_the_service_and_starts_nothing() {
    let test_box = TestBox::new();
    let (repo, common) = set_up(&test_box);
    let (code, installed) = install(&test_box, &repo, &[]);
    assert_eq!(code, 0, "{installed}");
    assert_eq!(installed["detail"]["written"], true, "{installed}");
    let dir = service_dir(&test_box.home);
    let file =
        only_file(&dir).unwrap_or_else(|| panic!("no one service file in {}", dir.display()));
    assert_eq!(
        installed["detail"]["path"],
        file.display().to_string(),
        "{installed}"
    );
    let content = std::fs::read_to_string(&file).unwrap();
    // The running bay: the path it was started by, or its real path where the platform
    // resolves links (Linux reads `/proc/self/exe`).
    let started = PathBuf::from(env!("CARGO_BIN_EXE_bay"));
    let real = std::fs::canonicalize(&started).unwrap();
    let names_bay = |text: &dyn Fn(&Path) -> String| {
        content.contains(&text(&started)) || content.contains(&text(&real))
    };
    let log = common.join("bay").join("executor.log");
    let load = installed["detail"]["load"].as_str().unwrap_or_default();
    #[cfg(target_os = "macos")]
    {
        assert!(
            file.extension().is_some_and(|ext| ext == "plist"),
            "{}",
            file.display()
        );
        let program = |bay: &Path| {
            format!(
                "\t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{}</string>\n\t\t<string>-C</string>\n\t\t<string>{}</string>\n\t\t<string>executor</string>\n\t\t<string>run</string>\n\t</array>\n",
                bay.display(),
                repo.display()
            )
        };
        assert!(names_bay(&program), "{content}");
        assert!(
            content.contains("<key>KeepAlive</key>\n\t<true/>"),
            "{content}"
        );
        assert!(
            content.contains(&format!(
                "<key>StandardOutPath</key>\n\t<string>{}</string>",
                log.display()
            )),
            "{content}"
        );
        assert!(
            content.contains(&format!(
                "<key>StandardErrorPath</key>\n\t<string>{}</string>",
                log.display()
            )),
            "{content}"
        );
        assert_eq!(
            load,
            format!("launchctl load -w '{}'", file.display()),
            "{installed}"
        );
    }
    #[cfg(target_os = "linux")]
    {
        assert!(
            file.extension().is_some_and(|ext| ext == "service"),
            "{}",
            file.display()
        );
        let exec = |bay: &Path| {
            format!(
                "\nExecStart={} -C {} executor run\n",
                bay.display(),
                repo.display()
            )
        };
        assert!(names_bay(&exec), "{content}");
        assert!(content.contains("\nRestart=always\n"), "{content}");
        assert!(
            content.contains(&format!("\nStandardOutput=append:{}\n", log.display())),
            "{content}"
        );
        assert!(
            content.contains(&format!("\nStandardError=append:{}\n", log.display())),
            "{content}"
        );
        assert!(content.contains("\nWantedBy=default.target\n"), "{content}");
        let unit = file.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            load,
            format!("systemctl --user enable --now {unit}"),
            "{installed}"
        );
    }
}

#[test]
fn a_file_already_there_is_refused_unless_force() {
    let test_box = TestBox::new();
    let (repo, _) = set_up(&test_box);
    let (code, first) = install(&test_box, &repo, &[]);
    assert_eq!(code, 0, "{first}");
    let file = PathBuf::from(first["detail"]["path"].as_str().unwrap());
    std::fs::write(&file, "edited by the person\n").unwrap();
    let (code, refused) = install(&test_box, &repo, &[]);
    assert_eq!(code, 20, "{refused}");
    assert_eq!(refused["decision"], "failed", "{refused}");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "edited by the person\n",
        "the refusal replaced the file"
    );
    let (code, forced) = install(&test_box, &repo, &["--force"]);
    assert_eq!(code, 0, "{forced}");
    assert_eq!(forced["detail"]["replaced"], true, "{forced}");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        first["detail"]["content"].as_str().unwrap()
    );
}
