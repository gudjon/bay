//! G786 (owner, 2026-11-18: "I just want to know exactly when this will be on origin/dev, and
//! what to do for speeding that up"): when a live change will be on main, from the stage it is
//! in, derived on read from the terminal records of the changes that landed in the last
//! [`WINDOW_S`]. Never stored, never a model: each figure is a percentile of what happened, and
//! names how many cases it stands on.
//!
//! A live change is in one of four stages, each named by what bay sees:
//! - `draft`: no operation under way; it counts from the change's `started_at`.
//! - `sent`: its land request waits for a place in line, or for a runner when the line is empty.
//! - `in_line`: it holds a place, with `ahead` places before it.
//! - `at_head`: a runner holds it: it is checked and added to main.
//!
//! Each past landing gives one case per stage, from its terminal record ([`crate::timing`]):
//! draft, `submitted_at - started_at` (its first operation only); sent, `queued_at` (or
//! `head_at` when it never queued) less `submitted_at`; at head, `published_at - head_at`; and
//! the line by how many places were ahead of it when it joined, counted from the same records:
//! with none, `head_at - queued_at` (zero when it never queued); with some, that time per place.
//!
//! The stage a change is in is judged by survival: of the past cases that lasted at least as
//! long as this one has so far, what remained of them. Each later stage is its spread, the
//! line's its per-place spread times the places ahead now. Low, likely and high are the 10th,
//! 50th and 90th percentiles, summed over the stages left. A measurement (2026-11-18, 1,270
//! changes in line on a busy store, each predicted from the eight hours before it): this range
//! held 85% of the outcomes, 7% came earlier and 8% later; the two pace windows before it held
//! 6%. A stage with fewer than [`MIN_CASES`] cases has no estimate, and then the whole has none:
//! never a guess.
//!
//! [`held`] replays the estimate each recent landing would have been given when it was sent and
//! when it joined the line, from the records before that moment, and says how often the range
//! held the outcome: the estimate's own accuracy, derived the same way.

use serde_json::{Value, json};

/// How far back the past landings an estimate stands on reach.
pub const WINDOW_S: u64 = 8 * 3600;

/// The fewest cases a stage's estimate stands on.
pub const MIN_CASES: usize = 3;

/// Where a live change is on its way to main.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Draft,
    Sent,
    InLine,
    AtHead,
}

impl Stage {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Sent => "sent",
            Self::InLine => "in_line",
            Self::AtHead => "at_head",
        }
    }

    /// This stage and the ones after it, in order.
    const fn ahead_of_main(self) -> &'static [Self] {
        match self {
            Self::Draft => &[Self::Draft, Self::Sent, Self::InLine, Self::AtHead],
            Self::Sent => &[Self::Sent, Self::InLine, Self::AtHead],
            Self::InLine => &[Self::InLine, Self::AtHead],
            Self::AtHead => &[Self::AtHead],
        }
    }
}

/// A time to come in seconds: the 10th, 50th and 90th percentile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub low: u64,
    pub likely: u64,
    pub high: u64,
}

impl Range {
    const fn plus(self, other: Self) -> Self {
        Self {
            low: self.low + other.low,
            likely: self.likely + other.likely,
            high: self.high + other.high,
        }
    }

    const fn times(self, count: u64) -> Self {
        Self {
            low: self.low * count,
            likely: self.likely * count,
            high: self.high * count,
        }
    }

    fn seconds(self) -> Value {
        json!({"low": self.low, "likely": self.likely, "high": self.high})
    }

    fn at(self, now: u64) -> Value {
        let at = |secs: u64| crate::clock::rfc3339_of(now + secs);
        json!({"low": at(self.low), "likely": at(self.likely), "high": at(self.high)})
    }
}

/// The `percent`th percentile of `sorted` (ascending, not empty), by nearest rank.
fn percentile(sorted: &[u64], percent: usize) -> u64 {
    let rank = (sorted.len() * percent).div_ceil(100).max(1);
    sorted[rank.min(sorted.len()) - 1]
}

/// The range of `values`; `None` with fewer than [`MIN_CASES`].
fn spread(mut values: Vec<u64>) -> Option<Range> {
    if values.len() < MIN_CASES {
        return None;
    }
    values.sort_unstable();
    Some(Range {
        low: percentile(&values, 10),
        likely: percentile(&values, 50),
        high: percentile(&values, 90),
    })
}

