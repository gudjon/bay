//! The adapter conformance kit: `bay plugin test <kind> -- <command...>` (G250; `INTERFACES`
//! § 3, § 4 and § 7).
//!
//! A connector proves it is bay-compatible by passing a fixed set of contract cases, run the way
//! bay itself runs that kind of adapter: through bay's own invocation code ([`check::run_all`],
//! [`wake`], [`notify`]), with the same environment baseline and the same time limits, so the
//! kit cannot drift from the product. It prints one `bay.result/1` whose `detail.cases` lists
//! each case: its `name`, what was `sent`, what `came_back`, its `result` (`pass`, `fail` or
//! `not_judged`) and `why`. `detail.bay_records` is what bay would record for the first request.
//! The command exits 0 only when every case passes.
//!
//! The kit is side-effect free: it never lands, pushes or writes a ref of any repository. A check
//! adapter runs on a tiny candidate in a scratch repository of the kit's own, removed when the
//! kit ends. A wake or notify adapter runs from the `-C` directory, as bay runs it from the work
//! tree. A wake or notify adapter that answers late is waited for up to twice its limit, so its
//! late answer is still judged, and classified as bay would record it: bay stops waiting at the
//! limit.

use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::change::Actor;
use crate::check;
use crate::evidence::{EvidenceRecord, Verdict};
use crate::git::Git;
use crate::notify;
use crate::result::{BayResult, Decision, Reason};
use crate::talk::wake;

/// The adapter kinds the kit tests: each has a stdin and stdout contract.
pub const KINDS: [&str; 4] = ["check", "wake", "notify", "assets"];

/// How many times its limit the kit waits for a wake or notify adapter.
const GRACE: u32 = 2;

/// The most bytes of an adapter's stdout or stderr a case shows.
const SHOWN_BYTES: usize = 4096;

/// The candidate id and producer name of the kit's check run.
const CANDIDATE_ID: &str = "adapter-test.1";
const PRODUCER: &str = "adapter-test";

const CHECK_RESULT_SCHEMA: &str = "bay.check.result/1";
const VERDICTS: [&str; 4] = ["pass", "product_failure", "infra_failure", "unknown"];

const CHECK_BASELINE: &str = "PATH from the policy's path, HOME and TMPDIR in the check's scratch, \
     LANG=C.UTF-8, BAY_ADAPTER_DIR, and the names in env";
const PATH_AND_HOME: &str = "PATH and HOME";

/// One `bay plugin test` request.
pub struct Test {
    /// `check`, `wake` or `notify`.
    pub kind: String,
    /// The adapter's command, as its config or policy entry would name it.
    pub command: Vec<String>,
    /// Check: the entry's `timeout_s` (the policy's default when not named).
    pub timeout_s: Option<u64>,
    /// Check: the entry's `expected_minimum`.
    pub expected_minimum: u64,
    /// Check: the entry's `path`.
    pub path: Vec<String>,
    /// Check: the entry's `env`.
    pub env: Vec<String>,
    /// Check: more adapter paths beside the program, relative to the `-C` directory.
    pub with: Vec<String>,
}

/// Runs the contract cases of `test.kind` against `test.command`, from `dir`.
#[must_use]
pub fn test(dir: &Path, test: &Test) -> BayResult {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let command = test.command.join(" ");
    if command.trim().is_empty() {
        return refused(&test.kind, "name the plugin's command after --");
    }
    match test.kind.as_str() {
        "check" => test_check(&dir, &command, test),
        "wake" => test_wake(&dir, &command),
        "notify" => test_notify(&dir, &command),
        "assets" => test_assets(&dir, &command, test.timeout_s),
        other => refused(
            other,
            &format!(
                "bay has no {other} plugin with a stdin and stdout contract; the kinds are {}",
                KINDS.join(", ")
            ),
        ),
    }
}

/// One case's judgement and why.
enum Judgement {
    Pass(String),
    Fail(String),
    NotJudged(String),
}

use Judgement::{Fail, NotJudged, Pass};

/// One exchange as a case shows it: what was sent, and what came back.
struct Shown {
    sent: Value,
    came_back: Value,
}

fn case(name: &str, shown: &Shown, judgement: Judgement) -> Value {
    let (result, why) = match judgement {
        Pass(why) => ("pass", why),
        Fail(why) => ("fail", why),
        NotJudged(why) => ("not_judged", why),
    };
    json!({"name": name, "sent": shown.sent, "came_back": shown.came_back, "result": result,
        "why": why})
}

