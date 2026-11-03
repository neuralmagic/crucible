//! The declared files a run's tasks captured, under `state/files/<task>/<declared>`. A task's
//! set is published whole or not at all: a directory exists only for a task that delivered
//! every file it declared, so a partial capture never reaches a descendant.

use std::path::{Path, PathBuf};

use crate::plan::ir::{Task, TaskName};

pub struct CapturedFiles {
    root: PathBuf,
}

impl CapturedFiles {
    pub fn new(state: &Path) -> Self {
        CapturedFiles {
            root: state.join("files"),
        }
    }

    /// The directory every set is published under.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn dir(&self, task: &TaskName) -> PathBuf {
        self.root.join(task.as_str())
    }

    pub fn path(&self, task: &TaskName, declared: &str) -> PathBuf {
        self.dir(task).join(declared)
    }

    /// Whether a complete set captured in this run is there to stage for `task`'s dependents.
    pub fn has(&self, task: &TaskName) -> bool {
        self.dir(task).is_dir()
    }

    /// The bytes of one declared file captured under `producer`'s name, if any.
    pub fn read(&self, producer: &TaskName, declared: &str) -> Option<Vec<u8>> {
        std::fs::read(self.path(producer, declared)).ok()
    }

    /// Discard the set published under `task`'s name, and for a mapped node the sets published
    /// under its instances' names too: a node that did not expand this run has no instance
    /// rows, so nothing else in the run ever reaches them. Called when a task settles without
    /// producing evidence, so a set from an earlier run cannot outlive its producer's silence.
    pub fn drop(&self, task: &Task) {
        let _ = std::fs::remove_dir_all(self.dir(&task.name));
        if task.over.is_none() {
            return;
        }
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if TaskName(name.to_string()).is_instance_of(&task.name) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::plan::files::CapturedFiles;
    use crate::plan::ir::{ItemKey, Join, Stage, Task, TaskKind, TaskName};

    fn mapped(name: &str) -> Task {
        Task {
            name: name.into(),
            task: TaskKind::Command {
                command: "true".into(),
            },
            depends_on: Vec::new(),
            session: None,
            needs: "any".into(),
            required: true,
            isolation: None,
            join: Join::default(),
            stage: Stage::Iteration,
            emits: crate::plan::ir::Emits::default(),
            emits_files: Vec::new(),
            over: Some(crate::plan::ir::OutputRef {
                task: "discover".into(),
                field: crate::plan::ir::OutputField("items".into()),
            }),
            max_fanout: Some(4),
            keyed: Vec::new(),
            when: None,
            revise: None,
            timeout: None,
            history: None,
            repair: 0,
            optional: Vec::new(),
        }
    }

    #[test]
    fn a_set_is_seen_once_published_and_dropped_with_its_instances() {
        let state = tempfile::tempdir().unwrap();
        let files = CapturedFiles::new(state.path());
        let audit = mapped("audit");
        let alpha = TaskName::instance(&audit.name, &ItemKey::new("alpha").unwrap());
        let other: TaskName = "report".into();
        assert!(!files.has(&audit.name));
        assert_eq!(files.read(&alpha, "out.json"), None);
        for name in [&audit.name, &alpha, &other] {
            std::fs::create_dir_all(files.dir(name)).unwrap();
        }
        std::fs::write(files.path(&alpha, "out.json"), b"{}").unwrap();
        assert!(files.has(&audit.name));
        assert!(files.has(&alpha));
        assert_eq!(files.read(&alpha, "out.json"), Some(b"{}".to_vec()));
        files.drop(&audit);
        assert!(!files.has(&audit.name), "the node's own set is dropped");
        assert!(!files.has(&alpha), "an instance's set goes with its node");
        assert!(files.has(&other), "another task's set stays");
    }
}
