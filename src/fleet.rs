//! `bay status --fleet`: the fleet map read from the local cache (`DESIGN` § 11a,
//! `INTERFACES` § 5a; G23, G49).
//!
//! `bay status` is the fetch ([`fetch`]): after its trunk fetch it copies the remote's
//! `refs/bay/live/*` into [`FLEET_PREFIX`], a namespace of its own, so it never moves the
//! `refs/bay/live/<machine>` this machine's live pusher writes or the live refs `bay start`'s
//! takeover watch reads. It records the fetch in `<git-common-dir>/bay/fleet/fetched.json`:
//! the remote, its URL, the time, and per machine the summary commit fetched and the time (this
//! machine's clock) of the fetch that first brought it. The map reads that record and nothing
//! else of the network: it opens no SSH, fetches nothing and starts no session. Without the
//! record it fails closed with `REMOTE_NOT_FETCHED` (G49).
//!
//! A row's age never compares two machines' clocks (ruling R-A2-3): it is the summary's age on
//! this machine's clock (now, against the fetch that first brought that summary commit) plus
//! the row's age inside the summary (the summary's `at` against the row's `at`, both the
//! publisher's). A row a takeover window old drops out. This machine's sessions are read from
//! its own records, never from the remote's copy of its summary, which may be older than them
//! or written by someone else.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::change::{CHANGES_PREFIX, Chain, ChangeRecord, ChangeState};
use crate::clock::{now_rfc3339, unix_of};
use crate::git::Git;
use crate::live;
use crate::repo::{Bootstrap, DEFAULT_REMOTE, Fetch, OpenError, Repo};
use crate::result::{BayResult, Freshness, Reason};

/// Where `bay status` keeps its copy of the remote's live refs, one ref per machine.
pub const FLEET_PREFIX: &str = crate::names::FLEET;
/// The most live summaries the map reads; the host refuses more (`crate::hook`).
const FLEET_MAX_MACHINES: usize = 256;

/// `bay status --fleet`: the fleet map from the local cache. A cache with no recorded fetch of
/// the configured remote's live refs is not the fleet: the result is `blocked` with
/// `REMOTE_NOT_FETCHED`, and its detail lists no machines, actors, talks or sessions.
#[must_use]
pub fn fleet(dir: &Path) -> BayResult {
    run(dir).unwrap_or_else(BayResult::failed)
}

fn run(dir: &Path) -> Result<BayResult, String> {
    // Never a fetch (G49). A trunk never fetched is a fleet never fetched: `bay status` fetches
    // trunk before the fleet refs.
    let repo = match Repo::open_with(dir, Fetch::Never) {
        Ok(repo) => repo,
        Err(OpenError::ConfigMissing(_, detail)) if detail["fetched"] == false => {
            let remote = match Bootstrap::of(dir) {
                Some(Ok(bootstrap)) => bootstrap.remote,
                _ => DEFAULT_REMOTE.to_owned(),
            };
            return Ok(
                BayResult::blocked(Reason::RemoteNotFetched).with_detail(json!({
                    "remote": remote,
                    "fetched": false,
                    "config": detail,
                    "note": "this repository's trunk was never fetched, so the local cache is not the fleet",
                })),
            );
        }
        Err(error) => return Ok(error.into_result()),
    };
    let remote = repo.remote();
    let url = repo
        .git
        .remote_url(remote)
        .map_err(|error| error.message())?;
    let fetched = match &url {
        Some(url) => recorded(&repo, remote, url)?,
        None => None,
    };
    let Some(fetched) = fetched else {
        return Ok(
            BayResult::blocked(Reason::RemoteNotFetched).with_detail(json!({
                "remote": remote,
                "remote_configured": url.is_some(),
                "fetched": false,
                "note": "the local cache has no fetch of the configured remote, so it is not the fleet",
            })),
        );
    };
    map(&repo, &fetched)
}

