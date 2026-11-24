//! File checkpoints: a copy of a project's files taken before an agent turn, so the user can put
//! them back the way they were if the turn went wrong.
//!
//! The copies live in a *shadow* git repository (the idea comes from Hermes Agent's
//! `agent/checkpoints/manager.py`): git is good at storing many versions of a folder cheaply and
//! at telling what changed, and a shadow repository never touches the project's own `.git` (its
//! HEAD, index, branches and stash stay as the user left them, and a folder that isn't a git
//! repository works just the same).
//!
//! One store serves every project, so files that several projects (or many checkpoints) share are
//! kept once. Each project has its own index file (`<store>/index-<key>`) and its own refs
//! (`refs/devcat/<key>/head`), the key being a stable hash of the project's folder ([`key_of`]).
//! A checkpoint is a commit whose parent is the project's previous checkpoint.
//!
//! git runs as a plain storage tool here, never as the user's or the project's git: no system or
//! global config (so no filters, fsmonitor, hooks path or LFS from `~/.gitconfig`), no hooks, no
//! `.gitattributes` (no filters or line-ending changes: files come back byte for byte), no stdin,
//! its own process group and a time limit. The project's own `.gitignore` files still apply, and a
//! list of big generated folders ([`EXCLUDE`]) is always left out.
use serde::Serialize;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// How long one git call may take before it's stopped (with everything it started).
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// `git gc` in [`Checkpoints::prune`] packs the whole shared store, which takes longer.
const GC_TIMEOUT: Duration = Duration::from_secs(300);

/// A folder with more files than this isn't a project an agent works on file by file (a home or
/// downloads folder, a data dump): copying it before every turn would be slow and fill the disk.
pub const MAX_FILES: usize = 20_000;

/// Likewise for the total size of the files a checkpoint would hold.
pub const MAX_BYTES: u64 = 200 * 1024 * 1024;

/// [`Checkpoints::list`] returns at most this many, newest first.
pub const MAX_LIST: usize = 200;

/// Labels are one line of plain text, at most this many characters.
pub const MAX_LABEL: usize = 200;

/// Big or generated folders and files that are never part of a checkpoint, whatever the project's
/// `.gitignore` says: they're rebuilt or downloaded, and copying them would make every checkpoint
/// slow (and [`MAX_FILES`] would refuse most JavaScript projects because of `node_modules`).
pub const EXCLUDE: &[&str] = &["node_modules/", "target/", ".venv/", "venv/", "__pycache__/", "dist/", "build/", ".next/", ".gradle/", ".idea/", "*.log"];

/// The empty tree's id: git knows it without storing it. `GIT_ATTR_SOURCE` set to it means "no
/// `.gitattributes` anywhere" (git 2.40+; older git ignores the variable).
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// The label of the checkpoint [`Checkpoints::restore`] takes first, so a restore can be undone.
pub const BEFORE_RESTORE: &str = "before restore";

/// The result of [`Checkpoints::snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Snapshot {
    /// The files are saved in `commit`. `created` is false when nothing changed since the latest
    /// checkpoint: `commit` is then that one, and no new checkpoint was added.
    Saved { commit: String, created: bool },
    /// No checkpoint was taken, and why (the home folder, too many files, …). Not an error: the
    /// turn goes on, it just can't be undone this way.
    Skipped { reason: String },
}

impl Snapshot {
    /// The checkpoint's commit, if one was saved (or reused).
    pub fn commit(&self) -> Option<&str> {
        match self {
            Snapshot::Saved { commit, .. } => Some(commit),
            Snapshot::Skipped { .. } => None,
        }
    }
}

/// How a file differs between a checkpoint and the files as they are now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// There now, not in the checkpoint.
    Added,
    /// In both, with other content (or now a link instead of a file, or the other way round).
    Modified,
    /// In the checkpoint, gone now.
    Deleted,
}

/// One file that changed, its path relative to the project folder with `/` between folders.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Change {
    pub path: String,
    pub status: Status,
}

/// One checkpoint in [`Checkpoints::list`]: its commit, label and when it was taken (Unix seconds).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub commit: String,
    pub label: String,
    pub time: u64,
}

/// The checkpoints of one project folder, kept in a store shared by all projects.
pub struct Checkpoints {
    /// The shadow repository (its `GIT_DIR`).
    store: PathBuf,
    /// The project folder, canonical.
    cwd: PathBuf,
    /// [`key_of`] the project folder: names its index file and refs.
    key: String,
    /// The user's home folder, canonical: never checkpointed.
    home: Option<PathBuf>,
    max_files: usize,
    max_bytes: u64,
    /// Extra environment for every git call (tests only: a fake HOME).
    #[cfg(test)]
    env: Vec<(OsString, OsString)>,
}

/// What a git call printed and how it ended.
struct Out {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: String,
}

/// A change with the path as git wrote it (bytes: on Unix a file name needn't be UTF-8).
struct RawChange {
    path: Vec<u8>,
    status: Status,
}

