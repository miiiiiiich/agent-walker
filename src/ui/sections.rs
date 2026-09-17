//! Panel-specific renderers live in one file per panel (`sections/*.rs`) so
//! their changes are visible by filename.
mod agents;
mod completion;
mod context;
mod cost;
mod models;
mod modes;
mod parallel;
mod projects;
mod signal;
mod skills;
mod time;
mod tools;

pub(super) use agents::agent_lines;
pub(super) use completion::duration_lines;
pub(super) use context::context_lines;
pub(super) use cost::cost_lines;
pub(super) use models::model_lines;
pub(super) use modes::modes_lines;
pub(super) use parallel::parallel_lines;
pub(super) use projects::project_lines;
pub(super) use signal::signal_lines;
pub(super) use skills::skill_lines;
pub(super) use time::time_lines;
pub(super) use tools::tool_lines;
