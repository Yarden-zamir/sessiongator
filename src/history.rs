//! A journal of the operations sessiongator can undo: conversions (with the
//! note appended to the source, or the source moved to the trash) and
//! deletes. One JSON object per line in `<state dir>/history.jsonl`.

use std::{
    fs,
    path::{Path, PathBuf},
};

use gator::AppResult;
use serde::{Deserialize, Serialize};

use crate::model::Tool;
use crate::stores::{env_path, home, store_for, Footprint};
use crate::time::now_ms;
use crate::trash::{restore, trash, Trashed};

/// `$SESSIONGATOR_STATE_DIR`, else `$XDG_STATE_HOME/sessiongator`, else
/// `~/.local/state/sessiongator`.
pub fn state_dir() -> PathBuf {
    env_path("SESSIONGATOR_STATE_DIR").unwrap_or_else(|| {
        env_path("XDG_STATE_HOME")
            .unwrap_or_else(|| home().join(".local").join("state"))
            .join("sessiongator")
    })
}

/// A session and the store it lives in (`None`: the default store).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionRef {
    pub tool: Tool,
    pub id: String,
    pub store: Option<PathBuf>,
}

impl SessionRef {
    pub fn key(&self) -> String {
        format!("{}:{}", self.tool.name(), self.id)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Action {
    Convert {
        source: SessionRef,
        target: SessionRef,
        /// The migration note appended to the source session.
        note: Option<Footprint>,
        /// The source session, moved to the trash after the conversion.
        source_trash: Option<Trashed>,
    },
    Delete {
        session: SessionRef,
        trash: Trashed,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub time_ms: i64,
    #[serde(flatten)]
    pub action: Action,
    pub undone_ms: Option<i64>,
}

impl Operation {
    pub fn summary(&self) -> String {
        match &self.action {
            Action::Convert {
                source,
                target,
                note,
                source_trash,
            } => {
                let extra = if source_trash.is_some() {
                    " (source deleted)"
                } else if note.is_some() {
                    " (source marked)"
                } else {
                    ""
                };
                format!("convert {} -> {}{extra}", source.key(), target.key())
            }
            Action::Delete { session, .. } => format!("delete {}", session.key()),
        }
    }
}

pub struct History {
    path: PathBuf,
}

impl History {
    pub fn open() -> Self {
        Self::at(&state_dir())
    }

    pub fn at(dir: &Path) -> Self {
        Self {
            path: dir.join("history.jsonl"),
        }
    }

    pub fn dir(&self) -> &Path {
        self.path.parent().expect("the history file has a parent")
    }

    /// A trash directory for a new operation.
    pub fn trash_dir(&self, name: &str) -> PathBuf {
        self.dir()
            .join("trash")
            .join(format!("{}-{name}", now_ms()))
    }

    pub fn load(&self) -> AppResult<Vec<Operation>> {
        let content = match fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line)
                    .map_err(|error| format!("{}: {error}", self.path.display()).into())
            })
            .collect()
    }

    pub fn record(&self, action: Action) -> AppResult<Operation> {
        let mut operations = self.load()?;
        let time_ms = now_ms();
        let mut serial = operations.len() + 1;
        let id = loop {
            let id = format!("op{serial}");
            if operations.iter().all(|operation| operation.id != id) {
                break id;
            }
            serial += 1;
        };
        let operation = Operation {
            id,
            time_ms,
            action,
            undone_ms: None,
        };
        operations.push(operation.clone());
        self.save(&operations)?;
        Ok(operation)
    }

    /// The operation `id`, or the newest one that is not undone.
    pub fn find(&self, id: Option<&str>) -> AppResult<Operation> {
        let operations = self.load()?;
        let found = match id {
            Some(id) => operations.into_iter().find(|operation| operation.id == id),
            None => operations
                .into_iter()
                .rev()
                .find(|operation| operation.undone_ms.is_none()),
        };
        found.ok_or_else(|| match id {
            Some(id) => format!("no operation {id} in {}", self.path.display()).into(),
            None => "nothing to undo".into(),
        })
    }

    /// Revert `operation` and mark it undone. Returns one line per change.
    pub fn undo(&self, operation: &Operation, remove_target: bool) -> AppResult<Vec<String>> {
        if let Some(undone) = operation.undone_ms {
            return Err(format!(
                "{} was already undone at {}",
                operation.id,
                crate::time::format_utc(undone)
            )
            .into());
        }
        let mut done = Vec::new();
        match &operation.action {
            Action::Convert {
                source,
                target,
                note,
                source_trash,
            } => {
                if let Some(note) = note {
                    trash(note, &self.trash_dir(&format!("{}-note", operation.id)))?;
                    done.push(format!("removed the migration note from {}", source.key()));
                }
                if let Some(trashed) = source_trash {
                    restore(trashed)?;
                    done.push(format!("restored {}", source.key()));
                }
                if remove_target {
                    let store = store_for(target.tool, target.store.as_deref());
                    let footprint = store.footprint(&target.id)?;
                    let trashed = trash(
                        &footprint,
                        &self.trash_dir(&format!("{}-target", operation.id)),
                    )?;
                    done.push(format!(
                        "moved {} to {}",
                        target.key(),
                        trashed.dir.display()
                    ));
                } else {
                    done.push(format!(
                        "kept {}; pass --target to remove it too",
                        target.key()
                    ));
                }
            }
            Action::Delete { session, trash } => {
                restore(trash)?;
                done.push(format!("restored {}", session.key()));
            }
        }
        let mut operations = self.load()?;
        for kept in operations.iter_mut().filter(|kept| kept.id == operation.id) {
            kept.undone_ms = Some(now_ms());
        }
        self.save(&operations)?;
        Ok(done)
    }

    fn save(&self, operations: &[Operation]) -> AppResult<()> {
        fs::create_dir_all(self.dir())?;
        let content: String = operations
            .iter()
            .map(|operation| serde_json::to_string(operation).map(|line| line + "\n"))
            .collect::<Result<_, _>>()?;
        let tmp = self.path.with_extension("jsonl.tmp");
        fs::write(&tmp, content)?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stores::tests::temp_dir;
    use crate::stores::LineSet;

    #[test]
    fn records_finds_and_undoes_the_newest_open_operation() {
        let root = temp_dir("history");
        let history = History::at(&root);
        let log = root.join("source.jsonl");
        fs::write(&log, "first\nnote\n").unwrap();
        let reference = |tool, id: &str| SessionRef {
            tool,
            id: id.to_string(),
            store: None,
        };
        let first = history
            .record(Action::Convert {
                source: reference(Tool::Claude, "a"),
                target: reference(Tool::Codex, "b"),
                note: Some(Footprint {
                    lines: vec![LineSet {
                        file: log.clone(),
                        lines: vec!["note".to_string()],
                    }],
                    ..Footprint::default()
                }),
                source_trash: None,
            })
            .unwrap();
        assert_eq!(first.id, "op1");
        assert_eq!(history.find(None).unwrap().id, "op1");

        let done = history.undo(&first, false).unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "first\n");
        assert!(done.iter().any(|line| line.contains("pass --target")));
        assert!(history.find(None).is_err());
        assert!(history
            .undo(&history.find(Some("op1")).unwrap(), false)
            .is_err());
    }
}
