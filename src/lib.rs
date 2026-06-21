//! agent-walker: a local dashboard for AI coding-agent usage.
//!
//! It reads the session logs that Claude Code, Codex CLI, and (opt-in)
//! Antigravity CLI already write to disk, aggregates tokens, API-equivalent
//! cost, activity, and autonomy, and renders a terminal dashboard plus a
//! shareable "codename" stats card. Everything is computed locally; the only
//! network access is a pricing-metadata fetch.
//!
//! This crate backs the `agent-walker` / `agw` binaries. Its library API is an
//! internal seam and carries no stability guarantee.
#![warn(clippy::pedantic)]
#![allow(
    clippy::missing_errors_doc,
    reason = "This is an application crate; anyhow contexts document boundary failures."
)]
#![allow(
    clippy::missing_panics_doc,
    reason = "Public functions are internal application seams exercised by integration tests."
)]
#![allow(
    clippy::module_name_repetitions,
    reason = "Domain names stay explicit at collector/analyzer boundaries."
)]
#![allow(
    clippy::must_use_candidate,
    reason = "Internal constructors and formatters are clearer without noisy attributes."
)]

mod analyzer;
mod app;
mod codename;
mod collector;
mod cost;
mod demo;
mod format;
mod model;
mod share;
mod ui;

// The binaries are the only consumers; expose just the entry point.
pub use app::{Args, run};