impl Checkpoints {
    /// The checkpoints of `cwd`, kept in the store at `store_root` (e.g. `<data>/checkpoints`). Does
    /// nothing on disk: the store is made on first use.
    pub fn new(store_root: &Path, cwd: &Path) -> Checkpoints {
        let cwd = canonical(cwd);
        let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).filter(|h| !h.is_empty()).map(|h| canonical(Path::new(&h)));
        Checkpoints {
            store: canonical(store_root),
            key: key_of(&cwd),
            cwd,
            home,
            max_files: MAX_FILES,
            max_bytes: MAX_BYTES,
            #[cfg(test)]
            env: Vec::new(),
        }
    }

    /// The project folder the checkpoints are of (canonical).
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Save the project's files as a checkpoint labeled `label`, unless they're the same as in the
    /// latest one (then that one is returned with `created: false`). A folder that shouldn't be
    /// copied is [`Snapshot::Skipped`], not an error.
    pub async fn snapshot(&self, label: &str) -> Result<Snapshot, String> {
        let lock = self.lock();
        let _held = lock.lock().await;
        self.snapshot_locked(label).await
    }

    /// What changed in the project's files since checkpoint `commit` (a full or abbreviated id from
    /// [`Checkpoints::list`] or [`Checkpoints::snapshot`]), sorted by path. Folders git can't take
    /// (a nested repository) are left out.
    pub async fn changed_since(&self, commit: &str) -> Result<Vec<Change>, String> {
        let lock = self.lock();
        let _held = lock.lock().await;
        let commit = self.resolve(commit).await?;
        let now = match self.tree_now().await? {
            Ok(tree) => tree,
            Err(reason) => return Err(reason),
        };
        Ok(public(self.diff(&commit, &now).await?))
    }

    /// Put the project's files back the way they were in checkpoint `commit`: files changed or
    /// deleted since are written again from it, and files added since are deleted (with the folders
    /// that leaves empty). Nothing else is touched: ignored files, the generated folders in
    /// [`EXCLUDE`], nested repositories and the project's `.git` stay as they are.
    ///
    /// First the files as they are now are saved as a checkpoint labeled [`BEFORE_RESTORE`], so
    /// the restore can be undone by restoring that one; if that can't be saved, nothing is
    /// restored. Returns how the files differed from `commit` before the restore (what it undid).
    pub async fn restore(&self, commit: &str) -> Result<Vec<Change>, String> {
        let lock = self.lock();
        let _held = lock.lock().await;
        let commit = self.resolve(commit).await?;
        let before = match self.snapshot_locked(BEFORE_RESTORE).await? {
            Snapshot::Saved { commit, .. } => commit,
            Snapshot::Skipped { reason } => return Err(format!("Not restored: the files as they are now couldn't be saved first, so the restore couldn't be undone. {reason}")),
        };
        let changes = self.diff(&commit, &before).await?;
        // added files go first: one may stand where a folder of the checkpoint's must come back
        for c in changes.iter().filter(|c| c.status == Status::Added) {
            self.remove_added(&c.path)?;
        }
        let back: Vec<&[u8]> = changes.iter().filter(|c| c.status != Status::Added).map(|c| c.path.as_slice()).collect();
        if !back.is_empty() {
            for p in &back {
                safe_rel(p).ok_or_else(|| format!("Not restored: the checkpoint has an unsafe path {:?}.", String::from_utf8_lossy(p)))?;
            }
            // a list in a file: no command-line length limit, and no stdin
            let list = self.store.join(format!("restore-{}.paths", self.key));
            let mut text = Vec::new();
            for p in &back {
                text.extend_from_slice(p);
                text.push(0);
            }
            std::fs::write(&list, text).map_err(|e| format!("Couldn't write {}: {e}", list.display()))?;
            let mut args = os(&["checkout", &commit]);
            args.push(join_os("--pathspec-from-file=", &list));
            args.push("--pathspec-file-nul".into());
            let done = self.git(&args).await;
            let _ = std::fs::remove_file(&list);
            done.map_err(|e| format!("Restoring files from the checkpoint failed: {e}"))?;
        }
        Ok(public(changes))
    }

    /// This project's checkpoints, newest first, at most [`MAX_LIST`].
    pub async fn list(&self) -> Result<Vec<Entry>, String> {
        let Some(head) = self.head().await? else {
            return Ok(Vec::new());
        };
        let out = self.git(&os(&["log", "--first-parent", &format!("--max-count={MAX_LIST}"), "--format=%H %ct %s", &head])).await?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|line| {
                let mut it = line.splitn(3, ' ');
                let commit = it.next()?.to_string();
                let time = it.next()?.parse().ok()?;
                Some(Entry { commit, label: it.next().unwrap_or("").to_string(), time })
            })
            .collect())
    }

    /// Keep only the newest `keep` checkpoints of this project, and return how many were dropped.
    ///
    /// The kept ones are committed again as a new chain (same files, labels and times, so new
    /// commit ids) and the project's ref moves to it: the dropped ones are then reachable from
    /// nothing, and restoring one of them is refused. Then `git gc --prune=2.weeks.ago` packs the
    /// store and deletes what has been unreachable for two weeks; the grace period is there
    /// because another project's snapshot may be writing files into the shared store right now,
    /// and their objects aren't referenced until it's done. So disk space comes back with a delay.
    pub async fn prune(&self, keep: usize) -> Result<usize, String> {
        let lock = self.lock();
        let _held = lock.lock().await;
        let Some(head) = self.head().await? else {
            return Ok(0);
        };
        let count: usize = String::from_utf8_lossy(&self.git(&os(&["rev-list", "--first-parent", "--count", &head])).await?).trim().parse().map_err(|_| "git rev-list --count gave no number".to_string())?;
        if count <= keep {
            return Ok(0);
        }
        if keep == 0 {
            self.git(&os(&["update-ref", "-d", &self.ref_name(), &head])).await?;
        } else {
            let out = self.git(&os(&["log", "--first-parent", &format!("--max-count={keep}"), "--format=%T %ct %s", &head])).await?;
            let text = String::from_utf8_lossy(&out).into_owned();
            let mut parent: Option<String> = None;
            for line in text.lines().rev() {
                let mut it = line.splitn(3, ' ');
                let (Some(tree), Some(time)) = (it.next(), it.next()) else {
                    return Err(format!("Unexpected git log line: {line}"));
                };
                let label = it.next().unwrap_or("");
                let date = format!("@{time} +0000");
                let mut args = os(&["commit-tree", tree, "-m", label]);
                if let Some(p) = &parent {
                    args.extend(os(&["-p", p]));
                }
                let id = self.git_env(&args, &[("GIT_AUTHOR_DATE", &date), ("GIT_COMMITTER_DATE", &date)], TIMEOUT).await?;
                parent = Some(String::from_utf8_lossy(&id).trim().to_string());
            }
            let new = parent.ok_or("No checkpoint to keep was found.")?;
            self.git(&os(&["update-ref", &self.ref_name(), &new, &head])).await?;
        }
        self.git_env(&os(&["gc", "--quiet", "--prune=2.weeks.ago"]), &[], GC_TIMEOUT).await?;
        Ok(count - keep)
    }

    // --- the steps -------------------------------------------------------------------------------

    async fn snapshot_locked(&self, label: &str) -> Result<Snapshot, String> {
        let tree = match self.tree_now().await? {
            Ok(tree) => tree,
            Err(reason) => return Ok(Snapshot::Skipped { reason }),
        };
        let head = self.head().await?;
        if let Some(head) = &head
            && self.tree_of(head).await? == tree
        {
            return Ok(Snapshot::Saved { commit: head.clone(), created: false });
        }
        let mut args = os(&["commit-tree", &tree, "-m", &clean_label(label)]);
        if let Some(head) = &head {
            args.extend(os(&["-p", head]));
        }
        let commit = String::from_utf8_lossy(&self.git(&args).await?).trim().to_string();
        // the old value checked: another process moving the ref meanwhile fails this, not loses it
        self.git(&os(&["update-ref", &self.ref_name(), &commit, head.as_deref().unwrap_or("")])).await?;
        Ok(Snapshot::Saved { commit, created: true })
    }

    /// The files as they are now, as a tree in the store (`Err` inside: why it wasn't taken).
    async fn tree_now(&self) -> Result<Result<String, String>, String> {
        if let Some(reason) = self.refusal() {
            return Ok(Err(reason));
        }
        self.init().await?;
        if let Some(reason) = self.too_big().await? {
            return Ok(Err(reason));
        }
        match self.add_and_write().await {
            Ok(tree) => Ok(Ok(tree)),
            Err(_) => {
                // the index may point at objects `gc` has since dropped (a tree only `changed_since`
                // wrote, long ago): start it again from the files
                let _ = std::fs::remove_file(self.index());
                Ok(Ok(self.add_and_write().await?))
            }
        }
    }

    async fn add_and_write(&self) -> Result<String, String> {
        self.git(&os(&["add", "-A"])).await?;
        Ok(String::from_utf8_lossy(&self.git(&os(&["write-tree"])).await?).trim().to_string())
    }

    /// Why this folder is never checkpointed, if it isn't.
    fn refusal(&self) -> Option<String> {
        if !self.cwd.is_dir() {
            return Some(format!("{} is not a folder.", self.cwd.display()));
        }
        if self.cwd.parent().is_none() {
            return Some(format!("{} is the root of a drive: too much to copy.", self.cwd.display()));
        }
        if self.home.as_deref() == Some(self.cwd.as_path()) {
            return Some("This is the home folder: too much to copy, and not one project.".into());
        }
        if self.store.starts_with(&self.cwd) {
            return Some("The checkpoint store is inside this folder.".into());
        }
        None
    }

    /// Why the files are too many or too big to copy, if they are. Checked before anything is
    /// copied, with the same rules `add -A` follows: what the index has plus what isn't ignored.
    async fn too_big(&self) -> Result<Option<String>, String> {
        let out = self.git(&os(&["ls-files", "-z", "--cached", "--others", "--exclude-standard"])).await?;
        let mut seen = std::collections::HashSet::new();
        let mut bytes = 0u64;
        for p in out.split(|b| *b == 0).filter(|p| !p.is_empty()) {
            if !seen.insert(p) {
                continue;
            }
            if seen.len() > self.max_files {
                return Ok(Some(format!("More than {} files: too many to copy before every turn.", self.max_files)));
            }
            if let Some(rel) = rel_path(p)
                && let Ok(m) = std::fs::symlink_metadata(self.cwd.join(rel))
                && m.is_file()
            {
                bytes += m.len();
                if bytes > self.max_bytes {
                    return Ok(Some(format!("The files add up to more than {} MB: too much to copy before every turn.", self.max_bytes / (1024 * 1024))));
                }
            }
        }
        Ok(None)
    }

    /// Make the store if it isn't there yet, and keep its exclude list current.
    async fn init(&self) -> Result<(), String> {
        if !self.store.join("HEAD").exists() {
            std::fs::create_dir_all(&self.store).map_err(|e| format!("Couldn't make {}: {e}", self.store.display()))?;
            self.run(&os(&["init", "--bare", "--quiet"]), &[], TIMEOUT, false).await.and_then(ok)?;
        }
        let exclude = self.exclude_file();
        let want = EXCLUDE.join("\n") + "\n";
        if std::fs::read_to_string(&exclude).ok().as_deref() != Some(want.as_str()) {
            std::fs::write(&exclude, want).map_err(|e| format!("Couldn't write {}: {e}", exclude.display()))?;
        }
        Ok(())
    }

    /// `commit` checked and made a full id: hex only (never an option or a ref name), a commit, and
    /// one of this project's checkpoints (not another project's, nor one [`prune`] dropped).
    ///
    /// [`prune`]: Checkpoints::prune
    async fn resolve(&self, commit: &str) -> Result<String, String> {
        let commit = commit.trim();
        if !(4..=64).contains(&commit.len()) || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("{commit:?} is not a checkpoint id."));
        }
        if self.refusal().is_none() {
            self.init().await?;
        }
        let head = self.head().await?.ok_or("This folder has no checkpoints.")?;
        let out = self.run(&os(&["rev-parse", "--quiet", "--verify", &format!("{commit}^{{commit}}")]), &[], TIMEOUT, true).await?;
        let full = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.code != Some(0) || full.is_empty() {
            return Err(format!("No checkpoint {commit}."));
        }
        let mine = self.run(&os(&["merge-base", "--is-ancestor", &full, &head]), &[], TIMEOUT, true).await?;
        if mine.code != Some(0) {
            return Err(format!("{commit} is not one of this folder's checkpoints (or it was pruned)."));
        }
        Ok(full)
    }

    /// The latest checkpoint's commit, if there is one.
    async fn head(&self) -> Result<Option<String>, String> {
        if !self.store.join("HEAD").exists() {
            return Ok(None);
        }
        let out = self.run(&os(&["rev-parse", "--quiet", "--verify", &format!("{}^{{commit}}", self.ref_name())]), &[], TIMEOUT, true).await?;
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok((out.code == Some(0) && !id.is_empty()).then_some(id))
    }

    async fn tree_of(&self, commit: &str) -> Result<String, String> {
        Ok(String::from_utf8_lossy(&self.git(&os(&["rev-parse", &format!("{commit}^{{tree}}")])).await?).trim().to_string())
    }

    /// How `to` differs from `from` (commits or trees), file by file, sorted by path. Nested
    /// repositories (gitlinks) are left out: their files aren't in the checkpoint, and deleting
    /// one would delete a whole repository.
    async fn diff(&self, from: &str, to: &str) -> Result<Vec<RawChange>, String> {
        let out = self.git(&os(&["diff-tree", "-r", "--no-renames", "--raw", "-z", from, to])).await?;
        let mut parts = out.split(|b| *b == 0);
        let mut changes = Vec::new();
        while let Some(meta) = parts.next() {
            if meta.is_empty() {
                continue;
            }
            let path = parts.next().ok_or("git diff-tree's output ended early")?;
            // ":100644 100644 <old> <new> M"
            let meta = String::from_utf8_lossy(meta);
            let fields: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
            if fields.len() < 5 {
                return Err(format!("Unexpected git diff-tree output: {meta}"));
            }
            if fields[0] == "160000" || fields[1] == "160000" {
                continue;
            }
            let status = match fields[4].chars().next() {
                Some('A') => Status::Added,
                Some('D') => Status::Deleted,
                _ => Status::Modified,
            };
            changes.push(RawChange { path: path.to_vec(), status });
        }
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(changes)
    }

    /// Delete a file added since the checkpoint, and the folders that leaves empty. Only inside the
    /// project: the path is checked, and no folder on the way may be a link (it could lead out).
    fn remove_added(&self, path: &[u8]) -> Result<(), String> {
        let rel = safe_rel(path).ok_or_else(|| format!("Not restored: unsafe path {:?}.", String::from_utf8_lossy(path)))?;
        let full = self.cwd.join(&rel);
        let mut dir = self.cwd.clone();
        let parts: Vec<_> = rel.components().collect();
        for part in &parts[..parts.len() - 1] {
            dir.push(part);
            match std::fs::symlink_metadata(&dir) {
                Ok(m) if m.is_dir() => {}
                // gone already, or a link or a file now: nothing of ours to delete under it
                _ => return Ok(()),
            }
        }
        match std::fs::symlink_metadata(&full) {
            Ok(m) if m.is_dir() => return Ok(()),
            Ok(_) => {}
            Err(_) => return Ok(()),
        }
        std::fs::remove_file(&full).map_err(|e| format!("Couldn't delete {}: {e}", full.display()))?;
        let mut up = full.parent();
        while let Some(d) = up {
            if d == self.cwd || !d.starts_with(&self.cwd) || std::fs::remove_dir(d).is_err() {
                break;
            }
            up = d.parent();
        }
        Ok(())
    }

    // --- running git ------------------------------------------------------------------------------

    fn ref_name(&self) -> String {
        format!("refs/devcat/{}/head", self.key)
    }

    fn index(&self) -> PathBuf {
        self.store.join(format!("index-{}", self.key))
    }

    fn exclude_file(&self) -> PathBuf {
        self.store.join("devcat-exclude")
    }

    /// One lock per project in this process: two snapshots of one folder at once would fight over
    /// its index file. (Across processes git's own `index.lock` and the checked ref update keep
    /// it safe; the loser gets an error.)
    fn lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        static LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
        let mut map = LOCKS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
        map.entry(format!("{}\0{}", self.store.display(), self.key)).or_default().clone()
    }

    /// `git <args>` in the project; its stdout, or an error unless it exited with 0.
    async fn git(&self, args: &[OsString]) -> Result<Vec<u8>, String> {
        self.git_env(args, &[], TIMEOUT).await
    }

    async fn git_env(&self, args: &[OsString], env: &[(&str, &str)], limit: Duration) -> Result<Vec<u8>, String> {
        self.run(args, env, limit, true).await.and_then(ok)
    }

    /// Run git on the store with the settings described in the module docs. `work_tree`: with the
    /// project as the work tree and its own index (every call but `init`).
    async fn run(&self, args: &[OsString], env: &[(&str, &str)], limit: Duration, work_tree: bool) -> Result<Out, String> {
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let mut cmd = tokio::process::Command::new("git");
        for (k, v) in [
            ("core.hooksPath", OsString::from(null)),
            ("core.fsmonitor", "false".into()),
            ("core.autocrlf", "false".into()),
            ("core.safecrlf", "false".into()),
            ("core.symlinks", (!cfg!(windows)).to_string().into()),
            ("core.quotePath", "false".into()),
            ("core.excludesFile", self.exclude_file().into_os_string()),
            ("gc.auto", "0".into()),
            ("commit.gpgSign", "false".into()),
            ("advice.addEmbeddedRepo", "false".into()),
            ("user.name", "DevCat".into()),
            ("user.email", "devcat@localhost".into()),
        ] {
            let mut kv = OsString::from(k);
            kv.push("=");
            kv.push(v);
            cmd.arg("-c").arg(kv);
        }
        cmd.args(args);
        // whatever git the caller runs inside (a hook, a rebase) mustn't redirect this one
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(&k);
            }
        }
        cmd.env("GIT_DIR", &self.store)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null)
            .env("GIT_ATTR_SOURCE", EMPTY_TREE)
            .env("GIT_LITERAL_PATHSPECS", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_EDITOR", "devcat-checkpoint-no-editor");
        if work_tree {
            cmd.env("GIT_WORK_TREE", plain(&self.cwd)).env("GIT_INDEX_FILE", plain(&self.index())).current_dir(&self.cwd);
        } else {
            cmd.current_dir(&self.store);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        #[cfg(test)]
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        #[cfg(unix)]
        // its own process group, so a timeout stops everything it started
        unsafe {
            cmd.pre_exec(|| {
                crate::tools::libc_setsid();
                Ok(())
            });
        }
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let mut child = cmd.spawn().map_err(|e| format!("Couldn't start git (is it installed?): {e}"))?;
        let tree = crate::hooks::Tree::of(&child);
        let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let read = async {
            let (mut o, mut e) = (Vec::new(), Vec::new());
            let _ = tokio::join!(out.read_to_end(&mut o), err.read_to_end(&mut e));
            (o, e, child.wait().await)
        };
        let Ok((stdout, stderr, status)) = tokio::time::timeout(limit, read).await else {
            drop(tree);
            if work_tree {
                // the killed git can't remove its lock; left there, every later call would fail
                let _ = std::fs::remove_file(join_os(self.index().as_os_str(), ".lock"));
            }
            let what = args.first().map(|a| a.to_string_lossy().into_owned()).unwrap_or_default();
            return Err(format!("git {what} took more than {} s and was stopped.", limit.as_secs()));
        };
        tree.done();
        Ok(Out { code: status.ok().and_then(|s| s.code()), stdout, stderr: String::from_utf8_lossy(&stderr).trim().to_string() })
    }
}

