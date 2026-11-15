//! Framewise – media optimizer for Plex/Jellyfin libraries.
//!
//! See `docs/ARCHITECTURE.md` for the module overview.

pub mod i18n;

pub mod analyzer;
pub mod core;
pub mod encoding;
pub mod gui;
pub mod health;
pub mod media;
pub mod monitor;
pub mod persistence;
pub mod provider;
pub mod queue;
pub mod scanner;
