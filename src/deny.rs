//! The shell deny list (`DESIGN` § 11, G39): a fixed list, not a plugin and not a verb.
//!
//! The host hook sees a push; it does not see a local shell command. `bay hook pre-tool` judges
//! a shell command an agent is about to run and denies only the shapes on this list. A normal
//! push of `refs/bay/*` stays allowed: offline submission and the change ref need it.
//!
//! A shape is found behind the words that run another command (`env [VAR=x …]`, `xargs`,
//! `sudo`, `command`, `exec`): Git is judged wherever it is a word of a simple command. It is
//! also found in the script of a shell's `-c` (`bash -c '…'`, `sh -lc "…"`), read with the same
//! tokenizer, and in a shell wrapper inside that one. bay never evaluates a shell: no
//! expansion, no aliases, no `eval`. A script nested deeper than [`MAX_DEPTH`] is not read, and
//! is denied when it mentions Git (fail closed).
//!
//! NEW-4: in the view of a change of no lane, the shapes that copy a sandbox lane's work into
//! the view are denied too ([`denied_lane`]): `git merge` and `git cherry-pick` of a lane ref,
//! `git checkout <lane ref> -- <paths>` and `git restore --source <lane ref>`. The list is
//! conservative: it names the lane ref in a word of the command, as a full ref name or as Git
//! would complete a short one. Other ways to copy bytes (`git show <ref>:<path>`, a patch, an
//! editor) are not judged here; a merge or cherry-pick that made it into the view's history is
//! refused at land (`LANE_LEAK`), and content copied by hand cannot be detected.

/// One denial: the deny-list shape the command matched, the shell wrapper it was found in, and
/// the line the agent is told. `shape` and `wrapped` come from fixed lists, never from the
/// command, so a record of them never holds the command text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denial {
    pub shape: &'static str,
    pub wrapped: Option<&'static str>,
    pub reason: String,
}

/// How many shell `-c` scripts deep bay reads: a wrapper and one wrapper inside it
/// (`bash -c "sh -c '…'"`).
pub const MAX_DEPTH: usize = 2;

/// The shells whose `-c` script bay reads, each with the wrapper's name in a record.
const SHELLS: [(&str, &str); 5] = [
    ("bash", "bash -c"),
    ("sh", "sh -c"),
    ("zsh", "zsh -c"),
    ("dash", "dash -c"),
    ("ksh", "ksh -c"),
];

/// Why a shell command is denied, or `None` when it may run. `trunk` is the trunk branch
/// name; `current` is the branch checked out where the command runs, if any.
#[must_use]
pub fn denied(command: &str, trunk: &str, current: Option<&str>) -> Option<Denial> {
    denied_with(command, Trunk::Named(trunk), &[], current)
}

/// [`denied`] where `targets` are the refs of the config's `[[target]]`s (NEW-5): a push to one
/// is denied too, as a push of trunk is. A target moves only by bay's promotion rules.
#[must_use]
pub fn denied_targets(
    command: &str,
    trunk: &str,
    targets: &[String],
    current: Option<&str>,
) -> Option<Denial> {
    denied_with(command, Trunk::Named(trunk), targets, current)
}

/// The trunk a push is judged against.
#[derive(Clone, Copy)]
enum Trunk<'a> {
    Named(&'a str),
    /// The repository names no trunk bay can read (`CONFIG_MISSING`): every push that is not
    /// only of `refs/bay/*` could be of trunk, and is denied (fail closed).
    Unknown,
}

fn denied_with(
    command: &str,
    trunk: Trunk<'_>,
    targets: &[String],
    current: Option<&str>,
) -> Option<Denial> {
    denied_in(command, trunk, targets, current, 0)
}

/// [`denied_with`] for a command `depth` shell scripts deep.
fn denied_in(
    command: &str,
    trunk: Trunk<'_>,
    targets: &[String],
    current: Option<&str>,
    depth: usize,
) -> Option<Denial> {
    simple_commands(command)
        .into_iter()
        .find_map(|words| denied_simple(&words, trunk, targets, current, depth))
}