fn ok(out: Out) -> Result<Vec<u8>, String> {
    match out.code {
        Some(0) => Ok(out.stdout),
        Some(c) => Err(if out.stderr.is_empty() { format!("git exited with {c}") } else { out.stderr }),
        None => Err("git was killed by a signal".into()),
    }
}

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn join_os(a: impl AsRef<OsStr>, b: impl AsRef<OsStr>) -> OsString {
    let mut s = a.as_ref().to_os_string();
    s.push(b);
    s
}

fn public(changes: Vec<RawChange>) -> Vec<Change> {
    changes.into_iter().map(|c| Change { path: String::from_utf8_lossy(&c.path).into_owned(), status: c.status }).collect()
}

/// A stable name for a project folder: FNV-1a (64 bit) of its path, as 16 hex digits. Stable
/// across runs and builds (unlike `DefaultHasher`), so the same folder finds its checkpoints again.
pub fn key_of(path: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.as_os_str().as_encoded_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// A label as a commit message: one line, at most [`MAX_LABEL`] characters, never empty.
fn clean_label(label: &str) -> String {
    let one: String = label.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let one = one.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut: String = one.chars().take(MAX_LABEL).collect();
    if cut.is_empty() { "checkpoint".into() } else { cut }
}

/// The canonical form of a path (links resolved). For a path that doesn't exist yet (a store not
/// made yet) the nearest folder that does is resolved and the rest added, so comparing it with a
/// canonical project folder still works.
fn canonical(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return plain(&c);
    }
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => canonical(parent).join(name),
        _ => p.to_path_buf(),
    }
}