/// What remained of the cases in `values` that lasted at least `elapsed`: survival. `None` with
/// fewer than [`MIN_CASES`] such cases.
fn survival(values: &[u64], elapsed: u64) -> Option<Range> {
    spread(
        values
            .iter()
            .filter(|value| **value >= elapsed)
            .map(|value| value - elapsed)
            .collect(),
    )
}

/// One past landing's times, from its terminal record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Case {
    started: Option<u64>,
    first: bool,
    submitted: u64,
    queued: Option<u64>,
    head: u64,
    published: u64,
    /// How many places were ahead of it when it joined the line.
    ahead: u64,
}

impl Case {
    fn of(record: &Value) -> Option<Self> {
        if record["outcome"] != "published" {
            return None;
        }
        let at = |key: &str| record[key].as_u64();
        Some(Self {
            started: at("started_at"),
            first: record["op_id"]
                .as_str()
                .is_some_and(|op| op.ends_with(".1")),
            submitted: at("submitted_at")?,
            queued: at("queued_at"),
            head: at("head_at")?,
            published: at("published_at")?,
            ahead: 0,
        })
    }

    /// Whether it held a place in line at `at`: it joined at or before, and no runner held it
    /// yet.
    fn in_line_at(&self, at: u64) -> bool {
        self.queued.is_some_and(|queued| queued <= at) && at < self.head
    }
}

/// The cases of `records`, each with the places ahead of it when it joined: the other cases that
/// joined before it and were not yet held. One record per operation.
fn cases(records: &[Value]) -> Vec<Case> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out: Vec<Case> = records
        .iter()
        .filter(|record| seen.insert(record["op_id"].to_string()))
        .filter_map(Case::of)
        .collect();
    let ahead: Vec<u64> = out
        .iter()
        .enumerate()
        .map(|(index, case)| {
            case.queued.map_or(0, |joined| {
                out.iter()
                    .enumerate()
                    .filter(|(other, before)| {
                        *other != index
                            && before.queued.is_some_and(|queued| queued < joined)
                            && joined < before.head
                    })
                    .count() as u64
            })
        })
        .collect();
    for (case, ahead) in out.iter_mut().zip(ahead) {
        case.ahead = ahead;
    }
    out
}

/// What the landings of the window before a moment say of each stage.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Past {
    landed: usize,
    draft: Vec<u64>,
    sent: Vec<u64>,
    alone: Vec<u64>,
    per_place: Vec<u64>,
    head: Vec<u64>,
}

impl Past {
    /// The landings of `records` published in the [`WINDOW_S`] up to `now`.
    #[must_use]
    pub fn of(records: &[Value], now: u64) -> Self {
        Self::from_cases(&cases(records), now)
    }

    fn from_cases(cases: &[Case], now: u64) -> Self {
        let mut past = Self::default();
        let since = now.saturating_sub(WINDOW_S);
        for case in cases
            .iter()
            .filter(|case| case.published >= since && case.published <= now)
        {
            past.landed += 1;
            if case.first
                && let Some(started) = case.started
            {
                past.draft.push(case.submitted.saturating_sub(started));
            }
            past.sent.push(
                case.queued
                    .unwrap_or(case.head)
                    .saturating_sub(case.submitted),
            );
            match case.queued {
                Some(queued) if case.ahead > 0 => {
                    let waited = case.head.saturating_sub(queued);
                    past.per_place.push((waited + case.ahead / 2) / case.ahead);
                }
                Some(queued) => past.alone.push(case.head.saturating_sub(queued)),
                None => past.alone.push(0),
            }
            past.head.push(case.published.saturating_sub(case.head));
        }
        past
    }

    /// How many cases the estimate of `stage` stands on.
    fn cases(&self, stage: Stage, ahead: u64) -> usize {
        match stage {
            Stage::Draft => self.draft.len(),
            Stage::Sent => self.sent.len(),
            Stage::InLine if ahead == 0 => self.alone.len(),
            Stage::InLine => self.per_place.len(),
            Stage::AtHead => self.head.len(),
        }
    }

