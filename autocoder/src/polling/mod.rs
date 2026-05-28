//! Polling-loop submodules for chat-driven flows. Each submodule owns
//! the "process one queue entry" logic invoked from `polling_loop::run`.

pub mod brownfield;
pub mod scout;
pub mod spec_it;
