//! The design a window is looking at, written down for the other front ends.
//!
//! The GUI and the MCP server are two ways into the same crates, and until now
//! they had no way of being pointed at the same thing: every MCP call carried
//! its own list of files, typed out again. This is that list, left where the
//! other can find it — so an agent asked about "the design" answers about the
//! one on screen, and the two cannot describe different designs to the same
//! person.
//!
//! What is handed over is the **sources**, not an elaborated design, and that
//! is the whole reason this works. A parsed design would start drifting from
//! the files the moment one changed, and the two tools would quietly disagree.
//! The arguments cannot drift: both sides read the same files, so both get the
//! same answer, and the MCP server's cache — keyed on each file's modified
//! time — notices an edit without being told.
//!
//! It lives in this crate because it is exactly the arguments to
//! [`lower_files`](crate::lower_files): which files, which top, which defines
//! and include paths. Anything that can read SystemVerilog already depends on
//! this crate; nothing needs a new one.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ParseOptions;

/// What a design was read from, and by whom.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub files: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top: Option<String>,
    /// `NAME` or `NAME=VALUE`, spelled as they would be on a command line.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub defines: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub includes: Vec<PathBuf>,
    /// The process that wrote this and when, so a reader can say where an
    /// answer came from rather than presenting it as the only possible one.
    #[serde(default)]
    pub pid: u32,
    /// Seconds since the epoch.
    #[serde(default)]
    pub written_at: u64,
}

impl Session {
    /// What is being looked at right now.
    pub fn of(files: &[PathBuf], top: Option<&str>, options: &ParseOptions) -> Self {
        Self {
            files: files.to_vec(),
            top: top.map(str::to_owned),
            defines: options
                .defines
                .iter()
                .map(|(name, value)| match value {
                    Some(value) => format!("{name}={value}"),
                    None => name.clone(),
                })
                .collect(),
            includes: options.include_paths.clone(),
            pid: std::process::id(),
            written_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_secs())
                .unwrap_or_default(),
        }
    }

    /// The defines and include paths again, in the form the parser takes.
    pub fn options(&self) -> ParseOptions {
        ParseOptions {
            defines: self
                .defines
                .iter()
                .map(|entry| match entry.split_once('=') {
                    Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
                    None => (entry.clone(), None),
                })
                .collect(),
            include_paths: self.includes.clone(),
        }
    }

    /// Where the file lives.
    ///
    /// State, not configuration: it says what a program is doing at the moment,
    /// not how the user wants it to behave, so it goes where the system puts
    /// the former. `None` when the environment names no such place, which is
    /// not an error — it means this machine has nowhere to leave the note.
    pub fn path() -> Option<PathBuf> {
        let base = if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        } else {
            std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })
        };
        Some(base?.join("rtlscope").join("session.json"))
    }

    /// Leaves the note. Returns where it went.
    pub fn write(&self) -> std::io::Result<PathBuf> {
        let path = Session::path().ok_or_else(|| {
            std::io::Error::other("this machine names no place to keep session state")
        })?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&path, text)?;
        Ok(path)
    }

    /// Reads the note, if there is one that still makes sense.
    ///
    /// Anything wrong with it — missing, unreadable, not the shape it should
    /// be, naming files that are not there any more — is `None`. A reader of
    /// this has a perfectly good fallback (ask for the files), and
    /// half-understanding someone else's note is worse than not having one:
    /// "no file at X" sends the reader hunting for a file they never named.
    pub fn read() -> Option<Session> {
        let text = std::fs::read_to_string(Session::path()?).ok()?;
        let session: Session = serde_json::from_str(&text).ok()?;
        if session.files.is_empty() || all_present(&session.files).is_some() {
            return None;
        }
        Some(session)
    }

    /// Removes the note, for a window that is closing.
    pub fn clear() {
        if let Some(path) = Session::path() {
            let _ = std::fs::remove_file(path);
        }
    }

    /// One line saying which design this is, for an answer to carry.
    ///
    /// Every answer that came from a session says this. Being asked about "the
    /// design" and answering about a different one is the failure worth
    /// spending a line on.
    pub fn describe(&self) -> String {
        let named = match &self.top {
            Some(top) => format!(", top `{top}`"),
            None => String::new(),
        };
        let first = self
            .files
            .first()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "?".to_string());
        let what = match self.files.len() {
            1 => format!("the design open in RTLScope: `{first}`{named}"),
            n => format!("the design open in RTLScope: `{first}` and {} more{named}", n - 1),
        };
        match self.age() {
            Some(age) => format!("{what} (opened there {age})"),
            None => what,
        }
    }

    /// How long ago the window said this, in words.
    ///
    /// A window that has since been closed leaves its note behind, so the age
    /// is what lets a reader judge it. `None` when the clock disagrees with
    /// the note, which is not worth a sentence of its own.
    fn age(&self) -> Option<String> {
        let now =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
        let seconds = now.checked_sub(self.written_at)?;
        Some(match seconds {
            0..=90 => "just now".to_string(),
            91..=5400 => format!("{} minute(s) ago", seconds / 60),
            5401..=172_800 => format!("{} hour(s) ago", seconds / 3600),
            _ => format!("{} day(s) ago", seconds / 86_400),
        })
    }
}

