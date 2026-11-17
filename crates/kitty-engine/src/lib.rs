//! Patch engine for M0on's packer & patcher.
//!
//! A Rust reimplementation of the behaviour of the m0on KittyPatcher 0.7.6 v2
//! (`kittypatcher dev\M0on-Kittypatcher v0.7.6 v2\`). Milestone M1 covers the
//! parse step: reading mod files into the same registries the Python
//! `load_mods` builds, including the per-mod and cross-mod inner-replacement
//! folding. Applying the patches to the HTML comes in later milestones.
//!
//! Everything order-sensitive uses insertion-ordered maps, so the same input
//! always gives the same output.

pub mod apply;
pub mod dump;
pub mod escape;
pub mod fold;
pub mod guards;
pub mod html;
pub mod load;
pub mod model;
pub mod parse;
pub mod pytext;
pub mod search;
