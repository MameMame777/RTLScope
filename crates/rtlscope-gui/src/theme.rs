//! The one place a colour is allowed to come from.
//!
//! Every widget egui draws is styled through [`install`], and everything
//! RTLScope draws itself — the diagram, the waveform, the badges — asks
//! [`Theme::of`] for its colours. A hex value written anywhere else is a bug:
//! it is how one panel drifts into its own idea of what "warning" looks like,
//! and how a colour that reads on one background becomes invisible on the
//! other.
//!
//! The palette is the same one the project's roadmap page uses: a cool grey
//! ground, teal as the single accent, and green/amber/red reserved for
//! meaning — never for decoration. The hot colours in the diagram are the
//! exception that proves it: clocks and resets are drawn warm because that is
//! what a schematic reader expects, and they are meaning too.

use egui::style::Selection;
use egui::{Color32, Context, CornerRadius, FontId, Margin, Stroke, Style, TextStyle, Vec2};

/// Every colour the application uses, for one of the two grounds.
pub struct Theme {
    // ---- the chrome ----
    /// The window ground, behind the panels.
    pub bg: Color32,
    /// Panels and cards.
    pub surface: Color32,
    /// A surface that must read as raised against `surface`: buttons, inputs.
    pub surface_alt: Color32,
    pub ink: Color32,
    pub muted: Color32,
    pub line: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,

    // ---- meaning ----
    pub ok: Color32,
    pub ok_soft: Color32,
    pub warn: Color32,
    pub warn_soft: Color32,
    pub err: Color32,
    pub err_soft: Color32,

    // ---- the diagram ----
    pub canvas_bg: Color32,
    pub grid: Color32,
    pub box_fill: Color32,
    pub box_stroke: Color32,
    pub port_fill: Color32,
    pub blackbox_fill: Color32,
    pub blackbox_stroke: Color32,
    pub wire: Color32,
    pub clock: Color32,
    pub reset: Color32,
    pub highlight: Color32,

    // ---- the waveform ----
    pub wave_line: Color32,
    pub wave_busy: Color32,
    pub cursor: Color32,
}

pub const LIGHT: Theme = Theme {
    bg: Color32::from_rgb(0xf2, 0xf5, 0xf7),
    surface: Color32::from_rgb(0xff, 0xff, 0xff),
    surface_alt: Color32::from_rgb(0xe9, 0xed, 0xf0),
    ink: Color32::from_rgb(0x1a, 0x25, 0x30),
    muted: Color32::from_rgb(0x5a, 0x6b, 0x7a),
    line: Color32::from_rgb(0xd9, 0xe0, 0xe6),
    accent: Color32::from_rgb(0x0e, 0x74, 0x90),
    accent_soft: Color32::from_rgb(0xe0, 0xf1, 0xf5),

    ok: Color32::from_rgb(0x15, 0x73, 0x47),
    ok_soft: Color32::from_rgb(0xe3, 0xf2, 0xe9),
    warn: Color32::from_rgb(0x92, 0x61, 0x0a),
    warn_soft: Color32::from_rgb(0xf7, 0xed, 0xd8),
    err: Color32::from_rgb(0x96, 0x33, 0x1f),
    err_soft: Color32::from_rgb(0xf9, 0xe5, 0xe0),

    canvas_bg: Color32::from_rgb(0xfb, 0xfc, 0xfd),
    grid: Color32::from_rgb(0xf0, 0xf3, 0xf5),
    box_fill: Color32::from_rgb(0xff, 0xff, 0xff),
    box_stroke: Color32::from_rgb(0x54, 0x62, 0x6f),
    port_fill: Color32::from_rgb(0xf1, 0xf4, 0xf6),
    blackbox_fill: Color32::from_rgb(0xf3, 0xe2, 0xda),
    blackbox_stroke: Color32::from_rgb(0xa9, 0x3c, 0x0c),
    wire: Color32::from_rgb(0x2b, 0x4e, 0x6e),
    clock: Color32::from_rgb(0xa9, 0x3c, 0x0c),
    reset: Color32::from_rgb(0x8a, 0x64, 0x12),
    highlight: Color32::from_rgb(0x0e, 0x74, 0x90),

    wave_line: Color32::from_rgb(0x1e, 0x7a, 0x4d),
    wave_busy: Color32::from_rgb(0x86, 0xba, 0x9e),
    cursor: Color32::from_rgb(0xa9, 0x3c, 0x0c),
};

