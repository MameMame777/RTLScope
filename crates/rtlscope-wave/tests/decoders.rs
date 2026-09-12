//! What each decoder makes of a waveform.
//!
//! VCD is text, so these fixtures are written by hand rather than simulated.
//! That is not only faster: it is the only way to get the cases that matter on
//! demand — a line two pixels short, a marker pulsing while `valid` is low, a
//! dump that stops in the middle of a frame. A simulator produces correct
//! waveforms, and correct waveforms exercise the easy half of a decoder.

use rtlscope_wave::Dump;
use rtlscope_wave::decode::{Binding, DecodeReport, Level, ResolvedBindings};

// ------------------------------------------------------------ a builder ---

/// Writes a VCD with one scope and a clock that ticks every 10 units.
struct Vcd {
    vars: Vec<(String, u32, char)>,
    next_id: u8,
}

impl Vcd {
    fn new() -> Self {
        Vcd { vars: Vec::new(), next_id: 0 }
    }

    fn var(&mut self, name: &str, width: u32) -> &mut Self {
        let id = (b'!' + self.next_id) as char;
        self.next_id += 1;
        self.vars.push((name.to_string(), width, id));
        self
    }
}

/// Writes the VCD: values settle while the clock is low, then an edge samples
/// them, then it falls again so the next cycle has an edge of its own.
fn build(vcd: &Vcd, steps: &[Vec<(&str, u64)>]) -> Dump {
    let mut out = String::from("$timescale 1ns $end\n$scope module tb $end\n");
    out.push_str("$var wire 1 ~ clk $end\n");
    for (name, width, id) in &vcd.vars {
        out.push_str(&format!("$var wire {width} {id} {name} $end\n"));
    }
    out.push_str("$upscope $end\n$enddefinitions $end\n#0\n0~\n");
    for (_, width, id) in &vcd.vars {
        out.push_str(&format!("b{} {id}\n", "0".repeat(*width as usize)));
    }

    let mut time = 0u64;
    for step in steps {
        time += 5;
        out.push_str(&format!("#{time}\n"));
        for (name, value) in step {
            let (_, width, id) =
                vcd.vars.iter().find(|(n, _, _)| n == name).unwrap_or_else(|| panic!("{name}"));
            let bits: String =
                (0..*width).rev().map(|b| if value >> b & 1 == 1 { '1' } else { '0' }).collect();
            out.push_str(&format!("b{bits} {id}\n"));
        }
        time += 5;
        out.push_str(&format!("#{time}\n1~\n"));
        // Fall again so the next cycle has an edge to find.
        out.push_str(&format!("#{}\n0~\n", time + 1));
        time += 1;
    }
    Dump::open_vcd_bytes(out.into_bytes()).expect("the fixture parses")
}

fn decode(mut dump: Dump, protocol: &str, bindings: &[&str]) -> DecodeReport {
    let decoder = rtlscope_wave::decode::by_name(protocol).expect("a decoder by that name");
    let parsed: Vec<Binding> =
        bindings.iter().map(|b| Binding::parse(b).expect("a binding")).collect();
    let resolved = ResolvedBindings::resolve(&mut dump, decoder.channels(), &parsed)
        .expect("the bindings resolve");
    decoder.decode(&dump, &resolved)
}

fn stat<'a>(report: &'a DecodeReport, name: &str) -> Option<&'a str> {
    report.stats.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
}

// -------------------------------------------------------- pixel stream ---

/// The clean case: two frames of two lines of three pixels.
fn two_tidy_frames() -> Dump {
    let mut vcd = Vcd::new();
    vcd.var("valid", 1).var("sof", 1).var("eol", 1).var("eof", 1).var("err", 1).var("data", 8);

    let mut steps: Vec<Vec<(&str, u64)>> = Vec::new();
    steps.push(vec![]); // an idle cycle first
    for frame in 0..2u64 {
        for line in 0..2u64 {
            for pixel in 0..3u64 {
                let mut step = vec![("valid", 1), ("data", frame * 16 + line * 4 + pixel)];
                if frame_start(frame, line, pixel) {
                    step.push(("sof", 1));
                } else {
                    step.push(("sof", 0));
                }
                let last_pixel = pixel == 2;
                step.push(("eol", u64::from(last_pixel)));
                step.push(("eof", u64::from(last_pixel && line == 1)));
                steps.push(step);
            }
        }
        steps.push(vec![("valid", 0), ("eol", 0), ("eof", 0), ("sof", 0)]);
    }
    build(&vcd, &steps)
}

