//! The remote's own store (BACKLOG 66 E, owner rulings 2026-10-24 and 2026-10-25): large files
//! never go in Git, and their store is derived from the remote, so a clone that can push needs no
//! store setup of its own.
//!
//! With `[plugin.assets]` naming no `command`, bay is its own assets plugin: `bay assets store
//! <remote> <verb>` speaks the plugin protocol (`super::Plugin`: the same verbs, output and exit codes), so
//! every caller, `bay plugin test assets` included, sees one contract. Where the bytes go follows
//! the remote's URL:
//! - a path (or `file://`): `<remote>/bay/store/objects/sha256/<hex>`, beside the repository; a
//!   purged object leaves `<hex>.purged` and its address;
//! - `ssh://host/path` or `host:path`: `bay assets store <path> <verb>` on the host, over Git's own ssh
//!   command and its login, bytes on stdin and stdout, so the host side is the path case;
//! - `https://`: `<remote>/bay-store/v1/`, through `curl` with Git's credential helper: an upload
//!   in parts the server sizes, numbered from 1, then a complete it hashes; 404 never stored,
//!   410 purged.
//!
//! A remote that serves no store refuses the put, so a file over the limit is refused, never put
//! in Git. bay never purges.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{EXIT_ABSENT, EXIT_PURGED, EXIT_UNSUPPORTED, identity, is_id};

/// Any other failure (a plugin's own codes stop at [`EXIT_PURGED`]; 4 is a hold's refusal).
const EXIT_FAILED: i32 = 5;

/// Where a remote's store is.
#[derive(Debug, PartialEq, Eq)]
enum Remote {
    /// A local path: the store is a directory beside the repository.
    Path(PathBuf),
    /// An ssh remote: the host, and the repository's path there.
    Ssh { host: String, path: String },
    /// An https remote, its URL.
    Https(String),
}

impl Remote {
    fn parse(url: &str) -> Self {
        if url.starts_with("https://") || url.starts_with("http://") {
            return Self::Https(url.trim_end_matches('/').to_owned());
        }
        if let Some(rest) = url.strip_prefix("ssh://") {
            let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
            return Self::Ssh {
                host: host.to_owned(),
                path: format!("/{path}"),
            };
        }
        if let Some(path) = url.strip_prefix("file://") {
            return Self::Path(PathBuf::from(path));
        }
        // Git's scp form, `host:path`: a colon before any slash, and not a Windows drive.
        if let Some((host, path)) = url.split_once(':')
            && !host.contains('/')
            && host.len() > 1
        {
            return Self::Ssh {
                host: host.to_owned(),
                path: path.to_owned(),
            };
        }
        Self::Path(PathBuf::from(url))
    }
}

/// `bay assets store <remote> <verb> <args>`: the plugin protocol's exit code, with its stdout and
/// stderr written here.
#[must_use]
pub fn run(remote: &str, verb: &str, args: &[String]) -> i32 {
    match Remote::parse(remote) {
        Remote::Path(root) => local(&root, verb, args),
        Remote::Ssh { host, path } => ssh(&host, &path, verb, args),
        Remote::Https(url) => https(&url, verb, args),
    }
}

/// The directory a local store keeps its objects in.
fn objects(root: &Path) -> PathBuf {
    root.join("bay")
        .join("store")
        .join("objects")
        .join("sha256")
}

fn hex(id: &str) -> &str {
    id.trim_start_matches("sha256:")
}

