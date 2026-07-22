//! Workspace checkpoints — snapshot files before mutation so edits can be rewound.
//!
//! Before a write tool changes a file, it records the file's prior content as a checkpoint.
//! Rewinding restores files to a captured state (removing files that did not exist then) and
//! drops the undone checkpoints. This is the safety net behind one-key undo of agent edits.

use std::path::PathBuf;

/// One recorded snapshot: for each touched path, its content before the change (`None` = the
/// file did not exist yet).
pub struct Checkpoint {
    pub id: usize,
    pub label: String,
    files: Vec<(PathBuf, Option<String>)>,
}

/// An ordered stack of checkpoints.
#[derive(Default)]
pub struct CheckpointStore {
    checkpoints: Vec<Checkpoint>,
    next_id: usize,
}

impl CheckpointStore {
    pub fn new() -> Self {
        CheckpointStore::default()
    }

    /// Capture the current content of `paths` as a new checkpoint (taken *before* modifying them).
    pub fn snapshot(&mut self, label: impl Into<String>, paths: &[PathBuf]) -> usize {
        let files = paths
            .iter()
            .map(|p| (p.clone(), std::fs::read_to_string(p).ok()))
            .collect();
        let id = self.next_id;
        self.next_id += 1;
        self.checkpoints.push(Checkpoint {
            id,
            label: label.into(),
            files,
        });
        id
    }

    /// Restore files to the state captured at checkpoint `id`, then drop it and every later
    /// checkpoint. Returns the number of files restored.
    pub fn rewind(&mut self, id: usize) -> std::io::Result<usize> {
        let Some(pos) = self.checkpoints.iter().position(|c| c.id == id) else {
            return Ok(0);
        };
        // Apply captured contents from newest back to `pos` so the oldest capture wins.
        let mut restored = 0;
        for cp in self.checkpoints[pos..].iter().rev() {
            for (path, content) in &cp.files {
                match content {
                    Some(c) => std::fs::write(path, c)?,
                    None => {
                        let _ = std::fs::remove_file(path);
                    }
                }
                restored += 1;
            }
        }
        self.checkpoints.truncate(pos);
        Ok(restored)
    }

    /// Rewind the most recent `n` checkpoints (n >= 1). Returns files restored.
    pub fn rewind_last(&mut self, n: usize) -> std::io::Result<usize> {
        if self.checkpoints.is_empty() || n == 0 {
            return Ok(0);
        }
        let n = n.min(self.checkpoints.len());
        let id = self.checkpoints[self.checkpoints.len() - n].id;
        self.rewind(id)
    }

    pub fn len(&self) -> usize {
        self.checkpoints.len()
    }

    pub fn is_empty(&self) -> bool {
        self.checkpoints.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewind_restores_prior_content() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "v1").unwrap();

        let mut store = CheckpointStore::new();
        let id0 = store.snapshot("edit1", std::slice::from_ref(&file));
        std::fs::write(&file, "v2").unwrap();
        store.snapshot("edit2", std::slice::from_ref(&file));
        std::fs::write(&file, "v3").unwrap();

        // Undo last edit -> v2.
        store.rewind_last(1).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v2");

        // Rewind all the way to the first snapshot -> v1.
        store.rewind(id0).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1");
        assert!(store.is_empty());
    }

    #[test]
    fn rewind_removes_files_that_did_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("new.txt");

        let mut store = CheckpointStore::new();
        store.snapshot("create", std::slice::from_ref(&file)); // file absent -> None captured
        std::fs::write(&file, "created").unwrap();
        assert!(file.exists());

        store.rewind_last(1).unwrap();
        assert!(
            !file.exists(),
            "rewind should remove a file that did not exist at snapshot time"
        );
    }
}