pub const DARK: Theme = Theme {
    bg: Color32::from_rgb(0x10, 0x16, 0x1c),
    surface: Color32::from_rgb(0x18, 0x20, 0x28),
    surface_alt: Color32::from_rgb(0x21, 0x2b, 0x35),
    ink: Color32::from_rgb(0xe2, 0xe8, 0xed),
    muted: Color32::from_rgb(0x90, 0xa2, 0xb1),
    line: Color32::from_rgb(0x2a, 0x35, 0x40),
    accent: Color32::from_rgb(0x3c, 0xc1, 0xd8),
    accent_soft: Color32::from_rgb(0x12, 0x33, 0x3c),

    ok: Color32::from_rgb(0x4c, 0xc3, 0x8a),
    ok_soft: Color32::from_rgb(0x14, 0x31, 0x23),
    warn: Color32::from_rgb(0xe0, 0xb2, 0x5c),
    warn_soft: Color32::from_rgb(0x33, 0x29, 0x0f),
    err: Color32::from_rgb(0xe0, 0x7a, 0x6a),
    err_soft: Color32::from_rgb(0x3a, 0x1c, 0x16),

    canvas_bg: Color32::from_rgb(0x0e, 0x12, 0x16),
    grid: Color32::from_rgb(0x14, 0x1a, 0x20),
    box_fill: Color32::from_rgb(0x16, 0x1c, 0x22),
    box_stroke: Color32::from_rgb(0x94, 0xa3, 0xb1),
    port_fill: Color32::from_rgb(0x1d, 0x24, 0x2b),
    blackbox_fill: Color32::from_rgb(0x2e, 0x1e, 0x16),
    blackbox_stroke: Color32::from_rgb(0xe0, 0x82, 0x4f),
    wire: Color32::from_rgb(0x83, 0xaa, 0xcb),
    clock: Color32::from_rgb(0xe0, 0x82, 0x4f),
    reset: Color32::from_rgb(0xd3, 0xac, 0x58),
    highlight: Color32::from_rgb(0x3c, 0xc1, 0xd8),

    wave_line: Color32::from_rgb(0x78, 0xc8, 0x96),
    wave_busy: Color32::from_rgb(0x4a, 0x77, 0x5e),
    cursor: Color32::from_rgb(0xe0, 0x82, 0x4f),
};

impl Theme {
    /// The palette for the ground being drawn on.
    pub fn of(ui: &egui::Ui) -> &'static Theme {
        if ui.visuals().dark_mode { &DARK } else { &LIGHT }
    }
}

/// Styles both of egui's themes, once, at startup.
///
/// The preference itself is left at its default — follow the system — and the
/// toolbar's toggle changes it from there.
pub fn install(ctx: &Context) {
    ctx.style_mut_of(egui::Theme::Light, |style| apply(style, &LIGHT));
    ctx.style_mut_of(egui::Theme::Dark, |style| apply(style, &DARK));
}