fn frame_start(_frame: u64, line: u64, pixel: u64) -> bool {
    line == 0 && pixel == 0
}

#[test]
fn a_tidy_stream_is_counted() {
    let report = decode(
        two_tidy_frames(),
        "pixel",
        &[
            "clock=tb.clk",
            "valid=tb.valid",
            "data=tb.data",
            "sof=tb.sof",
            "eol=tb.eol",
            "eof=tb.eof",
            "err=tb.err",
        ],
    );

    assert_eq!(stat(&report, "frames"), Some("2"), "{report:#?}");
    assert_eq!(stat(&report, "lines"), Some("4"), "{report:#?}");
    assert_eq!(stat(&report, "beats"), Some("12"), "{report:#?}");
    assert_eq!(stat(&report, "pixels per line"), Some("3"), "{report:#?}");
    assert!(report.problems.is_empty(), "{:#?}", report.problems);

    let frames: Vec<&str> = report.transactions.iter().map(|t| t.kind.as_str()).collect();
    assert_eq!(frames, ["frame", "frame"]);
    assert_eq!(report.errors(), 0);
}

/// The bug this decoder exists for: one line quietly a pixel short.
#[test]
fn a_line_that_disagrees_with_the_rest_is_pointed_at() {
    let mut vcd = Vcd::new();
    vcd.var("valid", 1).var("sof", 1).var("eol", 1).var("eof", 1);

    let mut steps: Vec<Vec<(&str, u64)>> = vec![vec![]];
    // Three lines of three pixels, then one of two.
    for (line, pixels) in [(0u64, 3u64), (1, 3), (2, 3), (3, 2)] {
        for pixel in 0..pixels {
            steps.push(vec![
                ("valid", 1),
                ("sof", u64::from(line == 0 && pixel == 0)),
                ("eol", u64::from(pixel == pixels - 1)),
                ("eof", u64::from(line == 3 && pixel == pixels - 1)),
            ]);
        }
    }
    let report = decode(
        build(&vcd, &steps),
        "pixel",
        &["clock=tb.clk", "valid=tb.valid", "sof=tb.sof", "eol=tb.eol", "eof=tb.eof"],
    );

    assert_eq!(stat(&report, "lines"), Some("4"), "{report:#?}");
    assert_eq!(stat(&report, "pixels per line"), Some("3"));
    assert_eq!(stat(&report, "lines of another length"), Some("1"), "{report:#?}");

    let short: Vec<&rtlscope_wave::decode::Annotation> =
        report.annotations.iter().filter(|a| a.kind == "line" && a.level == Level::Error).collect();
    assert_eq!(short.len(), 1, "{:#?}", report.annotations);
    assert_eq!(short[0].label, "2 px");
    assert!(short[0].fields.iter().any(|(k, v)| k == "expected" && v == "3"), "{short:#?}");
}

/// A marker with no beat under it is not counted, and not ignored either.
#[test]
fn a_marker_without_a_beat_is_reported() {
    let mut vcd = Vcd::new();
    vcd.var("valid", 1).var("sof", 1).var("eol", 1).var("eof", 1);

    let steps: Vec<Vec<(&str, u64)>> = vec![
        vec![("valid", 0), ("sof", 1)], // a start-of-frame with nothing behind it
        vec![("valid", 1), ("sof", 0), ("eol", 0)],
        vec![("valid", 1), ("eol", 1), ("eof", 1)],
    ];
    let report = decode(
        build(&vcd, &steps),
        "pixel",
        &["clock=tb.clk", "valid=tb.valid", "sof=tb.sof", "eol=tb.eol", "eof=tb.eof"],
    );

    assert!(
        report.problems.iter().any(|p| p.contains("`valid` was low")),
        "{:#?}",
        report.problems
    );
    // The frame never opened, so ending it is reported too rather than counted.
    assert!(
        report.problems.iter().any(|p| p.contains("without starting")),
        "{:#?}",
        report.problems
    );
}

