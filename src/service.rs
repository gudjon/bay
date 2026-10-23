//! `bay runner install` (G286): the service definition that keeps `bay runner run` running
//! for one repository, under the supervisor the person's own machine already has: a launchd
//! agent in `~/Library/LaunchAgents/` on macOS, a systemd user unit in
//! `~/.config/systemd/user/` on Linux. It runs `<bay> -C <repository> runner run`, where
//! `<bay>` is the running executable, restarts it whenever it ends (`KeepAlive`,
//! `Restart=always`), and logs to `<common dir>/bay/executor.log`.
//!
//! It writes the file and prints the one command that loads it; it never runs that command,
//! so starting a service stays the person's decision. `--print` writes nothing and shows the
//! file. A file already there is refused unless `--force` is given.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::repo::Repo;
use crate::result::{BayResult, Decision};

/// What `bay runner install` was asked to do.
pub struct Request {
    /// Show the file and write nothing.
    pub print: bool,
    /// Replace a file that is already there.
    pub force: bool,
}

/// The supervisor a service definition is written for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A launchd agent (macOS).
    Launchd,
    /// A systemd user unit (Linux).
    Systemd,
}

impl Kind {
    /// This platform's supervisor, or none where bay writes no service.
    #[must_use]
    pub const fn here() -> Option<Self> {
        if cfg!(target_os = "macos") {
            Some(Self::Launchd)
        } else if cfg!(target_os = "linux") {
            Some(Self::Systemd)
        } else {
            None
        }
    }

    const fn word(self) -> &'static str {
        match self {
            Self::Launchd => "launchd",
            Self::Systemd => "systemd",
        }
    }
}

/// One service definition: where it goes, what it holds, and the command that loads it.
#[derive(Debug)]
pub struct Service {
    pub kind: Kind,
    /// The launchd label, or the systemd unit's name.
    pub name: String,
    pub path: PathBuf,
    pub content: String,
    pub log: PathBuf,
}