/// The paths as the parser wants them: absolute where that is possible.
pub fn absolute(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths.iter().map(|path| dunce::canonicalize(path).unwrap_or_else(|_| path.clone())).collect()
}

/// True when every file named is still there to read.
pub fn all_present(paths: &[PathBuf]) -> Option<&Path> {
    paths.iter().find(|path| !path.exists()).map(PathBuf::as_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> ParseOptions {
        ParseOptions {
            defines: vec![("SIM".to_string(), None), ("WIDTH".to_string(), Some("8".to_string()))],
            include_paths: vec![PathBuf::from("/rtl/inc")],
        }
    }

    /// The point of the file: two tools reading the same arguments read the
    /// same design. A define that survives the round trip differently would
    /// make them disagree about `ifdef` regions and nothing would say so.
    #[test]
    fn the_arguments_survive_the_round_trip_unchanged() {
        let session = Session::of(&[PathBuf::from("/rtl/top.sv")], Some("top"), &options());

        let text = serde_json::to_string(&session).expect("serialises");
        let back: Session = serde_json::from_str(&text).expect("reads");

        assert_eq!(back, session);
        assert_eq!(back.options().defines, options().defines);
        assert_eq!(back.options().include_paths, options().include_paths);
    }

    #[test]
    fn a_bare_define_stays_bare_and_a_valued_one_keeps_its_value() {
        let session = Session::of(&[PathBuf::from("a.sv")], None, &options());
        assert_eq!(session.defines, ["SIM", "WIDTH=8"]);
        assert_eq!(session.options().defines, options().defines);
    }

    /// A reader has a perfectly good fallback, so anything it cannot fully
    /// understand is nothing rather than a guess.
    #[test]
    fn a_note_that_names_no_files_is_no_note_at_all() {
        let empty: Session = serde_json::from_str("{\"files\":[]}").expect("reads");
        assert!(empty.files.is_empty());

        let nonsense = serde_json::from_str::<Session>("{\"files\":\"top.sv\"}");
        assert!(nonsense.is_err(), "a file list that is not a list is not understood");
    }

    /// A window that has been closed and a design that has been deleted leave
    /// the same trace. Answering from either would send the reader hunting for
    /// a file they never named.
    #[test]
    fn a_note_naming_files_that_are_gone_is_not_used() {
        let here = std::env::temp_dir().join("rtlscope-session-present.sv");
        std::fs::write(&here, "module m; endmodule\n").expect("writable temp dir");

        assert!(all_present(std::slice::from_ref(&here)).is_none(), "the one that is there");
        assert!(
            all_present(&[here.clone(), PathBuf::from("/gone/away.sv")]).is_some(),
            "and the one that is not"
        );
        let _ = std::fs::remove_file(&here);
    }

    #[test]
    fn the_description_names_the_design_and_counts_the_rest() {
        let one =
            Session::of(&[PathBuf::from("/rtl/top.sv")], Some("top"), &ParseOptions::default());
        assert_eq!(
            one.describe(),
            "the design open in RTLScope: `top.sv`, top `top` (opened there just now)"
        );

        let many = Session::of(
            &[PathBuf::from("/rtl/top.sv"), PathBuf::from("/rtl/b.sv"), PathBuf::from("/rtl/c.sv")],
            None,
            &ParseOptions::default(),
        );
        assert!(many.describe().starts_with("the design open in RTLScope: `top.sv` and 2 more"));

        // A window closed yesterday still leaves its note; the age is what
        // tells a reader so.
        let mut old = one.clone();
        old.written_at = old.written_at.saturating_sub(7_200);
        assert!(old.describe().contains("hour(s) ago"), "{}", old.describe());
    }
}
