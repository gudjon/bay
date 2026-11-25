//! G896 (BACKLOG 296, a fleet measurement, 2026-11-25: two runners that reserved one place in
//! the line both pushed it 13 times in 90 s): a ref another push forwards with another value is
//! taken.
//!
//! An origin in `through` mode marks each ref a push forwards while the forward runs, and its
//! mirror leaves a marked ref to that push. When a third writer took a place on the hosted
//! remote, two runners that pushed that place to the origin were both refused as stale, and the
//! mirror of each refusal left the place to the other runner's mark, so the origin never took
//! it and both read the same free place again. A mark now holds a ref back only while the hosted
//! remote holds the value that mark's push sends; any other push of the ref is refused there.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/origin.rs` `mirror`: leave every marked ref, whatever its value (the refused push
//!   and the serve pass leave the third writer's place).
//! - `src/origin.rs` `forwarding`: say no push sends the hosted value (the origin takes a value
//!   the forwarding push's own update would take).

// Each case runs shell scripts (the check adapter), so these run on Unix only.
#![cfg(unix)]

use std::path::Path;

use crate::g879_an_origin_sends_its_hosted_remote_every_push_it_takes::{
    commit_on, fleet, serve_once, value,
};
use crate::support::TestBox;

/// Marks `name` as forwarded now with `new`, as another push to `origin` does while it runs.
fn forwarding(origin: &Path, name: &str, new: &str) {
    let dir = origin.join("bay/forwarding");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("1-{}", name.replace('/', "-")));
    std::fs::write(file, format!("{name} {new}\n")).unwrap();
}

/// Pushes `commit` to `name` on `remote` from `repo`.
fn push(
    test_box: &TestBox,
    repo: &Path,
    remote: &Path,
    commit: &str,
    name: &str,
) -> (bool, String) {
    let out = test_box
        .git(repo)
        .args([
            "push",
            remote.to_str().unwrap(),
            &format!("{commit}:{name}"),
        ])
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_place_another_push_forwards_with_another_value_is_taken_when_the_hosted_remote_refuses() {
    let test_box = TestBox::new();
    let (hosted, origin, repo) = fleet(&test_box);
    let trunk = value(&test_box, &origin, "refs/heads/main").unwrap();
    let third = commit_on(&test_box, &repo, &trunk, "a third writer's place");
    let other = commit_on(&test_box, &repo, &trunk, "the other runner's place");
    let mine = commit_on(&test_box, &repo, &trunk, "my place");

    // A third writer takes place 5 on the hosted remote, and the other runner forwards its own
    // place 5 now.
    let (took, why) = push(&test_box, &repo, &hosted, &third, "refs/bay/queue/5");
    assert!(took, "{why}");
    forwarding(&origin, "refs/bay/queue/5", &other);

    // My push of place 5 is refused as stale, and the origin takes the third writer's place, so
    // the next read sees it.
    let (took, why) = push(&test_box, &repo, &origin, &mine, "refs/bay/queue/5");
    assert!(!took, "the stale push was taken");
    assert!(why.contains("stale info"), "{why}");
    assert_eq!(
        value(&test_box, &origin, "refs/bay/queue/5"),
        Some(third.clone()),
        "the origin left the place to a push the hosted remote refuses: {why}"
    );

    // The other runner's forward reached the hosted remote with place 6: its own update takes
    // it, so my refused push leaves it.
    let (took, why) = push(&test_box, &repo, &hosted, &other, "refs/bay/queue/6");
    assert!(took, "{why}");
    forwarding(&origin, "refs/bay/queue/6", &other);
    let (took, why) = push(&test_box, &repo, &origin, &mine, "refs/bay/queue/6");
    assert!(!took, "the stale push was taken");
    assert!(why.contains("stale info"), "{why}");
    assert_eq!(value(&test_box, &origin, "refs/bay/queue/6"), None, "{why}");

    // A serve pass does the same: it takes place 7, which a push forwards with another value,
    // and leaves place 6 to the push that sent it.
    let (took, why) = push(&test_box, &repo, &hosted, &third, "refs/bay/queue/7");
    assert!(took, "{why}");
    forwarding(&origin, "refs/bay/queue/7", &mine);
    let served = serve_once(&test_box, &origin);
    assert_eq!(
        value(&test_box, &origin, "refs/bay/queue/7"),
        Some(third),
        "{served}"
    );
    assert_eq!(
        value(&test_box, &origin, "refs/bay/queue/6"),
        None,
        "{served}"
    );
}