/// A dump that stops mid-frame says so rather than reporting a short frame.
#[test]
fn a_frame_the_dump_cuts_off_is_reported_not_counted() {
    let mut vcd = Vcd::new();
    vcd.var("valid", 1).var("sof", 1).var("eol", 1).var("eof", 1);

    let steps: Vec<Vec<(&str, u64)>> = vec![
        vec![("valid", 1), ("sof", 1)],
        vec![("valid", 1), ("sof", 0), ("eol", 1)],
        vec![("valid", 1), ("eol", 0)],
    ];
    let report = decode(
        build(&vcd, &steps),
        "pixel",
        &["clock=tb.clk", "valid=tb.valid", "sof=tb.sof", "eol=tb.eol", "eof=tb.eof"],
    );

    assert_eq!(stat(&report, "frames"), Some("1"), "one frame started");
    assert!(report.transactions.is_empty(), "but none finished: {:#?}", report.transactions);
    assert!(
        report.problems.iter().any(|p| p.contains("ends inside frame")),
        "{:#?}",
        report.problems
    );
    assert!(
        report.problems.iter().any(|p| p.contains("never reached its `eol`")),
        "{:#?}",
        report.problems
    );
}

/// An `err` beat becomes an annotation someone can find, not just a number.
#[test]
fn an_error_beat_is_marked_where_it_happened() {
    let mut vcd = Vcd::new();
    vcd.var("valid", 1).var("eol", 1).var("eof", 1).var("err", 1);

    let steps: Vec<Vec<(&str, u64)>> = vec![
        vec![("valid", 1)],
        vec![("valid", 1), ("err", 1)],
        vec![("valid", 1), ("err", 0), ("eol", 1), ("eof", 1)],
    ];
    let report = decode(
        build(&vcd, &steps),
        "pixel",
        &["clock=tb.clk", "valid=tb.valid", "eol=tb.eol", "eof=tb.eof", "err=tb.err"],
    );

    assert_eq!(stat(&report, "err beats"), Some("1"), "{report:#?}");
    let errs: Vec<&rtlscope_wave::decode::Annotation> =
        report.annotations.iter().filter(|a| a.kind == "err").collect();
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].level, Level::Error);
}

/// Leaving out a channel narrows what can be said, and the report says which.
#[test]
fn what_was_not_bound_is_named() {
    let mut vcd = Vcd::new();
    vcd.var("valid", 1);
    let steps: Vec<Vec<(&str, u64)>> = vec![vec![("valid", 1)], vec![("valid", 1)]];

    let report = decode(build(&vcd, &steps), "pixel", &["clock=tb.clk", "valid=tb.valid"]);
    assert_eq!(stat(&report, "beats"), Some("2"));
    assert!(report.problems.iter().any(|p| p.contains("no `eol`")), "{:#?}", report.problems);
    assert!(report.problems.iter().any(|p| p.contains("no `eof`")), "{:#?}", report.problems);
}

/// A required channel left out is refused, with what it is for.
#[test]
fn a_missing_required_channel_says_what_it_was_for() {
    let mut dump = {
        let mut vcd = Vcd::new();
        vcd.var("valid", 1);
        build(&vcd, &[vec![("valid", 1)]])
    };
    let decoder = rtlscope_wave::decode::by_name("pixel").unwrap();
    let bindings = [Binding::parse("valid=tb.valid").unwrap()];
    let error = ResolvedBindings::resolve(&mut dump, decoder.channels(), &bindings)
        .expect_err("no clock means no decode");
    let text = error.to_string();
    assert!(text.contains("clock"), "{text}");
    assert!(text.contains("the clock"), "{text}");
}

/// A signal that is not in the dump is named, rather than silently unbound.
#[test]
fn binding_something_that_is_not_there_says_so() {
    let mut dump = {
        let mut vcd = Vcd::new();
        vcd.var("valid", 1);
        build(&vcd, &[vec![("valid", 1)]])
    };
    let decoder = rtlscope_wave::decode::by_name("pixel").unwrap();
    let bindings =
        [Binding::parse("clock=tb.clk").unwrap(), Binding::parse("valid=tb.nope").unwrap()];
    let error = ResolvedBindings::resolve(&mut dump, decoder.channels(), &bindings)
        .expect_err("an unknown signal is an error");
    assert!(error.to_string().contains("tb.nope"), "{error}");
}

// ---------------------------------------------------------- AXI-Stream ---

