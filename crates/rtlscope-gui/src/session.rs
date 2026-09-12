//! Where the reader was, in terms that survive the sources being read again.
//!
//! A `ModuleId`, a `NetId` and a `BlockNode` are all positions in an arena.
//! They mean something only for the design they came from, so after a reparse
//! they do not point at what they used to — they point at whatever is now in
//! that slot, which is worse than pointing at nothing. Anything that has to
//! outlive a read is therefore carried by **name**: the instance path down to
//! the module being drawn, the label on the selected box, the nets that are
//! lit, the file and line the Source tab is on.
//!
//! That is also why this cannot always succeed, and why it says so. Reading
//! again is exactly the moment when a module might have been deleted, an
//! instance renamed, a net taken out. Putting the reader back on something
//! that merely happens to sit where the old thing sat would be a lie about
//! their own design, and landing silently at the top would leave them
//! wondering how they got there. So every step that cannot be followed is
//! named, and the window says what it could not put back.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use egui::Rect;
use rtlscope_ir::{Design, FileTable, ModuleId, NetId, Span};

use crate::tree::Crumb;
use crate::views::Hop;

/// A place in a design, written down so it can be found again in the next one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bookmark {
    /// The top the reader was under. Kept to notice when reading again picked
    /// a different one, which happens when a module stops being instantiated.
    pub top: Option<String>,
    /// The instance names walked down from the top, the top itself excluded.
    pub instances: Vec<String>,
    /// The label on the selected box.
    pub selected: Option<String>,
    /// The lit nets, by name, in the module being drawn.
    pub highlighted: Vec<String>,
    /// Where the view was, per module *name*. A `BTreeMap` so two bookmarks of
    /// one place compare equal, which a test can then rest on.
    pub camera: BTreeMap<String, Rect>,
    /// The state machine the FSM tab was on, as `module.state`.
    pub fsm: Option<String>,
    pub pipeline_selected: usize,
    /// The file and position the Source tab was looking at.
    pub source_at: Option<(PathBuf, u32, u32, u32)>,
    /// The provenance trail: per hop, the instance names down to it and the
    /// name of the net there.
    pub trail: Vec<(Vec<String>, String)>,
}

/// What a bookmark came to in a design read again.
#[derive(Debug, Clone, Default)]
pub struct Restored {
    pub path: Vec<Crumb>,
    pub camera: HashMap<ModuleId, Rect>,
    pub highlighted: HashSet<NetId>,
    pub source: Option<Span>,
    pub trail: Vec<Hop>,
    /// What could not be put back, in words for the reader. Empty means
    /// everything came back.
    pub lost: Vec<String>,
}

impl Bookmark {
    /// Finds this place in a design that has been read again.
    ///
    /// Everything resolvable without laying a diagram out or running an
    /// analysis is resolved here; the selected box and the chosen machine stay
    /// as names, because finding those means building things the caller is
    /// about to build anyway.
    pub fn resolve(&self, design: &Design, files: &FileTable) -> Restored {
        let mut lost = Vec::new();

        if let Some(was) = &self.top {
            let now = &design.module(design.top).name;
            if now != was {
                lost.push(format!("the top is `{now}` now, not `{was}`"));
            }
        }

        let path = self.follow(design, &mut lost);
        let current = path.last().map_or(design.top, |crumb| crumb.module);

        Restored {
            camera: self.camera(design),
            highlighted: self.nets(design, current, &mut lost),
            source: self.span(files, &mut lost),
            trail: self.trail(design, &mut lost),
            path,
            lost,
        }
    }

