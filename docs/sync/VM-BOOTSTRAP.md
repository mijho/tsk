# exe.dev VM bootstrap

## Maintainer, once

1. Create the VM. Give it GitHub push access to `mijho/tsk` only: a deploy key on that repo,
   or a fine-grained token scoped to it. Then nothing on the VM can write to `smarzban/tsk`.
2. Open Shelley on the VM and paste the prompt below.

## Prompt for Shelley

```text
Set up this VM to develop tsk, then start the development loop.

1. Clone git@github.com:mijho/tsk.git to ~/tsk on main. Add remote
   upstream https://github.com/smarzban/tsk and make it fetch-only:
   git remote set-url --push upstream DISABLED-hard-fork
   Install scripts/pre-push as .git/hooks/pre-push. Set git user.name/user.email to mine.
2. Install build-essential, pkg-config, curl, python3, then rustup (minimal profile).
   rust-toolchain.toml pins the toolchain; let cargo install it on first build.
3. Install stable tsk for tracking:
   curl -fsSL https://gettsk.sh/install.sh | TSK_INSTALL_DIR="$HOME/.local/bin" sh
   Put ~/.local/bin on PATH. Run `tsk setup codex` so the tsk skill lands in
   ~/.agents/skills, which you read.
4. From ~/tsk run scripts/sync-seed-tasks. It is safe to re-run.
5. Read AGENTS.md, docs/sync/PLAN.md and docs/sync/PROCESS.md. PROCESS.md is how you work
   from now on: rounds, a fresh validator subagent per review, push to main, no PRs,
   rotation every fifth done task.
6. Start with a rotation, then run rounds until a task needs me. Block it on me and keep
   going with the next one.
```
