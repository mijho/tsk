//! The authority: the one `DomainState` every client's ops land on, in arrival order. It
//! assigns each accepted op a `seq` and numbers what it creates, and keeps every result so a
//! resent op gets the same answer.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::db::DbError;
use crate::domain::{DomainState, Intent, Op};

/// What became of one submitted op.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status", deny_unknown_fields)]
pub enum OpResult {
    /// Applied at `seq`; `numbers` are the tasks it created, by id.
    Accepted {
        op_id: Uuid,
        seq: i64,
        numbers: BTreeMap<Uuid, u64>,
    },
    /// Not applied, and never will be: resend a new op instead.
    Rejected { op_id: Uuid, reason: String },
}

/// The authoritative state and the database it lives in.
pub struct Authority {
    db: Connection,
    state: DomainState,
    seq: i64,
}

impl Authority {
    /// Load the projection from a migrated database; an empty one starts from nothing.
    pub fn load(db: Connection) -> Result<Self, DbError> {
        let row: Option<(i64, String)> = db
            .query_row(
                "SELECT seq, state FROM projection WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (seq, state) = match row {
            Some((seq, state)) => (seq, serde_json::from_str(&state).map_err(DbError::Json)?),
            None => (0, DomainState::new()),
        };
        Ok(Self { db, state, seq })
    }

    /// The last accepted op's `seq`, 0 before any.
    pub fn seq(&self) -> i64 {
        self.seq
    }

    pub fn state(&self) -> &DomainState {
        &self.state
    }

    /// How many ops the log holds.
    pub fn op_count(&self) -> Result<i64, DbError> {
        Ok(self
            .db
            .query_row("SELECT count(*) FROM ops", [], |row| row.get(0))?)
    }

    /// Apply `ops` in order as one durable batch. Each op is accepted or refused on its own;
    /// a resent op (same id, actor, and body) gets its stored result, and an op id reused
    /// for anything else is refused. Nothing is kept unless the whole batch commits.
    pub fn submit(&mut self, ops: &[Op]) -> Result<Vec<OpResult>, DbError> {
        let mut state = self.state.clone();
        let mut seq = self.seq;
        let transaction = self.db.transaction()?;
        let mut results = Vec::with_capacity(ops.len());
        for op in ops {
            let body = serde_json::to_string(op).map_err(DbError::Json)?;
            if let Some(result) = stored(&transaction, op)? {
                results.push(result);
                continue;
            }
            let mut staged = state.clone();
            let applied = match server_refusal(op) {
                Some(reason) => Err(reason.to_string()),
                None => staged.apply(op).map_err(|error| error.to_string()),
            };
            let result = match applied {
                Ok(_) => {
                    let before = numbered(&state);
                    staged.assign_numbers_for_persistence();
                    // Release planning and launches are the server's later work (T20, T22).
                    staged.take_completed();
                    staged.take_pending_launches();
                    staged.clear_merge_bases();
                    let numbers: BTreeMap<Uuid, u64> = staged
                        .tasks()
                        .iter()
                        .filter(|task| !before.contains(&task.id))
                        .filter_map(|task| Some((task.id, task.number?)))
                        .collect();
                    transaction.execute(
                        "INSERT INTO ops (op_id, actor, at, body, numbers)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            op.id.to_string(),
                            op.actor,
                            serde_json::to_string(&op.at).map_err(DbError::Json)?,
                            body,
                            serde_json::to_string(&numbers).map_err(DbError::Json)?,
                        ],
                    )?;
                    seq = transaction.last_insert_rowid();
                    state = staged;
                    OpResult::Accepted {
                        op_id: op.id,
                        seq,
                        numbers,
                    }
                }
                Err(reason) => {
                    transaction.execute(
                        "INSERT INTO rejections (op_id, actor, body, reason)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![op.id.to_string(), op.actor, body, reason],
                    )?;
                    OpResult::Rejected {
                        op_id: op.id,
                        reason,
                    }
                }
            };
            results.push(result);
        }
        state.prune_undo_for_persistence();
        transaction.execute(
            "INSERT INTO projection (id, seq, state) VALUES (1, ?1, ?2)
             ON CONFLICT (id) DO UPDATE SET seq = excluded.seq, state = excluded.state",
            params![seq, serde_json::to_string(&state).map_err(DbError::Json)?],
        )?;
        transaction.commit()?;
        self.state = state;
        self.seq = seq;
        Ok(results)
    }
}