    /// What remains of `stage`, `elapsed` into it when it is the stage the change is in.
    fn remaining(&self, stage: Stage, elapsed: Option<u64>, ahead: u64) -> Option<Range> {
        match (stage, elapsed) {
            (Stage::InLine, _) if ahead == 0 => spread(self.alone.clone()),
            (Stage::InLine, _) => spread(self.per_place.clone()).map(|one| one.times(ahead)),
            (Stage::Draft, Some(elapsed)) => survival(&self.draft, elapsed),
            (Stage::Sent, Some(elapsed)) => survival(&self.sent, elapsed),
            (Stage::AtHead, Some(elapsed)) => survival(&self.head, elapsed),
            (_, None) => None,
        }
    }
}

/// Where a live change stands: its stage, how long it has been in it (`None` when bay cannot
/// tell), and the places ahead of it: for a change in line, before its own; else the whole line,
/// which it joins at the end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Live {
    pub stage: Stage,
    pub elapsed_s: Option<u64>,
    pub ahead: u64,
}

/// Each stage left and what remains of it, from `past`.
fn parts(past: &Past, live: &Live) -> Vec<(Stage, Option<Range>)> {
    live.stage
        .ahead_of_main()
        .iter()
        .map(|&stage| {
            let elapsed = if stage == live.stage {
                live.elapsed_s
            } else {
                Some(0)
            };
            (stage, past.remaining(stage, elapsed, live.ahead))
        })
        .collect()
}

/// The sum of `parts`, when every one is known.
fn total(parts: &[(Stage, Option<Range>)]) -> Option<Range> {
    parts.iter().try_fold(
        Range {
            low: 0,
            likely: 0,
            high: 0,
        },
        |sum, (_, part)| Some(sum.plus((*part)?)),
    )
}

/// G786: the estimate of when `live` is on main, at `now`, from `past`:
/// - `stage`, the stage it is in, and `ahead`, the places before it;
/// - `on_main_at` `{low, likely, high}` in RFC 3339, and `remaining_s` the same in seconds;
///   null when a stage has no estimate (`why` says which);
/// - `stages`, each stage left, its `remaining_s` (null without enough cases) and its `cases`;
/// - `basis`, the window the cases came from and how many landed in it.
#[must_use]
pub fn estimate(past: &Past, live: &Live, now: u64) -> Value {
    let parts = parts(past, live);
    let stages: Vec<Value> = parts
        .iter()
        .map(|(stage, part)| {
            json!({
                "stage": stage.name(),
                "remaining_s": part.map_or(Value::Null, Range::seconds),
                "cases": past.cases(*stage, live.ahead),
            })
        })
        .collect();
    let total = total(&parts);
    let mut out = json!({
        "stage": live.stage.name(),
        "ahead": live.ahead,
        "on_main_at": total.map_or(Value::Null, |total| total.at(now)),
        "remaining_s": total.map_or(Value::Null, Range::seconds),
        "stages": stages,
        "basis": {"window_s": WINDOW_S, "landed": past.landed},
    });
    if total.is_none() {
        out["why"] = json!(why_unknown(past, live, &parts));
    }
    out
}

/// Why `parts` has no total: the first stage without one.
fn why_unknown(past: &Past, live: &Live, parts: &[(Stage, Option<Range>)]) -> String {
    let Some((stage, _)) = parts.iter().find(|(_, part)| part.is_none()) else {
        return String::new();
    };
    let hours = WINDOW_S / 3600;
    let cases = past.cases(*stage, live.ahead);
    match stage {
        Stage::Draft if *stage == live.stage && live.elapsed_s.is_none() => {
            "it was started before bay kept when a change starts".to_owned()
        }
        Stage::InLine if cases < MIN_CASES && live.ahead > 0 => format!(
            "too few of the changes that landed in the last {hours} hours waited behind another ({cases} of at least {MIN_CASES})"
        ),
        _ if cases < MIN_CASES => format!(
            "too few changes landed in the last {hours} hours ({cases} of at least {MIN_CASES})"
        ),
        _ => format!(
            "it has been {} longer than nearly every one of the {cases} changes that landed in the last {hours} hours",
            stage.name()
        ),
    }
}

