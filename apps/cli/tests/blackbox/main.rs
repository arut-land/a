//! The built `a` as a black box: every module here drives the binary
//! against a daemon in an isolated home (the echo agent, through the
//! fixtures), over the harness in `common`. One binary, so the graph links
//! once; `mise run acceptance`, `test:web` and `budgets` pick their module
//! by name (`-E 'test(/^budgets::/)'`).

#[cfg(unix)]
mod common;

#[cfg(unix)]
mod acceptance;
#[cfg(unix)]
mod budgets;
#[cfg(unix)]
mod crash;
#[cfg(unix)]
mod home;
#[cfg(feature = "web")]
#[cfg(unix)]
mod hygiene;
#[cfg(feature = "index")]
mod index;
#[cfg(feature = "lua")]
#[cfg(unix)]
mod lua;
#[cfg(all(unix, feature = "mcp"))]
mod mcp;
#[cfg(unix)]
mod pipe;
#[cfg(unix)]
mod reactor;
#[cfg(unix)]
mod remote;
#[cfg(all(unix, feature = "skills"))]
mod skills;
#[cfg(unix)]
mod tui;
#[cfg(feature = "web")]
#[cfg(unix)]
mod web;

mod daemon_acceptance;
mod daemons;
mod query;

mod support;
