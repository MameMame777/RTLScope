//! The instance hierarchy, as a collapsible tree.
//!
//! Shows the design the way the RTL is organised, so a module buried three
//! levels down can be reached without drilling through every diagram on the
//! way. Each row is one instance; the label is the instance name and the
//! specialised module it is of, because `params_sub$W=16` and `params_sub$W=8`
//! are different boxes and the tree has to say which is which.

use egui::{CollapsingHeader, RichText, Ui};
use rtlscope_ir::{Design, ModuleId};

/// What the tree was asked for.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Asked {
    /// A module to look at, by clicking its row.
    pub open: Option<ModuleId>,
    /// A module to read the sources again with as the top.
    ///
    /// Not the same as looking at it: looking keeps the design and moves
    /// within it, while this makes the module *be* the design — the diagram,
    /// the analyses and the simulation all start from it, and everything above
    /// it is gone until the top is chosen again.
    pub top: Option<ModuleId>,
}

/// Draws the tree and returns what the user asked of it.
pub fn show(ui: &mut Ui, design: &Design, current: ModuleId) -> Asked {
    let top = design.top;
    let mut asked = Asked::default();
    // The salt is a traversal counter, not the module: one module instantiated
    // three times under one parent is three rows, and rows with one id fold
    // and unfold together — egui flags it as a duplicate-widget error.
    let mut salt = 0usize;
    node(
        ui,
        design,
        top,
        design.top_module().shown().into_owned(),
        current,
        &mut asked,
        0,
        &mut salt,
    );
    asked
}

/// The right-click menu on a row: make this module the top.
///
/// On the row rather than in a toolbar, because the question "which of these
/// is the one I mean" is asked while looking at the list of them. The current
/// top gets no such entry — it already is.
fn top_menu(response: &egui::Response, design: &Design, module: ModuleId, asked: &mut Asked) {
    if module == design.top {
        return;
    }
    response.context_menu(|ui| {
        if ui
            .button("set as top")
            .on_hover_text(
                "Read the sources again with this module as the top. The diagram, the \
                 analyses and the simulation all start from it.",
            )
            .clicked()
        {
            asked.top = Some(module);
            ui.close();
        }
    });
}

/// Depth limit for a design whose instance tree is somehow cyclic. Elaboration
/// has already rejected that, so this is only a guard against a future bug
/// drawing forever.
const MAX_DEPTH: usize = 64;

#[allow(clippy::too_many_arguments)]
fn node(
    ui: &mut Ui,
    design: &Design,
    module: ModuleId,
    label: String,
    current: ModuleId,
    asked: &mut Asked,
    depth: usize,
    salt: &mut usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    *salt += 1;
    let this_salt = *salt;
    let item = design.module(module);
    let is_current = module == current;

    // Identifiers are monospace everywhere in this UI; the row being looked at
    // takes the accent, and a module with no source wears the warning colour
    // the diagram gives its box.
    let theme = crate::theme::Theme::of(ui);
    let title = if item.is_blackbox {
        RichText::new(format!("{label}  (no source)")).monospace().italics().color(theme.warn)
    } else if is_current {
        RichText::new(&label).monospace().color(theme.accent).strong()
    } else {
        RichText::new(&label).monospace()
    };

    if item.insts.is_empty() {
        // A leaf is a plain clickable row; a header with no children reads as
        // if something is hidden inside.
        let row = ui.selectable_label(is_current, title);
        if row.clicked() {
            asked.open = Some(module);
        }
        top_menu(&row, design, module, asked);
        return;
    }

    let header = CollapsingHeader::new(title).id_salt(this_salt).default_open(depth < 2);

    let response = header.show(ui, |ui| {
        for inst in &item.insts {
            let child = design.module(inst.of);
            let child_label = format!("{} : {}", inst.shown(), child.shown());
            node(ui, design, inst.of, child_label, current, asked, depth + 1, salt);
        }
    });

    if response.header_response.clicked() {
        asked.open = Some(module);
    }
    top_menu(&response.header_response, design, module, asked);
}

/// The chain of modules from the top down to one module.
///
/// Used to rebuild the breadcrumb when the tree is used to jump somewhere,
/// rather than drilling in one level at a time. Returns just the top when there
/// is no path, which is the case for a module that is not instantiated.
pub fn path_to(design: &Design, target: ModuleId) -> Vec<Crumb> {
    let mut path = Vec::new();
    if walk(design, design.top, target, &mut path, 0) {
        return path;
    }
    vec![Crumb::top(design.top)]
}

