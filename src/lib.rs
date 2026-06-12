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

pub mod analyzer;
pub mod app;
pub mod collector;
pub mod cost;
pub mod demo;
pub mod format;
pub mod model;
pub mod ui;
