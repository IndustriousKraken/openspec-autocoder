//! State-file modules for chatops-driven flows.
//!
//! Each submodule owns one on-disk JSON state file shape (atomic-rename
//! writes, with `read_state` / `write_state` accessors). Modules are
//! grouped here rather than at the crate root for readability as the set
//! of chatops flows grows.

pub mod brownfield_request;
pub mod scout_run;