/// Two lines of three beats, with the sink refusing for two cycles in the
/// middle — the case a plain valid-only stream has no way to show.
fn stalling_stream() -> Dump {
    let mut vcd = Vcd::new();
    vcd.var("tvalid", 1).var("tready", 1).var("tlast", 1).var("tuser", 1).var("tdata", 8);

    let mut steps: Vec<Vec<(&str, u64)>> = vec![vec![]];
    for line in 0..2u64 {
        for beat in 0..3u64 {
            // Before the second beat of the first line, the sink says no twice.
            if line == 0 && beat == 1 {
                for _ in 0..2 {
                    steps.push(vec![("tvalid", 1), ("tready", 0), ("tuser", 0), ("tlast", 0)]);
                }
            }
            steps.push(vec![
                ("tvalid", 1),
                ("tready", 1),
                ("tdata", line * 8 + beat),
                ("tuser", u64::from(line == 0 && beat == 0)),
                ("tlast", u64::from(beat == 2)),
            ]);
        }
    }
    // An idle cycle at the end.
    steps.push(vec![("tvalid", 0), ("tready", 1), ("tlast", 0)]);
    build(&vcd, &steps)
}

#[test]
fn a_beat_needs_both_sides_to_agree() {
    let report = decode(
        stalling_stream(),
        "axis",
        &[
            "clock=tb.clk",
            "tvalid=tb.tvalid",
            "tready=tb.tready",
            "tdata=tb.tdata",
            "tlast=tb.tlast",
            "tuser=tb.tuser",
        ],
    );

    assert_eq!(stat(&report, "beats"), Some("6"), "{report:#?}");
    assert_eq!(stat(&report, "lines"), Some("2"), "{report:#?}");
    assert_eq!(stat(&report, "frames"), Some("1"), "{report:#?}");
    assert_eq!(stat(&report, "beats per line"), Some("3"));
    assert!(report.problems.is_empty(), "{:#?}", report.problems);
}

/// The number the protocol exists to give: how much of what was offered got
/// through.
#[test]
fn back_pressure_is_measured_rather_than_ignored() {
    let report = decode(
        stalling_stream(),
        "axis",
        &[
            "clock=tb.clk",
            "tvalid=tb.tvalid",
            "tready=tb.tready",
            "tlast=tb.tlast",
            "tuser=tb.tuser",
        ],
    );

    assert_eq!(stat(&report, "stall cycles"), Some("2"), "{report:#?}");
    // Two stalls against six beats: 2/8.
    assert_eq!(stat(&report, "stalled"), Some("25.0%"), "{report:#?}");
    assert_eq!(stat(&report, "longest stall"), Some("2 cycles"), "{report:#?}");
    // A cycle where the source has nothing is not a stall.
    assert_eq!(stat(&report, "idle cycles"), Some("2"), "{report:#?}");
}

/// `tlast` marks a line, so a frame is only closed when the dump ends or the
/// next one starts — a decoder that waited for a frame marker would report
/// none at all.
#[test]
fn a_frame_is_closed_by_the_next_one_or_by_the_end() {
    let report = decode(
        stalling_stream(),
        "axis",
        &[
            "clock=tb.clk",
            "tvalid=tb.tvalid",
            "tready=tb.tready",
            "tlast=tb.tlast",
            "tuser=tb.tuser",
        ],
    );
    assert_eq!(report.transactions.len(), 1, "{:#?}", report.transactions);
    let frame = &report.transactions[0];
    assert_eq!(frame.kind, "frame");
    assert!(frame.fields.iter().any(|(k, v)| k == "lines" && v == "2"), "{frame:#?}");
}

/// A `tready` that never resolves is not a stall and not a beat.
#[test]
fn an_undriven_handshake_is_reported() {
    let text = "$timescale 1ns $end\n$scope module tb $end\n\
        $var wire 1 ! clk $end\n$var wire 1 \" tvalid $end\n$var wire 1 # tready $end\n\
        $upscope $end\n$enddefinitions $end\n\
        #0\n0!\n1\"\nx#\n#5\n1!\n#10\n0!\n#15\n1!\n";
    let report = decode(
        Dump::open_vcd_bytes(text.as_bytes().to_vec()).unwrap(),
        "axis",
        &["clock=tb.clk", "tvalid=tb.tvalid", "tready=tb.tready"],
    );
    assert_eq!(stat(&report, "beats"), Some("0"), "{report:#?}");
    assert!(report.problems.iter().any(|p| p.contains("undriven")), "{:#?}", report.problems);
}

// ----------------------------------------------------------------- I2C ---