    /// The provenance trail, hop by hop.
    ///
    /// Unlike the breadcrumb, a trail that stops early is not a shorter answer
    /// to the same question — it is the wrong answer to a different one, since
    /// each hop is there because the one after it was asked about. So the
    /// first hop that cannot be found ends the trail, and says where.
    fn trail(&self, design: &Design, lost: &mut Vec<String>) -> Vec<Hop> {
        let mut out = Vec::new();
        'hops: for (instances, net) in &self.trail {
            let mut path = vec![Crumb::top(design.top)];
            let mut at = design.top;
            for name in instances {
                let Some(inst) = design.module(at).insts.iter().find(|inst| inst.name == *name)
                else {
                    lost.push(trail_stops(instances, net));
                    break 'hops;
                };
                at = inst.of;
                path.push(Crumb { module: at, instance: Some(name.clone()) });
            }
            let found =
                design.module(at).nets.iter_enumerated().find(|(_, have)| have.name == *net);
            let Some((id, _)) = found else {
                lost.push(trail_stops(instances, net));
                break;
            };
            out.push(Hop { path, net: id });
        }
        out
    }

    /// The breadcrumb again, walked down by instance name from the new top.
    ///
    /// Stops at the last step that still exists rather than giving up on the
    /// whole path: a reader who deleted the innermost instance still wants to
    /// be standing where it was.
    fn follow(&self, design: &Design, lost: &mut Vec<String>) -> Vec<Crumb> {
        let mut path = vec![Crumb::top(design.top)];
        let mut at = design.top;
        for (step, name) in self.instances.iter().enumerate() {
            let Some(inst) = design.module(at).insts.iter().find(|inst| inst.name == *name) else {
                lost.push(format!("`{}` is not there any more", self.instances[step..].join(".")));
                break;
            };
            at = inst.of;
            path.push(Crumb { module: inst.of, instance: Some(name.clone()) });
        }
        path
    }

    /// Where the view was, re-keyed. A module that has gone takes its view with
    /// it, which is not worth telling anyone about — nobody was looking at it.
    fn camera(&self, design: &Design) -> HashMap<ModuleId, Rect> {
        self.camera
            .iter()
            .filter_map(|(name, rect)| Some((design.module_by_name(name)?.0, *rect)))
            .collect()
    }

    /// The lit nets, looked up by name in the module now being drawn.
    fn nets(&self, design: &Design, module: ModuleId, lost: &mut Vec<String>) -> HashSet<NetId> {
        if self.highlighted.is_empty() {
            return HashSet::new();
        }
        let module = design.module(module);
        let mut found = HashSet::new();
        let mut gone = 0usize;
        for name in &self.highlighted {
            match module.nets.iter_enumerated().find(|(_, net)| net.name == *name) {
                Some((id, _)) => {
                    found.insert(id);
                }
                None => gone += 1,
            }
        }
        if gone > 0 {
            lost.push(format!("{gone} highlighted signal(s) are gone"));
        }
        found
    }

    /// The source location, if that file is still one the design was read from.
    ///
    /// The line is kept as it was rather than tracked through the edit: this
    /// knows the file changed, not how. The same line is right far more often
    /// than the top of the file is, and it is honest about being a place rather
    /// than a thing.
    fn span(&self, files: &FileTable, lost: &mut Vec<String>) -> Option<Span> {
        let (path, line, col, len) = self.source_at.as_ref()?;
        match files.iter().find(|(_, known)| *known == path.as_path()) {
            Some((file, _)) => Some(Span::new(file, *line, *col, *len)),
            None => {
                lost.push(format!(
                    "`{}` is no longer part of the design",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ));
                None
            }
        }
    }
}

