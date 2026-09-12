//! The designs that ship inside the window, for a reader with nothing to open.
//!
//! RTLScope is a tool for looking at a design, and a reader who has just
//! installed it usually has none to hand — or has a real one, three hundred
//! files deep, which is the wrong thing to learn a tool on. Each sample here
//! was written to answer one question the tool can answer, and says which.
//!
//! They are compiled into the binary rather than installed beside it, so a
//! window that was double-clicked has them whether or not the checkout is
//! there. And they are written to disk before being opened, because a sample
//! is only useful if it behaves exactly like the reader's own files: the
//! Source tab reads it, an editor can open it, a drawn stimulus is saved next
//! to it, a simulation writes its harness beside it. A file that existed only
//! in memory would be a demonstration, not a design.

use std::path::{Path, PathBuf};

/// One design, and the question it was built to answer.
pub struct Sample {
    /// Its name: the folder it is written into, and what `--sample` takes.
    pub id: &'static str,
    /// What it is for, in a sentence a reader can act on.
    pub what: &'static str,
    /// The top module, when the sources hold more than one design and
    /// inference cannot pick.
    pub top: Option<&'static str>,
    /// The view the question is answered in, by the name the tab bar uses.
    pub tab: Option<&'static str>,
    /// Every file, by its path under the sample's folder.
    pub files: &'static [(&'static str, &'static [u8])],
    /// Nets to put on the waveform once it is open, in this order: the ones
    /// the recording was made to show. The top's own by name, or `instance.net`
    /// for one inside. Empty when the ports say enough on their own.
    pub watch: &'static [&'static str],
    /// A testbench shipped beside the design, by its path under the folder.
    ///
    /// Loaded the way one the reader picked would be — as a second attribute
    /// of the session, never as part of the design. A testbench is a module
    /// nothing instantiates, and read as a design it would be one more answer
    /// to "which of these is the top", and the wrong one.
    pub bench: Option<&'static str>,
    /// What to hand the window once the files are on disk, relative to the
    /// folder. `"."` is the folder itself, which is how a project is opened.
    pub open: &'static [&'static str],
}

/// The bytes of one file under `tests/fixtures/`, at compile time.
///
/// The same files the tests read, so a sample cannot drift from what the
/// analyses are checked against: if `trace_demo.sv` changes, the sample of it
/// changes in the same build.
macro_rules! fixture {
    ($name:literal) => {
        include_bytes!(concat!("../../../tests/fixtures/", $name))
    };
}