/// Without Windows' `\\?\` prefix on an ordinary drive path: git doesn't take it everywhere.
fn plain(p: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\")
            && rest.as_bytes().get(1) == Some(&b':')
        {
            return PathBuf::from(rest);
        }
    }
    p.to_path_buf()
}

/// A path as git wrote it, as a relative path (no checks).
fn rel_path(p: &[u8]) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(PathBuf::from(OsStr::from_bytes(p)))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(p).ok().map(PathBuf::from)
    }
}

/// A path from git that is safe to write or delete under the project folder: relative, `/`
/// between plain names (no empty part, `.`, `..` or `.git`), so it can't lead outside the folder
/// or into a repository's own files.
fn safe_rel(p: &[u8]) -> Option<PathBuf> {
    if p.is_empty() || p.contains(&0) || p[0] == b'/' {
        return None;
    }
    for part in p.split(|b| *b == b'/') {
        if part.is_empty() || part == b"." || part == b".." || part.eq_ignore_ascii_case(b".git") {
            return None;
        }
        // Windows reads these as separators or a drive
        if cfg!(windows) && (part.contains(&b'\\') || part.contains(&b':')) {
            return None;
        }
    }
    let rel = rel_path(p)?;
    rel.components().all(|c| matches!(c, std::path::Component::Normal(_))).then_some(rel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git_available() -> bool {
        std::process::Command::new("git").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
    }

    /// A fresh folder with `store/` and `proj/` (or `name`) in it.
    fn temp(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("devcat-checkpoint-{}", uuid::Uuid::new_v4()));
        let proj = root.join(name);
        fs::create_dir_all(&proj).unwrap();
        (root.clone(), root.join("store"), proj)
    }

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn read(dir: &Path, rel: &str) -> Option<String> {
        fs::read_to_string(dir.join(rel)).ok()
    }

    /// The user's own git in `dir`, without their config.
    fn user_git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=T", "-c", "user.email=t@t", "-c", "commit.gpgSign=false"])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn change(path: &str, status: Status) -> Change {
        Change { path: path.into(), status }
    }

    async fn saved(cp: &Checkpoints, label: &str) -> (String, bool) {
        match cp.snapshot(label).await.unwrap() {
            Snapshot::Saved { commit, created } => (commit, created),
            Snapshot::Skipped { reason } => panic!("skipped: {reason}"),
        }
    }

    #[tokio::test]
    async fn snapshot_and_restore_round_trip() {
        if !git_available() {
            return;
        }
        let (root, store, proj) = temp("proj");
        write(&proj, "a.txt", "one\n");
        write(&proj, "src/lib/deep.rs", "fn deep() {}\n");
        write(&proj, "docs/x.md", "# x\n");
        let cp = Checkpoints::new(&store, &proj);
        let (first, created) = saved(&cp, "first").await;
        assert!(created);
        // nothing changed: the same checkpoint again
        assert_eq!(saved(&cp, "again").await, (first.clone(), false));
        assert!(cp.changed_since(&first).await.unwrap().is_empty());

        write(&proj, "a.txt", "two\n");
        fs::remove_file(proj.join("src/lib/deep.rs")).unwrap();
        write(&proj, "new/dir/n.txt", "new\n");
        write(&proj, "b.txt", "b\n");
        let want = vec![change("a.txt", Status::Modified), change("b.txt", Status::Added), change("new/dir/n.txt", Status::Added), change("src/lib/deep.rs", Status::Deleted)];
        assert_eq!(cp.changed_since(&first).await.unwrap(), want);
        // an abbreviated id works too
        assert_eq!(cp.changed_since(&first[..10]).await.unwrap(), want);

        assert_eq!(cp.restore(&first).await.unwrap(), want);
        assert_eq!(read(&proj, "a.txt").as_deref(), Some("one\n"));
        assert_eq!(read(&proj, "src/lib/deep.rs").as_deref(), Some("fn deep() {}\n"));
        assert_eq!(read(&proj, "docs/x.md").as_deref(), Some("# x\n"));
        assert!(!proj.join("b.txt").exists());
        // the folders the added file left empty are gone too
        assert!(!proj.join("new").exists());
        assert!(cp.changed_since(&first).await.unwrap().is_empty());

        let list = cp.list().await.unwrap();
        assert_eq!(list.iter().map(|e| e.label.as_str()).collect::<Vec<_>>(), [BEFORE_RESTORE, "first"]);
        assert_eq!(list[1].commit, first);
        assert!(list[0].time >= list[1].time && list[1].time > 1_600_000_000);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_restore_can_be_undone() {
        if !git_available() {
            return;
        }
        let (root, store, proj) = temp("proj");
        write(&proj, "keep.txt", "v1\n");
        let cp = Checkpoints::new(&store, &proj);
        let (first, _) = saved(&cp, "first").await;
        write(&proj, "keep.txt", "v2\n");
        write(&proj, "agent/made.txt", "agent\n");
        cp.restore(&first).await.unwrap();
        assert_eq!(read(&proj, "keep.txt").as_deref(), Some("v1\n"));

        let before = cp.list().await.unwrap()[0].clone();
        assert_eq!(before.label, BEFORE_RESTORE);
        let undone = cp.restore(&before.commit).await.unwrap();
        assert_eq!(undone, vec![change("agent/made.txt", Status::Deleted), change("keep.txt", Status::Modified)]);
        assert_eq!(read(&proj, "keep.txt").as_deref(), Some("v2\n"));
        assert_eq!(read(&proj, "agent/made.txt").as_deref(), Some("agent\n"));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn the_projects_own_git_repository_is_untouched() {
        if !git_available() {
            return;
        }
        let (root, store, proj) = temp("proj");
        user_git(&proj, &["init", "-q"]);
        write(&proj, "tracked.txt", "committed\n");
        user_git(&proj, &["add", "tracked.txt"]);
        user_git(&proj, &["commit", "-q", "-m", "init"]);
        write(&proj, "staged.txt", "staged\n");
        user_git(&proj, &["add", "staged.txt"]);
        let head = user_git(&proj, &["rev-parse", "HEAD"]);
        let index = fs::read(proj.join(".git/index")).unwrap();
        let status = user_git(&proj, &["status", "--porcelain"]);
        let refs = user_git(&proj, &["for-each-ref"]);

        let cp = Checkpoints::new(&store, &proj);
        let (first, _) = saved(&cp, "first").await;
        // nothing of .git went into the checkpoint
        let files = String::from_utf8(cp.git(&os(&["ls-tree", "-r", "--name-only", &first])).await.unwrap()).unwrap();
        assert_eq!(files.lines().collect::<Vec<_>>(), ["staged.txt", "tracked.txt"]);
        write(&proj, "tracked.txt", "changed\n");
        write(&proj, "extra.txt", "x\n");
        cp.restore(&first).await.unwrap();

        assert_eq!(user_git(&proj, &["rev-parse", "HEAD"]), head);
        assert_eq!(fs::read(proj.join(".git/index")).unwrap(), index);
        assert_eq!(user_git(&proj, &["status", "--porcelain"]), status);
        assert_eq!(user_git(&proj, &["for-each-ref"]), refs);
        assert!(user_git(&proj, &["stash", "list"]).is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn ignored_and_generated_files_are_never_copied_restored_or_deleted() {
        if !git_available() {
            return;
        }
        let (root, store, proj) = temp("proj");
        write(&proj, ".gitignore", "secret.env\ncache/\n");
        write(&proj, "secret.env", "TOKEN=1\n");
        write(&proj, "node_modules/pkg/index.js", "v1\n");
        write(&proj, "target/debug/app", "bin1\n");
        write(&proj, "app.log", "log1\n");
        write(&proj, "src/main.rs", "fn main() {}\n");
        let cp = Checkpoints::new(&store, &proj);
        let (first, _) = saved(&cp, "first").await;
        let files = String::from_utf8(cp.git(&os(&["ls-tree", "-r", "--name-only", &first])).await.unwrap()).unwrap();
        assert_eq!(files.lines().collect::<Vec<_>>(), [".gitignore", "src/main.rs"]);

        write(&proj, "secret.env", "TOKEN=2\n");
        write(&proj, "node_modules/pkg/index.js", "v2\n");
        write(&proj, "node_modules/new/index.js", "new\n");
        write(&proj, "cache/blob", "c\n");
        write(&proj, "debug.log", "d\n");
        write(&proj, "src/main.rs", "fn main() { changed() }\n");
        assert_eq!(cp.changed_since(&first).await.unwrap(), vec![change("src/main.rs", Status::Modified)]);
        cp.restore(&first).await.unwrap();
        assert_eq!(read(&proj, "src/main.rs").as_deref(), Some("fn main() {}\n"));
        assert_eq!(read(&proj, "secret.env").as_deref(), Some("TOKEN=2\n"));
        assert_eq!(read(&proj, "node_modules/pkg/index.js").as_deref(), Some("v2\n"));
        assert_eq!(read(&proj, "node_modules/new/index.js").as_deref(), Some("new\n"));
        assert_eq!(read(&proj, "cache/blob").as_deref(), Some("c\n"));
        assert_eq!(read(&proj, "debug.log").as_deref(), Some("d\n"));
        assert_eq!(read(&proj, "target/debug/app").as_deref(), Some("bin1\n"));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_nested_repository_added_later_is_not_deleted() {
        if !git_available() {
            return;
        }
        let (root, store, proj) = temp("proj");
        write(&proj, "a.txt", "a\n");
        let cp = Checkpoints::new(&store, &proj);
        let (first, _) = saved(&cp, "first").await;
        let inner = proj.join("vendor/inner");
        fs::create_dir_all(&inner).unwrap();
        write(&inner, "lib.txt", "lib\n");
        user_git(&inner, &["init", "-q"]);
        user_git(&inner, &["add", "."]);
        user_git(&inner, &["commit", "-q", "-m", "x"]);
        assert!(cp.changed_since(&first).await.unwrap().is_empty());
        cp.restore(&first).await.unwrap();
        assert_eq!(read(&inner, "lib.txt").as_deref(), Some("lib\n"));
        assert!(inner.join(".git").is_dir());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn the_home_folder_a_root_and_a_missing_folder_are_skipped() {
        let (root, store, proj) = temp("proj");
        let skipped = |s: Snapshot| matches!(s, Snapshot::Skipped { .. });
        if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).filter(|h| !h.is_empty()) {
            let cp = Checkpoints::new(&store, Path::new(&home));
            let s = cp.snapshot("x").await.unwrap();
            assert!(matches!(&s, Snapshot::Skipped { reason } if reason.contains("home")), "{s:?}");
        }
        // the home folder as this test sets it: nothing reaches git
        let mut cp = Checkpoints::new(&store, &proj);
        cp.home = Some(cp.cwd.clone());
        assert!(skipped(cp.snapshot("x").await.unwrap()));
        let root_dir = if cfg!(windows) { PathBuf::from(r"C:\") } else { PathBuf::from("/") };
        assert!(skipped(Checkpoints::new(&store, &root_dir).snapshot("x").await.unwrap()));
        assert!(skipped(Checkpoints::new(&store, &proj.join("missing")).snapshot("x").await.unwrap()));
        write(&proj, "file.txt", "x");
        assert!(skipped(Checkpoints::new(&store, &proj.join("file.txt")).snapshot("x").await.unwrap()));
        // a store inside the project would copy itself
        assert!(skipped(Checkpoints::new(&proj.join(".devcat/checkpoints"), &proj).snapshot("x").await.unwrap()));
        assert!(!store.exists(), "a skipped folder made the store");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn too_many_or_too_big_files_are_skipped_before_anything_is_copied() {
        if !git_available() {
            return;
        }
        let (root, store, proj) = temp("proj");
        for i in 0..10 {
            write(&proj, &format!("f{i}.txt"), "0123456789");
        }
        let mut cp = Checkpoints::new(&store, &proj);
        cp.max_files = 5;
        let s = cp.snapshot("x").await.unwrap();
        assert!(matches!(&s, Snapshot::Skipped { reason } if reason.contains("files")), "{s:?}");
        cp.max_files = 100;
        cp.max_bytes = 50;
        let s = cp.snapshot("x").await.unwrap();
        assert!(matches!(&s, Snapshot::Skipped { reason } if reason.contains("MB")), "{s:?}");
        // no file was copied into the store
        let objects = String::from_utf8(cp.git(&os(&["count-objects"])).await.unwrap()).unwrap();
        assert!(objects.starts_with("0 objects"), "{objects}");
        cp.max_bytes = 1_000;
        assert!(saved(&cp, "x").await.1);
        // restoring needs a snapshot first: too big now means not restored
        let first = cp.list().await.unwrap()[0].commit.clone();
        cp.max_bytes = 50;
        assert!(cp.restore(&first).await.is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn paths_with_spaces_and_non_ascii_letters_work() {
        if !git_available() {
            return;
        }
        let (root, store, proj) = temp("my proj ünï 日本");
        write(&proj, "sub dir/dätä file.txt", "før\n");
        write(&proj, "плейн.md", "a\n");
        let cp = Checkpoints::new(&store, &proj);
        let (first, _) = saved(&cp, "with \"quotes\" and ünï").await;
        write(&proj, "sub dir/dätä file.txt", "after\n");
        fs::remove_file(proj.join("плейн.md")).unwrap();
        write(&proj, "nëw dir/ñ.txt", "n\n");
        let changes = cp.changed_since(&first).await.unwrap();
        assert_eq!(changes, vec![change("nëw dir/ñ.txt", Status::Added), change("sub dir/dätä file.txt", Status::Modified), change("плейн.md", Status::Deleted)]);
        cp.restore(&first).await.unwrap();
        assert_eq!(read(&proj, "sub dir/dätä file.txt").as_deref(), Some("før\n"));
        assert_eq!(read(&proj, "плейн.md").as_deref(), Some("a\n"));
        assert!(!proj.join("nëw dir").exists());
        assert_eq!(cp.list().await.unwrap()[1].label, "with \"quotes\" and ünï");
        let _ = fs::remove_dir_all(root);
    }

    /// The user's global config (and the project's own) can name programs: filters, fsmonitor,
    /// hooks. A checkpoint must run none of them, and must store files byte for byte.
    #[tokio::test]
    async fn global_and_project_git_config_and_attributes_are_not_used() {
        if !git_available() {
            return;
        }
        let (root, store, proj) = temp("proj");
        let home = root.join("home");
        let marker = root.join("ran");
        let script = format!("touch '{}'; exit 1", marker.display());
        let config = format!("[filter \"x\"]\n\tclean = sh -c \"{script}\"\n\tsmudge = sh -c \"{script}\"\n\trequired = true\n[core]\n\tfsmonitor = sh -c \"{script}\"\n\tautocrlf = true\n");
        write(&home, ".gitconfig", &config);
        write(&home, ".config/git/config", &config);
        write(&proj, ".gitattributes", "* filter=x text eol=crlf\n");
        write(&proj, "crlf.txt", "a\r\nb\r\n");
        write(&proj, "lf.txt", "a\nb\n");

        // the same config makes the user's own git fail here: the test would notice it being read
        if cfg!(unix) {
            let other = root.join("other");
            fs::create_dir_all(&other).unwrap();
            write(&other, ".gitattributes", "* filter=x\n");
            write(&other, "f.txt", "f\n");
            let plain = |args: &[&str]| std::process::Command::new("git").args(args).current_dir(&other).env("HOME", &home).env("XDG_CONFIG_HOME", home.join(".config")).env_remove("GIT_CONFIG_GLOBAL").output().unwrap();
            assert!(plain(&["init", "-q"]).status.success());
            assert!(!plain(&["add", "f.txt"]).status.success(), "a failing global filter should break plain git");
            let _ = fs::remove_file(&marker);
        }

        let mut cp = Checkpoints::new(&store, &proj);
        cp.env = vec![("HOME".into(), home.clone().into()), ("XDG_CONFIG_HOME".into(), home.join(".config").into())];
        let (first, _) = saved(&cp, "first").await;
        fs::remove_file(proj.join("crlf.txt")).unwrap();
        write(&proj, "lf.txt", "changed\n");
        cp.restore(&first).await.unwrap();
        assert_eq!(fs::read(proj.join("crlf.txt")).unwrap(), b"a\r\nb\r\n");
        assert_eq!(fs::read(proj.join("lf.txt")).unwrap(), b"a\nb\n");
        assert!(!marker.exists(), "a program from the git config ran");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn prune_keeps_the_newest_checkpoints() {
        if !git_available() {
            return;
        }
        let (root, store, proj) = temp("proj");
        let cp = Checkpoints::new(&store, &proj);
        let mut ids = Vec::new();
        for i in 0..5 {
            write(&proj, "f.txt", &format!("v{i}\n"));
            ids.push(saved(&cp, &format!("turn {i}")).await.0);
        }
        let times: Vec<u64> = cp.list().await.unwrap().iter().map(|e| e.time).collect();
        assert_eq!(cp.prune(10).await.unwrap(), 0);
        assert_eq!(cp.prune(2).await.unwrap(), 3);
        let list = cp.list().await.unwrap();
        assert_eq!(list.iter().map(|e| e.label.as_str()).collect::<Vec<_>>(), ["turn 4", "turn 3"]);
        assert_eq!(list.iter().map(|e| e.time).collect::<Vec<_>>(), times[..2]);
        // the dropped ones can't be restored; the kept ones can
        assert!(cp.restore(&ids[0]).await.is_err());
        cp.restore(&list[1].commit).await.unwrap();
        assert_eq!(read(&proj, "f.txt").as_deref(), Some("v3\n"));
        // (the snapshot before that restore was the files as of "turn 4": nothing new to keep)
        assert_eq!(cp.prune(0).await.unwrap(), 2);
        assert!(cp.list().await.unwrap().is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn only_this_projects_checkpoints_are_accepted() {
        if !git_available() {
            return;
        }
        let (root, store, proj) = temp("a");
        let other = root.join("b");
        fs::create_dir_all(&other).unwrap();
        write(&proj, "a.txt", "a\n");
        write(&other, "b.txt", "b\n");
        let a = Checkpoints::new(&store, &proj);
        let b = Checkpoints::new(&store, &other);
        assert!(a.restore("abcdef").await.is_err(), "no checkpoints yet");
        let (ca, _) = saved(&a, "a").await;
        let (cb, _) = saved(&b, "b").await;
        assert_ne!(a.key, b.key);
        assert!(a.restore(&cb).await.is_err());
        assert!(b.changed_since(&ca).await.is_err());
        for bad in ["--help", "HEAD", "refs/devcat/x", "abc", "zzzzzz", "../../etc"] {
            assert!(a.restore(bad).await.is_err(), "{bad}");
        }
        assert_eq!(read(&other, "b.txt").as_deref(), Some("b\n"));
        assert!(!other.join("a.txt").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn keys_are_stable() {
        // FNV-1a 64's published test values
        assert_eq!(key_of(Path::new("")), "cbf29ce484222325");
        assert_eq!(key_of(Path::new("a")), "af63dc4c8601ec8c");
        assert_eq!(key_of(Path::new("/home/me/proj")), key_of(Path::new("/home/me/proj")));
        assert_ne!(key_of(Path::new("/home/me/proj")), key_of(Path::new("/home/me/proj2")));
    }

    #[test]
    fn labels_are_one_short_line() {
        assert_eq!(clean_label("fix\nthe\r\n bug\t now"), "fix the bug now");
        assert_eq!(clean_label("  "), "checkpoint");
        assert_eq!(clean_label(&"é".repeat(500)).chars().count(), MAX_LABEL);
    }

    #[test]
    fn only_plain_relative_paths_are_safe() {
        assert!(safe_rel(b"a/b/c.txt").is_some());
        assert!(safe_rel("dätä/ñ file.txt".as_bytes()).is_some());
        for bad in [&b""[..], b"/etc/passwd", b"../x", b"a/../../x", b"a//b", b"./a", b"a/.", b".git/config", b"sub/.GIT/hooks/x", b"a\0b"] {
            assert!(safe_rel(bad).is_none(), "{:?}", String::from_utf8_lossy(bad));
        }
    }
}