/// G786c: how well the estimate held, replayed for each landing of `records` published at or
/// after `since`: what it would have been told when it was sent (`sent`) and when it joined the
/// line (`in_line`), from only the records published before that moment, beside when it reached
/// main. For each moment: how many it could judge, the share whose range held the outcome, the
/// shares that came earlier and later, and the median error of the likely time in seconds; null
/// when none could be judged.
#[must_use]
pub fn held(records: &[Value], since: u64) -> Value {
    let cases = cases(records);
    let mut sent = Vec::new();
    let mut joined = Vec::new();
    for case in cases.iter().filter(|case| case.published >= since) {
        let at = case.submitted;
        let live = Live {
            stage: Stage::Sent,
            elapsed_s: Some(0),
            ahead: cases.iter().filter(|other| other.in_line_at(at)).count() as u64,
        };
        if let Some(range) = total(&parts(
            &Past::from_cases(&cases, at.saturating_sub(1)),
            &live,
        )) {
            sent.push((range, case.published.saturating_sub(at)));
        }
        if let Some(queued) = case.queued {
            let live = Live {
                stage: Stage::InLine,
                elapsed_s: Some(0),
                ahead: case.ahead,
            };
            let past = Past::from_cases(&cases, queued.saturating_sub(1));
            if let Some(range) = total(&parts(&past, &live)) {
                joined.push((range, case.published.saturating_sub(queued)));
            }
        }
    }
    json!({"sent": accuracy(&sent), "in_line": accuracy(&joined)})
}

/// `{cases, inside_pct, early_pct, late_pct, median_error_s}` of `judged` (each estimate and
/// what happened); null when empty.
fn accuracy(judged: &[(Range, u64)]) -> Value {
    if judged.is_empty() {
        return Value::Null;
    }
    let count = judged.len() as u64;
    let share = |part: usize| (part as u64 * 100 + count / 2) / count;
    let early = judged
        .iter()
        .filter(|(range, was)| *was < range.low)
        .count();
    let late = judged
        .iter()
        .filter(|(range, was)| *was > range.high)
        .count();
    let mut errors: Vec<u64> = judged
        .iter()
        .map(|(range, was)| range.likely.abs_diff(*was))
        .collect();
    errors.sort_unstable();
    json!({
        "cases": count,
        "inside_pct": share(judged.len() - early - late),
        "early_pct": share(early),
        "late_pct": share(late),
        "median_error_s": percentile(&errors, 50),
    })
}

