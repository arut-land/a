//! The built `a` as a black box: every module here drives the binary
//! against a daemon in an isolated home (the echo agent, through the
//! fixtures), over the harness in `common`. One binary, so the graph links
//! once; `mise run acceptance`, `test:web` and `budgets` pick their module
//! by name (`-E 'test(/^budgets::/)'`).
#![cfg(unix)]

mod common;

mod acceptance;
mod budgets;
mod crash;
mod home;
mod reactor;
mod remote;
mod tui;
#[cfg(feature = "web")]
mod web;
