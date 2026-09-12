# AGENTS.md

Guidelines for AI agents working in this repository.

## Repository layout

This is the `app` repository: the application code for Corral, an
agent-first terminal multiplexer. It is one of several repositories held
side by side in a container folder. Each repository keeps its git history
in a `.bare` directory and checks out branches into worktrees. This
repo's history is in `app/.bare/`; this file lives in the `app/main/`
worktree. All real work happens in worktrees, never in the bare repo
itself.

The multi-repo layout and the rules for working across repositories are
in the `project` repository under `docs/process/repository-structure.md`.

## What lives here

```
crates/corral-core/   emulation wrapper (libghostty-vt) and the split tree
crates/corrald/       daemon: PTYs, socket server, JSON-lines protocol
crates/corral/        TUI client: crossterm input, ratatui rendering
```

Implementation plans live in the `project` repository under
`docs/plans/`; sprint docs with stories and acceptance criteria are under
`docs/sprints/`.

## Before writing code

Work here starts from an approved plan task, never from the story alone.
The v0.1 plan is
`project/main/docs/plans/2026-09-12-corral-v0.1-mux-skeleton.md`. No
plan, no editor.

## Stack

Rust 2024, `libghostty-vt` pinned at `=0.2.1` (never bumped transitively;
the lockfile is committed), `portable-pty`, `crossterm`, `ratatui`,
`serde`/`serde_json`, Unix domain sockets. Zig 0.16.x must be on PATH;
`libghostty-vt-sys` compiles the Ghostty VT core through it.

Do not introduce new dependencies without an explicit decision from
Sushanth.

## Commands

Run from the worktree root.

```sh
cargo build                                   # first build compiles the Ghostty core; slow is normal
cargo test                                    # workspace unit and integration tests
cargo test -p <crate>                         # one crate
cargo clippy -- -D warnings                   # lint gate
cargo fmt --check                             # format gate
cargo test -p corral --release --ignored      # benchmarks
```

## Quality gates

Every change passes the same checks, whether a human or an agent wrote
it. A failing gate blocks the merge. The working agreements are in the
`project` repository under `docs/process/code-quality.md` and
`docs/process/testing.md`.

1. **cargo fmt --check**
2. **cargo clippy -- -D warnings** - a suppression is inline on the line
   it applies to, never a rule turned off across the project.
3. **cargo test** - workspace green.
4. **gitleaks** - no committed secrets.

A TODO or FIXME without a linked plan task or story is rejected.
Bypassing a gate is not a workflow. If a gate is wrong, fix the gate.

## Testing policy

The full policy is in the `project` repository under
`docs/process/testing.md`. Key points:

- Tests ship in the same commit as the code they cover. A task without
  tests is unfinished.
- Unit tests: pure logic (tree, input, protocol serde). Fast, no I/O.
- Integration tests: real PTYs, real Unix sockets, real libghostty-vt.
  No mocks for the PTY, the emulator, or the protocol - fakes cannot
  enforce tty semantics, escape-sequence handling, or framing.
- TUI journeys are verified by human smoke tests in Ghostty at the
  sprint demo gate; renderer assertions use `TestBackend`.
- Determinism: no wall-clock, no randomness, no network. Waits use
  channel timeouts with generous margins.

## The !Send constraint

libghostty-vt types are `!Send + !Sync`. Every `Terminal` is created and
used on one thread (the daemon core thread); PTY reader threads
communicate only through channels. Code that moves an `Emulator` across
threads does not compile, and working around it with `unsafe` is a bug,
not a fix.

## Git layout

Work in a worktree, never in `.bare/`. Add one from inside `app/`:

```sh
git --git-dir=.bare fetch origin main
git --git-dir=.bare worktree add v01-mux-skeleton -b v01-mux-skeleton origin/main
```

Branch naming follows the milestone pattern `<milestone>-<slug>` per
`docs/process/repository-structure.md`. Merge to `main` only after the
gates pass.

## Writing

Follow the writing style in the `project` repository
(`docs/writing-style.md`). Use "we", "our", "us"; never address Sushanth
as "you". Sentence case for headings. No emojis, no em dashes.
Documents stand alone. The smallest useful addition.