/// The store beside the repository at `root`.
fn local(root: &Path, verb: &str, args: &[String]) -> i32 {
    let fail = |why: String| {
        eprintln!("bay store: {why}");
        EXIT_FAILED
    };
    if !root.exists() {
        return fail(format!("{} is no repository here", root.display()));
    }
    let dir = objects(root);
    match (verb, args) {
        ("put", [file]) => match put_local(&dir, file) {
            Ok(id) => {
                println!("{id}");
                0
            }
            Err(why) => fail(why),
        },
        ("get", [id, file]) if is_id(id) => {
            let object = dir.join(hex(id));
            if dir.join(format!("{}.purged", hex(id))).exists() {
                return EXIT_PURGED;
            }
            if !object.is_file() {
                return fail(format!("{id} is not held here"));
            }
            match copy_out(&object, file) {
                Ok(()) => 0,
                Err(why) => fail(why),
            }
        }
        ("exists", [id]) if is_id(id) => {
            if dir.join(hex(id)).is_file() {
                0
            } else if dir.join(format!("{}.purged", hex(id))).exists() {
                EXIT_PURGED
            } else {
                EXIT_ABSENT
            }
        }
        // bay never purges, and a store bay keeps offers no range reads yet.
        ("purge" | "get-range", _) => EXIT_UNSUPPORTED,
        _ => fail(format!("{verb} {} is no store request", args.join(" "))),
    }
}

/// Stores `file` (`-`: stdin) in `dir`, named by its bytes, and returns their identity. Bytes
/// already held stay as they are.
fn put_local(dir: &Path, file: &str) -> Result<String, String> {
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("cannot make {}: {error}", dir.display()))?;
    let tmp = dir.join(format!(".put-{}", std::process::id()));
    copy_in(file, &tmp)?;
    let id = identity(&tmp)?;
    let object = dir.join(hex(&id));
    if object.is_file() {
        let _ = std::fs::remove_file(&tmp);
    } else {
        crate::durable::rename(&tmp, &object)
            .map_err(|error| format!("cannot store {}: {error}", object.display()))?;
    }
    Ok(id)
}

fn copy_in(file: &str, to: &Path) -> Result<(), String> {
    let written = if file == "-" {
        std::fs::File::create(to).and_then(|mut out| std::io::copy(&mut std::io::stdin(), &mut out))
    } else {
        std::fs::copy(file, to)
    };
    written
        .map(|_| ())
        .map_err(|error| format!("cannot read {file}: {error}"))
}

fn copy_out(object: &Path, file: &str) -> Result<(), String> {
    let written = if file == "-" {
        std::fs::File::open(object)
            .and_then(|mut from| std::io::copy(&mut from, &mut std::io::stdout()))
    } else {
        std::fs::copy(object, file)
    };
    written
        .map(|_| ())
        .map_err(|error| format!("cannot write {file}: {error}"))
}

/// The ssh command Git uses: `GIT_SSH_COMMAND`, else `core.sshCommand`, else `ssh`.
fn ssh_command() -> Vec<String> {
    let configured = std::env::var("GIT_SSH_COMMAND")
        .ok()
        .filter(|command| !command.trim().is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["config", "--get", "core.sshCommand"])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
                .filter(|command| !command.is_empty())
        })
        .unwrap_or_else(|| "ssh".to_owned());
    configured.split_whitespace().map(str::to_owned).collect()
}

