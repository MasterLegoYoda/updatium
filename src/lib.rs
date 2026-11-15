//! Updatium library: providers, engine, installers, storage.
//! The `updatium` binary is a thin dispatcher over this library.

pub mod cli;
pub mod config;
pub mod desktop;
pub mod engine;
pub mod exec;
pub mod hash;
pub mod installer;
pub mod net;
mod onboarding;
pub mod onboarding_validation;
pub mod recipes;
pub mod provider;
pub mod policy;
pub mod store;
pub mod tui;
pub mod version;
