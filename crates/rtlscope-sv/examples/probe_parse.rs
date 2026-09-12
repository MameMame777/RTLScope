//! Development aid: parse only, print nothing but a count.
//!
//! Splits "sv-parser cannot handle this file" from "something downstream of it
//! cannot", which a probe that also walks the tree cannot tell apart.

use std::path::PathBuf;

use rtlscope_sv::{ParseOptions, parse_file};

fn main() {
    let path: PathBuf = std::env::args().nth(1).expect("usage: probe_parse <file.sv>").into();
    match parse_file(&path, &ParseOptions::default()) {
        Ok(parsed) => {
            let nodes = (&parsed.tree).into_iter().count();
            println!("parsed ok, {nodes} nodes");
        }
        Err(error) => println!("parse error: {error}"),
    }
}