/// The store of the repository at `path` on `host`: `bay assets store <path>` there, bytes over the
/// connection, so a file never needs a path on the other side.
fn ssh(host: &str, path: &str, verb: &str, args: &[String]) -> i32 {
    let fail = |why: String| {
        eprintln!("bay store: {why}");
        EXIT_FAILED
    };
    let (remote_args, local_in, local_out): (Vec<String>, Option<&str>, Option<&str>) =
        match (verb, args) {
            ("put", [file]) => (vec!["put".into(), "-".into()], Some(file), None),
            ("get", [id, file]) => (vec!["get".into(), id.clone(), "-".into()], None, Some(file)),
            _ => (
                std::iter::once(verb.to_owned())
                    .chain(args.iter().cloned())
                    .collect(),
                None,
                None,
            ),
        };
    let mut words = ssh_command();
    let Some(program) = words.first().cloned() else {
        return fail("no ssh command".to_owned());
    };
    // The fixed words as they are; the path and the arguments quoted for the remote shell.
    let quoted: Vec<String> = ["bay".to_owned(), "assets".to_owned(), "store".to_owned()]
        .into_iter()
        .chain(
            std::iter::once(path.to_owned())
                .chain(remote_args)
                .map(|word| shell_quote(&word)),
        )
        .collect();
    words.remove(0);
    let mut cmd = Command::new(program);
    cmd.args(words).arg(host).arg(quoted.join(" "));
    cmd.stdin(if local_in.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => return fail(format!("cannot run ssh: {error}")),
    };
    if let (Some(file), Some(mut stdin)) = (local_in, child.stdin.take()) {
        let sent =
            std::fs::File::open(file).and_then(|mut from| std::io::copy(&mut from, &mut stdin));
        if let Err(error) = sent {
            return fail(format!("cannot read {file}: {error}"));
        }
    }
    let mut out = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_end(&mut out);
    }
    let code = child
        .wait()
        .ok()
        .and_then(|status| status.code())
        .unwrap_or(EXIT_FAILED);
    if code != 0 {
        return code;
    }
    let Some(file) = local_out else {
        let _ = std::io::stdout().write_all(&out);
        return 0;
    };
    match std::fs::write(file, &out) {
        Ok(()) => 0,
        Err(error) => fail(format!("cannot write {file}: {error}")),
    }
}

/// The store under the https remote `url` (`<remote>/bay-store/v1/`), as Landingbay serves it.
fn https(url: &str, verb: &str, args: &[String]) -> i32 {
    let fail = |why: String| {
        eprintln!("bay store: {why}");
        EXIT_FAILED
    };
    let base = format!("{url}/bay-store/v1");
    let login = match Login::fill(url) {
        Ok(login) => login,
        Err(why) => return fail(why),
    };
    match (verb, args) {
        ("put", [file]) => match put_https(&base, &login, file) {
            Ok(id) => {
                println!("{id}");
                0
            }
            Err(why) => fail(why),
        },
        ("get", [id, file]) if is_id(id) => {
            let target = if file == "-" {
                None
            } else {
                Some(Path::new(file))
            };
            match login.curl(
                &["-X", "GET"],
                &format!("{base}/objects/sha256/{}", hex(id)),
                None,
                target,
            ) {
                Ok((200, body)) => {
                    if target.is_none() {
                        let _ = std::io::stdout().write_all(&body);
                    }
                    0
                }
                Ok((410, _)) => EXIT_PURGED,
                Ok((code, _)) => fail(format!("the remote answered {code} for {id}")),
                Err(why) => fail(why),
            }
        }
        ("exists", [id]) if is_id(id) => {
            match login.curl(
                &["-I"],
                &format!("{base}/objects/sha256/{}", hex(id)),
                None,
                None,
            ) {
                Ok((200, _)) => 0,
                Ok((404, _)) => EXIT_ABSENT,
                Ok((410, _)) => EXIT_PURGED,
                Ok((code, _)) => fail(format!("the remote answered {code} for {id}")),
                Err(why) => fail(why),
            }
        }
        ("purge" | "get-range", _) => EXIT_UNSUPPORTED,
        _ => fail(format!("{verb} {} is no store request", args.join(" "))),
    }
}

