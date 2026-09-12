//! The faces the window draws text in.
//!
//! egui ships Ubuntu-Light and Hack and uses them when nobody says otherwise,
//! which is what RTLScope did until now. Two things are wrong with that, and only
//! the second one is about looks.
//!
//! **Glyphs.** Those two faces cover little more than Latin. Every symbol this
//! window has reached for — an arrow, a cross, a house, a not-equals, the two
//! joined squares that mean "open in a window" — drew as a hollow box, and each
//! time the fix was to write a word instead. Five times. The words are often
//! the better label anyway, but the choice was never really made: the font made
//! it. And it is not only decoration at stake — a path or an instance name with
//! a Japanese character in it draws as boxes too, and that is data.
//!
//! **Weight.** Ubuntu-Light is a 300-weight face. This window is dense — a
//! hierarchy tree, a signal table, bus values inside runs 30 pixels wide — and
//! thin strokes at 10.5pt are the first thing to go when a reader is tired.
//!
//! There is deliberately no test that every mark in the interface has a glyph,
//! though that is the test this module seems to want. [`epaint`]'s
//! `Fonts::has_glyphs` is the only way to ask, and it disagrees with the
//! screen: it reports the half-filled circle on the theme button as missing
//! while a screenshot shows it drawn, and its own source carries a `TODO` about
//! returning false negatives. It was right about the arrow, and wrong about the
//! circle, which is the worst of both — a gate that fails on working marks
//! teaches people to delete it. The marks are checked by looking at the window.
//!
//! Everything here is best effort. A face that is not on this machine is
//! skipped, and anything missing falls through to the fonts egui brought with
//! it, which is exactly today's behaviour — so nothing here can stop the window
//! opening. Only the Windows list is measured; the other two are the usual
//! locations, declared so this is not Windows-only by construction, but they
//! have not been run.

use std::path::PathBuf;
use std::sync::Arc;

use egui::{Context, FontData, FontDefinitions, FontFamily};

/// A face to look for, and what it is for.
struct Face {
    /// What to call it in [`FontDefinitions`].
    name: &'static str,
    /// File names to try, best first, inside each of [`font_dirs`].
    files: &'static [&'static str],
    /// Which face inside the file. `.ttc` files hold several; 0 is the regular
    /// upright weight in every collection named here.
    index: u32,
}

/// The text of the interface: labels, buttons, module names in the tree.
const UI: Face = Face {
    name: "rtlscope-ui",
    files: &["segoeui.ttf", "SFNSText.ttf", "DejaVuSans.ttf"],
    index: 0,
};

/// Arrows, maths, and the marks a schematic reader expects.
const SYMBOL: Face =
    Face { name: "rtlscope-symbol", files: &["seguisym.ttf", "DejaVuSans.ttf"], index: 0 };

/// So a Japanese path, comment, or instance name is text rather than boxes.
const CJK: Face = Face {
    name: "rtlscope-cjk",
    files: &["YuGothR.ttc", "meiryo.ttc", "msgothic.ttc", "NotoSansCJK-Regular.ttc"],
    index: 0,
};

/// Which faces each family tries, and on which side of the ones egui shipped.
///
/// Order is absolute — the first face holding a character draws it — so where a
/// face goes decides what it is allowed to affect. `before` replaces: the face
/// the family is set in. `after` only covers: consulted for a character nothing
/// ahead of it has, and unable to touch anything that already worked.
///
/// Only the proportional family is replaced. Ubuntu-Light is a 300-weight face
/// with barely more than Latin in it, and this window is dense enough to want
/// neither. egui's monospace is Hack, which is a real coding face and was never
/// the problem — and putting Cascadia Mono in front of it cost the underscore
/// in every diagram box title at 11pt, measured: `u_gate` drew as `u gate`
/// while the same face at 8pt was fine. Nothing is worth that, so the
/// monospace family only gains fallbacks behind what it already had.
fn chain(family: &FontFamily) -> (&'static [&'static Face], &'static [&'static Face]) {
    match family {
        FontFamily::Proportional => (&[&UI], &[&SYMBOL, &CJK]),
        FontFamily::Monospace => (&[], &[&SYMBOL, &CJK]),
        _ => (&[], &[]),
    }
}

/// Where this system keeps its fonts.
fn font_dirs() -> Vec<PathBuf> {
    if cfg!(windows) {
        // `%WINDIR%` rather than `C:` — Windows is not always on C.
        let mut dirs: Vec<PathBuf> = std::env::var_os("WINDIR")
            .map(|windir| vec![PathBuf::from(windir).join("Fonts")])
            .unwrap_or_default();
        // Fonts installed for one user rather than for the machine.
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(local).join("Microsoft").join("Windows").join("Fonts"));
        }
        dirs
    } else if cfg!(target_os = "macos") {
        ["/System/Library/Fonts", "/Library/Fonts"].iter().map(PathBuf::from).collect()
    } else {
        ["/usr/share/fonts/truetype/dejavu", "/usr/share/fonts/TTF", "/usr/share/fonts/opentype"]
            .iter()
            .map(PathBuf::from)
            .collect()
    }
}

