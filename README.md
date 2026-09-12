# Corral (working title)

An agent-first terminal multiplexer: a Rust daemon that owns panes, agent
sessions, and state, with TUI, CLI, and HTTP surfaces over it.

This repository is an empty scaffold. The implementation plan in
`project/main/docs/` drives what lands here, and nothing in this repository
is finished work yet.

Planned shape, subject to the design spec and the implementation plan: a
Cargo workspace with a daemon crate, a TUI client crate, ACP adapter worker
crates, and a CLI. Stack decisions are recorded in the design spec, not in
this README.
