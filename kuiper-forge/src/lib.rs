//! CI Runner Coordinator library
//!
//! This library provides the core functionality for the coordinator daemon.
//! The binary entry point is in main.rs.

// Compile-time check: enforce mutually exclusive database features
#[cfg(all(feature = "sqlite", feature = "postgres"))]
compile_error!("Cannot enable both 'sqlite' and 'postgres' features simultaneously. Choose one.");

#[cfg(not(any(feature = "sqlite", feature = "postgres")))]
compile_error!("At least one database feature must be enabled: 'sqlite' or 'postgres'");

pub mod admin;
pub mod agent_registry;
pub mod auth;
pub mod config;
pub mod db;
pub mod fleet;
pub mod github;
pub mod management;
pub mod pending_jobs;
pub mod runner_state;
pub mod server;
pub mod tls;
mod sql;
pub mod webhook;