/// Writes an I²C bus a wire event at a time.
///
/// The shape matches the design's master: SCL spends two thirds of each bit
/// low, so nothing here can quietly depend on a 50% duty cycle.
struct Bus {
    events: Vec<(u64, char, bool)>,
    now: u64,
    scl: bool,
    sda: bool,
}

impl Bus {
    /// Both lines released, which is what an idle open-drain bus looks like.
    fn new() -> Self {
        Bus { events: vec![(0, 'c', true), (0, 'd', true)], now: 0, scl: true, sda: true }
    }

    fn wait(&mut self, ticks: u64) -> &mut Self {
        self.now += ticks;
        self
    }

    fn scl(&mut self, high: bool) -> &mut Self {
        self.scl = high;
        self.events.push((self.now, 'c', high));
        self
    }

    fn sda(&mut self, high: bool) -> &mut Self {
        self.sda = high;
        self.events.push((self.now, 'd', high));
        self
    }

    fn start(&mut self) -> &mut Self {
        // SDA falls while SCL is high.
        self.sda(true).wait(10).scl(true).wait(10).sda(false).wait(10).scl(false).wait(10)
    }

    /// A repeated START: release both, then fall again while the clock is high.
    fn restart(&mut self) -> &mut Self {
        self.sda(true).wait(10).scl(true).wait(10).sda(false).wait(10).scl(false).wait(10)
    }

    /// SCL and SDA released on the *same* edge, which is what the design does
    /// and what a naive decoder misses.
    fn stop_together(&mut self) -> &mut Self {
        self.sda(false);
        self.wait(10);
        let at = self.now;
        self.events.push((at, 'c', true));
        self.events.push((at, 'd', true));
        self.scl = true;
        self.sda = true;
        self.wait(20)
    }

    /// Eight bits, most significant first, then the acknowledgement bit.
    fn byte(&mut self, value: u8, acked: bool) -> &mut Self {
        for bit in (0..8).rev() {
            self.sda(value >> bit & 1 == 1);
            // Low for two thirds of the bit, high for one — the design's shape.
            self.wait(20).scl(true).wait(10).scl(false);
        }
        // The receiver pulls SDA low to acknowledge.
        self.sda(!acked);
        self.wait(20).scl(true).wait(10).scl(false).wait(10)
    }

    /// As a dump, optionally recorded the way an open-drain model records it:
    /// a `drive_low` enable, which is the opposite of the wire.
    fn dump(&self, inverted: bool) -> Dump {
        let mut out = String::from("$timescale 1ns $end\n$scope module tb $end\n");
        out.push_str("$var wire 1 ! scl $end\n$var wire 1 \" sda $end\n");
        out.push_str("$upscope $end\n$enddefinitions $end\n");

        let mut at = u64::MAX;
        for (time, line, high) in &self.events {
            if *time != at {
                out.push_str(&format!("#{time}\n"));
                at = *time;
            }
            let level = if inverted { !*high } else { *high };
            let id = if *line == 'c' { '!' } else { '"' };
            out.push_str(&format!("{}{id}\n", u8::from(level)));
        }
        Dump::open_vcd_bytes(out.into_bytes()).expect("the fixture parses")
    }
}

/// One SCCB register write: address, two bytes of register, one of value.
fn sccb_write(register: u16, value: u8) -> Bus {
    let mut bus = Bus::new();
    bus.start()
        .byte(0x78, true)
        .byte((register >> 8) as u8, true)
        .byte(register as u8, true)
        .byte(value, true)
        .stop_together();
    bus
}

#[test]
fn a_register_write_is_read_back_as_one() {
    let report = decode(sccb_write(0x300A, 0x56).dump(false), "i2c", &["scl=tb.scl", "sda=tb.sda"]);

    assert_eq!(stat(&report, "transfers"), Some("1"), "{report:#?}");
    assert_eq!(stat(&report, "bytes"), Some("4"), "{report:#?}");
    assert_eq!(stat(&report, "not acknowledged"), None, "{report:#?}");
    assert!(report.problems.is_empty(), "{:#?}", report.problems);

    let transfer = &report.transactions[0];
    assert_eq!(transfer.kind, "sccb-write");
    let field = |name: &str| {
        transfer.fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).unwrap_or("")
    };
    assert_eq!(field("device"), "0x3c", "0x78 is the 7-bit address shifted up");
    assert_eq!(field("register"), "0x300a");
    assert_eq!(field("value"), "0x56");
}