/// The kit's result: `ok` when every case passed, else `failed`.
fn report(
    kind: &str,
    command: &str,
    limit_s: Value,
    bay_records: Value,
    cases: Vec<Value>,
) -> BayResult {
    let count = |word: &str| cases.iter().filter(|case| case["result"] == word).count();
    let (passed, failed, not_judged) = (count("pass"), count("fail"), count("not_judged"));
    let mut human = format!(
        "bay plugin test {kind}: {passed} passed, {failed} failed, {not_judged} not judged"
    );
    for case in &cases {
        let field = |name: &str| case[name].as_str().unwrap_or_default().to_owned();
        let _ = write!(
            human,
            "\n  {:<14} {:<10} {}",
            field("name"),
            field("result"),
            field("why")
        );
    }
    let decision = if failed + not_judged == 0 {
        Decision::Ok
    } else {
        Decision::Failed
    };
    let mut detail = json!({"kind": kind, "command": command});
    detail["limit_s"] = limit_s;
    detail["passed"] = json!(passed);
    detail["failed"] = json!(failed);
    detail["not_judged"] = json!(not_judged);
    detail["bay_records"] = bay_records;
    detail["cases"] = Value::Array(cases);
    let mut result = BayResult::new(decision).with_detail(detail);
    result.human = Some(human);
    result
}