/// A recorded fetch of the remote's live refs.
struct Fetched {
    at: String,
    /// Per machine: the summary commit fetched, and the Unix time (this machine's clock) of the
    /// fetch that first brought it.
    tips: BTreeMap<String, (String, u64)>,
}

/// Where bay records a fetch of the remote's live refs: a local file beside the views, never
/// pushed (ruling: what "fetched" means for `--fleet`).
fn record_path(repo: &Repo) -> PathBuf {
    repo.common_dir
        .join("bay")
        .join("fleet")
        .join("fetched.json")
}

/// The recorded fetch of `remote`'s live refs from `url`. A trunk tracking ref is not that
/// fetch: a plain push writes it, and a fetch of the trunk alone brings no live refs. A record
/// for another URL is a fetch of another remote, and an unreadable one is no proof of a fetch.
fn recorded(repo: &Repo, remote: &str, url: &str) -> Result<Option<Fetched>, String> {
    let path = record_path(repo);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let Ok(record) = serde_json::from_str::<Value>(&text) else {
        return Ok(None);
    };
    if record["remote"] != remote || record["url"] != url {
        return Ok(None);
    }
    let Some(at) = record["at"].as_str() else {
        return Ok(None);
    };
    let tips = record["tips"]
        .as_object()
        .map(|tips| {
            tips.iter()
                .filter_map(|(machine, tip)| {
                    let commit = tip["commit"].as_str()?.to_owned();
                    Some((machine.clone(), (commit, tip["since"].as_u64()?)))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Some(Fetched {
        at: at.to_owned(),
        tips,
    }))
}

/// `bay status`'s fetch of the remote's live refs into [`FLEET_PREFIX`], pruned to what the
/// remote holds, then the record [`recorded`] reads. Best effort, as the trunk fetch is: a
/// failed fetch keeps the earlier record, whose age `--fleet` shows. A summary commit the
/// earlier record already held keeps the time it was first fetched.
pub fn fetch(repo: &Repo) {
    let Ok(Some(url)) = repo.git.remote_url(repo.remote()) else {
        return;
    };
    let at = now_rfc3339();
    let Some(began) = unix_of(&at) else {
        return;
    };
    let earlier = recorded(repo, repo.remote(), &url)
        .ok()
        .flatten()
        .map(|fetched| fetched.tips)
        .unwrap_or_default();
    let refspec = format!("+{}*:{FLEET_PREFIX}*", live::LIVE_PREFIX);
    let git = &repo.git;
    if git
        .run(&[
            "fetch",
            "-q",
            "--no-tags",
            "--prune",
            repo.remote(),
            &refspec,
        ])
        .is_err()
    {
        return;
    }
    let Ok(listed) = git.run(&[
        "for-each-ref",
        "--format=%(refname) %(objectname)",
        FLEET_PREFIX,
    ]) else {
        return;
    };
    let tips: serde_json::Map<String, Value> = listed
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(name, commit)| {
            let machine = name.trim_start_matches(FLEET_PREFIX);
            let since = earlier
                .get(machine)
                .filter(|(held, _)| held == commit)
                .map_or(began, |(_, since)| *since);
            (
                machine.to_owned(),
                json!({"commit": commit, "since": since}),
            )
        })
        .collect();
    let path = record_path(repo);
    let record = json!({"remote": repo.remote(), "url": url, "at": at, "tips": tips});
    let tmp = path.with_extension("tmp");
    if path
        .parent()
        .is_some_and(|dir| std::fs::create_dir_all(dir).is_ok())
        && std::fs::write(&tmp, record.to_string()).is_ok()
    {
        let _ = crate::durable::rename(&tmp, &path);
    }
}

/// The map being built: machines by name, session entries, and actors with their freshest age
/// (`None` for an actor known only as a talk's recipient that has not acked it) and the machines
/// they are on.
struct Map {
    now: u64,
    window: u64,
    trunk: Option<String>,
    records: BTreeMap<String, ChangeRecord>,
    machines: BTreeMap<String, Value>,
    sessions: Vec<Value>,
    actors: BTreeMap<String, (Option<u64>, BTreeSet<String>)>,
    /// The post checks trunk is red on (G244b).
    red: Vec<crate::red::Red>,
}

impl Map {
    fn actor(&mut self, name: Option<&str>, age: u64, machine: &str) {
        let Some(name) = name else {
            return;
        };
        let entry = self
            .actors
            .entry(name.to_owned())
            .or_insert((Some(age), BTreeSet::new()));
        entry.0 = Some(entry.0.map_or(age, |held| held.min(age)));
        entry.1.insert(machine.to_owned());
    }

    /// An actor on an open talk (§ 11a), last heard `age` seconds ago on no machine the cache
    /// names; `None` when the talk says nothing it wrote.
    fn talk_actor(&mut self, name: Option<&str>, age: Option<u64>) {
        let Some(name) = name else {
            return;
        };
        let entry = self
            .actors
            .entry(name.to_owned())
            .or_insert((None, BTreeSet::new()));
        entry.0 = match (entry.0, age) {
            (Some(held), Some(age)) => Some(held.min(age)),
            (held, age) => held.or(age),
        };
    }

    /// Whether trunk moved past the base of `change`; `null` for a change this cache does not
    /// hold, or with no trunk tracking ref.
    fn behind(&self, change: &Value) -> Value {
        match (
            change.as_str().and_then(|id| self.records.get(id)),
            &self.trunk,
        ) {
            (Some(record), Some(trunk)) => json!(record.base != *trunk),
            _ => Value::Null,
        }
    }

    /// How many trunk commits the base of `change` is behind trunk (G166): the commits of the
    /// trunk tracking ref not in the base its view holds, counted in this cache alone. `null`
    /// where [`Self::behind`] is.
    fn behind_commits(&self, repo: &Git, change: &Value) -> Value {
        let (Some(record), Some(trunk)) = (
            change.as_str().and_then(|id| self.records.get(id)),
            &self.trunk,
        ) else {
            return Value::Null;
        };
        repo.run(&["rev-list", "--count", &format!("{}..{trunk}", record.base)])
            .ok()
            .and_then(|count| count.trim().parse::<u64>().ok())
            .map_or(Value::Null, Value::from)
    }

    /// The red checks (G244b) whose paths cover the paths `change` declared or the files its
    /// session last wrote (`wrote`), so an operator sees who works on red paths. `[]` when none
    /// does, or when this cache holds neither.
    fn on_red(&self, change: &Value, wrote: &Value) -> Vec<Value> {
        if self.red.is_empty() {
            return Vec::new();
        }
        let mut paths: Vec<String> = change
            .as_str()
            .and_then(|id| self.records.get(id))
            .map(|record| crate::inputs::paths(&record.declared_inputs))
            .unwrap_or_default();
        paths.extend(
            wrote
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned),
        );
        self.red
            .iter()
            .filter(|red| red.covers(&paths))
            .map(|red| json!({"check": red.check, "environment": red.environment}))
            .collect()
    }

    fn session(&mut self, repo: &Git, machine: &str, row: &Value, age: u64, wrote: &Value) {
        self.actor(row["actor"]["name"].as_str(), age, machine);
        let behind = self.behind(&row["change"]);
        let behind_commits = self.behind_commits(repo, &row["change"]);
        let session_age = session_age(row, age);
        let on_red = self.on_red(&row["change"], &wrote["wrote"]);
        self.sessions.push(json!({
            "machine": machine,
            "session": row["session"],
            "actor": row["actor"],
            "actor_source": row["actor_source"],
            "permission_mode": row["permission_mode"],
            "agent": row["agent"],
            "change": row["change"],
            "state": row["state"],
            "event": row["event"],
            "context": row["context"],
            "at": row["at"],
            "age_s": age,
            "wrote": wrote["wrote"],
            "wrote_at": wrote["wrote_at"],
            "behind": behind,
            "behind_commits": behind_commits,
            "started": row["started"],
            "session_age_s": session_age,
            "on_red": on_red,
            "tracked": true,
        }));
    }

    /// Another machine's summary, fetched at `since`: its fresh rows are sessions, and every
    /// row's actor and its signer are actors this cache knows.
    fn summary(&mut self, repo: &Git, machine: &str, summary: &Value, since: u64) -> usize {
        let summary_age = self.now.saturating_sub(since);
        self.actor(summary["signer"].as_str(), summary_age, machine);
        // Without the summary's own time no row's age can be judged: none is fresh.
        let Some(written) = live::unix_at(summary) else {
            return 0;
        };
        let rows = summary["sessions"].as_array().cloned().unwrap_or_default();
        let mut fresh = 0;
        for row in rows {
            let Some(at) = live::unix_at(&row) else {
                continue;
            };
            let age = summary_age + written.saturating_sub(at);
            if age >= self.window {
                self.actor(row["actor"]["name"].as_str(), age, machine);
                continue;
            }
            fresh += 1;
            self.session(
                repo,
                machine,
                &row,
                age,
                &json!({"wrote": null, "wrote_at": null}),
            );
        }
        fresh
    }
}

/// The session's age (G166): the row's age `age`, plus how long before the row's time the
/// session started, both times the publishing machine's, so the age never compares two
/// machines' clocks. `null` for a row that does not say when its session started.
fn session_age(row: &Value, age: u64) -> Value {
    match (
        live::unix_at(row),
        row["started"].as_str().and_then(unix_of),
    ) {
        (Some(at), Some(started)) => json!(age + at.saturating_sub(started)),
        _ => Value::Null,
    }
}

/// Whether live commit `commit` holding `summary` is signed by the agent key and agent it
/// names (ruling R-A2-2); the signer, `None` without a registry.
fn verified(
    git: &Git,
    commit: &str,
    summary: &Value,
    agents: Option<&Path>,
) -> Result<Option<String>, String> {
    let Some(agents) = agents else {
        return Ok(None);
    };
    let verification = crate::signing::verify_commit_key(git, commit, agents)
        .map_err(|error| format!("its signature cannot be checked: {}", error.message()))?;
    match verification {
        Some((principal, fingerprint))
            if summary["signer"] == principal && summary["key"] == fingerprint =>
        {
            Ok(Some(principal))
        }
        Some(_) => Err("it does not name the agent and key that signed it".to_owned()),
        None => Err("its signature does not verify against the agent keys".to_owned()),
    }
}

/// The other machines' summaries of the recorded fetch, at most [`FLEET_MAX_MACHINES`] of them
/// read in two `cat-file` calls, each checked against the agent keys when a registry exists. A
/// summary that cannot be read or does not verify lists no sessions. Returns whether machines
/// were left out.
fn remote(
    repo: &Repo,
    fetched: &Fetched,
    here: Option<&str>,
    map: &mut Map,
) -> Result<bool, String> {
    let git = &repo.git;
    let mut tips: Vec<(&String, &(String, u64))> = fetched
        .tips
        .iter()
        .filter(|(machine, _)| Some(machine.as_str()) != here)
        .collect();
    let truncated = tips.len() > FLEET_MAX_MACHINES;
    tips.truncate(FLEET_MAX_MACHINES);
    let commits: Vec<String> = tips.iter().map(|(_, (commit, _))| commit.clone()).collect();
    let summaries = live::summaries_at(git, &commits)?;
    let agents = live::agents_file(git)?;
    for ((machine, (commit, since)), summary) in tips.into_iter().zip(summaries) {
        // G104b: a summary the cache cannot read is named so, never read as a missing one.
        let checked = summary
            .and_then(|summary| {
                summary.ok_or_else(|| "its summary is missing, too large or not JSON".to_owned())
            })
            .and_then(|summary| {
                verified(git, commit, &summary, agents.as_deref()).map(|signer| (summary, signer))
            });
        let age = map.now.saturating_sub(*since);
        let row = match checked {
            Ok((summary, signer)) => {
                let fresh = map.summary(git, machine, &summary, *since);
                json!({"machine": machine, "commit": commit, "readable": true,
                    "signer": signer, "at": summary["at"], "age_s": age, "sessions": fresh})
            }
            Err(why) => json!({"machine": machine, "commit": commit, "readable": false,
                "why": why, "at": null, "age_s": age, "sessions": 0}),
        };
        map.machines.insert(machine.clone(), row);
    }
    Ok(truncated)
}

/// This machine's sessions, from its own records: their rows as a summary written now would
/// carry them, with the files each last wrote.
fn local(repo: &Repo, here: &str, fetched: &Fetched, lines: Option<&[Value]>, map: &mut Map) {
    let commit = fetched.tips.get(here).map(|(commit, _)| commit.clone());
    let machine = |sessions: usize| {
        json!({"machine": here, "commit": commit, "readable": true, "source": "local records",
            "at": now_rfc3339(), "age_s": 0, "sessions": sessions})
    };
    let Some(lines) = lines else {
        if commit.is_some() {
            map.machines.insert(here.to_owned(), machine(0));
        }
        return;
    };
    let rows = live::rows_of(lines.to_vec(), map.now, map.window);
    for row in &rows {
        let age = live::unix_at(row).map_or(0, |at| map.now.saturating_sub(at));
        let wrote = row["session"].as_str().map_or_else(
            || json!({"wrote": null, "wrote_at": null}),
            |session| live::last_wrote(repo, session),
        );
        map.session(&repo.git, here, row, age, &wrote);
    }
    map.machines.insert(here.to_owned(), machine(rows.len()));
}

/// Every change record this cache holds, by id.
fn local_records(git: &Git) -> Result<BTreeMap<String, ChangeRecord>, String> {
    let chain = Chain::new(git);
    let ids = git
        .run(&["for-each-ref", "--format=%(refname)", CHANGES_PREFIX])
        .map_err(|error| error.message())?;
    Ok(ids
        .lines()
        .map(|name| name.trim_start_matches(CHANGES_PREFIX))
        .filter_map(|id| {
            chain
                .read(id)
                .ok()
                .map(|(_, record)| (id.to_owned(), record))
        })
        .collect())
}

/// The held changes of this cache that no fresh session row of their actor names. A change
/// whose view is on this machine and that no session record here names has had no hooked
/// session: it is `untracked`, `tracked: false`, never idle (§ 11a: a session with hooks
/// disabled). One a record here names had a hooked session that ended or stopped beating, so
/// it gets no entry. A change whose view is elsewhere cannot be judged from here: its entry
/// says `unknown`, `tracked: null`. A change already on trunk is landed, not held; that costs
/// one trunk log per change that reaches the check.
fn unheld(repo: &Repo, map: &Map, lines: &[Value]) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    for (id, record) in &map.records {
        let held = matches!(
            record.state,
            ChangeState::Open
                | ChangeState::Invalidated
                | ChangeState::Repairing
                | ChangeState::Ready
        );
        let seen = map.sessions.iter().any(|row| {
            row["change"] == id.as_str() && row["actor"]["name"] == record.actor.name.as_str()
        });
        if !held || seen {
            continue;
        }
        let here = repo.view_path(id).is_dir();
        if here && lines.iter().any(|line| line["change"] == id.as_str()) {
            continue;
        }
        if let Some(trunk) = &map.trunk
            && !crate::publish::scan(&repo.git, &record.base_first, trunk, id)?
                .publications
                .is_empty()
        {
            continue;
        }
        let (state, tracked, why) = if here {
            ("untracked", json!(false), Value::Null)
        } else {
            (
                "unknown",
                Value::Null,
                json!(
                    "its view is not on this machine and no fresh live row of its actor names it"
                ),
            )
        };
        out.push(json!({
            "machine": null,
            "session": null,
            "actor": record.actor,
            "actor_source": record.actor_source,
            "change": id,
            "state": state,
            "why": why,
            "age_s": null,
            "wrote": null,
            "wrote_at": null,
            "behind": map.behind(&json!(id)),
            "behind_commits": map.behind_commits(&repo.git, &json!(id)),
            "started": null,
            "session_age_s": null,
            "on_red": map.on_red(&json!(id), &Value::Null),
            "tracked": tracked,
        }));
    }
    Ok(out)
}

