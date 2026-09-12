//! Development aid: answers "what coordinate system is `Locate::line` in?".
//!
//! The docs do not say whether it counts lines in the original file or in the
//! preprocessed text, and the answer decides whether spans can be trusted
//! across an `include`. This prints both readings side by side.
//!
//! Usage: `cargo run -p rtlscope-sv --example probe_locate -- <file.sv>`

use std::path::PathBuf;

use rtlscope_sv::{ParseOptions, parse_file};
use sv_parser::{NodeEvent, RefNode};

fn main() {
    let path: PathBuf = std::env::args().nth(1).expect("usage: probe_locate <file.sv>").into();
    let dir = path.parent().unwrap_or(std::path::Path::new(".")).to_path_buf();
    let options = ParseOptions { defines: Vec::new(), include_paths: vec![dir] };
    let parsed = parse_file(&path, &options).expect("parse");

    println!("{:<14} {:>12} {:>10}  get_origin", "identifier", "Locate.line", "offset");
    for event in parsed.tree.into_iter().event() {
        let NodeEvent::Enter(RefNode::SimpleIdentifier(node)) = event else { continue };
        let locate = node.nodes.0;
        let text = parsed.tree.get_str(&locate).unwrap_or("<none>");
        let origin = match parsed.tree.get_origin(&locate) {
            Some((origin_path, origin_offset)) => {
                let name = origin_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                format!("{name} @ byte {origin_offset}")
            }
            None => "<none>".to_string(),
        };
        println!("{text:<14} {:>12} {:>10}  {origin}", locate.line, locate.offset);
    }
}