impl Face {
    /// Reads the first of its files that is there, or nothing.
    fn load(&self) -> Option<FontData> {
        let dirs = font_dirs();
        // Files in preference order outside, directories inside: a better face
        // in a less usual place still beats a worse face in the usual one.
        for file in self.files {
            for dir in &dirs {
                if let Ok(bytes) = std::fs::read(dir.join(file)) {
                    return Some(FontData { index: self.index, ..FontData::from_owned(bytes) });
                }
            }
        }
        None
    }
}

/// Puts the system's own faces in front of the ones egui brought.
///
/// In front, not instead: egui's fonts stay at the end of both families, so a
/// character none of these cover still has somewhere to fall through to.
///
/// Returns the faces that were found. On a machine with none of them this does
/// nothing at all, and the tests have to be able to tell that from a pass.
pub fn install(ctx: &Context) -> Vec<&'static str> {
    let (fonts, found) = definitions();
    ctx.set_fonts(fonts);
    found
}

/// What [`install`] would install, without a [`Context`] to install it into.
///
/// Apart so the ordering can be looked at directly. Whether a face comes before
/// or after egui's own is most of what this module does, and it is not visible
/// from the far side of `set_fonts`.
fn definitions() -> (FontDefinitions, Vec<&'static str>) {
    let mut fonts = FontDefinitions::default();
    let mut found: Vec<&'static str> = Vec::new();

    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        let (before, after) = chain(&family);
        let mut load = |faces: &'static [&'static Face]| -> Vec<String> {
            let mut names = Vec::new();
            for face in faces {
                // Read once however many families ask for it.
                if !fonts.font_data.contains_key(face.name) {
                    let Some(data) = face.load() else { continue };
                    fonts.font_data.insert(face.name.to_owned(), Arc::new(data));
                    found.push(face.name);
                } else if !found.contains(&face.name) {
                    continue;
                }
                names.push(face.name.to_owned());
            }
            names
        };
        let (leading, trailing) = (load(before), load(after));

        let list = fonts.families.entry(family).or_default();
        for (at, name) in leading.into_iter().enumerate() {
            list.insert(at, name);
        }
        list.extend(trailing);
    }

    (fonts, found)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Signal names and paths are data. Boxes in place of them are a bug of a
    /// different order than a missing arrow.
    #[test]
    fn a_japanese_name_is_text_rather_than_boxes() {
        let ctx = Context::default();
        let found = install(&ctx);
        if !found.contains(&CJK.name) {
            eprintln!("no CJK face on this machine; nothing asserted");
            return;
        }
        let _ = ctx.run_ui(Default::default(), |_| {});
        let ok = ctx.fonts_mut(|fonts| {
            fonts.has_glyphs(&egui::FontId::proportional(13.0), "\u{6ce2}\u{5f62}")
                && fonts.has_glyphs(&egui::FontId::monospace(12.0), "\u{6ce2}\u{5f62}")
        });
        assert!(ok, "a Japanese name draws as boxes");
    }

    /// In front, not instead. Both halves matter: ours first, or the interface
    /// is still set in egui's font; egui's still there, or a character none of
    /// ours cover has nowhere left to fall through to.
    #[test]
    fn the_faces_found_come_first_and_the_shipped_ones_stay_behind() {
        let (fonts, found) = definitions();
        if found.is_empty() {
            eprintln!("no faces on this machine; nothing asserted");
            return;
        }
        let shipped = FontDefinitions::default();

        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            let ordered = &fonts.families[&family];
            let ours: Vec<&String> =
                ordered.iter().filter(|name| name.starts_with("rtlscope-")).collect();
            assert!(!ours.is_empty(), "nothing of ours reaches {family:?} at all");

            let tail: Vec<&String> =
                ordered.iter().filter(|name| !name.starts_with("rtlscope-")).collect();
            assert_eq!(
                tail,
                shipped.families[&family].iter().collect::<Vec<_>>(),
                "egui's own fallbacks for {family:?} are gone or reordered"
            );

            // A family we replace has to actually lead with the replacement;
            // one we only extend has to still lead with what egui shipped.
            match chain(&family).0.first() {
                Some(face) if found.contains(&face.name) => {
                    let first = ordered.first().map(|name| name.as_str());
                    assert_eq!(first, Some(face.name), "{family:?} leads with the wrong face");
                }
                _ => assert!(
                    ordered.first().is_some_and(|name| !name.starts_with("rtlscope-")),
                    "{family:?} is only meant to be extended, but we took the front of it"
                ),
            }
        }
    }
}