/// G47b: every open talk this cache holds, as `bay status` lists them (who owes the reply, the
/// ages, the standing, the change it is about). Each talk's sender is an actor heard at `sent`,
/// and its recipient one heard at its ack or backbrief. Each session entry gets `owes_talks`, the
/// open talks its actor owes the reply to, and `owed_talks`, those its actor sent that another
/// actor owes: talks go to actors, so every session of one actor carries the same counts. An
/// unreadable talk ref lists `{"unread": why}` and the counts are `null`, never `0`.
fn talks(git: &Git, map: &mut Map) -> Value {
    let rows = match crate::talk::standing::open_rows(git, crate::talk::standing::reader_now()) {
        Ok(rows) => rows,
        Err(why) => {
            for entry in &mut map.sessions {
                entry["owes_talks"] = Value::Null;
                entry["owed_talks"] = Value::Null;
            }
            return json!({"unread": why});
        }
    };
    for row in &rows {
        map.talk_actor(row["from"].as_str(), row["age_s"].as_u64());
        let heard = [row["ack_age_s"].as_u64(), row["reply_age_s"].as_u64()]
            .into_iter()
            .flatten()
            .min();
        map.talk_actor(row["who"].as_str(), heard);
    }
    for entry in &mut map.sessions {
        let actor = entry["actor"]["name"].clone();
        let (mut to_answer, mut awaited) = (0_usize, 0_usize);
        if actor.is_string() {
            for row in &rows {
                if row["owes"] == actor {
                    to_answer += 1;
                } else if row["from"] == actor && row["owes"].is_string() {
                    awaited += 1;
                }
            }
        }
        entry["owes_talks"] = json!(to_answer);
        entry["owed_talks"] = json!(awaited);
    }
    Value::from(rows)
}

