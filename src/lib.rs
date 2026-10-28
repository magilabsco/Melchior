//! Melchior: a dependency-free agent harness for agentic work over very large
//! datasets. It is built on a Rust port of the Cordis plugin model and
//! centred on Leviathan, which indexes the data and keeps the agent's memory.
//!
//! The `melchior` binary is [`Distribution::melchior`] run through
//! [`cli::main`]. Products embed Melchior by building their own
//! [`Distribution`] with extra plugins and profiles.
//!
//! See `README.md` and `docs/` in this crate for how it works.

pub mod util;

pub mod json;

pub mod agent;
pub mod cli;
pub mod cordis;
pub mod distribution;
pub mod http;
pub mod jsexpr;
pub mod leviathan;
pub mod llm;
pub mod mcp;
pub mod plugins;
pub mod proc;
pub mod prompt;
pub mod rpc;
pub mod runtime;
pub mod server;
pub mod session;
pub mod tools;
pub mod yaml;

pub use distribution::Distribution;
