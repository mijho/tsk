# Development process (fork `mijho/tsk`)

Continuous development on `main` from an exe.dev VM. tsk tracks the work. No PRs.

## Rules

1. Brevity. One way of doing things. Refactor relentlessly.
2. New code fails loudly: propagate the error, no fallbacks, no compat shims in `src/sync/`.
   Store migrations and every AGENTS.md invariant still hold: tsk has users.
3. Never add sleeps to tests.
4. The board is the tracker. Plans and notes go on tasks, not in markdown TODOs.
5. Track with the installed stable `tsk` against `~/.tsk`. Never point a repo build at
   `~/.tsk`: a dev build can migrate the store to an unreleased format. Repo builds run
   only with `TSK_STATE_DIR=$(mktemp -d)`.
6. Commit before ending a turn. Push only validated commits.

## Roles

- **Builder**: the main Shelley conversation. Picks, claims, builds, commits, pushes.
- **Validator**: a fresh Shelley subagent per review round. Gets only the task number and
  the commit range. Never edits code. Never sees the builder's reasoning.

## Round

1. `git pull --ff-only`. Pick the lowest-numbered `open` task on a `sync-*` thread whose
   `after` tasks are all done and that has no open block: `tsk list --json`.
2. Claim: `tsk edit T<n> --assignee builder && tsk status T<n> started`.
3. Build the smallest change that finishes the task. Test first; a regression test must
   fail without its fix. Tick steps as they land: `tsk steps T<n> toggle <id>`.
4. Commit. Subject `sync: <what>`, trailer `Task: T<n>`. One task, one commit where possible.
5. Open review:
   `tsk status T<n> review --on validator --done "<sha range>: <summary>"`
   with `--check` for each: green bar passes · new behaviour has a failing-first test ·
   docs and CHANGELOG updated (or not user-visible) · no AGENTS.md invariant broken ·
   matches `docs/sync/PLAN.md` (or PLAN.md updated in the same commit).
6. Spawn the validator with: "Validate T<n>, commits <range>. Read `tsk list T<n> --json`,
   `git show <range>`, AGENTS.md and docs/sync/PLAN.md. Run the green bar. Mark every check
   with `tsk checks T<n> set <i> passed|failed`. Put each finding in `tsk reply T<n>`.
   Do not edit files."
7. All checks passed: `git push origin HEAD:main`, then `tsk status T<n> done`.
   Any failed: fix in a new commit, re-run step 5 (new round). Third failed round:
   `tsk status T<n> blocked --on you --why "<what keeps failing>"` and move on.
8. Push refused: `git pull --rebase` (only unpushed commits move), green bar, push.

## Rotation

Every fifth done task, or first thing in a session:

1. **Upstream**: `git fetch upstream && git merge upstream/main`. Resolve, green bar, push.
   Merge, never rebase: `main` is published.
2. **Board**: split tasks that took more than one round, add follow-ups found while
   building, close what the code made moot. Keep the next three tasks unambiguous.
3. **Drift**: diff docs against `src/` for anything shipped since the last rotation.

## Needs a human

A decision only the maintainer can make: `tsk status T<n> blocked --on you --why ...
--option ...`, then take the next task. Never wait idle. The maintainer answers with
`tsk reply` or on the board; the next round picks the task back up.
