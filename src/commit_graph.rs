//! The commit-graph that bay's path-limited walks read (G191-G193): generation numbers and
//! changed-path Bloom filters on a bay-hosted trunk, an executor's clone and a shared cache.
//!
//! Git writes, reads and maintains the graph; bay only configures it, writes the first one
//! where Git would not, and doctor reports it (G192). It is a disposable index and never a
//! source of truth: a missing or stale graph only makes a walk slower, never its answer
//! different. bay runs no maintenance (G172). A host's graph is refreshed by Git's own
//! maintenance after a push (the commit-graph task `bay host init` enables), and a clone's by
//! `fetch.writeCommitGraph`.
//!
//! Git versions: changed-path filters 2.27; corrected commit dates (generation v2) 2.31, the
//! default since; version 2 filters (`commitGraph.changedPathsVersion`) 2.46;
//! `commitGraph.changedPaths` 2.52. A fetch's write and gc's write never start filters of
//! their own: they keep them only where the graph they extend already has them, so bay writes
//! the first graph with `--changed-paths` itself.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::git::{Git, GitError};
use crate::version::Version;

/// `commitGraph.changedPathsVersion` (2.46): version 2 filters, which hold on a system whose
/// `char` is unsigned. Version 1 filters read under version 2 are not used, so bay sets it
/// only where the store has no filters yet.
pub const BLOOM_V2_SINCE: &str = "2.46";
/// `commitGraph.changedPaths` (2.52): every `git commit-graph write`, maintenance's among them,
/// writes filters even with no graph to inherit them from.
pub const CHANGED_PATHS_CONFIG_SINCE: &str = "2.52";
/// The bound on the first graph a clone gets: one write over all of its history.
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// The settings `bay host init` writes on a host (whose floor, 2.54, has every one).
pub const HOST_SETTINGS: [(&str, &str); 5] = [
    ("core.commitGraph", "true"),
    ("gc.writeCommitGraph", "true"),
    ("commitGraph.changedPaths", "true"),
    ("commitGraph.changedPathsVersion", "2"),
    // Git's auto maintenance after each receive runs the commit-graph task whatever the
    // site's `maintenance.strategy`; the task's own threshold keeps it cheap.
    ("maintenance.commit-graph.enabled", "true"),
];

/// What a store's commit-graph gives Git's walks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// `core.commitGraph` is false: Git reads no graph.
    Disabled,
    /// No graph file.
    Missing,
    /// A graph, but some layer has no changed-path filters Git reads here.
    NoBloomFilters,
    /// A graph whose every layer carries filters Git reads here.
    Ok,
}

impl State {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Missing => "missing",
            Self::NoBloomFilters => "no_bloom_filters",
            Self::Ok => "ok",
        }
    }
}

/// A store's commit-graph, read from its files and config, never by a walk.
pub struct Graph {
    pub state: State,
    pub layers: usize,
    /// The filter version of each layer that has filters.
    pub bloom_versions: Vec<u32>,
}

impl Graph {
    #[must_use]
    pub fn detail(&self) -> Value {
        let mut versions = self.bloom_versions.clone();
        versions.sort_unstable();
        versions.dedup();
        json!({"state": self.state.name(), "layers": self.layers, "bloom_versions": versions})
    }
}

/// Reads the graph of the store at `git`: the layer files under `objects/info` and the config
/// that decides whether Git reads them.
#[must_use]
pub fn read(git: &Git) -> Graph {
    let layers = git
        .common_dir()
        .map(|common| layer_files(&Path::new(&common).join("objects").join("info")))
        .unwrap_or_default();
    let bloom: Vec<Option<u32>> = layers
        .iter()
        .map(|layer| bloom_version(layer).ok().flatten())
        .collect();
    let bloom_versions: Vec<u32> = bloom.iter().flatten().copied().collect();
    // Git reads filters of any version under -1 (the default), else only the configured one.
    let configured = git
        .config_get("commitGraph.changedPathsVersion")
        .ok()
        .flatten()
        .and_then(|text| text.trim().parse::<i64>().ok())
        .unwrap_or(-1);
    let state = if config_bool(git, "core.commitGraph") == Some(false) {
        State::Disabled
    } else if layers.is_empty() {
        State::Missing
    } else if bloom.iter().all(|version| {
        matches!(version, Some(version) if configured == -1 || configured == i64::from(*version))
    }) {
        State::Ok
    } else {
        State::NoBloomFilters
    };
    Graph {
        state,
        layers: layers.len(),
        bloom_versions,
    }
}

