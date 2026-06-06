//! Data sources that back the workspace panes.
//!
//! Each submodule reads from the outside world (a git repo, the Claude PTY,
//! token-usage files) and exposes plain data the `ui` layer renders.

pub mod git;
pub mod metrics;
