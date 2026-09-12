//! How the desk was arranged, so it comes back that way.
//!
//! Deliberately not [`rtlscope_sv::Session`], which lives next door and holds the
//! same kind of thing in the same kind of place. That one is a note saying
//! *what a window has open right now*, written for the agent to read and
//! deleted when the window closes. This is the opposite: how the reader likes
//! their desk, which has to outlive every session and belongs to them rather
//! than to a running process.
//!
//! Everything here is a preference, so everything here is optional. A file that
//! will not parse, names a view that no longer exists, or describes an
//! arrangement this build cannot make must cost the reader nothing worse than
//! the default one — a layout is not worth an error message, and certainly not
//! worth refusing to start.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// `%LOCALAPPDATA%\rtlscope`, or the XDG equivalent: where this program keeps
/// what belongs to the reader rather than to a design — the layout, and the
/// samples once they have been written out.
///
/// `None` on a machine that names no such place, which a caller treats as
/// "nothing remembered" rather than as an error.
pub fn settings_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    };
    Some(base?.join("rtlscope"))
}

/// The arrangement, as it was left.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    /// The whole desk: which views are where, how the splits are divided, and
    /// which of them have windows of their own.
    ///
    /// Opaque here on purpose. Resolving the names back into views is
    /// `crate::dock`'s job, and it is the one that knows which name it no
    /// longer recognises; this module's promise is only that a file it cannot
    /// make sense of costs the default arrangement and nothing more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dock: Option<serde_json::Value>,
    /// How big the main window was left.
    ///
    /// The size and not the position. A size cannot strand a window where
    /// nobody can reach it and a position can — a desk that had two monitors
    /// on Friday may have one on Monday. The operating system places a new
    /// window sensibly on its own; what it cannot know is how large the reader
    /// wants it, which is the part worth remembering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_size: Option<[f32; 2]>,
}

impl Layout {
    /// `%LOCALAPPDATA%\rtlscope\layout.json`, or the XDG equivalent.
    pub fn path() -> Option<PathBuf> {
        Some(settings_dir()?.join("layout.json"))
    }

    /// Reads it, or gives the default arrangement.
    ///
    /// Never an error. There is no state of this file worth stopping a reader
    /// over, and "your views are where you left them" is not a promise the
    /// tool should make so loudly that breaking it is a failure.
    pub fn read() -> Layout {
        let Some(path) = Layout::path() else { return Layout::default() };
        Layout::read_from(&path)
    }

    /// The same, from a named file.
    ///
    /// Separate so a test can exercise the round trip without reaching for the
    /// reader's own settings — a suite that deletes the file it is testing is
    /// one that costs its author their window layout every run.
    pub fn read_from(path: &std::path::Path) -> Layout {
        let Ok(text) = std::fs::read_to_string(path) else { return Layout::default() };
        serde_json::from_str(&text).unwrap_or_default()
    }

    /// Writes it, saying where it went or why it could not.
    pub fn write(&self) -> std::io::Result<PathBuf> {
        let path = Layout::path()
            .ok_or_else(|| std::io::Error::other("this machine names no place for settings"))?;
        self.write_to(&path)?;
        Ok(path)
    }

    /// The same, to a named file.
    pub fn write_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }

    /// Forgets it, so the next start is the default arrangement.
    pub fn clear() {
        if let Some(path) = Layout::path() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// What the reader likes, apart from where they put their views.
///
/// Its own file, beside the layout rather than inside it, because `reset
/// layout` deletes that one — and somebody who asked for their panes back has
/// not asked to be handed a different colour scheme and a different simulator
/// along with them. Two preferences with two lifetimes are two files.
///
/// Both are stored as *names*, for the reason the layout stores tab names: a
/// number would be a promise about the order of an enum that the file outlives.
/// A name this build does not recognise costs the default and nothing more.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// `auto`, `light` or `dark`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// `verilator` or `icarus`: which simulator the window runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// Where the simulator lives, for a window that inherited no shell's
    /// `PATH`. Looked in before the `PATH`, never instead of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sim_dir: Option<String>,
    /// The Python that plays a drawn pattern, for the same reason: a window
    /// opened by double-clicking a `.sv` file stands nowhere near a checkout,
    /// so there is no `.venv-cocotb` to find by looking around.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<String>,
    /// How to open a source location, with `{file}`, `{line}` and `{col}`
    /// substituted.
    ///
    /// The third thing a window started from a shortcut cannot be told any
    /// other way: `--editor` reaches a terminal and nothing else, so before
    /// this there was no way to use any editor but the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor: Option<String>,
}

impl Settings {
    /// `%LOCALAPPDATA%\rtlscope\settings.json`, or the XDG equivalent.
    pub fn path() -> Option<PathBuf> {
        Some(settings_dir()?.join("settings.json"))
    }

    /// Reads them, or gives the defaults. Never an error, for the reason
    /// [`Layout::read`] is not one.
    pub fn read() -> Settings {
        let Some(path) = Settings::path() else { return Settings::default() };
        Settings::read_from(&path)
    }