/// BACKLOG 49: the assets plugin's contract (`crate::assets`), run from `dir` on a sample file
/// in a scratch directory of the kit's own: `put` names the file's bytes, a second `put` names
/// them again, `get` writes exactly those bytes, and `get` of an identity it never stored fails.
fn test_assets(dir: &Path, command: &str, timeout_s: Option<u64>) -> BayResult {
    let limit = Duration::from_secs(timeout_s.unwrap_or(crate::assets::DEFAULT_TIMEOUT_S));
    let scratch = match Scratch::new() {
        Ok(scratch) => scratch,
        Err(why) => return refused("assets", &why),
    };
    let sample = scratch.0.join("sample.bin");
    let bytes: Vec<u8> = (0..65_536u32)
        .map(|n| (n.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    if let Err(error) = std::fs::write(&sample, &bytes) {
        return refused("assets", &format!("the sample cannot be written: {error}"));
    }
    let plugin = crate::assets::Plugin {
        work_root: dir,
        command,
        timeout: limit,
    };
    let id = crate::assets::identity(&sample).unwrap_or_default();
    let sent = |args: &str| json!(format!("{command} {args}"));
    let mut cases = Vec::new();
    let put = plugin.put(&sample);
    cases.push(case(
        "put",
        &Shown {
            sent: sent("put <64 KiB sample>"),
            came_back: json!(put.as_ref().map_or_else(Clone::clone, Clone::clone)),
        },
        match &put {
            Ok(_) => Pass(format!("it stored the sample and named its bytes, {id}")),
            Err(why) => Fail(why.clone()),
        },
    ));
    let again = plugin.put(&sample);
    cases.push(case(
        "put_again",
        &Shown {
            sent: sent("put <the same sample>"),
            came_back: json!(again.as_ref().map_or_else(Clone::clone, Clone::clone)),
        },
        match &again {
            Ok(_) => Pass("storing bytes it holds again names them again".to_owned()),
            Err(why) => Fail(why.clone()),
        },
    ));
    let got = scratch.0.join("got.bin");
    let get = plugin.get(&id, &got);
    cases.push(case(
        "get",
        &Shown {
            sent: sent(&format!("get {id} <file>")),
            came_back: json!(get.as_ref().err()),
        },
        match &get {
            Ok(()) => Pass("it wrote exactly the stored bytes".to_owned()),
            Err(why) => Fail(why.clone()),
        },
    ));
    let unknown = format!("sha256:{}", "0".repeat(64));
    let missing = scratch.0.join("missing.bin");
    let refused_get = plugin.get(&unknown, &missing);
    cases.push(case(
        "get_unknown",
        &Shown {
            sent: sent(&format!("get {unknown} <file>")),
            came_back: json!(refused_get.as_ref().err()),
        },
        match &refused_get {
            Err(_) => Pass("an identity it never stored is refused".to_owned()),
            Ok(()) => Fail(format!("it answered {unknown}, which it never stored")),
        },
    ));
    report(
        "assets",
        command,
        json!(limit.as_secs()),
        json!({"manifest_line": format!("{id} {} <path>", bytes.len())}),
        cases,
    )
}

/// The kit could not run its cases: `why` says what to change.
fn refused(kind: &str, why: &str) -> BayResult {
    BayResult::new(Decision::Failed).with_detail(json!({"kind": kind, "why": why, "cases": []}))
}

/// At most [`SHOWN_BYTES`] of `bytes` as text, saying how long the whole was when it is cut.
fn shown_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= SHOWN_BYTES {
        return text.into_owned();
    }
    let mut end = SHOWN_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... ({} bytes in all)", &text[..end], bytes.len())
}

fn seconds(elapsed: Duration) -> f64 {
    (elapsed.as_secs_f64() * 1000.0).round() / 1000.0
}

fn exit_code(status: Option<ExitStatus>) -> Value {
    status
        .and_then(|status| status.code())
        .map_or(Value::Null, Value::from)
}

fn blank(bytes: &[u8]) -> bool {
    bytes.iter().all(u8::is_ascii_whitespace)
}

/// The JSON document on `stdout`, and whether it is all of stdout: the whole of it, or else its
/// last non-empty line.
fn document(stdout: &[u8]) -> Option<(Value, bool)> {
    if let Ok(document) = serde_json::from_slice::<Value>(stdout) {
        return Some((document, true));
    }
    let text = String::from_utf8_lossy(stdout);
    let last = text.lines().rev().find(|line| !line.trim().is_empty())?;
    serde_json::from_str(last.trim())
        .ok()
        .map(|document| (document, false))
}

/// Whether stdout is one JSON document and nothing else: bay reads all of it as one.
fn one_document(stdout: &[u8]) -> Judgement {
    match document(stdout) {
        Some((_, true)) => Pass("stdout is one JSON document".to_owned()),
        Some((_, false)) => Fail(
            "stdout holds other text beside the document; bay reads all of stdout as one \
             document, so it reads none: write logs to stderr"
                .to_owned(),
        ),
        None if blank(stdout) => NotJudged("stdout is empty".to_owned()),
        None => Fail("stdout holds no JSON document".to_owned()),
    }
}

/// Whether the adapter answered within `limit`; `ran` is `None` when it did not start.
fn within(ran: Option<Duration>, limit: Duration, consequence: &str) -> Judgement {
    let Some(elapsed) = ran else {
        return NotJudged("it did not start".to_owned());
    };
    if elapsed <= limit {
        Pass(format!(
            "it answered in {} s of its {} s",
            seconds(elapsed),
            limit.as_secs()
        ))
    } else {
        Fail(format!(
            "it answered after {} s, past its {} s: {consequence}",
            seconds(elapsed),
            limit.as_secs()
        ))
    }
}

/// Whether the adapter relies on a variable bay clears: it is accepted with the caller's whole
/// environment (`with_callers`, `None` when not run) and not with bay's baseline alone. One that
/// answers the same either way relies on none, whatever else is wrong with its answer.
fn env_case(baseline_ok: bool, with_callers: Option<bool>, baseline: &str) -> Judgement {
    match (baseline_ok, with_callers) {
        (true, _) => Pass(format!(
            "it answers with bay's environment alone ({baseline})"
        )),
        (false, Some(true)) => Fail(format!(
            "it answers with your environment and not with bay's ({baseline}): it relies on a \
             variable bay clears"
        )),
        (false, Some(false)) => Pass(
            "it answers the same with your environment: no variable bay clears changes its \
             answer"
                .to_owned(),
        ),
        (false, None) => NotJudged("not run: the first request got no answer".to_owned()),
    }
}

/// `cmd` with the caller's whole environment over bay's baseline: what the adapter sees when
/// its author runs it by hand.
fn with_callers_env(mut cmd: Command) -> Command {
    cmd.envs(std::env::vars_os());
    cmd
}

// The wake adapter (`INTERFACES` § 4).

fn test_wake(dir: &Path, command: &str) -> BayResult {
    let limit = wake::WAKE_TIMEOUT;
    let bound = limit * GRACE;
    let (cmd, callers) = match (
        crate::run::command(dir, command),
        crate::run::command(dir, command),
    ) {
        (Some(Ok(cmd)), Some(Ok(callers))) => (cmd, callers),
        (Some(Err(why)), _) | (_, Some(Err(why))) => return refused("wake", &why),
        _ => return refused("wake", "name the plugin's command after --"),
    };
    let me = Actor {
        name: "adapter-test".to_owned(),
        session: "adapter-test-session".to_owned(),
    };
    let request = wake::request(
        &crate::ids::base32(b"bayadapter"),
        &me,
        "adapter-test",
        false,
        "Rebase onto the new parser",
    );
    let answer = wake::ask(cmd, &request, bound);
    let with_callers = answer
        .error
        .is_none()
        .then(|| wake::ask(with_callers_env(callers), &request, bound));
    // G307: the request an executor sends for an outcome its author must act on.
    let outcome = wake::outcome_request(
        &wake::Outcome {
            change: "adaptertest",
            op_id: "adaptertest.1",
            decision: Decision::Blocked,
            reason: Some(Reason::CheckFailed),
            actor: &me,
        },
        "adapter-test",
    );
    let to_outcome = match crate::run::command(dir, command) {
        Some(Ok(cmd)) if answer.error.is_none() => Some(wake::ask(cmd, &outcome, bound)),
        _ => None,
    };
    let shown = wake_shown(&request, &answer);
    let ran = (answer.error.is_none() || answer.timed_out).then_some(answer.elapsed);
    let late = "bay stops waiting at the limit and records uncertain / WAKE_ADAPTER_FAILED";
    let cases = vec![
        case("answers", &shown, wake_answers(&answer)),
        case("schema", &shown, wake_schema(&answer.stdout)),
        case("reason", &shown, wake_reason(&answer.stdout)),
        case("one_document", &shown, one_document(&answer.stdout)),
        case(
            "exit_code",
            &shown,
            exit_judgement(
                answer.status,
                answer.error.as_deref(),
                "bay records uncertain / WAKE_ADAPTER_FAILED for any exit other than 0",
            ),
        ),
        case("within_limit", &shown, within(ran, limit, late)),
        case("recorded", &shown, wake_recorded(&answer, limit)),
        case(
            "outcome",
            &to_outcome.as_ref().map_or_else(
                || wake_shown(&outcome, &answer),
                |other| wake_shown(&outcome, other),
            ),
            wake_outcome(to_outcome.as_ref(), limit),
        ),
        case(
            "env_baseline",
            &with_callers.as_ref().map_or_else(
                || wake_shown(&request, &answer),
                |other| wake_shown(&request, other),
            ),
            env_case(
                wake::answered(&answer).is_ok(),
                with_callers
                    .as_ref()
                    .map(|other| wake::answered(other).is_ok()),
                PATH_AND_HOME,
            ),
        ),
    ];
    report(
        "wake",
        command,
        json!(limit.as_secs()),
        wake::recorded(&answer, limit),
        cases,
    )
}

fn wake_shown(request: &Value, answer: &wake::Answer) -> Shown {
    Shown {
        sent: request.clone(),
        came_back: json!({
            "exit": exit_code(answer.status), "stdout": shown_text(&answer.stdout),
            "stderr": shown_text(&answer.stderr), "elapsed_s": seconds(answer.elapsed),
            "error": answer.error,
        }),
    }
}

fn wake_answers(answer: &wake::Answer) -> Judgement {
    if let Some(error) = &answer.error {
        return Fail(format!("no answer: {error}"));
    }
    if blank(&answer.stdout) {
        return Fail(format!(
            "stdout is empty; bay reads the {} document from stdout",
            wake::RESULT_SCHEMA
        ));
    }
    Pass("it read the bay.wake/1 request and printed on stdout".to_owned())
}

fn wake_schema(stdout: &[u8]) -> Judgement {
    let Some((document, _)) = document(stdout) else {
        return NotJudged("no JSON document on stdout".to_owned());
    };
    let mut wrong = Vec::new();
    if document["schema"] != wake::RESULT_SCHEMA {
        wrong.push(format!(
            "schema is {}, not {}",
            document["schema"],
            wake::RESULT_SCHEMA
        ));
    }
    let result = document["result"].as_str().unwrap_or_default();
    if !wake::ANSWERS.contains(&result) {
        wrong.push(format!(
            "result is {}, not one of {}",
            document["result"],
            wake::ANSWERS.join(", ")
        ));
    }
    if wrong.is_empty() {
        Pass(format!("{} with result {result}", wake::RESULT_SCHEMA))
    } else {
        Fail(wrong.join("; "))
    }
}

fn wake_reason(stdout: &[u8]) -> Judgement {
    let Some((document, _)) = document(stdout) else {
        return NotJudged("no JSON document on stdout".to_owned());
    };
    match &document["reason"] {
        Value::Null => Pass("no reason: bay records null".to_owned()),
        Value::String(reason) => {
            let kept = wake::kept_reason(reason);
            if kept == *reason {
                Pass(format!("bay keeps the reason {reason:?} as it is"))
            } else {
                Fail(format!(
                    "bay keeps {kept:?} of the reason {reason:?}: a reason is a code of A-Z \
                     and _, at most 64 characters"
                ))
            }
        }
        other => Fail(format!("reason is {other}, not a string: bay records null")),
    }
}

fn wake_recorded(answer: &wake::Answer, limit: Duration) -> Judgement {
    let recorded = wake::recorded(answer, limit);
    match wake::answered(answer) {
        Ok(_) if answer.elapsed <= limit => Pass(format!("bay records {recorded} on the ack")),
        Ok(_) => Fail(format!(
            "bay records {recorded} on the ack: the answer came after the {} s limit",
            limit.as_secs()
        )),
        Err(why) => Fail(format!("bay records {recorded} on the ack: {why}")),
    }
}

/// G307: whether the adapter answers a `cause: "outcome"` request as bay records it on the
/// outcome event; `None` when the talk request had no answer, so none was sent.
fn wake_outcome(answer: Option<&wake::Answer>, limit: Duration) -> Judgement {
    let Some(answer) = answer else {
        return NotJudged(
            "the talk request had no answer, so no outcome request was sent".to_owned(),
        );
    };
    let recorded = wake::recorded(answer, limit);
    match wake::answered(answer) {
        Ok(_) if answer.elapsed <= limit => Pass(format!(
            "it answered a cause: outcome request; bay records {recorded} on the outcome event"
        )),
        Ok(_) => Fail(format!(
            "bay records {recorded} on the outcome event: the answer came after the {} s limit",
            limit.as_secs()
        )),
        Err(why) => Fail(format!(
            "bay records {recorded} on the outcome event: {why}"
        )),
    }
}

/// Whether the adapter exited 0; `error` says why there is no status.
fn exit_judgement(status: Option<ExitStatus>, error: Option<&str>, consequence: &str) -> Judgement {
    match status {
        Some(status) if status.success() => Pass("it exited 0".to_owned()),
        Some(status) => Fail(format!("it exited with {status}: {consequence}")),
        None => NotJudged(format!(
            "no exit status: {}",
            error.unwrap_or("it was stopped at the bound")
        )),
    }
}

// The notify adapter (`INTERFACES` § 4, § 5).

/// An event as bay tells it (`INTERFACES` § 5).
fn sample_event() -> Value {
    json!({
        "schema": crate::log::EVENT_SCHEMA, "id": "evt_adapter_test_1",
        "at": crate::clock::now_rfc3339(), "kind": "check_failed", "change": "adaptertest",
        "actor": {"name": "adapter-test", "session": "adapter-test-session"},
        "inputs": [], "receipt": "adapter-test/check-1",
        "recurrence_key": "adaptertest/CHECK_FAILED/adapter-test",
    })
}

fn tell(dir: &Path, command: &str, event: &Value, callers: bool, bound: Duration) -> notify::Sent {
    let Some(Ok(cmd)) = crate::run::command(dir, command) else {
        return notify::Sent {
            status: None,
            elapsed: Duration::ZERO,
        };
    };
    let cmd = if callers { with_callers_env(cmd) } else { cmd };
    notify::send(cmd, notify::notice(event).as_bytes(), bound)
}

fn test_notify(dir: &Path, command: &str) -> BayResult {
    let limit = notify::NOTIFY_TIMEOUT;
    let bound = limit * GRACE;
    let event = sample_event();
    let other =
        crate::unattributed::event(&"0".repeat(40), "outside", "an edit pushed outside bay");
    let first = tell(dir, command, &event, false, bound);
    let again = tell(dir, command, &event, false, bound);
    let another = tell(dir, command, &other, false, bound);
    let with_callers = tell(dir, command, &event, true, bound);
    let shown = |event: &Value, sent: &notify::Sent| Shown {
        sent: event.clone(),
        came_back: json!({
            "exit": exit_code(sent.status), "elapsed_s": seconds(sent.elapsed),
            "stdout": "bay sends the plugin's stdout and stderr nowhere",
        }),
    };
    let ran = (first.status.is_some() || first.elapsed >= bound).then_some(first.elapsed);
    let delivered = |sent: &notify::Sent| sent.status.is_some_and(|status| status.success());
    let cases = vec![
        case(
            "exit_code",
            &shown(&event, &first),
            notify_exit(&first, bound, "it exited 0"),
        ),
        case(
            "within_limit",
            &shown(&event, &first),
            within(
                ran,
                limit,
                "bay stops waiting at the limit and the notice is lost",
            ),
        ),
        case(
            "repeat_id",
            &shown(&event, &again),
            notify_exit(
                &again,
                bound,
                "it took the same event id again: the id is the plugin's to deduplicate",
            ),
        ),
        case(
            "another_kind",
            &shown(&other, &another),
            notify_exit(&another, bound, "it took an event of another kind"),
        ),
        case(
            "recorded",
            &shown(&event, &first),
            notify_recorded(&first, limit),
        ),
        case(
            "env_baseline",
            &shown(&event, &with_callers),
            env_case(
                delivered(&first),
                Some(delivered(&with_callers)),
                PATH_AND_HOME,
            ),
        ),
    ];
    let records = if first.delivery(limit) == notify::Delivery::Delivered {
        "delivered"
    } else {
        "lost"
    };
    report(
        "notify",
        command,
        json!(limit.as_secs()),
        json!(records),
        cases,
    )
}

fn notify_exit(sent: &notify::Sent, bound: Duration, passed: &str) -> Judgement {
    match sent.status {
        Some(status) if status.success() => Pass(passed.to_owned()),
        Some(status) => Fail(format!(
            "it exited with {status}: bay records the notice as lost for any exit other than 0"
        )),
        None if sent.elapsed >= bound => NotJudged(format!(
            "it was still running at {} s, and was stopped",
            bound.as_secs()
        )),
        None => Fail("it did not start".to_owned()),
    }
}

fn notify_recorded(sent: &notify::Sent, limit: Duration) -> Judgement {
    if sent.delivery(limit) == notify::Delivery::Delivered {
        return Pass("bay records the notice as delivered".to_owned());
    }
    let why = match sent.status {
        Some(status) if status.success() => {
            format!("it exited 0 after the {} s limit", limit.as_secs())
        }
        Some(status) => format!("it exited with {status}"),
        None => "it did not exit within the limit, or did not start".to_owned(),
    };
    Fail(format!("bay records the notice as lost: {why}"))
}

// The check adapter (`INTERFACES` § 3, § 7).

/// A scratch directory of the kit's own, removed when the kit ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self, String> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let path =
            std::env::temp_dir().join(format!("bay-adapter-test-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(Self(path.canonicalize().unwrap_or(path)))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn test_check(dir: &Path, command: &str, test: &Test) -> BayResult {
    let scratch = match Scratch::new() {
        Ok(scratch) => scratch,
        Err(why) => return refused("check", &why),
    };
    check_in(dir, command, test, &scratch.0).unwrap_or_else(|why| refused("check", &why))
}

/// `path` as a plain path relative to the repository: a policy names its adapter paths so.
fn adapter_path(path: &str) -> Result<String, String> {
    let mut parts = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => {
                return Err(format!(
                    "{path}: name each adapter path relative to the repository (the -C \
                     directory) and inside it, as a policy's command and adapter list do"
                ));
            }
        }
    }
    if parts.is_empty() {
        return Err(format!("{path}: no adapter path"));
    }
    Ok(parts.join("/"))
}

