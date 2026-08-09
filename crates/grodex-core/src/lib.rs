//! Grodex Core — foundational types and traits for the Grodex AI coding agent.
//!
//! This crate has **zero** internal dependencies. Every other grodex crate
//! depends on it. It must stay small and avoid pulling in heavyweight
//! libraries.

pub mod context;
pub mod error;
pub mod id;
pub mod policy;
pub mod state;
pub mod tool;
