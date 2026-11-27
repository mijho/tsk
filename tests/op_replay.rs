//! The phase 0 gate: every mutation is an op, so replaying the ops a session applied, in
//! order, on an empty state gives the state that session saved.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use tsk_tui::cli::run_with;
use tsk_tui::domain::{record_ops, DomainState, Op};
use tsk_tui::store::TaskStore;
use tsk_tui::ui::board::{apply_intent, BoardModel, IntentOutcome};
use tsk_tui::ui::input::BoardIntent;

static SEQ: AtomicU64 = AtomicU64::new(0);

struct Temp(PathBuf);

impl Temp {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("tsk-op-replay-{label}-{nanos}-{seq}"));
        std::fs::create_dir_all(&dir).expect("dir");
        Self(dir)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run one CLI verb against `state`; it must succeed.
fn cli(state: &Path, args: &[&str]) {
    let mut argv: Vec<String> = ["tsk"].iter().chain(args).map(|a| a.to_string()).collect();
    argv.push("--state-dir".into());
    argv.push(state.to_string_lossy().into_owned());
    let output = run_with(argv.clone(), Cursor::new(Vec::<u8>::new()), true);
    assert_eq!(output.code, 0, "{argv:?}: {}", output.stderr);
}

/// The short id of `task`'s step at `index`.
fn step(store: &TaskStore, number: u64, index: usize) -> String {
    let state = store.load().expect("load");
    let task = state
        .tasks()
        .iter()
        .find(|task| task.number == Some(number))
        .expect("task");
    let id = task.steps[index].id.to_string();
    // A prefix only that step has.
    (1..=id.len())
        .map(|len| id[..len].to_string())
        .find(|prefix| {
            task.steps
                .iter()
                .filter(|step| step.id.to_string().starts_with(prefix.as_str()))
                .count()
                == 1
        })
        .expect("unique prefix")
}

/// Drive board intents on the saved store, saving each one that persists.
fn board(store: &TaskStore, script: impl FnOnce(&mut dyn FnMut(Option<u64>, BoardIntent))) {
    let mut domain = store.load().expect("load");
    let mut model = BoardModel::from_domain(&domain, None);
    let mut run = |target: Option<u64>, intent: BoardIntent| {
        if let Some(number) = target {
            let id = domain
                .tasks()
                .iter()
                .find(|task| task.number == Some(number))
                .expect("task")
                .id;
            let visible = model.visible_ids();
            let at = visible.iter().position(|v| *v == id).unwrap_or_else(|| {
                let shown: Vec<_> = visible
                    .iter()
                    .filter_map(|v| domain.get(*v).map(|task| (task.number, task.status)))
                    .collect();
                panic!("T{number} is not on the board: {shown:?}")
            });
            for _ in 0..visible.len() {
                let now = model
                    .selected_id()
                    .and_then(|selected| visible.iter().position(|v| *v == selected));
                let step = match now {
                    Some(now) if now == at => break,
                    Some(now) if now > at => BoardIntent::SelectPrev,
                    _ => BoardIntent::SelectNext,
                };
                apply_intent(&mut domain, &mut model, step, None).expect("move");
            }
        }
        let outcome = apply_intent(&mut domain, &mut model, intent, None).expect("intent");
        if outcome == IntentOutcome::Persist {
            store.reload_merge_save(&mut domain).expect("save");
            model.sync_from_domain(&domain);
        }
    };
    script(&mut run);
}

/// Replay `ops` on an empty state, then save and load it through a store of its own, as the
/// session's state was.
fn replay(ops: &[Op]) -> DomainState {
    let mut state = DomainState::new();
    for op in ops {
        let wire: Op =
            serde_json::from_str(&serde_json::to_string(op).expect("encode")).expect("decode");
        state
            .apply(&wire)
            .unwrap_or_else(|error| panic!("{error}: {wire:?}"));
        // Each verb saved, and the save numbered what it created.
        state.assign_numbers_for_persistence();
    }
    let dir = Temp::new("replayed");
    let store = TaskStore::new(&dir.0);
    store
        .reload_merge_save(&mut state)
        .expect("save the replay");
    store.load().expect("load the replay")
}