/// Ops the server does not take yet, and why: their server rules come with later tasks.
fn server_refusal(op: &Op) -> Option<&'static str> {
    match op.intent {
        Intent::Undo => {
            Some("not-on-server: undo is per actor on the server and lands with its own task (T21)")
        }
        Intent::Dispatch { .. } => Some(
            "not-on-server: dispatch records are host-tagged and land with their own task (T22)",
        ),
        _ => None,
    }
}

/// The tasks that already have a number.
fn numbered(state: &DomainState) -> std::collections::BTreeSet<Uuid> {
    state
        .tasks()
        .iter()
        .filter(|task| task.number.is_some())
        .map(|task| task.id)
        .collect()
}

/// Whether a stored body is `op`: compared parsed, so a change in how ops serialise never
/// turns a resend into a reuse.
fn same_op(stored: &str, op: &Op) -> Result<bool, DbError> {
    let stored: Op = serde_json::from_str(stored).map_err(DbError::Json)?;
    Ok(stored == *op)
}

/// The result an op id already has: its own when actor and body match, else a refusal.
fn stored(db: &Connection, op: &Op) -> Result<Option<OpResult>, DbError> {
    let id = op.id.to_string();
    let accepted: Option<(i64, String, String, String)> = db
        .query_row(
            "SELECT seq, actor, body, numbers FROM ops WHERE op_id = ?1",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let rejected: Option<(String, String, String)> = db
        .query_row(
            "SELECT actor, body, reason FROM rejections WHERE op_id = ?1",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let reused = || OpResult::Rejected {
        op_id: op.id,
        reason: "op-id-reused: this op id names a different op".into(),
    };
    Ok(match (accepted, rejected) {
        (Some((seq, actor, stored_body, numbers)), _) => {
            Some(if actor == op.actor && same_op(&stored_body, op)? {
                OpResult::Accepted {
                    op_id: op.id,
                    seq,
                    numbers: serde_json::from_str(&numbers).map_err(DbError::Json)?,
                }
            } else {
                reused()
            })
        }
        (None, Some((actor, stored_body, reason))) => {
            Some(if actor == op.actor && same_op(&stored_body, op)? {
                OpResult::Rejected {
                    op_id: op.id,
                    reason,
                }
            } else {
                reused()
            })
        }
        (None, None) => None,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::domain::{HumanStatus, Intent, PrerequisitePolicy, ProvenanceOrigin, TaskScope};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// A migrated database file that is removed on drop.
    struct TempDb(std::path::PathBuf);

    impl TempDb {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!("tsk-sync-authority-{nanos}-{seq}.db")))
        }

        fn authority(&self) -> Authority {
            Authority::load(super::super::db::open(&self.0).unwrap()).unwrap()
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
            }
        }
    }

    fn create(task: Uuid, title: &str) -> Op {
        Op::local(
            "you",
            Intent::Create {
                task,
                title: title.into(),
                notes: None,
                scope: TaskScope::Global,
                provenance: ProvenanceOrigin::Manual,
                thread: None,
                assignee: None,
                base: None,
                due: None,
                after: Vec::new(),
                steps: Vec::new(),
            },
        )
    }

    fn start(task: Uuid) -> Op {
        Op::local(
            "you",
            Intent::SetStatus {
                task,
                from: HumanStatus::Open,
                to: HumanStatus::Started,
                prerequisite_policy: PrerequisitePolicy::Check,
                undo: false,
            },
        )
    }

    #[test]
    fn accepted_ops_get_a_seq_and_numbers_and_survive_a_reload() {
        let db = TempDb::new();
        let mut authority = db.authority();
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
        let ops = [
            create(first, "first"),
            create(second, "second"),
            start(first),
        ];
        let results = authority.submit(&ops).unwrap();
        assert_eq!(
            results,
            [
                OpResult::Accepted {
                    op_id: ops[0].id,
                    seq: 1,
                    numbers: BTreeMap::from([(first, 1)]),
                },
                OpResult::Accepted {
                    op_id: ops[1].id,
                    seq: 2,
                    numbers: BTreeMap::from([(second, 2)]),
                },
                OpResult::Accepted {
                    op_id: ops[2].id,
                    seq: 3,
                    numbers: BTreeMap::new(),
                },
            ]
        );
        let state = serde_json::to_value(authority.state()).unwrap();
        drop(authority);
        let reloaded = db.authority();
        assert_eq!(reloaded.seq(), 3);
        assert_eq!(serde_json::to_value(reloaded.state()).unwrap(), state);
        assert_eq!(reloaded.op_count().unwrap(), 3);
        let started = reloaded.state().get(first).unwrap();
        assert_eq!(started.status, HumanStatus::Started);
    }

    #[test]
    fn a_resent_op_gets_its_original_result_and_a_reused_id_refuses() {
        let db = TempDb::new();
        let mut authority = db.authority();
        let task = Uuid::new_v4();
        let op = create(task, "once");
        let first = authority.submit(std::slice::from_ref(&op)).unwrap();
        let again = authority.submit(&[op.clone(), op.clone()]).unwrap();
        assert_eq!(again, [first[0].clone(), first[0].clone()]);
        assert_eq!(authority.op_count().unwrap(), 1, "applied once");
        assert_eq!(authority.state().tasks().len(), 1);

        let mut reused = create(Uuid::new_v4(), "something else");
        reused.id = op.id;
        let mut other_actor = op.clone();
        other_actor.actor = "pi".into();
        for op in [reused, other_actor] {
            assert!(matches!(
                authority.submit(std::slice::from_ref(&op)).unwrap().as_slice(),
                [OpResult::Rejected { reason, .. }] if reason.starts_with("op-id-reused")
            ));
        }
        assert_eq!(authority.op_count().unwrap(), 1);
    }

    #[test]
    fn a_refused_op_keeps_its_reason_and_changes_nothing() {
        let db = TempDb::new();
        let mut authority = db.authority();
        let missing = start(Uuid::new_v4());
        let before = serde_json::to_value(authority.state()).unwrap();
        let refused = authority.submit(std::slice::from_ref(&missing)).unwrap();
        let [OpResult::Rejected { reason, .. }] = refused.as_slice() else {
            panic!("{refused:?}");
        };
        assert!(reason.contains("unknown task id"), "{reason}");
        assert_eq!(authority.seq(), 0);
        assert_eq!(serde_json::to_value(authority.state()).unwrap(), before);
        drop(authority);
        let mut reloaded = db.authority();
        assert_eq!(reloaded.submit(&[missing]).unwrap(), refused);
    }

    /// A batch commits whole or not at all: a database failure on its second op keeps
    /// neither the first op nor any change to the state, the seq, or the numbering.
    #[test]
    fn a_batch_that_fails_midway_keeps_nothing() {
        let db = TempDb::new();
        let mut authority = db.authority();
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
        let ops = [create(first, "first"), create(second, "second")];
        authority
            .db
            .execute_batch(&format!(
                "CREATE TRIGGER fail_second BEFORE INSERT ON ops
                 WHEN NEW.op_id = '{}' BEGIN SELECT RAISE(ABORT, 'injected'); END;",
                ops[1].id
            ))
            .unwrap();
        let before = serde_json::to_value(authority.state()).unwrap();
        assert!(authority.submit(&ops).is_err());
        assert_eq!(authority.seq(), 0);
        assert_eq!(authority.op_count().unwrap(), 0);
        assert_eq!(serde_json::to_value(authority.state()).unwrap(), before);
        authority
            .db
            .execute_batch("DROP TRIGGER fail_second")
            .unwrap();
        drop(authority);
        let mut reloaded = db.authority();
        assert_eq!(reloaded.seq(), 0, "no projection was written");
        let results = reloaded.submit(&ops).unwrap();
        assert_eq!(
            results[0],
            OpResult::Accepted {
                op_id: ops[0].id,
                seq: 1,
                numbers: BTreeMap::from([(first, 1)]),
            }
        );
    }

    /// Undo and dispatch need their own server rules first: they are refused, never logged.
    #[test]
    fn undo_and_dispatch_are_refused_until_their_server_rules_land() {
        let db = TempDb::new();
        let mut authority = db.authority();
        let undo = Op::local("you", Intent::Undo);
        let results = authority.submit(std::slice::from_ref(&undo)).unwrap();
        assert!(matches!(
            results.as_slice(),
            [OpResult::Rejected { reason, .. }] if reason.starts_with("not-on-server")
        ));
        assert_eq!(authority.op_count().unwrap(), 0);
    }
}