/// The breadcrumb for an instance path, `u_rx.u_align`.
///
/// By path rather than by module, unlike [`path_to`]: a module instantiated
/// twice is two places, and picking one of them arbitrarily would put the
/// reader somewhere that merely looks like where they asked for. An empty path
/// is the top. A name that is not there stops the walk, leaving the reader as
/// deep as still exists.
pub fn crumbs_for(design: &Design, path: &str) -> Vec<Crumb> {
    let mut crumbs = vec![Crumb::top(design.top)];
    let mut at = design.top;
    for name in path.split('.').filter(|name| !name.is_empty()) {
        let Some(inst) = design.module(at).insts.iter().find(|inst| inst.name == name) else {
            break;
        };
        at = inst.of;
        crumbs.push(Crumb { module: at, instance: Some(name.to_string()) });
    }
    crumbs
}

/// One step of the way down: the module, and the instance of it that was
/// entered.
///
/// The instance name is what the dump knows a signal by — `u_rx.u_align.data`
/// — so navigation has to carry it, not just the module. A module reached
/// through the tree rather than by drilling in has no single instance, and says
/// so by leaving this `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crumb {
    pub module: ModuleId,
    pub instance: Option<String>,
}

impl Crumb {
    pub fn top(module: ModuleId) -> Self {
        Crumb { module, instance: None }
    }
}

/// The instance path of a chain of crumbs, in the form a flattened dump uses.
pub fn instance_path(crumbs: &[Crumb]) -> String {
    crumbs.iter().filter_map(|crumb| crumb.instance.as_deref()).collect::<Vec<_>>().join(".")
}

fn walk(
    design: &Design,
    from: ModuleId,
    target: ModuleId,
    path: &mut Vec<Crumb>,
    depth: usize,
) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    if path.is_empty() {
        path.push(Crumb::top(from));
    }
    if from == target {
        return true;
    }
    for inst in &design.module(from).insts {
        path.push(Crumb { module: inst.of, instance: Some(inst.name.clone()) });
        if walk(design, inst.of, target, path, depth + 1) {
            return true;
        }
        path.pop();
    }
    false
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

    fn names(design: &Design, path: &[Crumb]) -> Vec<String> {
        path.iter().map(|crumb| design.module(crumb.module).name.clone()).collect()
    }

    #[test]
    fn the_path_to_the_top_is_just_the_top() {
        let design = design("hier.sv", Some("hier_top"));
        assert_eq!(names(&design, &path_to(&design, design.top)), ["hier_top"]);
    }

    #[test]
    fn the_path_to_a_grandchild_lists_every_step() {
        // Jumping through the tree has to rebuild the whole breadcrumb, not
        // just append, or "back" would return somewhere the user never was.
        let design = design("params.sv", Some("params_top"));
        let (grandchild, _) =
            design.module_by_name("params_subsub$W=16").expect("the specialised grandchild");

        assert_eq!(
            names(&design, &path_to(&design, grandchild)),
            ["params_top", "params_sub$W=16", "params_subsub$W=16"]
        );
    }

    #[test]
    fn two_specialisations_have_different_paths() {
        // `params_sub$W=8` and `params_sub$W=16` are different boxes reached
        // through different instances; a tree that conflated them would send
        // the user to the wrong one.
        let design = design("params.sv", Some("params_top"));
        let (wide, _) = design.module_by_name("params_subsub$W=16").unwrap();
        let (narrow, _) = design.module_by_name("params_subsub$W=8").unwrap();

        assert_ne!(path_to(&design, wide), path_to(&design, narrow));
        assert!(names(&design, &path_to(&design, narrow)).contains(&"params_sub$W=8".to_string()));
    }

    /// Every step past the top carries the instance it was entered through,
    /// which is what a dump knows a signal by.
    #[test]
    fn the_path_carries_the_instances_it_went_through() {
        let design = design("params.sv", Some("params_top"));
        let (grandchild, _) = design.module_by_name("params_subsub$W=16").unwrap();
        let path = path_to(&design, grandchild);

        assert_eq!(path[0].instance, None, "the top was not entered through anything");
        assert!(path[1..].iter().all(|crumb| crumb.instance.is_some()), "{path:#?}");
        // And the instance path reads the way a flattened dump names things.
        let joined = instance_path(&path);
        assert_eq!(joined.matches('.').count(), 1, "two instances deep: `{joined}`");
    }

    #[test]
    fn a_module_outside_the_tree_falls_back_to_the_top() {
        let design = design("hier.sv", Some("hier_top"));
        // hier_alu is in the design, but ask for something that is not reachable
        // from the top by pretending an out-of-range id.
        let unreachable = ModuleId::from_raw(u32::MAX);
        assert_eq!(path_to(&design, unreachable), vec![Crumb::top(design.top)]);
    }
}