#[test]
fn replaying_a_sessions_ops_from_empty_gives_the_saved_state() {
    let temp = Temp::new("session");
    let project = Temp::new("project");
    let dir = temp.0.as_path();
    let store = TaskStore::new(dir);
    let path = project.0.to_string_lossy().into_owned();

    let ((), ops) = record_ops(|| {
        cli(dir, &["add", "-t", "first", "-n", "notes", "--desk"]);
        cli(
            dir,
            &["add", "-t", "second", "--desk", "--due", "2027-01-02"],
        );
        cli(
            dir,
            &["add", "-t", "in a project", "-p", &path, "--thread", "ship"],
        );
        let plan = format!(
            r#"[{{"title": "planned", "desk": true, "after": [1]}}, {{"title": "also", "project": "{path}"}}]"#
        );
        let output = run_with(
            ["tsk", "add", "--state-dir", &dir.to_string_lossy()],
            Cursor::new(plan.into_bytes()),
            false,
        );
        assert_eq!(output.code, 0, "{}", output.stderr);
        cli(
            dir,
            &[
                "edit",
                "T1",
                "--title",
                "first, renamed",
                "--append-notes",
                "more",
            ],
        );
        cli(
            dir,
            &[
                "edit",
                "T2",
                "--assignee",
                "dana",
                "--thread",
                "ship",
                "--clear-due",
            ],
        );
        cli(dir, &["edit", "T2", "--after", "T1", "--after", "T3"]);
        cli(dir, &["steps", "T1", "add", "one"]);
        cli(dir, &["steps", "T1", "add", "two"]);
        cli(dir, &["steps", "T1", "add", "zero", "--first"]);
        let two = step(&store, 1, 2);
        cli(dir, &["steps", "T1", "toggle", &two]);
        cli(dir, &["steps", "T1", "rename", &two, "two, done"]);
        cli(dir, &["steps", "T1", "move", &two, "--first"]);
        let zero = step(&store, 1, 1);
        cli(dir, &["steps", "T1", "remove", &zero]);
        cli(
            dir,
            &[
                "status",
                "T3",
                "blocked",
                "--why",
                "which db?",
                "--option",
                "postgres",
            ],
        );
        cli(dir, &["reply", "T3", "use postgres"]);
        cli(dir, &["status", "T3", "blocked", "--needs", "a decision"]);
        cli(dir, &["edit", "T3", "--on", "T5"]);
        cli(dir, &["status", "T3", "ready"]);
        cli(
            dir,
            &[
                "status", "T5", "review", "--done", "built", "--check", "a", "--check", "a",
            ],
        );
        cli(dir, &["checks", "T5", "set", "2", "failed"]);
        cli(dir, &["status", "T5", "started", "--no-dispatch"]);
        cli(dir, &["status", "T2", "ready"]);
        cli(dir, &["status", "T1", "done"]);
        cli(dir, &["status", "T3", "done"]);
        cli(dir, &["archive", "T4"]);
        cli(dir, &["unarchive", "T4"]);
        cli(dir, &["project", "title", &path, "Replayed"]);
        cli(dir, &["project", "archive", "Replayed"]);
        cli(dir, &["project", "unarchive", "Replayed"]);

        // The board shows T2 and T5, both started.
        board(&store, |run| {
            run(Some(2), BoardIntent::Complete);
            run(None, BoardIntent::Undo);
            run(Some(2), BoardIntent::SoftDelete);
            run(Some(2), BoardIntent::SoftDelete);
            run(None, BoardIntent::Undo);
            run(None, BoardIntent::ToggleMarkMode);
            run(Some(2), BoardIntent::MarkToggle);
            run(Some(5), BoardIntent::MarkToggle);
            run(None, BoardIntent::Complete);
            run(None, BoardIntent::Undo);
            run(Some(5), BoardIntent::ToggleBlock);
            run(None, BoardIntent::EditInsertText("which port?".into()));
            run(None, BoardIntent::BlockCardConfirm);
            run(Some(5), BoardIntent::BeginReply);
            run(None, BoardIntent::EditInsertText("8080".into()));
            run(None, BoardIntent::ReplySave);
            run(Some(2), BoardIntent::File);
        });
    });

    assert!(ops.len() > 30, "the session applied {} ops", ops.len());
    let saved = store.load().expect("load");
    let five = saved
        .tasks()
        .iter()
        .find(|task| task.number == Some(5))
        .expect("T5");
    let block = five.block.as_ref().expect("the board blocked T5");
    assert_eq!(block.why.as_deref(), Some("which port?"));
    assert_eq!(
        block.replies.last().map(|reply| reply.text.as_str()),
        Some("8080")
    );
    assert_eq!(
        serde_json::to_value(replay(&ops)).expect("replayed"),
        serde_json::to_value(&saved).expect("saved"),
        "replaying {} ops from empty gives the saved state",
        ops.len()
    );
}
