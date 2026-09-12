//! The work behind the `rtlscope` binary.
//!
//! Split out from `main.rs` so the report shapes can be golden-tested directly
//! rather than by scraping the output of a spawned process.

pub mod cmd;
pub mod filelist;