/// The STOP the design actually produces: both wires released at one instant.
/// A decoder that required SCL to rise strictly first would find none.
#[test]
fn a_stop_that_releases_both_wires_at_once_is_still_a_stop() {
    let report = decode(sccb_write(0x3008, 0x82).dump(false), "i2c", &["scl=tb.scl", "sda=tb.sda"]);
    assert_eq!(stat(&report, "stops"), Some("1"), "{report:#?}");
    assert_eq!(report.transactions.len(), 1, "{:#?}", report.transactions);
}

/// The open-drain model, bound inverted. The same bus, recorded the other way
/// up, must decode identically.
#[test]
fn an_open_drain_model_decodes_the_same_when_bound_inverted() {
    let plain = decode(sccb_write(0x300A, 0x56).dump(false), "i2c", &["scl=tb.scl", "sda=tb.sda"]);
    let drive_low =
        decode(sccb_write(0x300A, 0x56).dump(true), "i2c", &["scl=!tb.scl", "sda=!tb.sda"]);

    assert_eq!(plain.transactions, drive_low.transactions, "the same bus, recorded inverted");
    assert!(drive_low.problems.is_empty(), "{:#?}", drive_low.problems);
}

/// And bound the wrong way up, it says so rather than reporting nonsense.
#[test]
fn an_open_drain_model_bound_uninverted_is_reported() {
    let report = decode(sccb_write(0x300A, 0x56).dump(true), "i2c", &["scl=tb.scl", "sda=tb.sda"]);
    assert!(
        report.problems.iter().any(|p| p.contains("drive_low")),
        "the hint should name the idiom: {:#?}",
        report.problems
    );
}

/// A read: write the register address, repeated START, then take a byte and
/// refuse it, which is how a master says it wants no more.
#[test]
fn a_repeated_start_read_is_named_as_a_read() {
    let mut bus = Bus::new();
    bus.start()
        .byte(0x78, true)
        .byte(0x30, true)
        .byte(0x0A, true)
        .restart()
        .byte(0x79, true)
        .byte(0x56, false) // the master NACKs the last byte it wants
        .stop_together();

    let report = decode(bus.dump(false), "i2c", &["scl=tb.scl", "sda=tb.sda"]);
    assert_eq!(stat(&report, "starts"), Some("2"), "{report:#?}");
    assert_eq!(stat(&report, "not acknowledged"), Some("1"), "{report:#?}");

    let transfer = &report.transactions[0];
    assert_eq!(transfer.kind, "sccb-read", "{transfer:#?}");
    let field = |name: &str| {
        transfer.fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).unwrap_or("")
    };
    assert_eq!(field("register"), "0x300a");
    assert_eq!(field("value"), "0x56");
}

/// A device that never answers: every byte NACKed, and the report says so.
#[test]
fn a_device_that_does_not_answer_is_flagged() {
    let mut bus = Bus::new();
    bus.start().byte(0x78, false).stop_together();

    let report = decode(bus.dump(false), "i2c", &["scl=tb.scl", "sda=tb.sda"]);
    assert_eq!(stat(&report, "not acknowledged"), Some("1"), "{report:#?}");
    let flagged: Vec<&rtlscope_wave::decode::Annotation> =
        report.annotations.iter().filter(|a| a.level == Level::Warning).collect();
    assert!(!flagged.is_empty(), "{:#?}", report.annotations);
}

/// A dump that stops mid-transfer says so rather than reporting a short one.
#[test]
fn a_transfer_the_dump_cuts_off_is_reported() {
    let mut bus = Bus::new();
    bus.start().byte(0x78, true).byte(0x30, true);
    // No STOP.

    let report = decode(bus.dump(false), "i2c", &["scl=tb.scl", "sda=tb.sda"]);
    assert!(report.problems.iter().any(|p| p.contains("never stopped")), "{:#?}", report.problems);
}

/// Two writes back to back, which is what a boot sequence is made of.
#[test]
fn a_sequence_of_writes_comes_back_in_order() {
    let mut bus = Bus::new();
    for (register, value) in [(0x3008u16, 0x82u8), (0x300A, 0x56), (0x4300, 0x61)] {
        bus.start()
            .byte(0x78, true)
            .byte((register >> 8) as u8, true)
            .byte(register as u8, true)
            .byte(value, true)
            .stop_together()
            .wait(50);
    }

    let report = decode(bus.dump(false), "i2c", &["scl=tb.scl", "sda=tb.sda"]);
    assert_eq!(stat(&report, "transfers"), Some("3"), "{report:#?}");
    assert!(report.problems.is_empty(), "{:#?}", report.problems);

    let registers: Vec<&str> = report
        .transactions
        .iter()
        .filter_map(|t| t.fields.iter().find(|(k, _)| k == "register").map(|(_, v)| v.as_str()))
        .collect();
    assert_eq!(registers, ["0x3008", "0x300a", "0x4300"]);
}