/// Stores `file` at `base` in parts, and returns the identity the server hashed.
fn put_https(base: &str, login: &Login, file: &str) -> Result<String, String> {
    let path = Path::new(file);
    let id = identity(path)?;
    let size = std::fs::metadata(path)
        .map_err(|error| format!("cannot read {file}: {error}"))?
        .len();
    let asked = serde_json::json!({"id": id, "size": size}).to_string();
    let (code, body) = login.curl(
        &["-X", "POST", "-H", "Content-Type: application/json"],
        &format!("{base}/uploads"),
        Some(asked.as_bytes()),
        None,
    )?;
    if code != 200 {
        return Err(format!(
            "the remote refused the upload of {id}: {code} {}",
            lossy(&body)
        ));
    }
    let answer: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|_| format!("the remote's upload answer is not JSON: {}", lossy(&body)))?;
    if answer["stored"] == true {
        return Ok(id);
    }
    let upload = answer["upload"]
        .as_str()
        .ok_or("the remote named no upload")?;
    let part_bytes = answer["part_bytes"]
        .as_u64()
        .filter(|bytes| *bytes > 0)
        .ok_or("the remote named no part size")?;
    let mut reader =
        std::fs::File::open(path).map_err(|error| format!("cannot read {file}: {error}"))?;
    let mut part = vec![0u8; usize::try_from(part_bytes).map_err(|_| "a part too large to send")?];
    let mut number = 1u64;
    loop {
        let mut filled = 0;
        while filled < part.len() {
            let read = reader
                .read(&mut part[filled..])
                .map_err(|error| format!("cannot read {file}: {error}"))?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        if filled == 0 && number > 1 {
            break;
        }
        let (code, body) = login.curl(
            &["-X", "PUT", "-H", "Content-Type: application/octet-stream"],
            &format!("{base}/uploads/{upload}/parts/{number}"),
            Some(&part[..filled]),
            None,
        )?;
        if code != 200 {
            return Err(format!(
                "the remote refused part {number} of {id}: {code} {}",
                lossy(&body)
            ));
        }
        number += 1;
        if filled < part.len() {
            break;
        }
    }
    let (code, body) = login.curl(
        &["-X", "POST"],
        &format!("{base}/uploads/{upload}/complete"),
        Some(b""),
        None,
    )?;
    if code != 200 {
        return Err(format!(
            "the remote refused to complete {id}: {code} {}",
            lossy(&body)
        ));
    }
    // bay hashed the bytes itself: the server's answer is checked, never trusted.
    let named: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    if named["id"].as_str().is_some_and(|named| named != id) {
        return Err(format!(
            "the remote named {} for bytes that are {id}",
            named["id"]
        ));
    }
    Ok(id)
}

fn lossy(body: &[u8]) -> String {
    String::from_utf8_lossy(body)
        .trim()
        .chars()
        .take(200)
        .collect()
}

/// Git's login for an https remote (its `http.extraHeader`, and `git credential fill`, never
/// prompting), handed to curl
/// through a config file only this user reads, never on its command line.
struct Login {
    config: Option<tempfile_free::Private>,
}

impl Login {
    fn fill(url: &str) -> Result<Self, String> {
        let mut child = Command::new("git")
            .args(["credential", "fill"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("cannot run git credential: {error}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = writeln!(stdin, "url={url}\n");
        }
        let out = child
            .wait_with_output()
            .map_err(|error| format!("git credential failed: {error}"))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let field = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(&format!("{name}=")))
                .map(str::to_owned)
        };
        let escape = |value: &str| value.replace('\\', "\\\\").replace('"', "\\\"");
        let mut lines = String::new();
        // The header Git itself sends to this remote (`http.extraHeader` for its URL), as a
        // clone that logs in by header (an agent's Basic, a runner's Bearer) pushes with it.
        if let Some(header) = Command::new("git")
            .args(["config", "--get-urlmatch", "http.extraHeader", url])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            .filter(|header| !header.is_empty())
        {
            let _ = std::fmt::Write::write_fmt(
                &mut lines,
                format_args!("header = \"{}\"\n", escape(&header)),
            );
        }
        if let (Some(user), Some(password)) = (field("username"), field("password")) {
            let _ = std::fmt::Write::write_fmt(
                &mut lines,
                format_args!("user = \"{}:{}\"\n", escape(&user), escape(&password)),
            );
        }
        if lines.is_empty() {
            // No login: anonymous reads still work where the remote allows them.
            return Ok(Self { config: None });
        }
        Ok(Self {
            config: Some(tempfile_free::Private::write(&lines)?),
        })
    }

