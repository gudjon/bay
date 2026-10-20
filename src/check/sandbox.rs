//! The check sandbox is an allow-list (`INTERFACES` § 4, § 7; G62). A check reads nothing
//! outside its own scratch (its candidate tree and its adapter paths) and the system files a
//! program needs to start. The repository, its common dir, every view, `HOME` and every key
//! are absent because nothing binds them, not because a list hides them. The network is
//! denied unless the check's `[[check]]` entry, judged at the base, says `network = true`.
//!
//! The same list is used on both platforms: `bwrap` binds only these paths (never `/`), and
//! the `sandbox-exec` profile denies by default and allows only these paths.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The system directories bound read-only so a check can run programs: the commands, their
/// shared libraries and the dynamic loader. On a merged-`/usr` system all but `/usr` are
/// symlinks into it and are made as the same symlinks. Neither `/home`, `/root`, `/opt`,
/// `/srv`, `/var`, `/run` nor `/mnt` is here: they hold users' and services' files.
pub const SYSTEM_DIRS: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32",
];

/// The files of `/etc` a check gets, read-only. Never the rest of `/etc`: it holds the box's
/// Git and SSH configuration, host keys and credentials of services.
/// - `ld.so.cache`, `ld.so.conf`, `ld.so.conf.d`: the dynamic loader finds shared libraries
///   (on a multiarch system they are not on its built-in path).
/// - `alternatives`: on Debian-family systems commands such as `awk` are symlinks through it.
/// - `passwd`, `group`, `nsswitch.conf`: a program can name the check's own user and group
///   (Git and SSH refuse to run without one). They hold no password hashes; `shadow` does,
///   and is not here.
/// - `hosts`: `localhost` resolves.
/// - `localtime`: timestamps are in the box's time zone.
pub const SYSTEM_ETC: &[&str] = &[
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/etc/alternatives",
    "/etc/passwd",
    "/etc/group",
    "/etc/nsswitch.conf",
    "/etc/hosts",
    "/etc/localtime",
];

/// What a check with `network = true` also gets: name resolution and the CA certificates TLS
/// needs, which are of no use without a network.
pub const NETWORK_ETC: &[&str] = &[
    "/etc/resolv.conf",
    "/etc/ssl/certs",
    "/etc/ca-certificates",
    "/etc/pki/tls/certs",
    "/etc/pki/ca-trust",
];

fn real(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn os(text: &str) -> OsString {
    OsString::from(text)
}

/// The `bwrap` arguments of the system part of the sandbox: everything but the scratch and
/// the hidden paths. A private `/proc` (of a new PID namespace, so no other process of the
/// box and its root are visible), a minimal `/dev` and an empty `/tmp`, never `--dev-bind /`.
#[must_use]
pub fn bwrap_system_args(network: bool) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["--unshare-pid", "--die-with-parent"]
        .into_iter()
        .map(os)
        .collect();
    if !network {
        args.push(os("--unshare-net"));
    }
    for dir in SYSTEM_DIRS {
        let path = Path::new(dir);
        let Ok(meta) = path.symlink_metadata() else {
            continue;
        };
        if meta.file_type().is_symlink() {
            if let Ok(target) = std::fs::read_link(path) {
                args.extend([os("--symlink"), target.into_os_string(), os(dir)]);
            }
        } else if meta.is_dir() {
            args.extend([os("--ro-bind"), os(dir), os(dir)]);
        }
    }
    let network_etc: &[&str] = if network { NETWORK_ETC } else { &[] };
    for file in SYSTEM_ETC.iter().chain(network_etc) {
        args.extend([os("--ro-bind-try"), os(file), os(file)]);
    }
    for (kind, dir) in [("--proc", "/proc"), ("--dev", "/dev"), ("--tmpfs", "/tmp")] {
        args.extend([os(kind), os(dir)]);
    }
    args
}

/// Whether `path` lies inside something the system part binds, so it would be visible unless
/// it is hidden on top.
fn under_system(path: &Path, network: bool) -> bool {
    let path = real(path);
    let network_etc: &[&str] = if network { NETWORK_ETC } else { &[] };
    SYSTEM_DIRS
        .iter()
        .chain(SYSTEM_ETC)
        .chain(network_etc)
        .any(|root| path.starts_with(real(Path::new(root))))
}