/// One simple command: its Git, then each shell `-c` script among its words. A denial found in
/// a script names the outermost wrapper.
fn denied_simple(
    words: &[String],
    trunk: Trunk<'_>,
    targets: &[String],
    current: Option<&str>,
    depth: usize,
) -> Option<Denial> {
    if let Some(denial) = denied_git(words, trunk, targets, current) {
        return Some(denial);
    }
    shell_scripts(words).into_iter().find_map(|(wrapper, script)| {
        let denial = if depth < MAX_DEPTH {
            denied_in(script, trunk, targets, current, depth + 1)?
        } else if script.to_ascii_lowercase().contains("git") {
            Denial {
                shape: "nested too deep",
                wrapped: None,
                reason: format!(
                    "bay denies Git inside shell wrappers nested more than {MAX_DEPTH} deep in an agent shell; run it with fewer"
                ),
            }
        } else {
            return None;
        };
        Some(Denial {
            wrapped: Some(wrapper),
            ..denial
        })
    })
}

/// Every word of a shell `command`, read with the deny list's tokenizer: the words of each simple
/// command and of each shell `-c` script among them, [`MAX_DEPTH`] scripts deep (plan H2). The
/// claims judgement (`crate::claims`, G101) looks for a path among them.
#[must_use]
pub fn words(command: &str) -> Vec<String> {
    let mut all = Vec::new();
    words_in(command, 0, &mut all);
    all
}

fn words_in(command: &str, depth: usize, all: &mut Vec<String>) {
    for words in simple_commands(command) {
        if depth < MAX_DEPTH {
            for (_, script) in shell_scripts(&words) {
                words_in(script, depth + 1, all);
            }
        }
        all.extend(words);
    }
}

/// The `-c` scripts of the shells among `words`, each with its wrapper's name: `bash -c '…'`,
/// `sh -lc "…"`, `bash --norc -c '…'`, also after `env`, `xargs` or `sudo`. The script is the
/// first word after the shell's options when one of them holds `c`.
fn shell_scripts(words: &[String]) -> Vec<(&'static str, &str)> {
    let mut scripts = Vec::new();
    for (at, word) in words.iter().enumerate() {
        let stem = command_stem(word);
        let Some((_, wrapper)) = SHELLS
            .iter()
            .find(|(shell, _)| stem.eq_ignore_ascii_case(shell))
        else {
            continue;
        };
        let mut script_follows = false;
        for next in &words[at + 1..] {
            if next.starts_with("--") {
                continue;
            }
            if let Some(flags) = next.strip_prefix('-') {
                script_follows |= flags.contains('c');
                continue;
            }
            if script_follows {
                scripts.push((*wrapper, next.as_str()));
            }
            break;
        }
    }
    scripts
}

/// Splits a command line at `&&`, `||`, `;`, `|` and newlines into words, honouring simple
/// single and double quotes.
fn simple_commands(command: &str) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    let flush_word = |word: &mut String, words: &mut Vec<String>| {
        if !word.is_empty() {
            words.push(std::mem::take(word));
        }
    };
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some(open), close) if open == close => quote = None,
            (None, '\'' | '"') => quote = Some(ch),
            (None, ';' | '\n' | '|' | '&') => {
                if matches!(ch, '|' | '&') && chars.peek() == Some(&ch) {
                    chars.next();
                }
                flush_word(&mut word, &mut words);
                if !words.is_empty() {
                    commands.push(std::mem::take(&mut words));
                }
            }
            (None, space) if space.is_whitespace() => flush_word(&mut word, &mut words),
            (_, other) => word.push(other),
        }
    }
    flush_word(&mut word, &mut words);
    if !words.is_empty() {
        commands.push(words);
    }
    commands
}

/// The command a word runs: its base name, after the last `/` or `\`, without an extension a
/// Windows shell runs (`.exe`, `.cmd`, `.bat`, `.com`, in any case).
fn command_stem(word: &str) -> &str {
    let base = word.rsplit(['/', '\\']).next().unwrap_or(word);
    match base.rsplit_once('.') {
        Some((stem, extension))
            if ["exe", "cmd", "bat", "com"]
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known)) =>
        {
            stem
        }
        _ => base,
    }
}

