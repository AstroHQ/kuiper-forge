//! CI Runner Coordinator library
//!
//! This library provides the core functionality for the coordinator daemon.
//! The binary entry point is in main.rs.

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
mod sql;
pub mod webhook;