/// `git config --type=bool --get <key>`: `None` when unset or not a boolean.
#[must_use]
pub fn config_bool(git: &Git, key: &str) -> Option<bool> {
    match git
        .run(&["config", "--type=bool", "--get", key])
        .ok()?
        .as_str()
    {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// The graph files Git reads, in the order it prefers them: the single file
/// `commit-graph` when there is one, else each layer named by the split chain.
fn layer_files(info: &Path) -> Vec<PathBuf> {
    let single = info.join("commit-graph");
    if single.is_file() {
        return vec![single];
    }
    let graphs = info.join("commit-graphs");
    std::fs::read_to_string(graphs.join("commit-graph-chain"))
        .map(|chain| {
            chain
                .lines()
                .map(str::trim)
                .filter(|hash| !hash.is_empty())
                .map(|hash| graphs.join(format!("graph-{hash}.graph")))
                .collect()
        })
        .unwrap_or_default()
}

/// The version of a graph file's changed-path filters (its `BDAT` chunk), `None` when it has
/// none. Reads the header, the chunk table and four bytes, whatever the file's size.
fn bloom_version(path: &Path) -> std::io::Result<Option<u32>> {
    let malformed = || std::io::Error::new(std::io::ErrorKind::InvalidData, "not a commit-graph");
    let mut file = std::fs::File::open(path)?;
    let mut header = [0_u8; 8];
    file.read_exact(&mut header)?;
    if &header[..4] != b"CGPH" {
        return Err(malformed());
    }
    // One table entry per chunk plus the terminating one: a 4-byte id and an 8-byte offset.
    let mut table = vec![0_u8; (usize::from(header[6]) + 1) * 12];
    file.read_exact(&mut table)?;
    let Some(entry) = table
        .as_chunks::<12>()
        .0
        .iter()
        .find(|entry| &entry[..4] == b"BDAT")
    else {
        return Ok(None);
    };
    let offset = u64::from_be_bytes(entry[4..].try_into().map_err(|_| malformed())?);
    file.seek(SeekFrom::Start(offset))?;
    let mut version = [0_u8; 4];
    file.read_exact(&mut version)?;
    Ok(Some(u32::from_be_bytes(version)))
}

/// G193: sets the clone at `git` (an executor's clone or a shared cache) to keep its graph on
/// fetch, with changed-path filters where this Git has them, and writes the first graph when
/// the store has none with filters. Keys the site already set stay as they are. A graph that
/// cannot be written is reported, never a failure: the graph only speeds walks up.
///
/// # Errors
///
/// A config write fails.
pub fn configure_clone(git: &Git) -> Result<Value, String> {
    let version = Version::parse(&git.version().map_err(|error| error.message())?);
    let before = read(git);
    let mut settings = vec![("fetch.writeCommitGraph", "true")];
    if version >= Version::parse(CHANGED_PATHS_CONFIG_SINCE) {
        settings.push(("commitGraph.changedPaths", "true"));
    }
    if version >= Version::parse(BLOOM_V2_SINCE) && before.bloom_versions.is_empty() {
        settings.push(("commitGraph.changedPathsVersion", "2"));
    }
    let mut set = Vec::new();
    for (key, value) in settings {
        if git
            .config_get(key)
            .map_err(|error| error.message())?
            .is_none()
        {
            git.run(&["config", "--local", key, value])
                .map_err(|error: GitError| error.message())?;
            set.push(key);
        }
    }
    let mut written = None;
    if matches!(before.state, State::Missing | State::NoBloomFilters) {
        // Never a lazy fetch (G104): a tree-less store's graph is the site's to write.
        let outcome = git.run_bytes_for(
            &["commit-graph", "write", "--reachable", "--changed-paths"],
            &[("GIT_NO_LAZY_FETCH", "1")],
            None,
            WRITE_TIMEOUT,
        );
        written = Some(outcome.map_err(|error| error.message()));
    }
    let mut detail = read(git).detail();
    detail["set"] = json!(set);
    if let Some(Err(why)) = written {
        detail["write_failed"] = json!(why);
    }
    Ok(detail)
}