/// Whether `word` runs Git: its command stem is `git`, in any case. A case-insensitive file
/// system runs `GIT` as `git`; denying more is the fail-closed side.
fn is_git(word: &str) -> bool {
    command_stem(word).eq_ignore_ascii_case("git")
}

fn denied_git(
    words: &[String],
    trunk: Trunk<'_>,
    targets: &[String],
    current: Option<&str>,
) -> Option<Denial> {
    let git = words.iter().position(|word| is_git(word))?;
    // Skip git's own options (`-C <dir>`, `-c key=value`, `--git-dir=…`) to the subcommand.
    let mut rest = words[git + 1..].iter().peekable();
    let mut subcommand = None;
    while let Some(word) = rest.next() {
        if word == "-C" || word == "-c" {
            rest.next();
        } else if !word.starts_with('-') {
            subcommand = Some(word.as_str());
            break;
        }
    }
    let args: Vec<&str> = rest.map(String::as_str).collect();
    let has = |flag: &str| args.contains(&flag);
    let deny = |shape: &'static str| {
        Some(Denial {
            shape,
            wrapped: None,
            reason: format!("bay denies `{shape}` in an agent shell (DESIGN § 11)"),
        })
    };
    match subcommand? {
        "reset" if has("--hard") => deny("git reset --hard"),
        "clean"
            if args
                .iter()
                .any(|arg| arg.starts_with('-') && !arg.starts_with("--") && arg.contains('f'))
                || has("--force") =>
        {
            deny("git clean -f")
        }
        "branch" if has("-D") => deny("git branch -D"),
        "checkout" if has(".") => deny("git checkout ."),
        "restore" if has(".") => deny("git restore ."),
        "push" => denied_push(&args, trunk, targets, current),
        _ => None,
    }
}

fn denied_push(
    args: &[&str],
    trunk: Trunk<'_>,
    targets: &[String],
    current: Option<&str>,
) -> Option<Denial> {
    let denial = |shape: &'static str, reason: String| {
        Some(Denial {
            shape,
            wrapped: None,
            reason,
        })
    };
    let positional: Vec<&str> = args
        .iter()
        .copied()
        .filter(|arg| !arg.starts_with('-'))
        .collect();
    let refspecs = positional.get(1..).unwrap_or_default();
    // NEW-5: a target is judged before the force flags, so a force-push or a delete of one
    // (`--delete staging`, `:staging`) names the target it would move.
    let is_branch =
        |branch: &str, reference: &str| reference.strip_prefix("refs/heads/") == Some(branch);
    let pushes_target = |reference: &str| {
        if refspecs.is_empty() {
            return current.is_some_and(|branch| is_branch(branch, reference));
        }
        refspecs.iter().copied().any(|refspec| {
            let destination = refspec.rsplit(':').next().unwrap_or(refspec);
            let destination = destination.strip_prefix('+').unwrap_or(destination);
            destination == reference
                || is_branch(destination, reference)
                || (destination == "HEAD"
                    && current.is_some_and(|branch| is_branch(branch, reference)))
        })
    };
    if let Some(target) = targets
        .iter()
        .find(|reference| pushes_target(reference.as_str()))
    {
        return denial(
            "git push of a target",
            format!(
                "bay denies a push of the promotion target {target} from an agent shell; a target moves only by bay's promotion rules"
            ),
        );
    }
    let force = args.iter().any(|arg| {
        *arg == "-f"
            || *arg == "--force"
            || arg.starts_with("--force-with-lease")
            || arg.starts_with("--force-if-includes")
    }) || args
        .iter()
        .any(|arg| !arg.starts_with('-') && arg.starts_with('+'));
    if force {
        return denial(
            "git push --force",
            "bay denies a force-push from an agent shell (DESIGN § 11)".to_owned(),
        );
    }
    if args.iter().any(|arg| *arg == "--all" || *arg == "--mirror") {
        return denial(
            "git push --all",
            "bay denies pushing every branch, trunk with them, from an agent shell".to_owned(),
        );
    }
    let trunk = match trunk {
        Trunk::Named(trunk) => trunk,
        Trunk::Unknown => {
            let only_bay = !refspecs.is_empty()
                && refspecs.iter().all(|refspec| {
                    refspec
                        .rsplit(':')
                        .next()
                        .is_some_and(|destination| destination.starts_with(crate::names::BAY))
                });
            if only_bay {
                return None;
            }
            return denial(
                "git push, trunk unknown",
                "bay cannot tell this repository's trunk (CONFIG_MISSING: no git config bay.trunk and no remote default branch), so it denies a push that could be of trunk from an agent shell".to_owned(),
            );
        }
    };
    let pushes_trunk = |refspec: &str| {
        let destination = refspec.rsplit(':').next().unwrap_or(refspec);
        destination == trunk
            || destination == format!("refs/heads/{trunk}")
            || (destination == "HEAD" && current == Some(trunk))
    };
    let trunk_push = if refspecs.is_empty() {
        current == Some(trunk)
    } else {
        refspecs.iter().any(|refspec| pushes_trunk(refspec))
    };
    if !trunk_push {
        return None;
    }
    denial(
        "git push of trunk",
        format!("bay denies a push of trunk ({trunk}) from an agent shell; bay publishes trunk"),
    )
}

