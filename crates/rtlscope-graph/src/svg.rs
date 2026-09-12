//! SVG output for a [`DiagramGeom`].
//!
//! Written as plain strings rather than through a library: the output is a few
//! shape kinds, and a dependency would buy nothing but a build.
//!
//! Every box and wire carries a `<title>`, which browsers show as a tooltip.
//! That is where the source location goes, so the SVG on its own answers "which
//! line is this?" without the GUI.
//!
//! Coordinates are rounded to one decimal so a golden test compares stable
//! text rather than floating-point noise.

use std::fmt::Write as _;

use rtlscope_ir::FileTable;

use crate::block::WireKind;
use crate::geom::{BoxKind, DiagramGeom, NodeBox, Side, Wire};

#[derive(Debug, Clone, Copy, Default)]
pub struct SvgOptions {
    /// Clock and reset wires are hidden by default: drawing them puts a line
    /// from the clock to every flop, and the structure disappears behind them.
    pub show_clocks: bool,
}

pub fn render(geom: &DiagramGeom, files: &FileTable, options: &SvgOptions) -> String {
    let mut out = String::new();

    let _ = writeln!(
        out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" font-family="ui-monospace, SFMono-Regular, Menlo, monospace" font-size="11">"#,
        w = round(geom.width),
        h = round(geom.height)
    );
    let _ = writeln!(out, "<title>{}</title>", escape(&geom.module));
    let _ = writeln!(out, r##"<rect width="100%" height="100%" fill="#fbfcfd"/>"##);

    // Wires first, so a box is never drawn over by a line crossing it.
    let _ = writeln!(out, r#"<g fill="none" stroke-width="1.2">"#);
    for wire in &geom.wires {
        if !options.show_clocks && wire.kind != WireKind::Signal {
            continue;
        }
        write_wire(&mut out, wire, files);
    }
    let _ = writeln!(out, "</g>");

    for node in &geom.boxes {
        write_box(&mut out, node, files);
    }

    let _ = writeln!(out, "</svg>");
    out
}

fn write_wire(out: &mut String, wire: &Wire, files: &FileTable) {
    let points: Vec<String> =
        wire.points.iter().map(|point| format!("{},{}", round(point.x), round(point.y))).collect();

    let (stroke, dash) = match wire.kind {
        WireKind::Signal => ("#2b4e6e", ""),
        WireKind::Clock => ("#a93c0c", r#" stroke-dasharray="5 3""#),
        WireKind::Reset => ("#8a6412", r#" stroke-dasharray="2 3""#),
    };

    let _ = writeln!(
        out,
        r#"<polyline points="{}" stroke="{stroke}"{dash}><title>{} — {}</title></polyline>"#,
        points.join(" "),
        escape(&wire.label),
        escape(&files.render(wire.span))
    );

    // The label sits on the longest horizontal run, which is where there is
    // room for it.
    if let Some((x, y)) = label_anchor(wire) {
        let _ = writeln!(
            out,
            r##"<text x="{}" y="{}" fill="#54626f" font-size="9" text-anchor="middle">{}</text>"##,
            round(x),
            round(y - 3.0),
            escape(&wire.label)
        );
    }
}

/// The midpoint of the longest horizontal segment.
fn label_anchor(wire: &Wire) -> Option<(f64, f64)> {
    wire.points
        .windows(2)
        .filter(|pair| (pair[0].y - pair[1].y).abs() < 0.5)
        .max_by(|a, b| (a[0].x - a[1].x).abs().total_cmp(&(b[0].x - b[1].x).abs()))
        .filter(|pair| (pair[0].x - pair[1].x).abs() > 30.0)
        .map(|pair| ((pair[0].x + pair[1].x) / 2.0, pair[0].y))
}

fn write_box(out: &mut String, node: &NodeBox, files: &FileTable) {
    let (fill, stroke) = match node.kind {
        BoxKind::InputPort | BoxKind::OutputPort => ("#f1f4f6", "#8797a5"),
        BoxKind::Instance if node.blackbox => ("#f3e2da", "#a93c0c"),
        BoxKind::Instance => ("#ffffff", "#54626f"),
        BoxKind::Process => ("#ffffff", "#8797a5"),
    };
    let dash = if node.blackbox { r#" stroke-dasharray="4 3""# } else { "" };

    let _ = writeln!(out, "<g>");
    if node.comb {
        // A cloud, from the same bumps the window draws: outlines first, then
        // the fill over them, so only the outer envelope is left showing.
        let cloud = crate::geom::cloud(&node.rect);
        for bump in &cloud.bumps {
            let _ = writeln!(
                out,
                r#"<circle cx="{}" cy="{}" r="{}" fill="none" stroke="{stroke}"/>"#,
                round(bump.x),
                round(bump.y),
                round(cloud.radius)
            );
        }
        for bump in &cloud.bumps {
            let _ = writeln!(
                out,
                r#"<circle cx="{}" cy="{}" r="{}" fill="{fill}"/>"#,
                round(bump.x),
                round(bump.y),
                round(cloud.radius - 0.5)
            );
        }
        let _ = writeln!(
            out,
            r#"<rect x="{}" y="{}" width="{}" height="{}" fill="{fill}"/>"#,
            round(cloud.body.x),
            round(cloud.body.y),
            round(cloud.body.width),
            round(cloud.body.height)
        );
    } else {
        let _ = writeln!(
            out,
            r#"<rect x="{}" y="{}" width="{}" height="{}" rx="3" fill="{fill}" stroke="{stroke}"{dash}/>"#,
            round(node.rect.x),
            round(node.rect.y),
            round(node.rect.width),
            round(node.rect.height)
        );
    }
    let _ = writeln!(
        out,
        "<title>{}{} — {}</title>",
        escape(&node.label),
        node.sublabel.as_ref().map_or(String::new(), |s| format!(" : {}", escape(s))),
        escape(&files.render(node.span))
    );

    let centre = node.rect.x + node.rect.width / 2.0;
    let _ = writeln!(
        out,
        r##"<text x="{}" y="{}" text-anchor="middle" font-weight="600" fill="#14181d">{}</text>"##,
        round(centre),
        round(node.rect.y + 12.0),
        escape(&node.label)
    );
    if let Some(sublabel) = &node.sublabel {
        let _ = writeln!(
            out,
            r##"<text x="{}" y="{}" text-anchor="middle" font-size="9" fill="#8797a5">{}</text>"##,
            round(centre),
            round(node.rect.bottom() - 5.0),
            escape(sublabel)
        );
    }
    // A module RTLScope only half understood says so on the box itself (D2).
    if node.skipped > 0 {
        let _ = writeln!(
            out,
            r##"<text x="{}" y="{}" text-anchor="end" font-size="9" fill="#96331f">{} skipped</text>"##,
            round(node.rect.right() - 4.0),
            round(node.rect.y + 12.0),
            node.skipped
        );
    }

    for pin in &node.pins {
        let (dx, anchor) = match pin.side {
            Side::Left => (4.0, "start"),
            Side::Right => (-4.0, "end"),
        };
        let _ = writeln!(
            out,
            r#"<circle cx="{}" cy="{}" r="2" fill="{stroke}"/>"#,
            round(pin.at.x),
            round(pin.at.y)
        );
        let _ = writeln!(
            out,
            r##"<text x="{}" y="{}" text-anchor="{anchor}" font-size="9" fill="#54626f">{}</text>"##,
            round(pin.at.x + dx),
            round(pin.at.y + 3.0),
            escape(&pin.name)
        );
    }
    let _ = writeln!(out, "</g>");
}

fn round(value: f64) -> String {
    let rounded = (value * 10.0).round() / 10.0;
    if rounded.fract() == 0.0 { format!("{}", rounded as i64) } else { format!("{rounded}") }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinates_round_to_one_decimal() {
        assert_eq!(round(1.0), "1");
        assert_eq!(round(1.25), "1.3");
        assert_eq!(round(-0.04), "0");
    }

    #[test]
    fn markup_characters_in_a_name_are_escaped() {
        assert_eq!(escape("a<b & c>"), "a&lt;b &amp; c&gt;");
    }
}