fn apply(style: &mut Style, theme: &Theme) {
    // ---- type ----
    // egui's built-in faces, at a scale where identifiers read as identifiers:
    // anything that names a net or a module is monospace, and the proportional
    // face is for sentences.
    style.text_styles = [
        (TextStyle::Heading, FontId::proportional(15.0)),
        (TextStyle::Body, FontId::proportional(13.0)),
        (TextStyle::Button, FontId::proportional(13.0)),
        (TextStyle::Monospace, FontId::monospace(12.0)),
        (TextStyle::Small, FontId::proportional(10.5)),
    ]
    .into();

    // ---- space ----
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(10.0, 4.0);
    style.spacing.menu_margin = 8.0.into();
    style.spacing.indent = 16.0;
    style.spacing.scroll.bar_width = 7.0;
    style.spacing.scroll.floating = true;

    // A wider band to catch a splitter by. egui's five pixels are enough when
    // what lies beside the splitter is a list, and not enough here: the panes
    // are drawings that pan when dragged, so a miss by six pixels does not do
    // nothing, it throws the diagram off the screen. Measured on this window,
    // aiming at the seam under the diagram.
    style.interaction.resize_grab_radius_side = 9.0;

    // ---- surfaces ----
    let visuals = &mut style.visuals;
    visuals.panel_fill = theme.bg;
    visuals.window_fill = theme.surface;
    visuals.extreme_bg_color = theme.surface;
    visuals.faint_bg_color = theme.surface_alt;
    visuals.window_stroke = Stroke::new(1.0, theme.line);
    visuals.hyperlink_color = theme.accent;
    visuals.selection =
        Selection { bg_fill: theme.accent_soft, stroke: Stroke::new(1.0, theme.accent) };
    visuals.warn_fg_color = theme.warn;
    visuals.error_fg_color = theme.err;

    // ---- widgets ----
    // One radius everywhere; a UI with three different roundings reads as
    // three different UIs.
    let radius = CornerRadius::same(4);
    let widgets = &mut visuals.widgets;

    widgets.noninteractive.bg_fill = theme.surface;
    widgets.noninteractive.weak_bg_fill = theme.surface;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, theme.line);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0, theme.ink);
    widgets.noninteractive.corner_radius = radius;

    widgets.inactive.bg_fill = theme.surface_alt;
    widgets.inactive.weak_bg_fill = theme.surface_alt;
    widgets.inactive.bg_stroke = Stroke::new(1.0, theme.line);
    widgets.inactive.fg_stroke = Stroke::new(1.0, theme.ink);
    widgets.inactive.corner_radius = radius;

    widgets.hovered.bg_fill = theme.accent_soft;
    widgets.hovered.weak_bg_fill = theme.accent_soft;
    widgets.hovered.bg_stroke = Stroke::new(1.0, theme.accent);
    widgets.hovered.fg_stroke = Stroke::new(1.2, theme.ink);
    widgets.hovered.corner_radius = radius;

    widgets.active.bg_fill = theme.accent_soft;
    widgets.active.weak_bg_fill = theme.accent_soft;
    widgets.active.bg_stroke = Stroke::new(1.2, theme.accent);
    widgets.active.fg_stroke = Stroke::new(1.2, theme.ink);
    widgets.active.corner_radius = radius;

    widgets.open.bg_fill = theme.surface_alt;
    widgets.open.weak_bg_fill = theme.surface_alt;
    widgets.open.bg_stroke = Stroke::new(1.0, theme.line);
    widgets.open.fg_stroke = Stroke::new(1.0, theme.ink);
    widgets.open.corner_radius = radius;
}

/// The dock, dressed in the same palette as everything else.
///
/// Built here rather than beside the dock so this file stays what its own doc
/// comment says it is: the one place a colour comes from. `Style::from_egui`
/// derives what it can from the style installed above, and everything a tab
/// strip needs that a button does not is named below.
pub fn dock_style(ui: &egui::Ui) -> egui_dock::Style {
    let theme = Theme::of(ui);
    let radius = CornerRadius::same(4);
    let mut style = egui_dock::Style::from_egui(ui.style());

    style.dock_area_padding = None;
    style.main_surface_border_stroke = Stroke::NONE;
    style.main_surface_border_rounding = CornerRadius::ZERO;

    // A band to catch a seam by, and a floor under how small a pane can be
    // dragged. egui's own panels were given a nine-pixel radius for the same
    // reason (see `apply`): the panes here are drawings that pan when dragged,
    // so missing a seam does not do nothing, it throws the picture off screen.
    // The floor is what stops a drag past the end leaving a pane that has to be
    // found again before it can be used.
    style.separator.width = 1.0;
    style.separator.extra_interact_width = 10.0;
    style.separator.extra = 120.0;
    style.separator.color_idle = theme.line;
    style.separator.color_hovered = theme.accent;
    style.separator.color_dragged = theme.accent;

    style.tab_bar.bg_fill = theme.bg;
    style.tab_bar.hline_color = theme.line;
    style.tab_bar.corner_radius = CornerRadius::ZERO;
    style.tab_bar.inner_margin = Margin::symmetric(2, 0);
    // Tabs at their own width rather than stretched to fill the strip: a group
    // holding one view should not label it across the whole pane.
    style.tab_bar.fill_tab_bar = false;

    // Three states, and the ground says which. A tab in front is on the same
    // surface as its content so the two read as one sheet; a tab behind sits on
    // the window ground, like the strip it is in.
    let behind = egui_dock::TabInteractionStyle {
        outline_color: theme.line,
        corner_radius: radius,
        bg_fill: theme.bg,
        text_color: theme.muted,
    };
    let front = egui_dock::TabInteractionStyle {
        outline_color: theme.line,
        corner_radius: radius,
        bg_fill: theme.surface,
        text_color: theme.ink,
    };
    let under_the_pointer = egui_dock::TabInteractionStyle {
        outline_color: theme.accent,
        corner_radius: radius,
        bg_fill: theme.accent_soft,
        text_color: theme.ink,
    };
    style.tab.inactive = behind.clone();
    style.tab.inactive_with_kb_focus = behind;
    style.tab.active = front.clone();
    style.tab.focused = front.clone();
    style.tab.active_with_kb_focus = front.clone();
    style.tab.focused_with_kb_focus = front;
    style.tab.hovered = under_the_pointer;
    style.tab.hline_below_active_tab_name = true;

    // Small, because a pane is short of space and every view already spaces
    // itself; the margin is here only so text does not touch the seam.
    style.tab.tab_body = egui_dock::TabBodyStyle {
        inner_margin: Margin::same(4),
        stroke: Stroke::NONE,
        corner_radius: CornerRadius::ZERO,
        bg_fill: theme.surface,
    };

    style.buttons.close_tab_color = theme.muted;
    style.buttons.close_tab_active_color = theme.ink;
    style.buttons.close_tab_bg_fill = theme.accent_soft;
    style.buttons.close_all_tabs_color = theme.muted;
    style.buttons.close_all_tabs_active_color = theme.ink;
    style.buttons.close_all_tabs_bg_fill = theme.accent_soft;
    style.buttons.collapse_tabs_color = theme.muted;
    style.buttons.collapse_tabs_active_color = theme.ink;
    style.buttons.collapse_tabs_bg_fill = theme.accent_soft;

    // Where a dragged tab would land, shown while it is still in the air.
    style.overlay.selection_color = theme.accent.gamma_multiply(0.35);
    style.overlay.button_color = theme.muted;
    style.overlay.button_border_stroke = Stroke::new(1.0, theme.line);

    style
}