/// NEW-4: why a shell command that copies from a lane ref is denied, or `None`. `lane_refs` are
/// the lane ref patterns as this clone holds them (`crate::lane::local_patterns`).
#[must_use]
pub fn denied_lane(command: &str, lane_refs: &[String]) -> Option<Denial> {
    if lane_refs.is_empty() {
        return None;
    }
    let mut commands = Vec::new();
    lane_commands(command, 0, &mut commands);
    commands.iter().find_map(|(words, wrapped)| {
        denied_lane_git(words, lane_refs).map(|denial| Denial {
            wrapped: *wrapped,
            ..denial
        })
    })
}

/// The simple commands of `command` and of the shell `-c` scripts among them, [`MAX_DEPTH`]
/// scripts deep, each with its outermost wrapper.
fn lane_commands(command: &str, depth: usize, out: &mut Vec<(Vec<String>, Option<&'static str>)>) {
    for words in simple_commands(command) {
        if depth < MAX_DEPTH {
            for (wrapper, script) in shell_scripts(&words) {
                let mut inner = Vec::new();
                lane_commands(script, depth + 1, &mut inner);
                out.extend(inner.into_iter().map(|(words, _)| (words, Some(wrapper))));
            }
        }
        out.push((words, None));
    }
}

/// Whether `word`, a revision a Git command was given, names a ref of `lane_refs`: as a full
/// ref name, or as Git completes a short one (`refs/<word>`, `refs/tags/<word>`,
/// `refs/heads/<word>`, `refs/remotes/<word>`). A range names both its ends; a suffix
/// (`~1`, `^2`, `@{…}`) is not part of the name.
fn names_lane(word: &str, lane_refs: &[String]) -> bool {
    word.split("..")
        .map(|end| end.trim_start_matches('.').trim_start_matches('^'))
        .filter(|end| !end.is_empty())
        .any(|end| {
            let name = end.split(['~', '^', ':']).next().unwrap_or(end);
            let name = name.split("@{").next().unwrap_or(name);
            let candidates = [
                name.to_owned(),
                format!("refs/{name}"),
                format!("refs/tags/{name}"),
                format!("refs/heads/{name}"),
                format!("refs/remotes/{name}"),
            ];
            candidates.iter().any(|candidate| {
                lane_refs
                    .iter()
                    .any(|pattern| crate::lane::matches(pattern, candidate))
            })
        })
}

/// The positional words of `args` before `--`, past the options in `valued`, which take a value
/// of their own.
fn positional<'a>(args: &[&'a str], valued: &[&str]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut words = args.iter().copied();
    while let Some(word) = words.next() {
        if word == "--" {
            break;
        }
        if valued.contains(&word) {
            words.next();
        } else if !word.starts_with('-') {
            out.push(word);
        }
    }
    out
}

