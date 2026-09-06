//! Shadow-ref snapshots: one commit per pre-write image on `refs/interlock/history`,
//! written with gix in-process. Never touches index, HEAD, branches, or stash.

use anyhow::{anyhow, Context, Result};
use gix::bstr::BString;
use gix::ObjectId;
use std::collections::HashMap;
use std::path::Path;

pub const HISTORY_REF: &str = "refs/interlock/history";
const ENTRY_NAME: &str = "content";

pub struct Snapshotter {
    repo: gix::Repository,
    /// Latest snapshotted blob per repo key, to dedupe unchanged content.
    latest: HashMap<String, ObjectId>,
}

impl Snapshotter {
    pub fn open(repo_root: &Path) -> Result<Snapshotter> {
        let repo = gix::open(repo_root).context("open repo with gix")?;
        let mut s = Snapshotter { repo, latest: HashMap::new() };
        s.warm_latest(2000);
        Ok(s)
    }

    fn head(&self) -> Option<ObjectId> {
        let r = self.repo.try_find_reference(HISTORY_REF).ok()??;
        r.target().try_id().map(|id| id.to_owned())
    }

    fn warm_latest(&mut self, limit: usize) {
        let mut cur = self.head();
        let mut n = 0;
        while let Some(id) = cur {
            if n >= limit {
                break;
            }
            n += 1;
            let Ok(commit) = self.repo.find_commit(id) else { break };
            let msg = commit.message_raw_sloppy().to_string();
            let key = key_from_message(&msg);
            if let (Some(key), Ok(tree)) = (key, commit.tree()) {
                if let Some(entry) = tree.find_entry(ENTRY_NAME) {
                    self.latest.entry(key).or_insert_with(|| entry.oid().to_owned());
                }
            }
            cur = commit.parent_ids().next().map(|p| p.detach());
        }
    }

    /// Snapshot the current content of `abs` under `key`. Returns the blob oid, or `None` if
    /// the file does not exist or the latest snapshot already holds identical content.
    pub fn snapshot(&mut self, key: &str, abs: &Path) -> Result<Option<String>> {
        let Ok(data) = std::fs::read(abs) else { return Ok(None) };
        let blob = self.repo.write_blob(&data).context("write blob")?.detach();
        if self.latest.get(key) == Some(&blob) {
            return Ok(None);
        }
        let tree = gix::objs::Tree {
            entries: vec![gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryKind::Blob.into(),
                filename: BString::from(ENTRY_NAME),
                oid: blob,
            }],
        };
        let tree_id = self.repo.write_object(&tree).context("write tree")?.detach();
        let sig = gix::actor::Signature {
            name: BString::from("interlock"),
            email: BString::from("interlock@localhost"),
            time: gix::date::Time::now_utc(),
        };
        let parents: Vec<ObjectId> = self.head().into_iter().collect();
        let commit = gix::objs::Commit {
            tree: tree_id,
            parents: parents.into_iter().collect(),
            author: sig.clone(),
            committer: sig,
            encoding: None,
            message: BString::from(format!("snapshot {key}\n")),
            extra_headers: vec![],
        };
        let commit_id = self.repo.write_object(&commit).context("write commit")?.detach();
        self.repo
            .reference(HISTORY_REF, commit_id, gix::refs::transaction::PreviousValue::Any, "interlock snapshot")
            .context("update history ref")?;
        self.latest.insert(key.to_string(), blob);
        Ok(Some(blob.to_hex().to_string()))
    }

    /// Restore `key` to its `steps`-th most recent snapshot (1 = image before the last write).
    pub fn restore(&mut self, key: &str, abs: &Path, steps: usize) -> Result<String> {
        let steps = steps.max(1);
        let found = self.find_snapshot(key, steps)?;
        let Some((oid, data)) = found else {
            return Err(anyhow!("no snapshot #{steps} for {key}"));
        };
        // Snapshot the current content first so undo is itself undoable.
        let _ = self.snapshot(key, abs);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(abs, &data)?;
        self.latest.insert(key.to_string(), oid);
        Ok(oid.to_hex().to_string())
    }

    /// Walk the history ref for the `steps`-th snapshot of `key`, returning its blob oid and content.
    fn find_snapshot(&self, key: &str, steps: usize) -> Result<Option<(ObjectId, Vec<u8>)>> {
        let mut cur = self.head();
        let mut seen = 0;
        while let Some(id) = cur {
            let commit = self.repo.find_commit(id).context("walk history")?;
            let msg = commit.message_raw_sloppy().to_string();
            if key_from_message(&msg).as_deref() == Some(key) {
                seen += 1;
                if seen == steps {
                    let tree = commit.tree()?;
                    let entry = tree.find_entry(ENTRY_NAME).ok_or_else(|| anyhow!("snapshot tree missing content"))?;
                    let oid = entry.oid().to_owned();
                    let blob = self.repo.find_blob(oid)?;
                    return Ok(Some((oid, blob.data.clone())));
                }
            }
            cur = commit.parent_ids().next().map(|p| p.detach());
        }
        Ok(None)
    }
}

fn key_from_message(msg: &str) -> Option<String> {
    msg.lines().next()?.strip_prefix("snapshot ").map(|s| s.trim().to_string())
}