/// The whole `bwrap` argument list before `--`: the system part, then each hidden path that
/// lies inside it made empty, then `scratch` read-write (the candidate and the adapter).
#[must_use]
pub fn bwrap_args(hidden: &[PathBuf], scratch: &Path, network: bool) -> Vec<OsString> {
    let mut args = bwrap_system_args(network);
    for path in hidden.iter().filter(|path| under_system(path, network)) {
        let target = real(path).into_os_string();
        if path.is_file() {
            args.extend([os("--ro-bind"), os("/dev/null"), target]);
        } else {
            args.extend([os("--tmpfs"), target]);
        }
    }
    let scratch = real(scratch).into_os_string();
    args.extend([os("--bind"), scratch.clone(), scratch]);
    args
}

/// The macOS read-only allow-list: the system, the dynamic loader's cache, the time zone and
/// the same few files of `/etc` as on Linux (`/etc` is `/private/etc` there), and the link
/// `/bin/sh` reads to pick its shell.
pub const MACOS_READ: &[(&str, &str)] = &[
    ("subpath", "/usr"),
    ("subpath", "/bin"),
    ("subpath", "/sbin"),
    ("subpath", "/System"),
    ("subpath", "/Library/Apple"),
    ("subpath", "/private/var/db/dyld"),
    ("subpath", "/private/var/db/timezone"),
    ("literal", "/private/etc/passwd"),
    ("literal", "/private/etc/group"),
    ("literal", "/private/etc/hosts"),
    ("literal", "/private/etc/localtime"),
    ("literal", "/private/var/select/sh"),
    ("literal", "/dev/null"),
    ("literal", "/dev/zero"),
    ("literal", "/dev/random"),
    ("literal", "/dev/urandom"),
    ("literal", "/dev/dtracehelper"),
    ("subpath", "/dev/fd"),
];

/// The mach services a program on macOS needs to start and name its user. Not every service:
/// the keychain is one, and a check never reaches it.
pub const MACOS_MACH: &[&str] = &[
    "com.apple.system.opendirectoryd.libinfo",
    "com.apple.system.notification_center",
    "com.apple.system.logger",
    "com.apple.logd",
    "com.apple.diagnosticd",
];

/// The `sandbox-exec` profile: deny by default (never `allow default`), then allow running
/// programs, reading the system list, reading and writing the scratch, and, with
/// `network = true` only, the network. A hidden path inside the system list is denied on top.
#[must_use]
pub fn macos_profile(hidden: &[PathBuf], scratch: &Path, network: bool) -> String {
    let quote = |path: &Path| {
        format!(
            "\"{}\"",
            path.display()
                .to_string()
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
        )
    };
    let filter = |kind: &str, path: &Path| format!("({kind} {})", quote(path));
    let scratch = real(scratch);
    let mut rules = vec![
        "(version 1)(deny default)".to_owned(),
        "(allow process-fork)(allow process-exec)(allow process-info* (target same-sandbox))(allow signal (target same-sandbox))(allow sysctl-read)".to_owned(),
    ];
    let mach: Vec<String> = MACOS_MACH
        .iter()
        .map(|name| format!("(global-name \"{name}\")"))
        .collect();
    rules.push(format!("(allow mach-lookup {})", mach.join(" ")));
    // A path is opened by walking it: each directory above the scratch and the root itself
    // can be looked up, never listed or read.
    let mut ancestors: Vec<String> = scratch
        .ancestors()
        .skip(1)
        .map(|dir| filter("literal", dir))
        .collect();
    for link in ["/etc", "/var", "/tmp", "/private"] {
        ancestors.push(filter("literal", Path::new(link)));
    }
    rules.push(format!(
        "(allow file-read-metadata {})",
        ancestors.join(" ")
    ));
    // The dynamic loader reads the root's names before `main` (macOS 27 aborts every program
    // without it). That lists `/`, nothing below it.
    rules.push("(allow file-read-data (literal \"/\"))".to_owned());
    let read: Vec<String> = MACOS_READ
        .iter()
        .map(|(kind, path)| filter(kind, Path::new(path)))
        .collect();
    rules.push(format!("(allow file-read* {})", read.join(" ")));
    rules.push(
        "(allow file-write* (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/dtracehelper\") (subpath \"/dev/fd\"))".to_owned(),
    );
    if network {
        rules.push("(allow network*)(allow system-socket)(allow file-read* (literal \"/private/var/run/resolv.conf\") (subpath \"/private/etc/ssl\"))(allow mach-lookup (global-name \"com.apple.dnssd.service\") (global-name \"com.apple.SystemConfiguration.configd\") (global-name \"com.apple.trustd\"))".to_owned());
    }
    let denied: Vec<String> = hidden
        .iter()
        .map(|path| {
            let path = real(path);
            if path.is_file() {
                filter("literal", &path)
            } else {
                filter("subpath", &path)
            }
        })
        .collect();
    if !denied.is_empty() {
        rules.push(format!(
            "(deny file-read* file-write* {})",
            denied.join(" ")
        ));
    }
    rules.push(format!(
        "(allow file-read* file-write* {})",
        filter("subpath", &scratch)
    ));
    rules.concat()
}

