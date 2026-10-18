//! The shell deny list (`DESIGN` § 11, G39): a fixed list, not a plugin and not a verb.
//!
//! The host hook sees a push; it does not see a local shell command. `bay hook pre-tool` judges
//! a shell command an agent is about to run and denies only the shapes on this list. A normal
//! push of `refs/bay/*` stays allowed: offline submission and the change ref need it.

/// The reason a shell command is denied, or `None` when it may run. `trunk` is the trunk branch
/// name; `current` is the branch checked out where the command runs, if any.
#[must_use]
pub fn denied(command: &str, trunk: &str, current: Option<&str>) -> Option<String> {
    simple_commands(command)
        .into_iter()
        .find_map(|words| denied_simple(&words, trunk, current))
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

fn denied_simple(words: &[String], trunk: &str, current: Option<&str>) -> Option<String> {
    let git = words
        .iter()
        .position(|word| word == "git" || word.ends_with("/git"))?;
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
    let deny = |shape: &str| {
        Some(format!(
            "bay denies `{shape}` in an agent shell (DESIGN § 11)"
        ))
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
        "push" => denied_push(&args, trunk, current),
        _ => None,
    }
}

fn denied_push(args: &[&str], trunk: &str, current: Option<&str>) -> Option<String> {
    let force = args.iter().any(|arg| {
        *arg == "-f"
            || *arg == "--force"
            || arg.starts_with("--force-with-lease")
            || arg.starts_with("--force-if-includes")
    }) || args
        .iter()
        .any(|arg| !arg.starts_with('-') && arg.starts_with('+'));
    if force {
        return Some("bay denies a force-push from an agent shell (DESIGN § 11)".to_owned());
    }
    if args.iter().any(|arg| *arg == "--all" || *arg == "--mirror") {
        return Some(
            "bay denies pushing every branch, trunk with them, from an agent shell".to_owned(),
        );
    }
    let positional: Vec<&str> = args
        .iter()
        .copied()
        .filter(|arg| !arg.starts_with('-'))
        .collect();
    let refspecs = positional.get(1..).unwrap_or_default();
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
    trunk_push.then(|| {
        format!("bay denies a push of trunk ({trunk}) from an agent shell; bay publishes trunk")
    })
}

/// `bay hook <event>` for the shell deny list: a `pre-tool` event on Claude Code's `Bash` tool.
/// Returns the deny reason, or `None` to allow.
#[must_use]
pub fn judge_hook(event: &str, input: &str, dir: &std::path::Path) -> Option<String> {
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
    let trunk = crate::repo::Repo::open(&cwd)
        .map_or_else(|_| "main".to_owned(), |repo| repo.trunk().to_owned());
    let current = crate::git::Git::new(&cwd)
        .run(&["symbolic-ref", "--short", "-q", "HEAD"])
        .ok();
    denied(command, &trunk, current.as_deref())
}

#[cfg(test)]
mod tests {
    use super::denied;

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
}