/// What one run of the kit's policy records, and the exchange behind it.
struct Run {
    record: Option<EvidenceRecord>,
    unavailable: Option<String>,
    exchange: Option<check::Exchange>,
}

impl Run {
    /// Whether bay records the adapter's own answer: a verdict from its units, or its word that
    /// no worker could run the check; never bay's `unknown` or a failure with bay's reason.
    fn accepted(&self) -> bool {
        self.unavailable.is_some()
            || self
                .record
                .as_ref()
                .is_some_and(|record| record.why.is_none() && record.verdict != Verdict::Unknown)
    }
}

/// Builds the scratch repository, runs the check twice (bay's baseline, then with the caller's
/// environment declared in `env`), and judges the cases.
fn check_in(dir: &Path, command: &str, test: &Test, scratch: &Path) -> Result<BayResult, String> {
    let mut words = command.split_whitespace();
    let program = adapter_path(words.next().unwrap_or_default())?;
    let command: String = std::iter::once(program.as_str())
        .chain(words)
        .collect::<Vec<_>>()
        .join(" ");
    let sandbox = check::Sandbox::here();
    if let Some(why) = &sandbox.refusal {
        return Err(why.clone());
    }
    let work = scratch.join("work");
    std::fs::create_dir_all(&work).map_err(|error| format!("{}: {error}", work.display()))?;
    let git = Git::new(&work);
    git_ok(&git, &["init", "-q"])?;
    // G285: the box's read paths, as an executor of this box binds them.
    let sandbox = sandbox.reading(&git);
    if let Some(why) = &sandbox.refusal {
        return Err(why.clone());
    }
    write(
        &work,
        "README.md",
        "A scratch repository of bay plugin test.\n",
    )?;
    let mut adapter = vec![program.clone()];
    copy(&dir.join(&program), &work.join(&program))?;
    for path in &test.with {
        let path = adapter_path(path)?;
        copy(&dir.join(&path), &work.join(&path))?;
        adapter.push(if dir.join(&path).is_dir() {
            format!("{path}/")
        } else {
            path
        });
    }
    let callers: Vec<String> = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .filter(|name| {
            !crate::policy::BASELINE_VARIABLES.contains(&name.as_str())
                && !test.env.contains(name)
                && !name.is_empty()
        })
        .collect();
    let mut commits = Vec::new();
    let none: &[String] = &[];
    for extra in [none, callers.as_slice()] {
        write(
            &work,
            crate::policy::POLICY_PATH,
            &policy_text(&command, &adapter, test, extra),
        )?;
        write(&work, "src/example.txt", "one\n")?;
        let base = commit(&git, "base")?;
        write(&work, "src/example.txt", "two\n")?;
        commits.push((base, commit(&git, "candidate")?));
    }
    let first = run_check(&git, &work, &sandbox, &commits[0])?;
    let again = if first
        .exchange
        .as_ref()
        .is_some_and(|exchange| !exchange.timed_out)
    {
        Some(run_check(&git, &work, &sandbox, &commits[1])?)
    } else {
        None
    };
    Ok(check_report(&command, &first, again.as_ref()))
}

