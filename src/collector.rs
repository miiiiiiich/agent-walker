//! One collector per provider (`collector/<provider>.rs`) over shared
//! infrastructure split by responsibility: `events` (the serialized bundle),
//! `walk` (directory scanning), `cache` (the versioned parse cache — parsing
//! semantics changes bump `CACHE_VERSION` there), `merge` (cross-file keyed
//! dedup), and `project` (cwd normalization). Providers and callers keep
//! addressing `crate::collector::*` through the re-exports below.
pub mod agy;
mod agy_conv;
pub mod claude;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod grok;
pub mod opencode;

mod cache;
mod events;
mod merge;
mod project;
mod walk;

pub use cache::parse_files_cached;
pub use events::{
    FileEvents, KeyedCreditSample, KeyedDurationEvent, KeyedEffortEvent, KeyedModeEvent,
    KeyedPermissionEvent, KeyedRateLimitSample, KeyedToolEvent, KeyedUsageEvent,
};
pub use merge::merge_into;
pub use project::project_from_cwd;
pub use walk::list_files;
