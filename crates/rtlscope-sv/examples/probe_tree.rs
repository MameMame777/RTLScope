//! Development aid: dumps an sv-parser syntax tree so the lowering can be
//! written against the node names that actually come out of it.
//!
//! Usage: `cargo run -p rtlscope-sv --example probe_tree -- tests/fixtures/adder.sv`

use std::path::PathBuf;

use rtlscope_sv::{ParseOptions, parse_file};

fn main() {
    let path: PathBuf = std::env::args().nth(1).expect("usage: probe_tree <file.sv>").into();
    let parsed = parse_file(&path, &ParseOptions::default()).expect("parse");
    println!("{:?}", parsed.tree);
}
