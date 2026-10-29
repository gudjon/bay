//! #123: the runner's post work, side by side (`crate::bisect::plan`).
//!
//! Each [`Piece`] runs on its own thread, at most one for each environment, so a post check of
//! one environment never waits for one of another. A pass never waits for a piece: it starts
//! the pieces its plan names and collects the verdict of each piece that ended since
//! ([`Post::collect`]). `--once` waits for every piece it started ([`Post::finish`]), and so
//! does a runner before it moves to a new binary: a piece is never handed over, and a runner
//! never stops while one runs.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::thread::JoinHandle;

use serde_json::{Map, Value, json};

use crate::bisect::Piece;
use crate::repo::Repo;

/// One running piece.
struct Running {
    environment: String,
    kind: &'static str,
    thread: JoinHandle<Value>,
}

/// The runner's running post pieces.
pub struct Post {
    running: Vec<Running>,
    /// Whether each pass waits for the pieces it started (`--once`).
    pub waits: bool,
}

impl Post {
    /// No piece runs yet. With `waits`, each pass waits for the pieces it starts.
    #[must_use]
    pub fn new(waits: bool) -> Self {
        Self {
            running: Vec::new(),
            waits,
        }
    }

    /// The environments a running piece holds.
    #[must_use]
    pub fn busy(&self) -> BTreeSet<String> {
        self.running
            .iter()
            .map(|running| running.environment.clone())
            .collect()
    }

    /// Whether every piece started has ended.
    #[must_use]
    pub fn idle(&self) -> bool {
        self.pending() == 0
    }

    /// Whether a piece started whose rows no collect took yet.
    #[must_use]
    pub fn holds(&self) -> bool {
        !self.running.is_empty()
    }

    /// How many pieces still run.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.running
            .iter()
            .filter(|running| !running.thread.is_finished())
            .count()
    }

    /// Starts `piece` on its own thread, which writes its own journal line when it ends (E21).
    /// A thread that cannot start is its row, by kind.
    pub fn start(&mut self, repo: &Arc<Repo>, piece: Piece) -> Option<(&'static str, Value)> {
        let environment = piece.environment.clone();
        let kind = piece.kind;
        let repo = Arc::clone(repo);
        let spawned = std::thread::Builder::new()
            .name(format!("bay post {environment}"))
            .spawn(move || {
                let began = std::time::Instant::now();
                let done = piece.run(&repo);
                eprintln!(
                    "bay runner: {} ({}s)",
                    super::post_line(&kinded(kind, done.clone())),
                    began.elapsed().as_secs()
                );
                done
            });
        match spawned {
            Ok(thread) => {
                self.running.push(Running {
                    environment,
                    kind,
                    thread,
                });
                None
            }
            Err(error) => Some((
                kind,
                json!({"error": format!("the post work in {environment} could not start: {error}")}),
            )),
        }
    }

    /// The rows of each piece that ended since the last collect, by kind.
    pub fn collect(&mut self) -> Vec<(&'static str, Value)> {
        let (ended, running): (Vec<Running>, Vec<Running>) = std::mem::take(&mut self.running)
            .into_iter()
            .partition(|running| running.thread.is_finished());
        self.running = running;
        ended.into_iter().map(joined).collect()
    }

    /// Waits for every running piece: the rows of each, by kind.
    pub fn finish(&mut self) -> Vec<(&'static str, Value)> {
        std::mem::take(&mut self.running)
            .into_iter()
            .map(joined)
            .collect()
    }
}

impl Drop for Post {
    /// A runner that stops waits for its pieces, so none ends inside a write.
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// The rows of `running`, once it ended: its own, or an error row when its thread panicked.
fn joined(running: Running) -> (&'static str, Value) {
    let Running {
        environment,
        kind,
        thread,
    } = running;
    let done = thread.join().unwrap_or_else(|_| {
        let why = format!("the post work in {environment} stopped on a panic");
        eprintln!("bay runner: {why}");
        json!({"error": why})
    });
    (kind, done)
}

/// `value` under its kind: `{"<kind>": value}`.
#[must_use]
pub fn kinded(kind: &str, value: Value) -> Value {
    let mut row = Map::new();
    row.insert(kind.to_owned(), value);
    Value::Object(row)
}

/// The pass's post rows: one row for each kind, its arrays joined in the order they came, and
/// each error row on its own.
#[must_use]
pub fn rows(parts: Vec<(&'static str, Value)>) -> Vec<Value> {
    let mut joined: Vec<(&'static str, Vec<Value>)> = Vec::new();
    let mut errors = Vec::new();
    for (kind, value) in parts {
        match value {
            Value::Array(items) if items.is_empty() => {}
            Value::Array(items) => match joined.iter_mut().find(|(seen, _)| *seen == kind) {
                Some((_, all)) => all.extend(items),
                None => joined.push((kind, items)),
            },
            other => errors.push(kinded(kind, other)),
        }
    }
    joined
        .into_iter()
        .map(|(kind, items)| kinded(kind, Value::Array(items)))
        .chain(errors)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rows_of_one_kind_are_one_row_and_an_error_stands_alone() {
        let parts = vec![
            ("post", json!([{"check": "a"}])),
            ("bisect", json!([])),
            ("post", json!([{"check": "b"}])),
            ("warm", json!({"error": "no"})),
        ];
        assert_eq!(
            rows(parts),
            vec![
                json!({"post": [{"check": "a"}, {"check": "b"}]}),
                json!({"warm": {"error": "no"}}),
            ]
        );
    }
}