/// A colour for stage `n`, distinct from its neighbours on either ground.
///
/// The palette elsewhere in this application is three colours with meanings.
/// A pipeline's stages have no meanings — stage 4 is not worse than stage 3 —
/// so what they need is the opposite: a sequence with no ordering implied,
/// where any two are told apart at a glance. Hues are stepped by the golden
/// ratio, which is the standard way to get that: no small number of stages
/// lands on the same hue twice, and there is no cycle length to collide with.
///
/// Saturation and value are fixed per ground so every stage reads at the same
/// weight, and so the ink written over them stays legible.
pub fn stage_colour(stage: usize, dark: bool) -> Color32 {
    // 0.618… turns of the wheel per step.
    const STEP: f32 = 0.618_034;
    let hue = (0.08 + STEP * stage as f32).fract();
    let (saturation, value) = if dark { (0.52, 0.62) } else { (0.42, 0.90) };
    egui::ecolor::Hsva::new(hue, saturation, value, 1.0).into()
}

/// A pill that carries meaning: a count of errors, a synchroniser verdict, a
/// state's fate. `strong` is the text and border, `soft` the fill, so the same
/// pair works on both grounds.
pub fn badge(ui: &mut egui::Ui, text: &str, strong: Color32, soft: Color32) -> egui::Response {
    let galley =
        egui::WidgetText::from(egui::RichText::new(text).size(10.5).monospace().color(strong))
            .into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, TextStyle::Small);

    let padding = Vec2::new(7.0, 2.5);
    let (rect, response) =
        ui.allocate_exact_size(galley.size() + padding * 2.0, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        ui.painter().rect_filled(rect, CornerRadius::same(8), soft);
        ui.painter().galley(rect.min + padding, galley, strong);
    }
    response
}

/// The clock-edge mark, drawn rather than shipped as an image so it takes the
/// accent colour of whichever ground it is on.
pub fn brand_mark(ui: &mut egui::Ui, theme: &Theme) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(22.0, 14.0), egui::Sense::hover());
    let painter = ui.painter();
    let x = |f: f32| rect.left() + rect.width() * f;
    let (top, bottom) = (rect.top() + 2.0, rect.bottom() - 2.0);
    let points = vec![
        egui::Pos2::new(x(0.0), bottom),
        egui::Pos2::new(x(0.2), bottom),
        egui::Pos2::new(x(0.2), top),
        egui::Pos2::new(x(0.55), top),
        egui::Pos2::new(x(0.55), bottom),
        egui::Pos2::new(x(0.9), bottom),
        egui::Pos2::new(x(0.9), top),
        egui::Pos2::new(x(1.0), top),
    ];
    painter.add(egui::Shape::line(points, Stroke::new(2.0, theme.accent)));
}