    /// The same, from a named file — so a test never reads the settings of
    /// whoever is running it.
    pub fn read_from(path: &std::path::Path) -> Settings {
        let Ok(text) = std::fs::read_to_string(path) else { return Settings::default() };
        serde_json::from_str(&text).unwrap_or_default()
    }

    /// Writes them, saying where they went or why they could not.
    pub fn write(&self) -> std::io::Result<PathBuf> {
        let path = Settings::path()
            .ok_or_else(|| std::io::Error::other("this machine names no place for settings"))?;
        self.write_to(&path)?;
        Ok(path)
    }

    /// The same, to a named file.
    pub fn write_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_desk() -> serde_json::Value {
        serde_json::json!({ "surfaces": [], "focused_surface": null })
    }

    #[test]
    fn an_arrangement_survives_the_round_trip_it_travels_by() {
        let layout = Layout { dock: Some(a_desk()), main_size: Some([1600.0, 950.0]) };

        let text = serde_json::to_string(&layout).expect("serialises");
        let back: Layout = serde_json::from_str(&text).expect("and comes back");
        assert_eq!(back, layout);
    }

    /// The file is the whole feature: what is written has to come back, and
    /// forgetting has to actually forget.
    #[test]
    fn an_arrangement_goes_to_a_file_and_comes_back_from_it() {
        let dir = std::env::temp_dir().join("rtlscope-layout-round-trip");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("layout.json");

        assert_eq!(Layout::read_from(&path), Layout::default(), "nothing yet");

        let layout = Layout { dock: Some(a_desk()), main_size: Some([1600.0, 950.0]) };
        layout.write_to(&path).expect("makes the directory on the way");
        assert_eq!(Layout::read_from(&path), layout);

        std::fs::remove_file(&path).expect("forgotten");
        assert_eq!(Layout::read_from(&path), Layout::default(), "and stays forgotten");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Nothing about a saved layout is worth failing over.
    #[test]
    fn nonsense_reads_as_the_default_rather_than_an_error() {
        assert_eq!(serde_json::from_str::<Layout>("{}").unwrap(), Layout::default());
        // What a desk holds is checked when it is resolved, not when it is
        // read; reading keeps whatever was written.
        let odd: Layout = serde_json::from_str(r#"{"dock":{"nonsense":true}}"#).unwrap();
        assert!(odd.dock.is_some(), "kept to be judged later");
        assert_eq!(odd.main_size, None, "and the rest defaults rather than refusing");
    }

    /// A file written by the version before the dock names windows this one
    /// has never heard of. It must not cost the reader their window size, and
    /// it must not cost them a working program.
    #[test]
    fn a_file_from_before_the_dock_still_opens() {
        let old = r#"{
            "detached": ["Wave", "Trace"],
            "placed": { "Wave": { "x": 950.0, "y": 300.0, "w": 880.0, "h": 600.0 } },
            "main_size": [1600.0, 950.0],
            "tab": "Source",
            "source_beside": true
        }"#;
        let layout: Layout = serde_json::from_str(old).expect("reads");
        assert_eq!(layout.dock, None, "it described no desk this build can make");
        assert_eq!(layout.main_size, Some([1600.0, 950.0]), "but the size is still good");
    }

    /// The settings are a file of their own precisely so that resetting the
    /// desk leaves them alone. Written down as a test because the two live in
    /// one directory and one careless `clear` would take both.
    #[test]
    fn a_preference_survives_a_layout_being_forgotten() {
        let dir = std::env::temp_dir().join("rtlscope-settings-round-trip");
        let _ = std::fs::remove_dir_all(&dir);
        let layout = dir.join("layout.json");
        let settings = dir.join("settings.json");

        assert_eq!(Settings::read_from(&settings), Settings::default(), "nothing yet");

        // With the tool paths filled in: they are the whole reason this file
        // exists for somebody who never opens a terminal, so a round trip that
        // dropped them would lose the thing hardest to type again.
        let liked = Settings {
            theme: Some("dark".into()),
            engine: Some("icarus".into()),
            sim_dir: Some("E:/msys/ucrt64/bin".into()),
            python: Some("E:/work/.venv-cocotb/bin/python.exe".into()),
            editor: Some("subl {file}:{line}".into()),
        };
        liked.write_to(&settings).expect("makes the directory on the way");
        Layout { dock: Some(a_desk()), main_size: None }.write_to(&layout).expect("beside it");
        assert_eq!(Settings::read_from(&settings), liked);

        // What `reset layout` does, to the file it does it to.
        std::fs::remove_file(&layout).expect("forgotten");
        assert_eq!(Settings::read_from(&settings), liked, "the theme is not the desk");

        // A file with something unreadable in it costs the defaults, not a
        // start-up failure.
        std::fs::write(&settings, "{ this is not json").expect("write");
        assert_eq!(Settings::read_from(&settings), Settings::default());
    }
}
