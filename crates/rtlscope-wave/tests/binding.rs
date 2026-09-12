//! What the auto-binder makes of the design it was measured against.
//!
//! Skips when that design is not in the checkout: those sources are the user's,
//! not the tool's.

use rtlscope_sv::ParseOptions;
use rtlscope_wave::bind;

fn rtl(relative: &str) -> Option<std::path::PathBuf> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/rtl")
        .join(relative);
    path.exists().then_some(path)
}

fn suggest_for(file: &str, top: &str) -> Option<Vec<bind::Suggestion>> {
    let path = rtl(file)?;
    let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    let (design, _) = rtlscope_elab::elaborate(&uir, Some(top));
    let design = design?;
    Some(bind::suggest(&design, design.top, ""))
}

/// A pixel-stream filter is found, with its markers.
#[test]
fn a_pixel_stream_is_recognised_with_its_markers() {
    let Some(found) = suggest_for("img_proc/axis_rgb_dither.sv", "axis_rgb_dither") else {
        eprintln!("skipping: the user's design is not in this checkout");
        return;
    };

    let pixel: Vec<&bind::Suggestion> = found.iter().filter(|s| s.protocol == "pixel").collect();
    assert!(!pixel.is_empty(), "{found:#?}");

    // Both ends of the filter: what comes in and what goes out.
    let groups: Vec<&str> = pixel.iter().map(|s| s.group.as_str()).collect();
    assert!(groups.contains(&"in"), "{groups:?}");
    assert!(groups.contains(&"out"), "{groups:?}");

    let inbound = pixel.iter().find(|s| s.group == "in").unwrap();
    let roles: Vec<&str> = inbound.bindings.iter().map(|b| b.role.as_str()).collect();
    for wanted in ["clock", "valid", "data", "sof", "eol", "eof"] {
        assert!(roles.contains(&wanted), "{roles:?} for {inbound:#?}");
    }
}

/// The camera's two-wire bus, with the drive-low modelling recognised as
/// inverted — the detail that decides whether it decodes at all.
#[test]
fn the_open_drain_bus_is_bound_the_right_way_up() {
    let Some(found) = suggest_for("prototype/ov5640_sccb_init_probe.sv", "ov5640_sccb_init_probe")
    else {
        eprintln!("skipping: the user's design is not in this checkout");
        return;
    };

    let i2c = found.iter().find(|s| s.protocol == "i2c").expect("an I2C bus");
    let scl = i2c.bindings.iter().find(|b| b.role == "scl").expect("scl");
    let sda = i2c.bindings.iter().find(|b| b.role == "sda").expect("sda");

    // Either a pad readback uninverted, or a drive enable inverted — never a
    // drive enable read the wrong way up.
    assert_eq!(
        scl.invert,
        scl.path.contains("drive_low"),
        "a drive-low signal must be inverted and a pad must not: {scl:?}"
    );
    assert_eq!(sda.invert, sda.path.contains("drive_low"), "{sda:?}");
}

/// An AXI-Stream bundle, found by its handshake.
#[test]
fn an_axi_stream_bundle_is_recognised() {
    let Some(found) = suggest_for("img_proc/axis_rgb24_to_vdma32.sv", "axis_rgb24_to_vdma32")
    else {
        eprintln!("skipping: the user's design is not in this checkout");
        return;
    };

    let axis: Vec<&bind::Suggestion> = found.iter().filter(|s| s.protocol == "axis").collect();
    assert!(axis.len() >= 2, "both ends of the converter: {found:#?}");
    for suggestion in &axis {
        let roles: Vec<&str> = suggestion.bindings.iter().map(|b| b.role.as_str()).collect();
        assert!(roles.contains(&"tvalid"), "{roles:?}");
        assert!(roles.contains(&"tready"), "{roles:?}");
    }
}

/// The demo's memory-mapped bus is found by its `_awvalid`, whole, with the
/// clock it runs on — and none of its `*valid` lines is taken for a pixel
/// stream on the way. This one runs everywhere: the demo is the tool's own.
#[test]
fn an_axi4_bus_is_recognised_whole() {
    let path = rtlscope_fixtures::path("axi4_demo.sv");
    let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    let (design, _) = rtlscope_elab::elaborate(&uir, Some("axi4_demo"));
    let design = design.expect("the demo elaborates");
    let found = bind::suggest(&design, design.top, "");

    let axi4: Vec<&bind::Suggestion> = found.iter().filter(|s| s.protocol == "axi4").collect();
    assert_eq!(axi4.len(), 1, "{found:#?}");
    let bus = axi4[0];
    assert_eq!(bus.group, "axi");
    assert!(bus.is_complete(), "{bus:#?}");
    assert_eq!(bus.bindings.len(), 26, "the clock and twenty-five signals: {bus:#?}");
    let clock = bus.bindings.iter().find(|b| b.role == "clock").expect("a clock");
    assert_eq!(clock.path, "clk");
    let rlast = bus.bindings.iter().find(|b| b.role == "rlast").expect("rlast");
    assert_eq!(rlast.path, "axi_rlast");
    assert!(!found.iter().any(|s| s.protocol == "pixel"), "{found:#?}");
}