fn denied_lane_git(words: &[String], lane_refs: &[String]) -> Option<Denial> {
    let git = words.iter().position(|word| is_git(word))?;
    let mut rest = words[git + 1..].iter();
    let mut subcommand = None;
    while let Some(word) = rest.next() {
        if word == "-C" || word == "-c" {
            rest.next();
        } else if !word.starts_with('-') {
            subcommand = Some(word.as_str());
            break;
        }
    }
    let args: Vec<&str> = rest.map(String::as_str).collect();
    let deny = |shape: &'static str| {
        Some(Denial {
            shape,
            wrapped: None,
            reason: format!(
                "bay denies `{shape}` in the view of a change of no lane: a sandbox lane's work never reaches trunk (start the work with bay start --lane <lane>)"
            ),
        })
    };
    let lane = |word: &str| names_lane(word, lane_refs);
    let shape = match subcommand? {
        "merge" => positional(
            &args,
            &[
                "-m",
                "-F",
                "-s",
                "-X",
                "--file",
                "--strategy",
                "--strategy-option",
            ],
        )
        .into_iter()
        .any(lane)
        .then_some("git merge of a lane ref"),
        "cherry-pick" => positional(
            &args,
            &[
                "-m",
                "-s",
                "-X",
                "--mainline",
                "--strategy",
                "--strategy-option",
            ],
        )
        .into_iter()
        .any(lane)
        .then_some("git cherry-pick of a lane ref"),
        "checkout" => {
            // `git checkout <lane ref> -- <paths>` (or `<lane ref> <paths>`): files of the lane.
            // A plain switch to a lane branch moves the view's `HEAD`, which land refuses.
            let named = positional(&args, &["-b", "-B", "--orphan", "--conflict"]);
            let paths_follow = args.contains(&"--") || named.len() > 1;
            (paths_follow && named.first().is_some_and(|first| lane(first)))
                .then_some("git checkout <lane ref> --")
        }
        "restore" => {
            let mut words = args.iter().copied();
            let mut source = None;
            while let Some(word) = words.next() {
                if word == "--" {
                    break;
                }
                if word == "--source" || word == "-s" {
                    source = words.next();
                } else if let Some(value) = word.strip_prefix("--source=") {
                    source = Some(value);
                } else if let Some(value) = word
                    .strip_prefix("-s")
                    .filter(|value| !value.is_empty() && !word.starts_with("--"))
                {
                    source = Some(value);
                }
            }
            source
                .is_some_and(lane)
                .then_some("git restore --source <lane ref>")
        }
        _ => None,
    }?;
    deny(shape)
}

/// `bay hook <event>` for the shell deny list: a `pre-tool` event on Claude Code's `Bash` tool.
/// Returns the denial, or `None` to allow. The payload was read by the caller (G59): one this
/// cannot read is never judged here.
#[must_use]
pub fn judge_hook(event: &str, input: &str, dir: &std::path::Path) -> Option<Denial> {
    if event != "pre-tool" {
        return None;
    }
    let payload: serde_json::Value = serde_json::from_str(input).ok()?;
    if payload["tool_name"] != "Bash" {
        return None;
    }
    let command = payload["tool_input"]["command"].as_str()?;
    let cwd = payload["cwd"]
        .as_str()
        .map_or_else(|| dir.to_path_buf(), std::path::PathBuf::from);
    let current = crate::git::Git::new(&cwd)
        .run(&["symbolic-ref", "--short", "-q", "HEAD"])
        .ok();
    // The trunk is the one every bay command uses (ruling: config location): the bootstrap of
    // this repository's own git config or its remote's default branch, never a file in the
    // agent's work tree. Outside a repository a push fails anyway; `main` keeps the shape.
    let denial = match crate::repo::Bootstrap::of(&cwd) {
        None => denied(command, "main", current.as_deref()),
        Some(Ok(bootstrap)) => denied_targets(
            command,
            &bootstrap.trunk,
            &target_refs(command, &cwd),
            current.as_deref(),
        ),
        Some(Err(_)) => denied_with(command, Trunk::Unknown, &[], current.as_deref()),
    };
    denial.or_else(|| denied_lane(command, &view_lane_refs(command, &cwd)))
}