/// The kit's acceptance policy: one required check that runs `command`.
fn policy_text(command: &str, adapter: &[String], test: &Test, extra: &[String]) -> String {
    let mut env = test.env.clone();
    env.extend(extra.iter().cloned());
    let mut policy = format!(
        "revision = 1\n\n[[check]]\nid = \"adapter-test\"\nenvironment = \"local\"\n\
         command = {}\nadapter = {}\nexpected_minimum = {}\n",
        toml_value(command),
        toml_value(adapter),
        test.expected_minimum
    );
    if let Some(timeout) = test.timeout_s {
        let _ = writeln!(policy, "timeout_s = {timeout}");
    }
    if !test.path.is_empty() {
        let _ = writeln!(policy, "path = {}", toml_value(&test.path));
    }
    if !env.is_empty() {
        let _ = writeln!(policy, "env = {}", toml_value(&env));
    }
    policy
}

/// A string or a list of strings as a TOML value: a JSON string is a TOML basic string.
fn toml_value<T: serde::Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn git_ok(git: &Git, args: &[&str]) -> Result<String, String> {
    git.run(args).map_err(|error| error.message())
}

/// Commits the whole scratch tree, with no hook and no signature of the caller's own.
fn commit(git: &Git, message: &str) -> Result<String, String> {
    git_ok(git, &["add", "-A"])?;
    git_ok(
        git,
        &[
            "-c",
            "user.name=bay plugin test",
            "-c",
            "user.email=adapter-test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-q",
            "--no-verify",
            "--allow-empty",
            "-m",
            message,
        ],
    )?;
    git_ok(git, &["rev-parse", "HEAD"])
}