#[cfg(test)]
mod tests {
    use super::{bwrap_args, macos_profile};
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    /// G62 on macOS, as text: the profile denies by default, never allows by default, allows
    /// the scratch read-write, and the network only with `network = true`.
    #[test]
    fn the_macos_profile_is_an_allow_list() {
        let scratch = Path::new("/nonexistent/scratch/check-0");
        let hidden = [PathBuf::from("/nonexistent/repo")];
        let closed = macos_profile(&hidden, scratch, false);
        assert!(closed.starts_with("(version 1)(deny default)"), "{closed}");
        assert!(!closed.contains("(allow default)"), "{closed}");
        assert!(!closed.contains("network"), "{closed}");
        assert!(
            closed.ends_with(
                "(allow file-read* file-write* (subpath \"/nonexistent/scratch/check-0\"))"
            ),
            "{closed}"
        );
        assert!(
            closed.contains("(deny file-read* file-write* (subpath \"/nonexistent/repo\"))"),
            "{closed}"
        );
        // The loader lists the root's names to start any program (macOS 27 aborts it
        // otherwise), but nothing below the root is readable, and HOME is not on the list.
        assert!(
            closed.contains("(allow file-read-data (literal \"/\"))"),
            "{closed}"
        );
        assert!(!closed.contains("(subpath \"/\")"), "{closed}");
        assert!(!closed.contains("/Users"), "{closed}");
        let open = macos_profile(&hidden, scratch, true);
        assert!(open.contains("(allow network*)"), "{open}");
        assert!(!open.contains("(allow default)"), "{open}");
    }

    /// G62 on Linux, as arguments: `/` is never bound, the network is unshared unless the
    /// check declares it, and the scratch is bound read-write last.
    #[test]
    fn the_bwrap_arguments_are_an_allow_list() {
        let scratch = Path::new("/nonexistent/scratch/check-0");
        let args = bwrap_args(&[], scratch, false);
        let text: Vec<&str> = args.iter().filter_map(|arg| arg.to_str()).collect();
        assert!(
            !text
                .windows(2)
                .any(|pair| pair[1] == "/" && pair[0].ends_with("bind")),
            "{text:?}"
        );
        assert!(text.contains(&"--unshare-net"), "{text:?}");
        assert!(
            !text.contains(&"/home") && !text.contains(&"/etc"),
            "{text:?}"
        );
        assert_eq!(
            &args[args.len() - 3..],
            &[
                OsString::from("--bind"),
                OsString::from(scratch),
                OsString::from(scratch)
            ]
        );
        let open = bwrap_args(&[], scratch, true);
        assert!(!open.contains(&OsString::from("--unshare-net")));
        assert!(open.contains(&OsString::from("/etc/resolv.conf")));
    }
}