/// The name of the service for the repository at `work_root`: `bay-executor-<dir>-<hash>`,
/// where `<dir>` is the work tree's last component (letters, digits, `-` and `_`; anything
/// else is `-`) and `<hash>` the first 8 hex digits of the SHA-256 of its full path, so two
/// clones with one name get two services.
#[must_use]
pub fn name_for(work_root: &Path) -> String {
    let dir: String = work_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let digest = Sha256::digest(work_root.to_string_lossy().as_bytes());
    let hash = crate::agent_settings::hex(&digest);
    let hash = hash.get(..8).unwrap_or(&hash);
    if dir.is_empty() {
        format!("bay-executor-{hash}")
    } else {
        format!("bay-executor-{dir}-{hash}")
    }
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// One word of a systemd `ExecStart=` line: quoted when it holds a space, a quote or a
/// backslash; `%` is doubled, since systemd reads it as a specifier.
fn systemd_word(word: &str) -> String {
    let word = word.replace('%', "%%");
    if word.contains([' ', '\t', '"', '\'', '\\']) {
        format!("\"{}\"", word.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        word
    }
}

/// The service definition of `kind` that runs `<bay> -C <work_root> runner run` with `path`
/// as its `PATH` and `log` as its output, under `home` (or, for systemd, `config_home`, the
/// box's `XDG_CONFIG_HOME`, when set).
#[must_use]
pub fn service(
    kind: Kind,
    (bay, work_root): (&Path, &Path),
    (home, config_home): (&Path, Option<&Path>),
    log: &Path,
    path: &str,
) -> Service {
    let name = name_for(work_root);
    let args = [
        bay.display().to_string(),
        "-C".to_owned(),
        work_root.display().to_string(),
        "runner".to_owned(),
        "run".to_owned(),
    ];
    let log_text = log.display().to_string();
    match kind {
        Kind::Launchd => {
            let label = name.replace("bay-executor-", "bay.executor.");
            Service {
                kind,
                path: home
                    .join("Library")
                    .join("LaunchAgents")
                    .join(format!("{label}.plist")),
                content: plist(&label, &args, path, &log_text),
                name: label,
                log: log.to_path_buf(),
            }
        }
        Kind::Systemd => {
            let unit = format!("{name}.service");
            let dir =
                config_home.map_or_else(|| home.join(".config"), std::path::Path::to_path_buf);
            Service {
                kind,
                path: dir.join("systemd").join("user").join(&unit),
                content: systemd_unit(work_root, &args, path, &log_text),
                name: unit,
                log: log.to_path_buf(),
            }
        }
    }
}

/// The launchd agent `label` that runs `args` with `path` as its `PATH`, kept alive, with its
/// output in `log`.
fn plist(label: &str, args: &[String], path: &str, log: &str) -> String {
    let program = args.iter().fold(String::new(), |mut program, arg| {
        let _ = writeln!(program, "\t\t<string>{}</string>", xml(arg));
        program
    });
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{label}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         {program}\
         \t</array>\n\
         \t<key>EnvironmentVariables</key>\n\
         \t<dict>\n\
         \t\t<key>PATH</key>\n\
         \t\t<string>{path}</string>\n\
         \t</dict>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         \t<key>KeepAlive</key>\n\
         \t<true/>\n\
         \t<key>StandardOutPath</key>\n\
         \t<string>{log}</string>\n\
         \t<key>StandardErrorPath</key>\n\
         \t<string>{log}</string>\n\
         </dict>\n\
         </plist>\n",
        label = xml(label),
        path = xml(path),
        log = xml(log),
    )
}

/// The systemd user unit for `work_root` that runs `args` with `path` as its `PATH`, restarted
/// whenever it ends, with its output appended to `log`.
fn systemd_unit(work_root: &Path, args: &[String], path: &str, log: &str) -> String {
    let exec: Vec<String> = args.iter().map(|arg| systemd_word(arg)).collect();
    let log = log.replace('%', "%%");
    format!(
        "[Unit]\n\
         Description=bay runner for {repo}\n\
         \n\
         [Service]\n\
         ExecStart={exec}\n\
         Environment={env}\n\
         Restart=always\n\
         RestartSec=10\n\
         StandardOutput=append:{log}\n\
         StandardError=append:{log}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        repo = work_root.display().to_string().replace('%', "%%"),
        exec = exec.join(" "),
        env = systemd_word(&format!("PATH={path}")),
    )
}

/// The one command that loads `service`, which bay prints and never runs; `replaced` when a
/// file of the same name was there before, so the supervisor reads it again.
#[must_use]
pub fn load_command(service: &Service, replaced: bool) -> String {
    let path = service.path.display();
    let name = &service.name;
    match (service.kind, replaced) {
        (Kind::Launchd, false) => format!("launchctl load -w '{path}'"),
        (Kind::Launchd, true) => {
            format!("launchctl unload '{path}'; launchctl load -w '{path}'")
        }
        (Kind::Systemd, false) => format!("systemctl --user enable --now {name}"),
        (Kind::Systemd, true) => format!(
            "systemctl --user daemon-reload && systemctl --user enable --now {name} && systemctl --user restart {name}"
        ),
    }
}

/// `bay runner install [--print] [--force]` in the repository that holds `dir`.
#[must_use]
pub fn install(dir: &Path, request: &Request) -> BayResult {
    install_in(dir, request).unwrap_or_else(BayResult::failed)
}

fn refused(message: String, mut detail: serde_json::Value) -> BayResult {
    // The reason goes in `detail.why`, where `say` and every agent read it.
    detail["why"] = serde_json::Value::String(message.clone());
    let mut result = BayResult::new(Decision::Failed).with_detail(detail);
    result.human = Some(message);
    result
}

/// [`install`]; `Err` is a failure bay names (`failed`), and a file already there is refused
/// in the result itself.
fn install_in(dir: &Path, request: &Request) -> Result<BayResult, String> {
    let Some(kind) = Kind::here() else {
        return Err(
            "bay runner install writes a launchd agent (macOS) or a systemd user unit \
             (Linux); on this platform, run bay runner run under a supervisor of your own"
                .to_owned(),
        );
    };
    let repo = Repo::open(dir)?;
    let bay = std::env::current_exe()
        .map_err(|error| format!("the bay executable is not known: {error}"))?;
    let home = crate::platform::home()
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or("HOME is not set; bay cannot name the service path")?;
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    let log = repo.common_dir.join("bay").join("executor.log");
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_owned());
    let service = service(
        kind,
        (&bay, &repo.work_root),
        (&home, config_home.as_deref()),
        &log,
        &path,
    );
    let exists = service.path.exists();
    let mut detail = json!({
        "service": kind.word(),
        "name": service.name,
        "path": service.path.display().to_string(),
        "command": [bay.display().to_string(), "-C", repo.work_root.display().to_string(),
            "runner", "run"],
        "log": log.display().to_string(),
        "content": service.content,
        "exists": exists,
    });
    if request.print {
        detail["written"] = json!(false);
        let mut result = BayResult::ok(detail);
        result.human = Some(format!(
            "{}\nnothing written; without --print this goes to {}",
            service.content.trim_end(),
            service.path.display()
        ));
        return Ok(result);
    }
    if exists && !request.force {
        detail["written"] = json!(false);
        return Ok(refused(
            format!(
                "{} exists; bay left it as it is. Pass --force to replace it, or --print to see the new one",
                service.path.display()
            ),
            detail,
        ));
    }
    let made = |dir: &Path| {
        std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))
    };
    for dir in [log.parent(), service.path.parent()].into_iter().flatten() {
        made(dir)?;
    }
    std::fs::write(&service.path, &service.content)
        .map_err(|error| format!("{}: {error}", service.path.display()))?;
    let load = load_command(&service, exists);
    detail["written"] = json!(true);
    detail["replaced"] = json!(exists);
    detail["load"] = json!(load);
    let mut note = format!(
        "wrote {}; bay started nothing. To start the runner now and at each login, run:\n  {load}",
        service.path.display()
    );
    if kind == Kind::Systemd {
        let linger = "to keep it running while you are logged out: loginctl enable-linger";
        detail["note"] = json!(linger);
        note = format!("{note}\n{linger}");
    }
    let mut result = BayResult::ok(detail);
    result.human = Some(note);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::{Kind, load_command, name_for, service, systemd_word};
    use std::path::Path;

    #[test]
    fn a_name_is_the_directory_and_a_hash_of_its_path() {
        let one = name_for(Path::new("/work/my repo"));
        let two = name_for(Path::new("/other/my repo"));
        assert!(one.starts_with("bay-executor-my-repo-"), "{one}");
        assert_eq!(one.len(), "bay-executor-my-repo-".len() + 8, "{one}");
        assert_ne!(one, two);
    }

    #[test]
    fn a_systemd_word_is_quoted_only_when_it_must_be() {
        assert_eq!(systemd_word("/usr/bin/bay"), "/usr/bin/bay");
        assert_eq!(systemd_word("/a b/bay"), "\"/a b/bay\"");
        assert_eq!(systemd_word("100%"), "100%%");
    }

    #[test]
    fn each_service_restarts_and_logs_and_loads_by_one_command() {
        let (bay, repo, home) = (
            Path::new("/opt/bin/bay"),
            Path::new("/work/repo"),
            Path::new("/home/me"),
        );
        let log = Path::new("/work/repo/.git/bay/executor.log");
        let unit = service(Kind::Systemd, (bay, repo), (home, None), log, "/usr/bin");
        assert!(
            unit.path.starts_with("/home/me/.config/systemd/user"),
            "{unit:?}"
        );
        assert!(
            unit.content
                .contains("ExecStart=/opt/bin/bay -C /work/repo runner run\n"),
            "{}",
            unit.content
        );
        assert!(unit.content.contains("Restart=always\n"));
        assert!(
            unit.content
                .contains("StandardOutput=append:/work/repo/.git/bay/executor.log\n")
        );
        assert!(
            load_command(&unit, false)
                .starts_with("systemctl --user enable --now bay-executor-repo-")
        );
        let agent = service(Kind::Launchd, (bay, repo), (home, None), log, "/usr/bin");
        assert!(
            agent.path.starts_with("/home/me/Library/LaunchAgents"),
            "{agent:?}"
        );
        assert!(agent.content.contains("<key>KeepAlive</key>\n\t<true/>"));
        assert!(agent.content.contains("\t\t<string>/opt/bin/bay</string>\n\t\t<string>-C</string>\n\t\t<string>/work/repo</string>\n\t\t<string>runner</string>\n\t\t<string>run</string>\n"));
        assert!(
            load_command(&agent, false)
                .starts_with("launchctl load -w '/home/me/Library/LaunchAgents/bay.executor.repo-")
        );
    }
}
