//! Agix — context compaction strategies (snip, micro-compact, collapse, LLM summarize) and the trims that run before them.

pub mod budget;
pub mod cached;
pub mod compact;
pub mod early_trim;
pub mod grouping;
pub mod llm_summary;
pub mod time_based_mc;
pub mod trim;
