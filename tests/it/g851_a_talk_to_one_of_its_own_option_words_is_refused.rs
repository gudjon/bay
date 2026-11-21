//! G851 (a fleet finding, 2026-11-21): `bay talk show <id>`, the reading verb of an older bay
//! (today `--show <id>`), sent a discovery talk to an agent named `show` with the id as its
//! question, and pushed it. No agent is named `show`, so nothing acked or closed it, and its
//! sender has no verb to take it back. `bay talk` now refuses a recipient named as one of its own
//! options that take an id (`show`, `round`, `answer`, `backbrief`), names the option, and
//! sends nothing.
//!
//! Guard to mutate: in `src/talk/send.rs` `send`, drop the `option_word` call; the talk to
//! `show` is then sent and its ref is on the remote.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::talk::approvals_talk;

#[test]
fn a_talk_to_one_of_its_own_option_words_is_refused_and_sends_nothing() {
    // GIVEN a talk sent to agent-b
    let case = approvals_talk();
    let id = case.ask(
        "agent",
        "agent-b",
        "Pick a colour",
        &["--kind", "discovery", "--decision", "colour"],
        &[],
    );

    // WHEN its sender names an option word as the recipient, alone or in a round
    for (args, option) in [
        (vec!["talk", "show", id.as_str()], "--show"),
        (
            vec!["talk", "--to", "backbrief", "Say how it went"],
            "--backbrief",
        ),
    ] {
        let (code, refused) = case.bay("agent", &args);

        // THEN bay refuses it and names the option
        assert_eq!(code, 10, "{args:?}: {refused}");
        assert_eq!(refused["reason"], "TALK_REFUSED", "{args:?}: {refused}");
        assert!(refused.to_string().contains(option), "{args:?}: {refused}");
    }

    // AND nothing reached the remote under those names
    let refs = case.test_box.git_bare(
        &case.remote,
        &["for-each-ref", "--format=%(refname)", "refs/bay/talk/"],
    );
    assert!(refs.contains("refs/bay/talk/agent-b/"), "{refs}");
    assert!(!refs.contains("refs/bay/talk/show/"), "{refs}");
    assert!(!refs.contains("refs/bay/talk/backbrief/"), "{refs}");

    // AND the option itself still reads the talk
    let (code, shown) = case.bay("agent", &["talk", "--show", &id]);
    assert_eq!(code, 0, "{shown}");
    assert_eq!(shown["detail"]["talk"], id.as_str(), "{shown}");
}
