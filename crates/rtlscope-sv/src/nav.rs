//! Navigating an `sv-parser` syntax tree.
//!
//! Whitespace and comments are nodes in this tree like any other, so anything
//! that asks "where does this construct start?" or "what did the author write
//! here?" has to step around them. These three helpers are the only places that
//! know that.

use sv_parser::{Locate, NodeEvent, RefNode, SyntaxTree, unwrap_node};

/// The first token of a subtree, skipping leading whitespace and comments.
///
/// This is what anchors a [`rtlscope_ir::Span`] to a construct: the span starts
/// where the construct's first real token starts.
pub fn first_token(node: RefNode<'_>) -> Option<Locate> {
    let mut whitespace_depth = 0usize;
    for event in node.into_iter().event() {
        match event {
            NodeEvent::Enter(RefNode::WhiteSpace(_)) => whitespace_depth += 1,
            NodeEvent::Leave(RefNode::WhiteSpace(_)) => {
                whitespace_depth = whitespace_depth.saturating_sub(1)
            }
            NodeEvent::Enter(RefNode::Locate(locate)) if whitespace_depth == 0 => {
                return Some(*locate);
            }
            _ => {}
        }
    }
    None
}

/// The source text of a subtree, with runs of whitespace collapsed to one space.
///
/// Used for the `text` of an `Unsupported` node, so a diagnostic can quote what
/// the author actually wrote rather than describing it second-hand.
pub fn subtree_text(tree: &SyntaxTree, node: RefNode<'_>) -> String {
    let mut out = String::new();
    let mut whitespace_depth = 0usize;
    for event in node.into_iter().event() {
        match event {
            NodeEvent::Enter(RefNode::WhiteSpace(_)) => whitespace_depth += 1,
            NodeEvent::Leave(RefNode::WhiteSpace(_)) => {
                whitespace_depth = whitespace_depth.saturating_sub(1)
            }
            NodeEvent::Enter(RefNode::Locate(locate)) => {
                if whitespace_depth == 0 {
                    if let Some(text) = tree.get_str(locate) {
                        out.push_str(text);
                    }
                } else if !out.is_empty() && !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            _ => {}
        }
    }
    out.trim_end().to_string()
}

/// The first identifier token in a subtree.
pub fn identifier(node: RefNode<'_>) -> Option<Locate> {
    match unwrap_node!(node, SimpleIdentifier, EscapedIdentifier) {
        Some(RefNode::SimpleIdentifier(x)) => Some(x.nodes.0),
        Some(RefNode::EscapedIdentifier(x)) => Some(x.nodes.0),
        _ => None,
    }
}

/// The text of the first identifier in a subtree.
pub fn identifier_str(tree: &SyntaxTree, node: RefNode<'_>) -> Option<String> {
    let locate = identifier(node)?;
    tree.get_str(&locate).map(str::to_owned)
}

/// The package a name is qualified with: the `defs` of `defs::WIDTH`, if the
/// subtree holds such a scope.
///
/// `sv-parser` does not know which names are packages, and tries a class
/// scope before a package scope wherever the grammar allows both — so
/// `defs::WIDTH` usually arrives as a *class* scope named `defs`. A class is
/// outside the subset, and a package is not, so a scope of either shape is
/// read as a package: the name it qualifies then resolves if a package by
/// that name was read, and is reported as unknown if not, which is right
/// either way.
pub fn package_of(tree: &SyntaxTree, node: RefNode<'_>) -> Option<String> {
    match unwrap_node!(node, PackageScopePackage, ClassScope) {
        Some(RefNode::PackageScopePackage(x)) => {
            identifier_str(tree, RefNode::PackageIdentifier(&x.nodes.0))
        }
        Some(scope @ RefNode::ClassScope(_)) => identifier_str(tree, scope),
        _ => None,
    }
}

/// `defs::WIDTH` from its two halves, or the bare name when there is no package.
pub fn qualified(package: Option<&str>, name: &str) -> String {
    match package {
        Some(package) => format!("{package}::{name}"),
        None => name.to_string(),
    }
}

/// A name as it was written, package and hierarchy included: `WIDTH`,
/// `defs::WIDTH`, `bus.valid`.
///
/// Read back from the tokens as text, on purpose. The grammar gives a
/// qualified or dotted name in several shapes — a package scope, a class
/// scope that is really a package, a hierarchical identifier, a member chain
/// — and the first identifier in any of them is the *package* or the
/// *instance*, not the name. The text is the same whatever the shape, and it
/// is the spelling every later pass binds and looks up. `None` for a name
/// with a bracket in it — an instance array, `u[0].x`, is outside the subset
/// — and for anything from `$root`.
pub fn name_str(tree: &SyntaxTree, node: RefNode<'_>) -> Option<String> {
    let text = joined_text(tree, node);
    if text.is_empty() || text.contains('[') || text.contains("$root") {
        return None;
    }
    Some(text)
}

/// The name an assignment target starts with: the `bus.valid` of
/// `bus.valid[3:0]`. The brackets are applied by the caller.
pub fn name_before_select(tree: &SyntaxTree, node: RefNode<'_>) -> Option<String> {
    let text = joined_text(tree, node);
    let name = text.split('[').next().unwrap_or("");
    if name.is_empty() || name.contains("$root") {
        return None;
    }
    Some(name.to_string())
}

/// The tokens of a subtree run together, with no whitespace at all.
fn joined_text(tree: &SyntaxTree, node: RefNode<'_>) -> String {
    subtree_text(tree, node).chars().filter(|c| !c.is_whitespace()).collect()
}