/// NEW-5: the refs of the `[[target]]`s the config at trunk declares, to judge `command` by in
/// `cwd`: none unless it names `push`, and none when the config cannot be read (the trunk is
/// still judged). Only such a command pays for the read.
fn target_refs(command: &str, cwd: &std::path::Path) -> Vec<String> {
    if !command.contains("push") {
        return Vec::new();
    }
    crate::repo::Repo::open(cwd)
        .map(|repo| {
            repo.config
                .targets
                .iter()
                .map(crate::config::Target::reference)
                .collect()
        })
        .unwrap_or_default()
}

/// NEW-4: the lane ref patterns to judge `command` by in `cwd`: none unless it names one of the
/// Git commands [`denied_lane`] judges, `cwd` is the view of a change of no lane, and the
/// config at trunk declares lanes. Only such a command pays for the reads.
fn view_lane_refs(command: &str, cwd: &std::path::Path) -> Vec<String> {
    if !["merge", "cherry-pick", "checkout", "restore"]
        .iter()
        .any(|word| command.contains(word))
    {
        return Vec::new();
    }
    let Ok(repo) = crate::repo::Repo::open(cwd) else {
        return Vec::new();
    };
    if repo.config.lanes.is_empty() {
        return Vec::new();
    }
    let Some(id) = repo.view_change(cwd) else {
        return Vec::new();
    };
    let in_lane = crate::change::Chain::new(&repo.git)
        .read(&id)
        .is_ok_and(|(_, record)| record.lane.is_some());
    if in_lane {
        return Vec::new();
    }
    repo.config
        .lanes
        .iter()
        .flat_map(|lane| lane.refs.iter())
        .flat_map(|pattern| crate::lane::local_patterns(pattern, repo.remote()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{denied, denied_targets};

    #[test]
    fn every_shape_on_the_list_is_denied() {
        for command in [
            "git reset --hard",
            "git reset --hard HEAD~1",
            "git clean -f",
            "git clean -fd",
            "git branch -D old",
            "git checkout .",
            "git restore .",
            "git push --force origin topic",
            "git push --force-with-lease origin topic",
            "git push -f",
            "git push origin +topic",
            "git push origin main",
            "git push origin HEAD:main",
            "git push origin x:refs/heads/main",
            "git push --all origin",
            "cd sub && git -C .. reset --hard",
        ] {
            assert!(
                denied(command, "main", Some("topic")).is_some(),
                "{command}"
            );
        }
    }

    #[test]
    fn ordinary_commands_and_bay_ref_pushes_run() {
        for command in [
            "git status",
            "git checkout -b topic",
            "git restore src/a.rs",
            "git clean -n",
            "git branch -d merged",
            "git push origin refs/bay/changes/abcd2345",
            "git push origin refs/bay/changes/abcd2345:refs/bay/changes/abcd2345",
            "git push origin topic",
            "echo 'git reset --hard' > notes.txt",
            // G279: `bay save` is bay's own way to land the main checkout's edits.
            "bay save CARD-1",
            "bay save CARD-1 --message 'git push origin main'",
            "bash -c 'bay save CARD-1'",
        ] {
            assert!(
                denied(command, "main", Some("topic")).is_none(),
                "{command}"
            );
        }
    }

    #[test]
    fn a_bare_push_from_trunk_is_a_push_of_trunk() {
        assert!(denied("git push", "main", Some("main")).is_some());
        assert!(denied("git push", "main", Some("topic")).is_none());
    }

    /// NEW-5: a push to a target's ref is denied in every refspec form and names the target; a
    /// push of another branch still runs.
    #[test]
    fn a_push_of_a_target_is_denied() {
        let targets = [
            "refs/heads/staging".to_owned(),
            "refs/deploy/live".to_owned(),
        ];
        for (command, current) in [
            ("git push origin x:refs/heads/staging", "topic"),
            ("git push origin x:staging", "topic"),
            ("git push origin staging", "topic"),
            ("git push origin HEAD:staging", "topic"),
            ("git push origin +x:staging", "topic"),
            ("git push --force origin staging", "topic"),
            ("git push origin --delete staging", "topic"),
            ("git push origin :staging", "topic"),
            ("git push origin HEAD", "staging"),
            ("git push", "staging"),
            ("bash -c 'git push origin x:staging'", "topic"),
        ] {
            let denial = denied_targets(command, "main", &targets, Some(current))
                .unwrap_or_else(|| panic!("{command}"));
            assert_eq!(denial.shape, "git push of a target", "{command}");
            assert!(denial.reason.contains("refs/heads/staging"), "{command}");
        }
        let denial =
            denied_targets("git push origin x:refs/deploy/live", "main", &targets, None).unwrap();
        assert!(denial.reason.contains("refs/deploy/live"));
        for command in ["git push origin topic", "git push origin x:staging-notes"] {
            assert!(
                denied_targets(command, "main", &targets, Some("topic")).is_none(),
                "{command}"
            );
        }
    }

    /// Plan H2: each wrapper of a shape is the same shape, named with the outermost shell
    /// wrapper; the words that run another command are not wrappers in the record.
    #[test]
    fn a_wrapped_shape_is_the_same_shape() {
        for (command, shape, wrapped) in [
            (
                "bash -c 'git reset --hard'",
                "git reset --hard",
                Some("bash -c"),
            ),
            ("sh -c \"git push -f\"", "git push --force", Some("sh -c")),
            (
                "/usr/bin/bash -lc 'git clean -f'",
                "git clean -f",
                Some("bash -c"),
            ),
            (
                "BASH.EXE -c 'git branch -D x'",
                "git branch -D",
                Some("bash -c"),
            ),
            (
                "zsh --no-rcs -c 'git restore .'",
                "git restore .",
                Some("zsh -c"),
            ),
            (
                "env A=1 B=2 git push origin main",
                "git push of trunk",
                None,
            ),
            ("xargs -0 git checkout .", "git checkout .", None),
            ("sudo -E git reset --hard", "git reset --hard", None),
            ("command git push --all", "git push --all", None),
            ("exec git push --mirror", "git push --all", None),
            (
                "bash -c \"env git push origin main\"",
                "git push of trunk",
                Some("bash -c"),
            ),
            (
                "sudo sh -c 'xargs git clean -fd'",
                "git clean -f",
                Some("sh -c"),
            ),
            (
                "bash -c \"sh -c 'git reset --hard'\"",
                "git reset --hard",
                Some("bash -c"),
            ),
            (
                "ls; bash -c 'cd x || git push -f'",
                "git push --force",
                Some("bash -c"),
            ),
        ] {
            let denial = denied(command, "main", Some("topic"))
                .unwrap_or_else(|| panic!("{command} was allowed"));
            assert_eq!(denial.shape, shape, "{command}");
            assert_eq!(denial.wrapped, wrapped, "{command}");
        }
    }

    #[test]
    fn a_wrapped_ordinary_command_runs() {
        for command in [
            "bash -c 'git status'",
            "sh -c 'echo done > notes.txt' && true",
            "bash -c \"git push origin refs/bay/changes/abcd2345\"",
            "env GIT_TRACE=1 git log",
            "xargs git add",
            "bash script.sh -c",
            "bash -c",
            "sh -c 'sh -c \"ls\"'",
        ] {
            assert!(
                denied(command, "main", Some("topic")).is_none(),
                "{command}"
            );
        }
    }

    /// Past [`super::MAX_DEPTH`] scripts bay does not read on: a mention of Git is denied.
    #[test]
    fn a_wrapper_nested_too_deep_is_denied_when_it_mentions_git() {
        let deep = "bash -c \"sh -c 'bash -c git-status'\"";
        let denial = denied(deep, "main", Some("topic")).unwrap();
        assert_eq!(denial.shape, "nested too deep");
        assert_eq!(denial.wrapped, Some("bash -c"));
        assert!(denied("bash -c \"sh -c 'bash -c ls'\"", "main", Some("topic")).is_none());
    }

    /// NEW-4: in a view of no lane, the shapes that copy from a lane ref are denied; the same
    /// commands of other refs run.
    #[test]
    fn copying_from_a_lane_ref_is_denied() {
        let lanes: Vec<String> = crate::lane::local_patterns("refs/heads/research/*", "origin");
        for (command, shape, wrapped) in [
            ("git merge research/x", "git merge of a lane ref", None),
            (
                "git merge --no-ff -m 'take it' origin/research/x",
                "git merge of a lane ref",
                None,
            ),
            (
                "git merge refs/remotes/origin/research/x~1",
                "git merge of a lane ref",
                None,
            ),
            (
                "git cherry-pick research/x",
                "git cherry-pick of a lane ref",
                None,
            ),
            (
                "git cherry-pick main..research/x",
                "git cherry-pick of a lane ref",
                None,
            ),
            (
                "git cherry-pick -m 1 research/x^2",
                "git cherry-pick of a lane ref",
                None,
            ),
            (
                "git checkout research/x -- src/a.rs",
                "git checkout <lane ref> --",
                None,
            ),
            (
                "git checkout origin/research/x src/a.rs",
                "git checkout <lane ref> --",
                None,
            ),
            (
                "git restore --source research/x src/a.rs",
                "git restore --source <lane ref>",
                None,
            ),
            (
                "git restore --source=origin/research/x .",
                "git restore --source <lane ref>",
                None,
            ),
            (
                "git restore -sresearch/x src/a.rs",
                "git restore --source <lane ref>",
                None,
            ),
            (
                "bash -c 'git merge research/x'",
                "git merge of a lane ref",
                Some("bash -c"),
            ),
        ] {
            let denial = super::denied_lane(command, &lanes)
                .unwrap_or_else(|| panic!("{command} was allowed"));
            assert_eq!(denial.shape, shape, "{command}");
            assert_eq!(denial.wrapped, wrapped, "{command}");
        }
        for command in [
            "git merge topic",
            "git merge -m research/x topic",
            "git cherry-pick abc123",
            "git checkout research/x",
            "git checkout -b research/new",
            "git checkout main -- src/a.rs",
            "git restore --source main src/a.rs",
            "git restore src/a.rs",
            "git log research/x",
            "git diff research/x",
            "echo 'git merge research/x' > notes.txt",
            "git merge researchers/x",
        ] {
            assert!(super::denied_lane(command, &lanes).is_none(), "{command}");
        }
        assert!(super::denied_lane("git merge research/x", &[]).is_none());
    }

    /// Plan W1: a Windows shell runs Git as `git`, `git.exe` or `git.cmd`, by any case and by a
    /// path with either separator. Each is the same Git, so each shape is denied as `git` is.
    #[test]
    fn git_exe_is_git() {
        for command in [
            "git.exe reset --hard",
            "GIT.EXE push --force origin topic",
            "\"C:\\Program Files\\Git\\cmd\\git.exe\" clean -f",
            "C:/tools/Git.exe push origin main",
            "/c/tools/git.exe -C .. branch -D old",
            "C:\\Git\\cmd\\git reset --hard",
            "GIT reset --hard",
            "Git.Cmd push --force origin topic",
            "C:\\Git\\cmd\\git.bat clean -f",
        ] {
            assert!(
                denied(command, "main", Some("topic")).is_some(),
                "{command}"
            );
        }
        for command in [
            "git.exe status",
            "notgit.exe reset --hard",
            "git.exe.bak reset --hard",
            "gitk reset --hard",
        ] {
            assert!(
                denied(command, "main", Some("topic")).is_none(),
                "{command}"
            );
        }
    }
}