fn write(root: &Path, path: &str, text: &str) -> Result<(), String> {
    let file = root.join(path);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    std::fs::write(&file, text).map_err(|error| format!("{}: {error}", file.display()))
}

/// Copies the file or directory `from` to `to`, keeping each file's mode.
fn copy(from: &Path, to: &Path) -> Result<(), String> {
    let failed = |path: &Path, error: std::io::Error| format!("{}: {error}", path.display());
    let meta = std::fs::metadata(from).map_err(|error| failed(from, error))?;
    if meta.is_dir() {
        std::fs::create_dir_all(to).map_err(|error| failed(to, error))?;
        for entry in std::fs::read_dir(from).map_err(|error| failed(from, error))? {
            let entry = entry.map_err(|error| failed(from, error))?;
            copy(&entry.path(), &to.join(entry.file_name()))?;
        }
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|error| failed(parent, error))?;
    }
    std::fs::copy(from, to)
        .map(|_| ())
        .map_err(|error| failed(from, error))
}

/// Runs the kit's policy at `base` on `candidate` through bay's own check run, in the layout
/// bay uses: the check's scratch under the repository's git directory, the repository hidden.
fn run_check(
    git: &Git,
    work: &Path,
    sandbox: &check::Sandbox,
    (base, candidate): &(String, String),
) -> Result<Run, String> {
    let policy = crate::policy::load_at(git, base)
        .map_err(String::from)?
        .ok_or("the scratch repository holds no policy")?;
    let tree = git_ok(git, &["rev-parse", &format!("{candidate}^{{tree}}")])?;
    let base_tree = git_ok(git, &["rev-parse", &format!("{base}^{{tree}}")])?;
    let changed = check::changed_paths(git, base, candidate)?;
    let git_dir = work.join(".git");
    let limits = check::Limits {
        budget: None,
        finished: Vec::new(),
        heartbeat: None,
        hidden: vec![work.to_path_buf(), git_dir.clone()],
        sandbox: sandbox.clone(),
        // The conformance kit judges one exchange whole: it has no base to compare with.
        baseline: None,
        parallel: 1,
    };
    let mut seen = None;
    let mut keep = |_: &mut EvidenceRecord| Ok(());
    let mut look = |exchange: &check::Exchange| seen = Some(exchange.clone());
    let look: &mut dyn FnMut(&check::Exchange) = &mut look;
    let ran = check::run_all(
        git,
        &git_dir.join("bay/check").join(CANDIDATE_ID),
        &policy,
        &check::Candidate {
            commit: candidate,
            tree: &tree,
            base,
            base_tree: &base_tree,
            onto: base,
            change: None,
            changed_paths: &changed,
            id: CANDIDATE_ID,
            policy_provenance: crate::policy::Provenance::Bay,
            inputs: &[],
        },
        PRODUCER,
        &limits,
        &mut check::Hooks {
            keep: &mut keep,
            beat: &mut || {},
            seen: Some(look),
        },
    )?;
    Ok(Run {
        record: ran.records.into_iter().next(),
        unavailable: ran.unavailable.map(|(_, why)| why),
        exchange: seen,
    })
}