/// The fleet map from the recorded fetch (`DESIGN` § 11a, `INTERFACES` § 5a): one entry per
/// fresh session row, this machine's from its records, each with how many commits its view's
/// base is behind trunk (`behind_commits`) and the session's age (`session_age_s`, G166); an entry per held change no session
/// names ([`unheld`]); every machine and actor the cache knows, each with `age_s`, the actors of
/// open talks among them; every open talk the cache holds ([`talks`], G47b); and
/// `default_actor_sessions`, the fresh sessions whose actor's name came from a default rather
/// than `BAY_ACTOR` (G152). `red` lists the post checks trunk is red on,
/// as `bay status` does, and each entry's `on_red` names those whose paths cover its change's
/// declared paths or the files its session last wrote (G244b).
///
/// `detail.behind` is never a known `false`: whether the remote moved since the fetch cannot
/// be known without the network, which the map never touches. It is `true` once the fetch is a
/// takeover window old (rows refreshed since are missing), and `null`, unknown, before that.
///
/// The cost is bounded: one ref listing of the changes and one chain read each, at most
/// [`FLEET_MAX_MACHINES`] summaries of at most `live::LIVE_MAX_BYTES` read in two `cat-file`
/// calls, one signature check per summary under a registry, one bounded tail per session
/// record of this machine, one trunk log per held change no session names, one red
/// judgement over [`crate::red::WINDOW`] trunk commits, and one scan of the same window for
/// unattributed landings ([`crate::unattributed::scan`]), and one read of the talk refs this
/// cache holds ([`talks`]).
fn map(repo: &Repo, fetched: &Fetched) -> Result<BayResult, String> {
    let git = &repo.git;
    let trunk = git
        .run(&["rev-parse", "--verify", &repo.trunk_tracking_ref()])
        .ok();
    // As `bay status` reads it: local records only, and an unreadable judgement lists none and
    // says why (G104b), never read as not red.
    let judged = trunk.as_deref().map(|trunk| {
        crate::registry::Registry::current(git)
            .and_then(|registry| crate::red::red(git, trunk, registry.as_ref()))
    });
    let red_unread = judged
        .as_ref()
        .and_then(|judged| judged.as_ref().err().cloned());
    let red = judged.and_then(Result::ok).unwrap_or_default();
    let mut map = Map {
        now: live::unix_now(),
        window: repo.config.takeover().as_secs(),
        trunk,
        records: local_records(git)?,
        machines: BTreeMap::new(),
        sessions: Vec::new(),
        actors: BTreeMap::new(),
        red,
    };
    let here = live::machine(git).ok();
    let truncated = remote(repo, fetched, here.as_deref(), &mut map)?;
    let lines = live::lines(repo);
    if let Some(here) = &here {
        local(repo, here, fetched, lines.as_deref(), &mut map);
    }
    // G152: the fresh sessions whose actor was named by a default, not `BAY_ACTOR`; on a shared
    // account every one of them is the same actor. A row that does not say is not counted.
    let default_actor = map
        .sessions
        .iter()
        .filter(|row| {
            matches!(
                row["actor_source"].as_str(),
                Some("user-default" | "default")
            )
        })
        .count();
    let unheld = unheld(repo, &map, lines.as_deref().unwrap_or_default())?;
    map.sessions.extend(unheld);
    let talks = talks(git, &mut map);

    let fetch_age = unix_of(&fetched.at).map(|at| map.now.saturating_sub(at));
    let behind = if fetch_age.is_none_or(|age| age >= map.window) {
        json!(true)
    } else {
        Value::Null
    };
    let red: Value = red_unread.map_or_else(
        || map.red.iter().map(crate::red::Red::to_json).collect(),
        |why| json!({"unread": why}),
    );
    // G85: as `bay status` reads it, from the trunk this cache holds.
    let unattributed = map.trunk.as_deref().map(|trunk| {
        crate::unattributed::scan(git, trunk).unwrap_or_else(|why| json!({"unread": why}))
    });
    let mut result = BayResult::ok(json!({
        "remote": repo.remote(),
        "fetched_at": fetched.at,
        "fetch_age_s": fetch_age,
        "behind": behind,
        "behind_basis": "true once the fetch is a takeover window old; before that unknown (null): the map never asks the remote",
        "window_s": map.window,
        "trunk": map.trunk,
        "machines": map.machines.into_values().collect::<Vec<_>>(),
        "machines_truncated": truncated,
        "actors": map.actors.into_iter().map(|(actor, (age, on))| json!({
            "actor": actor, "machines": on, "age_s": age,
        })).collect::<Vec<_>>(),
        "talks": talks,
        "red": red,
        "unattributed": unattributed,
        "default_actor_sessions": default_actor,
        "sessions": map.sessions,
    }));
    if let Some(trunk) = map.trunk {
        result.freshness = Some(Freshness {
            trunk,
            fetched_at: fetched.at.clone(),
        });
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn a_sessions_age_is_its_rows_age_plus_its_run_on_the_publishers_clock() {
        // G166: both times are the publishing machine's.
        let row = json!({"at": "2026-10-19T10:00:30Z", "started": "2026-10-19T10:00:00Z"});
        assert_eq!(super::session_age(&row, 5), json!(35));
        assert_eq!(
            super::session_age(&json!({"at": "2026-10-19T10:00:30Z"}), 5),
            serde_json::Value::Null
        );
    }
}