    /// `curl <options> <url>`, with `body` on stdin and the answer in `target` or returned:
    /// the HTTP status and the body.
    fn curl(
        &self,
        options: &[&str],
        url: &str,
        body: Option<&[u8]>,
        target: Option<&Path>,
    ) -> Result<(u16, Vec<u8>), String> {
        let mut cmd = Command::new("curl");
        cmd.args(["-sS", "--fail-with-body", "-o"]);
        cmd.arg(target.map_or_else(
            || std::ffi::OsString::from("-"),
            |path| path.as_os_str().to_owned(),
        ));
        cmd.args(["-w", "\n%{http_code}"]);
        if let Some(config) = &self.config {
            cmd.arg("-K").arg(config.path());
        }
        cmd.args(options);
        if body.is_some() {
            cmd.args(["--data-binary", "@-"]);
        }
        cmd.arg(url);
        cmd.stdin(if body.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|error| format!("cannot run curl: {error}"))?;
        if let (Some(body), Some(mut stdin)) = (body, child.stdin.take()) {
            stdin
                .write_all(body)
                .map_err(|error| format!("cannot send to curl: {error}"))?;
        }
        let out = child
            .wait_with_output()
            .map_err(|error| format!("curl failed: {error}"))?;
        let text = out.stdout;
        let split = text.iter().rposition(|byte| *byte == b'\n').unwrap_or(0);
        let code: u16 = String::from_utf8_lossy(&text[split..])
            .trim()
            .parse()
            .unwrap_or(0);
        if code == 0 {
            return Err(format!("the remote did not answer: {}", lossy(&out.stderr)));
        }
        Ok((code, text[..split].to_vec()))
    }
}

/// A file only this user may read, removed when dropped.
mod tempfile_free {
    use std::path::{Path, PathBuf};

    pub struct Private(PathBuf);

    impl Private {
        pub fn write(text: &str) -> Result<Self, String> {
            use std::io::Write as _;
            let dir = std::env::temp_dir();
            let path = dir.join(format!(
                "bay-login-{}-{}",
                std::process::id(),
                crate::ids::new_change_id().unwrap_or_default()
            ));
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = options
                .open(&path)
                .map_err(|error| format!("cannot write the login for curl: {error}"))?;
            file.write_all(text.as_bytes())
                .map_err(|error| format!("cannot write the login for curl: {error}"))?;
            Ok(Self(path))
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Private {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

/// `word` in single quotes for the remote shell, any quote in it closed and escaped.
fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// The URL of `remote` in the repository `git` reads: its store's address.
#[must_use]
pub fn remote_url(git: &crate::git::Git, remote: &str) -> Option<String> {
    git.run(&["remote", "get-url", remote])
        .ok()
        .map(|url| url.trim().to_owned())
        .filter(|url| !url.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_url_names_its_store() {
        assert_eq!(
            Remote::parse("/srv/repo.git"),
            Remote::Path(PathBuf::from("/srv/repo.git"))
        );
        assert_eq!(
            Remote::parse("file:///srv/repo.git"),
            Remote::Path(PathBuf::from("/srv/repo.git"))
        );
        assert_eq!(
            Remote::parse("ssh://box/srv/repo.git"),
            Remote::Ssh {
                host: "box".to_owned(),
                path: "/srv/repo.git".to_owned()
            }
        );
        assert_eq!(
            Remote::parse("me@box:repo.git"),
            Remote::Ssh {
                host: "me@box".to_owned(),
                path: "repo.git".to_owned()
            }
        );
        assert_eq!(
            Remote::parse("https://host/team/repo.git/"),
            Remote::Https("https://host/team/repo.git".to_owned())
        );
        assert_eq!(
            Remote::parse("C:/repos/one.git"),
            Remote::Path(PathBuf::from("C:/repos/one.git"))
        );
    }

    #[test]
    fn a_word_is_quoted_for_the_remote_shell() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }
}
