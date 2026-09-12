//! `-f list.f` file lists.
//!
//! One path per line, `//` starts a comment, blank lines are ignored, and
//! relative paths resolve against the list file's own directory. That last rule
//! is what makes a list committable alongside the RTL it names.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub fn read(list_path: &Path) -> Result<Vec<PathBuf>> {
    let text = std::fs::read_to_string(list_path)
        .with_context(|| format!("reading file list {}", list_path.display()))?;
    let base = list_path.parent().unwrap_or_else(|| Path::new("."));

    let mut out = Vec::new();
    for line in text.lines() {
        let line = match line.find("//") {
            Some(at) => &line[..at],
            None => line,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let path = Path::new(line);
        out.push(if path.is_absolute() { path.to_path_buf() } else { base.join(path) });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_blanks_are_ignored() {
        let dir = std::env::temp_dir().join("rtlscope-filelist-test");
        std::fs::create_dir_all(&dir).unwrap();
        let list = dir.join("sources.f");
        std::fs::write(&list, "// header\n\n  a.sv  // trailing\nsub/b.sv\n").unwrap();

        let paths = read(&list).unwrap();
        assert_eq!(paths, vec![dir.join("a.sv"), dir.join("sub/b.sv")]);
    }
}