/// Every sample, in the order worth reading them in.
///
/// Listed rather than discovered, like the fixtures they come from: a list
/// that has to be edited by hand is one somebody has to think about, and the
/// sentence beside each name is the point of the list.
pub const ALL: &[Sample] = &[
    Sample {
        id: "hier",
        what: "Three levels of hierarchy and the three ways of connecting them. Start here: the \
               diagram, drilling into an instance, the Source tab.",
        top: None,
        tab: Some("Source"),
        files: &[("hier.sv", fixture!("hier.sv"))],
        open: &["hier.sv"],
        watch: &[],
        bench: None,
    },
    Sample {
        id: "trace_demo",
        what: "Why is `out_data` the value it is? A recording beside the design. Click a wire, \
               then follow it back to the edge in the Trace view.",
        top: None,
        tab: Some("Trace"),
        files: &[
            ("trace_demo.sv", fixture!("trace_demo.sv")),
            ("trace_demo.vcd", fixture!("waves/trace_demo.vcd")),
            ("trace_demo_stim.json", fixture!("trace_demo_stim.json")),
        ],
        open: &["trace_demo.sv", "trace_demo.vcd"],
        watch: &[],
        bench: None,
    },
    Sample {
        id: "depth_demo",
        what: "How many clocks from here to there? Four roads leave `sample_in`, and one is a \
               bug the count finds. Pipeline tab: clocks from `sample_in` to `stamped`.",
        top: None,
        tab: Some("Pipeline"),
        files: &[
            ("depth_demo.sv", fixture!("depth_demo.sv")),
            ("depth_demo.fst", fixture!("waves/depth_demo.fst")),
        ],
        open: &["depth_demo.sv", "depth_demo.fst"],
        watch: &[],
        bench: None,
    },
    Sample {
        id: "pipeline3",
        what: "A three-stage valid/data pipeline, with a recording of it. `stages…` in the \
               waveform lays the stages against the cycles.",
        top: None,
        tab: Some("Pipeline"),
        files: &[
            ("pipeline3.sv", fixture!("pipeline3.sv")),
            ("pipeline3.vcd", fixture!("waves/pipeline3.vcd")),
        ],
        open: &["pipeline3.sv", "pipeline3.vcd"],
        watch: &[],
        bench: None,
    },
    Sample {
        id: "fsm",
        what: "A two-process state machine, with a recording of it. The FSM tab draws it from \
               the case statement, and rings the state the cursor is in.",
        top: None,
        tab: Some("FSM"),
        files: &[("fsm.sv", fixture!("fsm.sv")), ("fsm.vcd", fixture!("waves/fsm.vcd"))],
        open: &["fsm.sv", "fsm.vcd"],
        watch: &["state"],
        bench: None,
    },
    Sample {
        id: "cdc",
        what: "Two clocks and three ways of crossing between them: a synchroniser, a \
               handshake, and one that is neither. CDC tab.",
        top: None,
        tab: Some("CDC"),
        files: &[("cdc.sv", fixture!("cdc.sv"))],
        open: &["cdc.sv"],
        watch: &[],
        bench: None,
    },
    Sample {
        id: "latch",
        what: "Combinational logic that holds a value instead of driving it: two latches, and \
               two that only look like one. Lint tab.",
        top: Some("latch_check"),
        tab: Some("Lint"),
        files: &[("latch.sv", fixture!("latch.sv"))],
        open: &["latch.sv"],
        watch: &[],
        bench: None,
    },
    Sample {
        id: "comb_loop",
        what: "A cycle of wires with nothing clocked in between, next to a chain of them that \
               is not one. Lint tab.",
        top: None,
        tab: Some("Lint"),
        files: &[("comb_loop.sv", fixture!("comb_loop.sv"))],
        open: &["comb_loop.sv"],
        watch: &[],
        bench: None,
    },
    Sample {
        id: "reconverge",
        what: "Two roads from `a` to `sum`, one clock longer than the other. The depth report \
               refuses to average them.",
        top: None,
        tab: Some("Pipeline"),
        files: &[("reconverge.sv", fixture!("reconverge.sv"))],
        open: &["reconverge.sv"],
        watch: &[],
        bench: None,
    },
    Sample {
        id: "fifo",
        what: "A parameterised FIFO: a memory array, `$clog2`, an asynchronous reset. Press \
               `simulate this design` for a waveform of it.",
        top: None,
        tab: None,
        files: &[("fifo.sv", fixture!("fifo.sv"))],
        open: &["fifo.sv"],
        watch: &[],
        bench: None,
    },
    Sample {
        id: "counter",
        what: "The smallest clocked design: a counter with an enable and a synchronous reset. \
               Draw a stimulus in the Stim tab and play it.",
        top: None,
        tab: Some("Stim"),
        files: &[("counter.sv", fixture!("counter.sv"))],
        open: &["counter.sv"],
        watch: &[],
        bench: None,
    },
    Sample {
        id: "axi4",
        what: "AXI4, the memory-mapped kind: a master writes a four-beat burst, reads it back, \
               and reads an address that is not there. The five channels in the diagram, the \
               sequence as the master's FSM, every handshake in the recording — and `decode…` \
               folds them into transactions. The master rests and runs it again, so the \
               recording holds two rounds.",
        top: None,
        tab: Some("FSM"),
        files: &[
            ("axi4_demo.sv", fixture!("axi4_demo.sv")),
            ("axi4_demo.fst", fixture!("waves/axi4_demo.fst")),
        ],
        open: &["axi4_demo.sv", "axi4_demo.fst"],
        // The three state machines first, by name — `M_AW`, `W_DATA` — since
        // they narrate the bus below them; then the bus, channel by channel,
        // handshake first. The recording exists to show these, and a reader
        // should not have to pick twenty names before the first is on screen.
        watch: &[
            "u_master.state",
            "u_slave.wstate",
            "u_slave.rstate",
            "axi_awvalid",
            "axi_awready",
            "axi_awaddr",
            "axi_awlen",
            "axi_wvalid",
            "axi_wready",
            "axi_wdata",
            "axi_wlast",
            "axi_bvalid",
            "axi_bready",
            "axi_bresp",
            "axi_arvalid",
            "axi_arready",
            "axi_araddr",
            "axi_arlen",
            "axi_rvalid",
            "axi_rready",
            "axi_rdata",
            "axi_rresp",
            "axi_rlast",
        ],
        bench: None,
    },
    Sample {
        id: "veryl",
        what: "The same kind of design as `hier` and `fsm`, written in Veryl: a start button, \
               a timer, a four-state controller and a row of lights, sharing a `Defs` package \
               and a `Ticker` interface. Every view reads the SystemVerilog Veryl wrote, and \
               every line it points at is in the `.veryl`.",
        top: None,
        tab: Some("Source"),
        files: &[
            ("Veryl.toml", fixture!("lights/Veryl.toml")),
            ("src/defs.veryl", fixture!("lights/src/defs.veryl")),
            ("src/ticker.veryl", fixture!("lights/src/ticker.veryl")),
            ("src/top.veryl", fixture!("lights/src/top.veryl")),
            ("src/timer.veryl", fixture!("lights/src/timer.veryl")),
            ("src/control.veryl", fixture!("lights/src/control.veryl")),
            ("src/show.veryl", fixture!("lights/src/show.veryl")),
            // What `veryl build` wrote, shipped so the sample opens on a machine
            // without Veryl. With Veryl installed it is built again on opening.
            ("lights.f", fixture!("lights/lights.f")),
            ("target/defs.sv", fixture!("lights/target/defs.sv")),
            ("target/defs.sv.map", fixture!("lights/target/defs.sv.map")),
            ("target/ticker.sv", fixture!("lights/target/ticker.sv")),
            ("target/ticker.sv.map", fixture!("lights/target/ticker.sv.map")),
            ("target/top.sv", fixture!("lights/target/top.sv")),
            ("target/top.sv.map", fixture!("lights/target/top.sv.map")),
            ("target/timer.sv", fixture!("lights/target/timer.sv")),
            ("target/timer.sv.map", fixture!("lights/target/timer.sv.map")),
            ("target/control.sv", fixture!("lights/target/control.sv")),
            ("target/control.sv.map", fixture!("lights/target/control.sv.map")),
            ("target/show.sv", fixture!("lights/target/show.sv")),
            ("target/show.sv.map", fixture!("lights/target/show.sv.map")),
        ],
        watch: &[],
        bench: None,
        open: &["."],
    },
    Sample {
        id: "cpu",
        what: "A small CPU across seven files, and a blinker beside it: two designs in one \
               folder, so the window asks which one is the top. Its testbench comes too, \
               so `simulate` runs that rather than a generated harness.",
        top: None,
        tab: None,
        files: &[
            ("cpu_pkg.sv", fixture!("cpu/cpu_pkg.sv")),
            ("imem.sv", fixture!("cpu/imem.sv")),
            ("alu.sv", fixture!("cpu/alu.sv")),
            ("regfile.sv", fixture!("cpu/regfile.sv")),
            ("control.sv", fixture!("cpu/control.sv")),
            ("datapath.sv", fixture!("cpu/datapath.sv")),
            ("cpu.sv", fixture!("cpu/cpu.sv")),
            ("blink.sv", fixture!("cpu/blink.sv")),
            ("cpu_tb.sv", fixture!("cpu/cpu_tb.sv")),
        ],
        // The design by name rather than the folder, so the testbench beside
        // it is not read as a third design.
        open: &[
            "cpu_pkg.sv",
            "imem.sv",
            "alu.sv",
            "regfile.sv",
            "control.sv",
            "datapath.sv",
            "cpu.sv",
            "blink.sv",
        ],
        watch: &[],
        bench: Some("cpu_tb.sv"),
    },
];