fn verdict_word(verdict: Verdict) -> String {
    serde_json::to_value(verdict)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// What bay records for one run: its verdict and why, or the adapter's `unavailable`.
fn records_of(run: &Run) -> Value {
    if let Some(reason) = &run.unavailable {
        return json!({"reason": Reason::CheckUnavailable.code(), "unavailable": reason,
            "evidence": false});
    }
    run.record.as_ref().map_or(Value::Null, |record| {
        json!({"verdict": verdict_word(record.verdict), "why": record.why,
            "units": record.units.len(), "selection": record.selection,
            "sandbox": record.sandbox})
    })
}

fn check_shown(exchange: &check::Exchange) -> Shown {
    Shown {
        sent: exchange.request.clone(),
        came_back: json!({
            "exit": exit_code(exchange.status), "stdout": shown_text(&exchange.stdout),
            "stderr": shown_text(exchange.stderr.as_bytes()),
            "elapsed_s": seconds(exchange.elapsed), "timed_out": exchange.timed_out,
            "error": exchange.error,
        }),
    }
}

fn check_report(command: &str, first: &Run, again: Option<&Run>) -> BayResult {
    let Some(exchange) = &first.exchange else {
        // bay never ran the adapter: a declared variable is unset, or the tree was refused.
        let why = first
            .record
            .as_ref()
            .and_then(|record| record.why.clone())
            .unwrap_or_else(|| "bay did not run the plugin".to_owned());
        let shown = Shown {
            sent: Value::Null,
            came_back: Value::Null,
        };
        return report(
            "check",
            command,
            Value::Null,
            records_of(first),
            vec![case("recorded", &shown, Fail(why))],
        );
    };
    let limit = exchange.request["timeout_s"].as_u64().unwrap_or_default();
    let shown = check_shown(exchange);
    let stopped = || {
        NotJudged(format!(
            "bay stopped it at its timeout_s ({limit} s): see within_timeout"
        ))
    };
    let unless_stopped = |judgement: Judgement| {
        if exchange.timed_out {
            stopped()
        } else {
            judgement
        }
    };
    let within_timeout = if exchange.timed_out {
        Fail(format!(
            "it ran past its timeout_s ({limit} s): bay kills it and records unknown"
        ))
    } else {
        within(
            exchange.status.map(|_| exchange.elapsed),
            Duration::from_secs(limit),
            "bay records unknown",
        )
    };
    let cases = vec![
        case("answers", &shown, unless_stopped(check_answers(exchange))),
        case(
            "schema",
            &shown,
            unless_stopped(check_schema(&exchange.stdout)),
        ),
        case("exercised", &shown, unless_stopped(exercised(exchange))),
        case(
            "one_document",
            &shown,
            unless_stopped(one_document(&exchange.stdout)),
        ),
        case("within_timeout", &shown, within_timeout),
        case("recorded", &shown, check_recorded(first)),
        case(
            "env_baseline",
            &again
                .and_then(|run| run.exchange.as_ref())
                .map_or_else(|| check_shown(exchange), check_shown),
            env_case(first.accepted(), again.map(Run::accepted), CHECK_BASELINE),
        ),
    ];
    report("check", command, json!(limit), records_of(first), cases)
}

fn check_answers(exchange: &check::Exchange) -> Judgement {
    if let Some(error) = &exchange.error {
        return Fail(format!("no answer: {error}"));
    }
    if blank(&exchange.stdout) {
        return Fail(format!(
            "stdout is empty; bay reads the {CHECK_RESULT_SCHEMA} document from stdout"
        ));
    }
    Pass(format!(
        "it read the bay.check.request/1 and printed on stdout (exit {}; bay judges the result, \
         never the exit code)",
        exit_code(exchange.status)
    ))
}

fn check_schema(stdout: &[u8]) -> Judgement {
    let Some((document, _)) = document(stdout) else {
        return NotJudged("no JSON document on stdout".to_owned());
    };
    let mut wrong = Vec::new();
    if document["schema"] != CHECK_RESULT_SCHEMA {
        wrong.push(format!(
            "schema is {}, not {CHECK_RESULT_SCHEMA}",
            document["schema"]
        ));
    }
    if document.get("checks").is_none()
        && document["unavailable"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty())
    {
        return if wrong.is_empty() {
            Pass("an unavailable answer: no worker could run the check".to_owned())
        } else {
            Fail(wrong.join("; "))
        };
    }
    for field in ["commit", "tree"] {
        if !document["exercised"][field].is_string() {
            wrong.push(format!("exercised.{field} is missing"));
        }
    }
    let plan = &document["plan"];
    if !plan["completed"].is_u64() {
        wrong.push("plan.completed is missing or not a whole number".to_owned());
    }
    if !plan["selected"].is_null() && !plan["selected"].is_u64() {
        wrong.push("plan.selected is not a whole number".to_owned());
    }
    match document.get("checks") {
        Some(Value::Array(units)) => {
            for (at, unit) in units.iter().enumerate() {
                wrong.extend(unit_problems(at, unit));
            }
        }
        _ => wrong.push("checks is not a list of units".to_owned()),
    }
    if wrong.is_empty() {
        Pass(format!("{CHECK_RESULT_SCHEMA} with every required field"))
    } else {
        Fail(wrong.join("; "))
    }
}

/// What is wrong with unit `at` of a result's `checks`.
fn unit_problems(at: usize, unit: &Value) -> Vec<String> {
    let mut wrong = Vec::new();
    if !unit["id"].is_string() {
        wrong.push(format!("checks[{at}].id is missing"));
    }
    if !unit["verdict"]
        .as_str()
        .is_some_and(|verdict| VERDICTS.contains(&verdict))
    {
        wrong.push(format!(
            "checks[{at}].verdict is {}, not one of {}",
            unit["verdict"],
            VERDICTS.join(", ")
        ));
    }
    if !unit["seconds"].is_null() && !unit["seconds"].is_number() {
        wrong.push(format!("checks[{at}].seconds is not a number"));
    }
    if !unit["reason"].is_null() && !unit["reason"].is_string() {
        wrong.push(format!("checks[{at}].reason is not a string"));
    }
    wrong
}

fn exercised(exchange: &check::Exchange) -> Judgement {
    let Some((document, _)) = document(&exchange.stdout) else {
        return NotJudged("no JSON document on stdout".to_owned());
    };
    if document.get("checks").is_none() && document.get("unavailable").is_some() {
        return NotJudged("an unavailable answer names no candidate".to_owned());
    }
    let sent = &exchange.request["candidate"];
    let named = &document["exercised"];
    if named["commit"] == sent["commit"] && named["tree"] == sent["tree"] {
        Pass("exercised names the commit and tree bay sent".to_owned())
    } else {
        Fail(format!(
            "exercised is {named}, and bay sent {sent}: bay rejects a result for another \
             candidate whole"
        ))
    }
}

fn check_recorded(run: &Run) -> Judgement {
    if let Some(reason) = &run.unavailable {
        return Pass(format!(
            "bay records {} ({reason}) and writes no evidence: nothing was observed",
            Reason::CheckUnavailable.code()
        ));
    }
    let Some(record) = &run.record else {
        return Fail("bay wrote no record".to_owned());
    };
    let verdict = verdict_word(record.verdict);
    if run.accepted() {
        Pass(format!(
            "bay records {verdict} from {} units",
            record.units.len()
        ))
    } else {
        Fail(format!(
            "bay records {verdict}: {}",
            record.why.as_deref().unwrap_or("no unit has a result")
        ))
    }
}
