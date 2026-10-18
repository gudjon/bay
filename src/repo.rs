//! The repository bay runs in: its common git directory, its main work tree, its config,
//! and where views live.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::git::{Git, GitError};

pub struct Repo {
    /// Git on the common directory: every view of this repository shares it.
    pub git: Git,
    pub common_dir: PathBuf,
    /// The main work tree, where `.bay/config.toml` lives.
    pub work_root: PathBuf,
    pub config: Config,
}

impl Repo {
    /// Opens the repository that contains `dir` (the main work tree or one of its views).
    ///
    /// # Errors
    ///
    /// `dir` is not inside a work repository, or its config does not parse.
    pub fn open(dir: &Path) -> Result<Self, String> {
        let here = Git::new(dir);
        let common = here
            .run(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .map_err(|error| error.message())?;
        let common_dir = PathBuf::from(common);
        let work_root = if common_dir.file_name().is_some_and(|name| name == ".git") {
            common_dir
                .parent()
                .map(Path::to_path_buf)
                .ok_or("the git directory has no parent")?
        } else {
            return Err(format!(
                "{} is a bare repository; run bay in a work repository",
                common_dir.display()
            ));
        };
        let config = Config::load_or_committed(&work_root, &Git::new(&common_dir))?;
        Ok(Self {
            git: Git::new(&common_dir),
            common_dir,
            work_root,
            config,
        })
    }

    #[must_use]
    pub fn views_dir(&self) -> PathBuf {
        self.common_dir.join("bay").join("views")
    }

    #[must_use]
    pub fn view_path(&self, id: &str) -> PathBuf {
        self.views_dir().join(id)
    }

    #[must_use]
    pub fn remote(&self) -> &str {
        self.config.remote()
    }

    #[must_use]
    pub fn trunk(&self) -> &str {
        self.config.trunk()
    }

    /// The local tracking ref of the remote trunk.
    #[must_use]
    pub fn trunk_tracking_ref(&self) -> String {
        format!("refs/remotes/{}/{}", self.remote(), self.trunk())
    }

    /// Fetches the remote trunk and returns its tip.
    ///
    /// # Errors
    ///
    /// The fetch fails or the remote has no trunk.
    pub fn fetch_trunk(&self) -> Result<String, GitError> {
        let refspec = format!("+refs/heads/{}:{}", self.trunk(), self.trunk_tracking_ref());
        self.git
            .run(&["fetch", "-q", "--no-tags", self.remote(), &refspec])?;
        self.git
            .run(&["rev-parse", "--verify", &self.trunk_tracking_ref()])
    }

    /// Fetches `refs/bay/registry` when the remote has one. A remote without a registry leaves
    /// the local one as it is.
    pub fn fetch_registry(&self) {
        let name = crate::registry::REGISTRY_REF;
        let _ = self.git.run(&[
            "fetch",
            "-q",
            "--no-tags",
            self.remote(),
            &format!("+{name}:{name}"),
        ]);
    }

    /// Fetches `refs` (full ref names) from the remote into the same names. A ref the
    /// remote lacks is an error.
    ///
    /// # Errors
    ///
    /// The fetch fails.
    pub fn fetch_refs(&self, refs: &[String]) -> Result<(), GitError> {
        let mut args = vec!["fetch", "-q", "--no-tags", self.remote()];
        let refspecs: Vec<String> = refs.iter().map(|name| format!("+{name}:{name}")).collect();
        args.extend(refspecs.iter().map(String::as_str));
        self.git.run(&args).map(|_| ())
    }

    /// The view directory's change id, when `dir` is one of this repository's views.
    #[must_use]
    pub fn view_change(&self, dir: &Path) -> Option<String> {
        let toplevel = Git::new(dir).run(&["rev-parse", "--show-toplevel"]).ok()?;
        let toplevel = PathBuf::from(toplevel).canonicalize().ok()?;
        let views = self.views_dir().canonicalize().ok()?;
        (toplevel.parent()? == views).then(|| toplevel.file_name()?.to_str().map(str::to_owned))?
    }
}