// ---------------------------------------------------------------- axi4 ---

/// The AXI4 roles of a hand-built dump, bound as `role=tb.role`.
fn axi4_bindings(roles: &[&str]) -> Vec<String> {
    let mut out = vec!["clock=tb.clk".to_string()];
    out.extend(roles.iter().map(|role| format!("{role}=tb.{role}")));
    out
}

/// One field of a transaction, or `?`.
fn field<'a>(transaction: &'a rtlscope_wave::decode::Transaction, name: &str) -> &'a str {
    transaction.fields.iter().find(|(k, _)| k == name).map_or("?", |(_, v)| v.as_str())
}

/// The write-side signals a hand-built dump declares.
const AXI4_WRITE_SIDE: &[(&str, u32)] = &[
    ("awvalid", 1),
    ("awready", 1),
    ("awaddr", 8),
    ("awlen", 8),
    ("wvalid", 1),
    ("wready", 1),
    ("wdata", 8),
    ("wlast", 1),
    ("bvalid", 1),
    ("bready", 1),
    ("bresp", 2),
];

/// The demo design's own recording: a four-beat write, the same four beats
/// read back, and a read of an address the slave does not have — twice, since
/// the master rests and runs its programme again. Six sentences, and nothing
/// left unaccounted for.
#[test]
fn an_axi4_recording_reads_as_two_rounds_of_a_write_and_two_reads() {
    let dump = Dump::open(&rtlscope_fixtures::wave("axi4_demo.fst")).expect("the fixture opens");
    let roles = [
        "awvalid", "awready", "awaddr", "awlen", "awsize", "awburst", "wvalid", "wready", "wdata",
        "wstrb", "wlast", "bvalid", "bready", "bresp", "arvalid", "arready", "araddr", "arlen",
        "arsize", "arburst", "rvalid", "rready", "rdata", "rresp", "rlast",
    ];
    let mut bindings = vec!["clock=tb_axi4_demo.u_dut.clk".to_string()];
    bindings.extend(roles.iter().map(|role| format!("{role}=tb_axi4_demo.u_dut.axi_{role}")));
    let refs: Vec<&str> = bindings.iter().map(String::as_str).collect();

    let report = decode(dump, "axi4", &refs);

    let summary: Vec<String> = report
        .transactions
        .iter()
        .map(|t| {
            format!("{} {} ×{} {}", t.kind, field(t, "addr"), field(t, "beats"), field(t, "resp"))
        })
        .collect();
    let round =
        ["write 0x00000010 ×4 OKAY", "read 0x00000010 ×4 OKAY", "read 0x00000080 ×1 SLVERR"];
    let two_rounds: Vec<&str> = round.iter().chain(round.iter()).copied().collect();
    assert_eq!(summary, two_rounds, "{report:#?}");
    assert!(report.problems.is_empty(), "{:#?}", report.problems);
    assert_eq!(stat(&report, "writes"), Some("2"));
    assert_eq!(stat(&report, "reads"), Some("4"));
    assert_eq!(stat(&report, "beats written"), Some("8"));
    assert_eq!(stat(&report, "beats read"), Some("10"));
    assert_eq!(stat(&report, "SLVERR responses"), Some("2"));
    assert_eq!(field(&report.transactions[0], "burst"), "INCR");
    assert_eq!(field(&report.transactions[0], "bytes/beat"), "4");

    // The slave held AWREADY off for two cycles per round and took W every
    // other cycle, and both show up as waiting rather than vanishing into the
    // beat count.
    assert_eq!(stat(&report, "AW waited"), Some("4 cycle(s), longest 2"), "{:?}", report.stats);
    assert!(stat(&report, "W waited").is_some(), "{:?}", report.stats);

    // The refused read is the one thing that goes wrong each round, marked
    // twice: as the transaction, and on the beat that carried the SLVERR.
    assert_eq!(report.errors(), 4, "{:#?}", report.annotations);
    assert_eq!(report.annotations.iter().filter(|a| a.kind == "wbeat").count(), 8);
    assert_eq!(report.annotations.iter().filter(|a| a.kind == "rbeat").count(), 10);
}

