//! `m1-mcp` — a Model Context Protocol server exposing the MoTeC M1 script
//! toolchain to AI agents.
//!
//! It wraps the M1 toolchain **library crates** (`m1-typecheck`, `m1-lint`,
//! `m1-fmt`) in-process and surfaces them as MCP tools so an agent editing
//! `.m1scr` scripts can look up M1 language semantics and run the analysers on
//! its own edits. See [`server::M1Server`] for the tool set.

pub mod analyze;
pub mod doc;
pub mod limits;
pub mod loader;
pub mod server;
pub mod symbols;

pub use server::M1Server;
