//! Deciding what to fire, before anything is fired.
//!
//! Parts of this are built ahead of the mint command that consumes them, so the
//! compiler is right that some of it is unused today. One allow here rather than
//! per item, deleted in one edit when the orchestration lands.
#![allow(dead_code)]

pub mod planner;
pub mod spend;
pub mod stage;