/// The sample called this, if there is one.
pub fn find(id: &str) -> Option<&'static Sample> {
    ALL.iter().find(|sample| sample.id == id)
}

/// `%LOCALAPPDATA%\rtlscope\samples`, or the XDG equivalent.
///
/// Beside the layout file rather than in a temporary directory, because a
/// temporary directory is emptied, and a reader who drew a stimulus against
/// a sample last week expects to find it where they left it.
pub fn home() -> Option<PathBuf> {
    Some(crate::layout::settings_dir()?.join("samples"))
}

/// Where a sample ended up.
pub struct Written {
    /// The folder it was written into.
    pub dir: PathBuf,
    /// What to open, as absolute paths.
    pub open: Vec<PathBuf>,
    /// How many files were actually written. The rest were already there,
    /// byte for byte.
    pub changed: usize,
}

impl Sample {
    /// Puts the files under `home/<id>/`, and says what to open.
    ///
    /// A file already there with the same bytes is left alone, so opening a
    /// sample twice does not touch the disk twice. One that differs is put
    /// back as shipped: a reader who edited it to see what would happen and
    /// then asked for the sample again is asking for the sample.
    pub fn write(&self, home: &Path) -> std::io::Result<Written> {
        let dir = home.join(self.id);
        let mut changed = 0;
        for (relative, bytes) in self.files {
            let path = dir.join(relative);
            if std::fs::read(&path).is_ok_and(|on_disk| on_disk == *bytes) {
                continue;
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, bytes)?;
            changed += 1;
        }
        let open = self
            .open
            .iter()
            .map(|relative| match *relative {
                "." => dir.clone(),
                other => dir.join(other),
            })
            .collect();
        Ok(Written { dir, open, changed })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rtlscope-samples-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn every_sample_has_a_name_of_its_own() {
        let mut ids: Vec<&str> = ALL.iter().map(|sample| sample.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(
            ids.len(),
            ALL.len(),
            "two samples share a name, so one would overwrite the other"
        );
        assert!(find("hier").is_some());
        assert!(find("no-such-sample").is_none());
    }

    /// The disk is the whole feature: what is embedded has to come out as the
    /// bytes that went in, and a second write must find nothing to do.
    #[test]
    fn every_sample_writes_itself_once() {
        let home = scratch("write-once");
        for sample in ALL {
            let first = sample.write(&home).expect("writes");
            assert_eq!(first.changed, sample.files.len(), "{}: every file was new", sample.id);
            for (relative, bytes) in sample.files {
                let on_disk = std::fs::read(first.dir.join(relative)).expect("is there");
                assert_eq!(on_disk, *bytes, "{}: {relative} came back as it went in", sample.id);
            }
            for path in &first.open {
                assert!(path.exists(), "{}: {} is something to open", sample.id, path.display());
            }
            let second = sample.write(&home).expect("writes again");
            assert_eq!(second.changed, 0, "{}: and had nothing to do the second time", sample.id);
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// An edited sample is put back as shipped, and only that file.
    #[test]
    fn an_edited_file_is_put_back() {
        let home = scratch("put-back");
        let sample = find("trace_demo").expect("is a sample");
        let written = sample.write(&home).expect("writes");
        let edited = written.dir.join("trace_demo.sv");
        std::fs::write(&edited, b"module gone; endmodule\n").expect("edits");
        let again = sample.write(&home).expect("writes again");
        assert_eq!(again.changed, 1, "one file differed");
        assert_eq!(std::fs::read(&edited).unwrap(), fixture!("trace_demo.sv"), "and is as shipped");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The cpu sample is the folder under `tests/fixtures/cpu`, whole.
    ///
    /// The same check the fixtures crate makes of its own list: a file added
    /// to the folder and not to the sample would ship a project with a module
    /// missing, which is a different design and not an obviously wrong one.
    #[test]
    fn the_cpu_sample_is_the_whole_folder() {
        let root = rtlscope_fixtures::dir().join("cpu");
        let mut on_disk = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("reads the folder") {
                let path = entry.expect("an entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let relative = path.strip_prefix(&root).expect("under the root");
                    on_disk.push(relative.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        on_disk.sort();
        let mut listed: Vec<String> =
            find("cpu").unwrap().files.iter().map(|(name, _)| (*name).to_owned()).collect();
        listed.sort();
        assert_eq!(on_disk, listed, "tests/fixtures/cpu and the cpu sample have drifted apart");
    }
}