/// G786: a span of seconds in plain words: `40 s`, `12 min`, `2 h 10 min`.
#[must_use]
pub fn plain_span(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs} s"),
        60..3600 => format!("{} min", (secs + 30) / 60),
        _ => {
            let minutes = (secs + 30) / 60;
            match minutes % 60 {
                0 => format!("{} h", minutes / 60),
                rest => format!("{} h {rest} min", minutes / 60),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Live, MIN_CASES, Past, Stage, estimate, held, plain_span, survival};
    use serde_json::{Value, json};

    /// A landing sent at `sent`, placed `placed_after` later (`None`: never placed), held
    /// `held_after` after that and on main `head_s` after that.
    fn landing(
        op: &str,
        started: Option<u64>,
        sent: u64,
        placed_after: Option<u64>,
        held_after: u64,
        head_s: u64,
    ) -> Value {
        let queued = placed_after.map(|after| sent + after);
        let head = queued.unwrap_or(sent) + held_after;
        let mut record = json!({
            "op_id": op,
            "outcome": "published",
            "submitted_at": sent,
            "head_at": head,
            "published_at": head + head_s,
        });
        if let Some(queued) = queued {
            record["queued_at"] = json!(queued);
        }
        if let Some(started) = started {
            record["started_at"] = json!(started);
        }
        record
    }

    #[test]
    fn survival_takes_only_the_cases_that_lasted_as_long() {
        let drafts = [10, 20, 30, 600, 1200, 1800];
        let fresh = survival(&drafts, 0).unwrap();
        assert_eq!((fresh.low, fresh.likely, fresh.high), (10, 30, 1800));
        // Past a minute only the three long drafts are left, and what remains of them.
        let old = survival(&drafts, 60).unwrap();
        assert_eq!((old.low, old.likely, old.high), (540, 1140, 1740));
        assert_eq!(
            survival(&drafts, 1300),
            None,
            "fewer than {MIN_CASES} cases left"
        );
    }

    #[test]
    fn a_draft_sums_every_stage_left() {
        // Three landings, each drafted 100 s, sent 10 s before its place, alone in line 20 s,
        // and 60 s from its claim to main, an hour apart.
        let records: Vec<Value> = (0..3)
            .map(|n| {
                let at = 10_000 + n * 3600;
                landing(&format!("c{n}.1"), Some(at - 100), at, Some(10), 20, 60)
            })
            .collect();
        let now = 20_000;
        let past = Past::of(&records, now);
        let eta = estimate(
            &past,
            &Live {
                stage: Stage::Draft,
                elapsed_s: Some(40),
                ahead: 0,
            },
            now,
        );
        assert_eq!(eta["stage"], "draft", "{eta}");
        // 60 s of draft left, then 10 + 20 + 60.
        assert_eq!(eta["remaining_s"]["likely"], 150, "{eta}");
        assert_eq!(eta["on_main_at"]["likely"], "1970-01-01T05:35:50Z", "{eta}");
        assert_eq!(eta["basis"]["landed"], 3, "{eta}");
        let stages: Vec<&str> = eta["stages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|stage| stage["stage"].as_str().unwrap())
            .collect();
        assert_eq!(stages, ["draft", "sent", "in_line", "at_head"], "{eta}");
    }

    #[test]
    fn the_line_costs_its_per_place_time_for_each_place_ahead() {
        // Four changes join a second apart, and the head takes each 100 s: the n-th waits
        // 10 + 99n s with n ahead, 109, 104 and 102 s a place; 104 is the likely one.
        let records: Vec<Value> = (0..4)
            .map(|n| landing(&format!("c{n}.1"), None, 999 + n, Some(1), 10 + 99 * n, 100))
            .collect();
        let past = Past::of(&records, 2000);
        let eta = estimate(
            &past,
            &Live {
                stage: Stage::InLine,
                elapsed_s: Some(0),
                ahead: 5,
            },
            2000,
        );
        assert_eq!(eta["stages"][0]["remaining_s"]["likely"], 520, "{eta}");
        assert_eq!(eta["stages"][0]["cases"], 3, "{eta}");
    }

    #[test]
    fn a_stage_with_too_few_cases_leaves_no_estimate_and_says_why() {
        let records = vec![landing("c0.1", Some(0), 100, None, 5, 60)];
        let eta = estimate(
            &Past::of(&records, 1000),
            &Live {
                stage: Stage::AtHead,
                elapsed_s: Some(10),
                ahead: 0,
            },
            1000,
        );
        assert!(eta["on_main_at"].is_null(), "{eta}");
        assert!(
            eta["why"].as_str().is_some_and(|why| !why.is_empty()),
            "{eta}"
        );
    }

    #[test]
    fn a_draft_with_no_start_has_no_estimate() {
        let records: Vec<Value> = (0..3)
            .map(|n| landing(&format!("c{n}.1"), Some(n), 10 + n, None, 5, 60))
            .collect();
        let eta = estimate(
            &Past::of(&records, 1000),
            &Live {
                stage: Stage::Draft,
                elapsed_s: None,
                ahead: 0,
            },
            1000,
        );
        assert!(eta["on_main_at"].is_null(), "{eta}");
        assert!(eta["stages"][1]["remaining_s"].is_object(), "{eta}");
    }

    #[test]
    fn the_replay_judges_only_from_what_came_before() {
        // Five alike landings: the first three have no past of three before them, the last two
        // do, and land exactly as their past did.
        let records: Vec<Value> = (0..5)
            .map(|n| landing(&format!("c{n}.1"), None, 1000 * (n + 1), None, 10, 50))
            .collect();
        let held = held(&records, 0);
        assert_eq!(held["sent"]["cases"], 2, "{held}");
        assert_eq!(held["sent"]["inside_pct"], 100, "{held}");
        assert_eq!(held["sent"]["median_error_s"], 0, "{held}");
        assert!(held["in_line"].is_null(), "none joined a line: {held}");
    }

    #[test]
    fn a_span_reads_plainly() {
        assert_eq!(plain_span(42), "42 s");
        assert_eq!(plain_span(12 * 60 + 10), "12 min");
        assert_eq!(plain_span(2 * 3600 + 10 * 60), "2 h 10 min");
        assert_eq!(plain_span(3 * 3600), "3 h");
    }
}
