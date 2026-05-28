//! Pre-executor pipeline checks run on every change before invoking the
//! executor. These are deterministic, mechanical, sub-millisecond checks
//! whose role is to catch failure modes that `openspec validate --strict`
//! doesn't catch but that would abort `openspec archive` later, after the
//! LLM cost has already been spent.

pub mod spec_archivability;