/// Why a trail could not be walked all the way back.
fn trail_stops(instances: &[String], net: &str) -> String {
    let at = match instances.is_empty() {
        true => net.to_string(),
        false => format!("{}.{net}", instances.join(".")),
    };
    format!("the trail stops at `{at}`, which is not there any more")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtlscope_sv::ParseOptions;

    fn design(fixture: &str, top: Option<&str>) -> Design {
        let path = rtlscope_fixtures::path(fixture);
        let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
        rtlscope_elab::elaborate(&uir, top).0.expect("elaborates")
    }

    fn at(design: &Design, instances: &[&str]) -> Bookmark {
        Bookmark {
            top: Some(design.module(design.top).name.clone()),
            instances: instances.iter().map(|name| (*name).to_string()).collect(),
            ..Bookmark::default()
        }
    }

    #[test]
    fn a_path_that_still_exists_comes_back_whole() {
        let design = design("hier.sv", Some("hier_top"));
        let inner = design.module(design.top).insts[0].name.clone();

        let restored = at(&design, &[&inner]).resolve(&design, &design.files);

        assert_eq!(restored.path.len(), 2, "the top and the instance");
        assert_eq!(restored.path[1].instance.as_deref(), Some(inner.as_str()));
        assert!(restored.lost.is_empty(), "{:?}", restored.lost);
    }

    /// Reading again is exactly when something might have been deleted. The
    /// reader is put as deep as still exists, and told what is missing.
    #[test]
    fn a_step_that_has_gone_stops_the_walk_and_is_named() {
        let design = design("hier.sv", Some("hier_top"));

        let restored = at(&design, &["u_not_here", "u_deeper"]).resolve(&design, &design.files);

        assert_eq!(restored.path.len(), 1, "back at the top, not guessing");
        assert_eq!(restored.lost.len(), 1, "{:?}", restored.lost);
        assert!(restored.lost[0].contains("u_not_here.u_deeper"), "{:?}", restored.lost);
    }

    /// A camera position belongs to a module, and modules are found by name
    /// because their ids mean nothing in the design read next.
    #[test]
    fn the_view_follows_a_module_across_a_read() {
        let design = design("hier.sv", Some("hier_top"));
        let name = design.module(design.top).name.clone();
        let rect = Rect::from_min_max(egui::pos2(1.0, 2.0), egui::pos2(3.0, 4.0));

        let bookmark = Bookmark {
            camera: BTreeMap::from([(name, rect), ("gone_module".into(), rect)]),
            ..Bookmark::default()
        };
        let restored = bookmark.resolve(&design, &design.files);

        assert_eq!(restored.camera.get(&design.top), Some(&rect));
        assert_eq!(restored.camera.len(), 1, "the module that went took its view with it");
    }

    #[test]
    fn highlighted_signals_that_have_gone_are_counted_not_dropped() {
        let design = design("counter.sv", None);
        let known = design.module(design.top).nets.iter().next().expect("a net").name.clone();

        let bookmark =
            Bookmark { highlighted: vec![known, "no_such_net".into()], ..Bookmark::default() };
        let restored = bookmark.resolve(&design, &design.files);

        assert_eq!(restored.highlighted.len(), 1);
        assert_eq!(restored.lost.len(), 1, "{:?}", restored.lost);
        assert!(restored.lost[0].contains('1'), "{:?}", restored.lost);
    }

    #[test]
    fn the_source_position_comes_back_when_the_file_is_still_read() {
        let design = design("counter.sv", None);
        let (_, path) = design.files.iter().next().expect("one file");
        let path = path.to_path_buf();

        let bookmark = Bookmark { source_at: Some((path, 7, 3, 4)), ..Bookmark::default() };
        let restored = bookmark.resolve(&design, &design.files);

        let span = restored.source.expect("the file is still there");
        assert_eq!((span.line, span.col, span.len), (7, 3, 4));
        assert!(restored.lost.is_empty(), "{:?}", restored.lost);
    }

    #[test]
    fn a_file_no_longer_in_the_design_is_said_out_loud() {
        let design = design("counter.sv", None);

        let bookmark = Bookmark {
            source_at: Some((PathBuf::from("/gone/away.sv"), 1, 1, 1)),
            ..Bookmark::default()
        };
        let restored = bookmark.resolve(&design, &design.files);

        assert!(restored.source.is_none());
        assert_eq!(restored.lost.len(), 1, "{:?}", restored.lost);
        assert!(restored.lost[0].contains("away.sv"), "{:?}", restored.lost);
    }

    /// A trail is a chain of questions, and it is carried by name like
    /// everything else here.
    #[test]
    fn a_trail_comes_back_across_a_read() {
        let design = design("hier.sv", Some("hier_top"));
        let inner = design.module(design.top).insts[0].name.clone();

        let bookmark = Bookmark {
            trail: vec![(vec![], "alu_y".to_string()), (vec![inner.clone()], "y".to_string())],
            ..Bookmark::default()
        };
        let restored = bookmark.resolve(&design, &design.files);

        assert_eq!(restored.trail.len(), 2, "{:?}", restored.lost);
        assert_eq!(restored.trail[0].path.len(), 1, "the top");
        assert_eq!(restored.trail[1].path.len(), 2, "the top and the instance");
        assert_eq!(restored.trail[1].path[1].instance.as_deref(), Some(inner.as_str()));
        assert!(restored.lost.is_empty(), "{:?}", restored.lost);
    }

    /// Half a trail answers none of the questions that made it, so it stops at
    /// the first hop that has gone — and says which.
    #[test]
    fn a_trail_stops_where_a_signal_has_gone() {
        let design = design("hier.sv", Some("hier_top"));

        let bookmark = Bookmark {
            trail: vec![
                (vec![], "alu_y".to_string()),
                (vec![], "no_such_net".to_string()),
                (vec![], "rf_rdata".to_string()),
            ],
            ..Bookmark::default()
        };
        let restored = bookmark.resolve(&design, &design.files);

        assert_eq!(restored.trail.len(), 1, "the hops after the missing one go too");
        assert_eq!(restored.lost.len(), 1, "{:?}", restored.lost);
        assert!(restored.lost[0].contains("no_such_net"), "{:?}", restored.lost);
    }

    /// Nothing bookmarked is not something to complain about.
    #[test]
    fn an_empty_bookmark_loses_nothing() {
        let design = design("counter.sv", None);
        let restored = Bookmark::default().resolve(&design, &design.files);

        assert_eq!(restored.path.len(), 1);
        assert!(restored.lost.is_empty(), "{:?}", restored.lost);
    }
}