/// AXI4 lets the data burst start before its address is accepted; the two
/// still belong together, and the response closes them as one write.
#[test]
fn axi4_write_data_may_lead_its_address_and_still_pairs_with_it() {
    let mut vcd = Vcd::new();
    for (name, width) in AXI4_WRITE_SIDE {
        vcd.var(name, *width);
    }
    let dump = build(
        &vcd,
        &[
            // A beat, before any address has been seen.
            vec![("wvalid", 1), ("wready", 1), ("wdata", 0x11)],
            // The last beat.
            vec![("wdata", 0x22), ("wlast", 1)],
            // The address, late — and accepted at once.
            vec![
                ("wvalid", 0),
                ("wlast", 0),
                ("awvalid", 1),
                ("awready", 1),
                ("awaddr", 0x20),
                ("awlen", 1),
            ],
            // And the response.
            vec![("awvalid", 0), ("awready", 0), ("bvalid", 1), ("bready", 1), ("bresp", 0)],
            vec![("bvalid", 0), ("bready", 0)],
        ],
    );
    let roles: Vec<&str> = AXI4_WRITE_SIDE.iter().map(|(name, _)| *name).collect();
    let bindings = axi4_bindings(&roles);
    let refs: Vec<&str> = bindings.iter().map(String::as_str).collect();

    let report = decode(dump, "axi4", &refs);

    assert_eq!(report.transactions.len(), 1, "{report:#?}");
    let write = &report.transactions[0];
    assert_eq!(write.kind, "write");
    assert_eq!(field(write, "addr"), "0x20");
    assert_eq!(field(write, "beats"), "2");
    assert_eq!(field(write, "len"), "2", "AWLEN=1 promised two beats");
    assert_eq!(field(write, "resp"), "OKAY");
    assert_eq!(report.annotations.iter().filter(|a| a.kind == "wbeat").count(), 2);
    // Only the read half is missing, and that is the one thing said.
    assert_eq!(
        report.problems,
        ["the read channels were not all bound, so reads were not decoded"],
        "{:#?}",
        report.problems
    );
}

/// The two rules a simulator will not flag: VALID that drops before READY,
/// and a payload that moves while VALID waits. Both are named where they
/// happened, and a response with nothing to answer is not silently a write.
#[test]
fn axi4_handshake_rules_are_checked_not_assumed() {
    let mut vcd = Vcd::new();
    for (name, width) in AXI4_WRITE_SIDE {
        vcd.var(name, *width);
    }
    let dump = build(
        &vcd,
        &[
            // Offered, not taken.
            vec![("awvalid", 1), ("awaddr", 0x10)],
            // Still waiting — and the address moved under it.
            vec![("awaddr", 0x14)],
            // Then dropped, with no transfer having happened.
            vec![("awvalid", 0)],
            // A response for nothing.
            vec![("bvalid", 1), ("bready", 1)],
            vec![("bvalid", 0), ("bready", 0)],
        ],
    );
    let roles: Vec<&str> = AXI4_WRITE_SIDE.iter().map(|(name, _)| *name).collect();
    let bindings = axi4_bindings(&roles);
    let refs: Vec<&str> = bindings.iter().map(String::as_str).collect();

    let report = decode(dump, "axi4", &refs);

    assert!(report.transactions.is_empty(), "{report:#?}");
    assert!(
        report.problems.iter().any(|p| p.contains("`awaddr` changed")),
        "{:#?}",
        report.problems
    );
    assert!(
        report.problems.iter().any(|p| p.contains("`awvalid` dropped")),
        "{:#?}",
        report.problems
    );
    assert!(
        report.problems.iter().any(|p| p.contains("no write outstanding")),
        "{:#?}",
        report.problems
    );
    assert_eq!(stat(&report, "handshake violations"), Some("2"));
    let violations: Vec<&str> = report
        .annotations
        .iter()
        .filter(|a| a.kind == "violation")
        .map(|a| a.label.as_str())
        .collect();
    assert_eq!(violations, ["awaddr changed", "awvalid dropped"]);
    assert!(
        report
            .annotations
            .iter()
            .filter(|a| a.kind == "violation")
            .all(|a| a.level == Level::Error)
    );
}
