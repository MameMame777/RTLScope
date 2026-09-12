//! The server, spoken to the way a client speaks to it.
//!
//! These drive the real binary over a real pipe rather than calling the tool
//! functions directly, because most of what could break lives between the two:
//! the schemas the macros generate, the JSON-RPC framing, and the shutdown that
//! happens when stdin closes.
//!
//! The pipe is kept open until every answer has come back. A client holds stdin
//! for the life of the session; closing it early cancels whatever is still
//! being worked on, and a test that did that would be testing its own harness.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Server {
    fn start() -> Self {
        Server::start_in(&somewhere_with_no_session())
    }

    /// A server whose idea of "where session state lives" is the given
    /// directory.
    ///
    /// Leaving that to the machine would make these tests depend on whether an
    /// RTLScope window happens to be open, since a window leaves a note saying
    /// which design it has and the server reads it when a query names no files.
    /// A test that passes or fails on that is not testing the server.
    fn start_in(state: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rtlscope-mcp"))
            .env("LOCALAPPDATA", state)
            .env("XDG_STATE_HOME", state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the server binary starts");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut server = Self { child, stdin, stdout, next_id: 1 };

        server.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "rtlscope-tests", "version": "0" },
            }),
        );
        server.notify("notifications/initialized");
        server
    }

    fn send(&mut self, message: &serde_json::Value) {
        writeln!(self.stdin, "{message}").expect("the server is still listening");
        self.stdin.flush().expect("flush");
    }

    fn notify(&mut self, method: &str) {
        let message = serde_json::json!({ "jsonrpc": "2.0", "method": method });
        self.send(&message);
    }

    /// Sends one request and reads until its answer comes back.
    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let message =
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.send(&message);

        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).expect("the server writes a reply");
            assert!(read > 0, "the server closed before answering `{method}`");
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
            if value.get("id").and_then(serde_json::Value::as_i64) == Some(id) {
                return value;
            }
        }
    }

    fn call(&mut self, tool: &str, arguments: serde_json::Value) -> serde_json::Value {
        self.request("tools/call", serde_json::json!({ "name": tool, "arguments": arguments }))
    }

    /// The `structuredContent` of a call that was meant to succeed.
    fn result(&mut self, tool: &str, arguments: serde_json::Value) -> serde_json::Value {
        let reply = self.call(tool, arguments);
        assert!(reply.get("error").is_none(), "`{tool}` failed: {reply}");
        reply["result"]["structuredContent"].clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fixture(name: &str) -> String {
    rtlscope_fixtures::path(name).display().to_string()
}

#[test]
fn every_view_is_offered_as_a_tool() {
    let mut server = Server::start();
    let reply = server.request("tools/list", serde_json::json!({}));
    let tools = reply["result"]["tools"].as_array().expect("a list of tools");

    let mut names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "buses",
            "clock_domains",
            "cone",
            "decode",
            "diagnostics",
            "diagram",
            "drivers",
            "dump_signals",
            "lint",
            "module",
            "modules",
            "path_latency",
            "pipeline_depth",
            "protocols",
            "signals",
            "stage_cycles",
            "state_machines",
            "test_results",
            "yosys_check",
        ]
    );

    // A tool that reads a design asks for its sources; one that reads a dump
    // asks for the dump; and only the ones that name a module need one.
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or_default();
        // A tool with no parameters has no `required` key at all.
        let required: Vec<&str> = tool["inputSchema"]["required"]
            .as_array()
            .map(|list| list.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        // `files` is required by nothing: leaving it out asks about the
        // design open in RTLScope's window. What each tool cannot do without is
        // its own subject — a dump to read, a netlist to compare against.
        assert!(!required.contains(&"files"), "`{name}` still demands files: {required:?}");
        match name {
            "dump_signals" | "decode" | "stage_cycles" => {
                assert!(required.contains(&"dump"), "{name}: {required:?}")
            }
            // A measurement needs the recording and both ends of the road.
            // `pipeline_depth` takes the same two ends and requires neither:
            // without them it answers the whole design's depth.
            "path_latency" => {
                for wanted in ["dump", "from", "to"] {
                    assert!(required.contains(&wanted), "{name} wants {wanted}: {required:?}");
                }
            }
            // A run's verdict is read on its own; the dump it happened in is
            // optional, because a moment in nanoseconds is useful without one.
            "test_results" => assert!(required.contains(&"results"), "{name}: {required:?}"),
            "yosys_check" => assert!(required.contains(&"netlist"), "{name}: {required:?}"),
            _ => {}
        }
        let names_a_module = matches!(name, "module" | "diagram" | "buses" | "drivers");
        assert_eq!(
            required.contains(&"module"),
            names_a_module,
            "`{name}` requires the wrong things: {required:?}"
        );
    }
}

#[test]
fn the_overview_names_the_modules_the_other_tools_take() {
    let mut server = Server::start();
    let overview = server
        .result("modules", serde_json::json!({ "files": [fixture("hier.sv")], "top": "hier_top" }));

    assert_eq!(overview["top"], "hier_top");
    let names: Vec<&str> = overview["modules"]
        .as_array()
        .expect("modules")
        .iter()
        .filter_map(|m| m["name"].as_str())
        .collect();
    assert!(names.contains(&"hier_top"), "{names:?}");
    assert!(names.contains(&"hier_alu"), "{names:?}");

    let hierarchy: Vec<&str> =
        overview["hierarchy"].as_array().unwrap().iter().filter_map(|h| h.as_str()).collect();
    assert!(hierarchy.contains(&"u_alu : hier_alu"), "{hierarchy:?}");

    // And a name it reports really is one `module` accepts.
    let detail = server.result(
        "module",
        serde_json::json!({ "files": [fixture("hier.sv")], "top": "hier_top", "module": "hier_alu" }),
    );
    assert_eq!(detail["name"], "hier_alu");
    assert!(detail["ports"].as_array().is_some_and(|p| !p.is_empty()));
}

/// One tool, two questions. Without `from` and `to` it is the whole design's
/// depth; with them it is the distance between two points. They come together
/// or not at all, because one signal is not a distance.
#[test]
fn the_depth_tool_answers_the_design_and_the_road_between_two_signals() {
    let mut server = Server::start();
    let sources = serde_json::json!({
        "files": [fixture("pipeline3.sv")],
        "top": "pipeline3",
    });

    let whole = server.result("pipeline_depth", sources.clone());
    assert_eq!(whole["registers"], 6, "the design's own depth still answers");
    assert!(whole["domains"].as_array().is_some_and(|d| !d.is_empty()));

    let mut asked = sources.as_object().expect("an object").clone();
    asked.insert("from".into(), "in_data".into());
    asked.insert("to".into(), "out_data".into());
    let road = server.result("pipeline_depth", serde_json::Value::Object(asked));
    assert_eq!(road["min_stages"], 3, "{road}");
    assert_eq!(road["max_stages"], 3);
    assert_eq!(road["clock"], "clk");

    // Half a question is refused rather than guessed at.
    let mut half = sources.as_object().expect("an object").clone();
    half.insert("from".into(), "in_data".into());
    let reply = server.call("pipeline_depth", serde_json::Value::Object(half));
    let said = reply.to_string();
    assert!(said.contains("come together"), "{said}");
}

/// The names the other tools take, listed. A real design has thousands, so the
/// answer is capped and says how many it did not show.
#[test]
fn the_signal_list_gives_names_the_other_tools_accept() {
    let mut server = Server::start();
    let listed = server.result(
        "signals",
        serde_json::json!({
            "files": [fixture("pipeline3.sv")],
            "top": "pipeline3",
            "pattern": "data",
        }),
    );

    let names: Vec<&str> = listed["signals"]
        .as_array()
        .expect("signals")
        .iter()
        .filter_map(|entry| entry["name"].as_str())
        .collect();
    assert!(names.contains(&"in_data"), "{names:?}");
    assert!(names.contains(&"data_d2"), "{names:?}");
    assert!(!names.iter().any(|name| name.contains("valid")), "the pattern narrowed: {names:?}");

    // And a name it gave really is one `pipeline_depth` accepts.
    let road = server.result(
        "pipeline_depth",
        serde_json::json!({
            "files": [fixture("pipeline3.sv")],
            "top": "pipeline3",
            "from": "in_data",
            "to": "data_d2",
        }),
    );
    assert_eq!(road["min_stages"], 2, "{road}");
}

/// The analyses reach the wire unchanged: same fixtures, same answers as the
/// library gives, so a question asked here and at the terminal cannot differ.
#[test]
fn the_analyses_come_back_as_the_library_produced_them() {
    let mut server = Server::start();

    let machines = server.result(
        "state_machines",
        serde_json::json!({ "files": [fixture("fsm.sv")], "top": "fsm" }),
    );
    let machines = machines.as_array().expect("a list of machines");
    assert_eq!(machines.len(), 1);
    assert_eq!(machines[0]["state_name"], "state");
    assert_eq!(machines[0]["reset_state"], "S_IDLE");

    let domains = server.result(
        "clock_domains",
        serde_json::json!({ "files": [fixture("cdc.sv")], "top": "cdc_top" }),
    );
    assert_eq!(domains["domains"].as_array().unwrap().len(), 2);
    let kinds: Vec<&str> = domains["crossings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"two_flop_synchroniser"), "{kinds:?}");
    assert!(kinds.contains(&"multi_bit_unsynchronised"), "{kinds:?}");

    let latches = server.result(
        "lint",
        serde_json::json!({ "files": [fixture("latch.sv")], "top": "latch_check" }),
    );
    assert_eq!(latches["latches"].as_array().unwrap().len(), 2);

    let depth = server.result(
        "pipeline_depth",
        serde_json::json!({ "files": [fixture("pipeline3.sv")], "top": "pipeline3" }),
    );
    assert_eq!(depth["registers"], 6);
    assert_eq!(depth["domains"][0]["depth"], 3);
}

#[test]
fn a_diagram_comes_back_as_svg() {
    let mut server = Server::start();
    let diagram = server.result(
        "diagram",
        serde_json::json!({ "files": [fixture("hier.sv")], "top": "hier_top", "module": "hier_top" }),
    );
    let svg = diagram["svg"].as_str().expect("svg text");
    assert!(svg.starts_with("<svg"), "{}", &svg[..40.min(svg.len())]);
    assert!(svg.contains("</svg>"));
}

/// A question that cannot be answered says why, in terms of what to do next.
#[test]
fn what_cannot_be_answered_says_what_to_do_instead() {
    let mut server = Server::start();

    let missing = server.call("modules", serde_json::json!({ "files": ["/no/such/file.sv"] }));
    let message = missing["error"]["message"].as_str().expect("a message");
    assert!(message.contains("no file at"), "{message}");

    let unknown = server.call(
        "module",
        serde_json::json!({ "files": [fixture("hier.sv")], "top": "hier_top", "module": "nope" }),
    );
    let message = unknown["error"]["message"].as_str().expect("a message");
    assert!(message.contains("`modules` lists the names"), "{message}");

    // No window has left a note here, so there is nothing to fall back to and
    // the server says what to do instead.
    let nothing = server.call("modules", serde_json::json!({ "files": [] }));
    let message = nothing["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("no files given"), "{nothing}");
    assert!(message.contains("rtlscope-gui"), "and how to avoid passing them: {message}");
}

/// The other half of that: when a window *has* left a note, an empty `files`
/// means the design it has open — and the answer says which one, because being
/// asked about "the design" and answering about another is the failure this
/// exists to prevent.
#[test]
fn leaving_out_the_files_reads_the_design_a_window_has_open() {
    let state = somewhere_with_no_session();
    let note = state.join("rtlscope");
    std::fs::create_dir_all(&note).expect("a scratch directory");
    std::fs::write(
        note.join("session.json"),
        serde_json::json!({
            "files": [fixture("hier.sv")],
            "top": "hier_top",
            "pid": 1,
            "written_at": 0,
        })
        .to_string(),
    )
    .expect("writes");

    let mut server = Server::start_in(&state);
    let overview = server.call("modules", serde_json::json!({}));
    let answer = &overview["result"]["structuredContent"];

    assert_eq!(answer["top"], "hier_top", "{overview}");
    let source = answer["source"].as_str().unwrap_or_default();
    assert!(source.contains("hier.sv"), "the answer says which design it read: {overview}");
}

/// A scratch directory for one test's session state, emptied first.
fn somewhere_with_no_session() -> std::path::PathBuf {
    let at = std::env::temp_dir().join("rtlscope-mcp-state").join(format!(
        "{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&at);
    std::fs::create_dir_all(&at).expect("a scratch directory");
    at
}

/// A dump read and decoded over the wire, and the buses proposed from the
/// design that produced it.
#[test]
fn a_waveform_is_read_and_decoded_through_the_protocol() {
    let mut server = Server::start();

    let vcd = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../rtlscope-cli/tests/data/sccb.vcd")
        .canonicalize()
        .expect("the checked-in fixture");
    let vcd = vcd.display().to_string();

    let listed = server.result("protocols", serde_json::json!({}));
    let names: Vec<&str> =
        listed.as_array().expect("a list").iter().filter_map(|p| p["protocol"].as_str()).collect();
    assert!(names.contains(&"i2c"), "{names:?}");
    assert!(names.contains(&"axi4"), "{names:?}");

    let summary = server.result("dump_signals", serde_json::json!({ "dump": &vcd }));
    let signals: Vec<&str> =
        summary["signals"].as_array().unwrap().iter().filter_map(|s| s.as_str()).collect();
    assert_eq!(signals, ["tb.scl", "tb.sda"], "{summary:#?}");

    let decoded = server.result(
        "decode",
        serde_json::json!({
            "dump": &vcd,
            "protocol": "i2c",
            "map": ["scl=tb.scl", "sda=tb.sda"],
        }),
    );
    let transactions = decoded["transactions"].as_array().expect("transactions");
    assert_eq!(transactions.len(), 1, "{decoded:#?}");
    assert_eq!(transactions[0]["kind"], "sccb-write");
    assert!(decoded["problems"].as_array().unwrap().is_empty(), "{decoded:#?}");
}

/// Asking for a protocol that does not exist names the ones that do.
#[test]
fn an_unknown_protocol_names_the_known_ones() {
    let mut server = Server::start();
    let reply = server
        .call("decode", serde_json::json!({ "dump": "/nope.vcd", "protocol": "spi", "map": [] }));
    let message = reply["error"]["message"].as_str().expect("a message");
    assert!(message.contains("axi4"), "{message}");
    assert!(message.contains("i2c"), "{message}");
}

/// The pipeline and the dump reach the wire together, which is the whole point
/// of the tool: the shape comes from the source and the occupancy from the run.
#[test]
fn the_stages_come_back_laid_against_the_dump() {
    let mut server = Server::start();
    let view = server.result(
        "stage_cycles",
        serde_json::json!({
            "files": [fixture("pipeline3.sv")],
            "top": "pipeline3",
            "dump": rtlscope_fixtures::wave("pipeline3.vcd").display().to_string(),
            "count": 16,
        }),
    );

    assert_eq!(view["clock"], "clk");
    assert_eq!(view["clock_path"], "tb.dut.clk");
    assert_eq!(view["depth"], 3);
    let rows = view["rows"].as_array().expect("a row per stage");
    assert_eq!(rows.len(), 3);

    // Which bit decided the row is part of the answer, not a footnote.
    assert_eq!(rows[0]["basis"]["from"], "valid");
    assert_eq!(rows[0]["basis"]["signal"], "valid_d1");
    assert_eq!(rows[0]["payload"], "data_d1");

    let cells: Vec<&str> =
        rows[0]["cells"].as_array().unwrap().iter().filter_map(|c| c.as_str()).collect();
    assert_eq!(cells.len(), 16);
    assert_eq!(&cells[..4], ["busy", "busy", "busy", "busy"]);
    assert_eq!(&cells[4..8], ["idle", "idle", "idle", "idle"]);
}

/// A failing test names a moment, and the moment is only useful as a tick of
/// the dump it happened in — so both come back, and so does how they were
/// arrived at.
#[test]
fn a_failing_test_comes_back_with_where_to_look_for_it() {
    let mut server = Server::start();
    let run = server.result(
        "test_results",
        serde_json::json!({
            "results": rtlscope_fixtures::wave("results.xml").display().to_string(),
            "dump": rtlscope_fixtures::wave("pipeline3.vcd").display().to_string(),
        }),
    );

    let tests = run["tests"].as_array().expect("a list of tests");
    assert_eq!(tests.len(), 2);
    assert_eq!(tests[0]["outcome"], "passed");
    assert_eq!(tests[1]["outcome"], "failed");
    assert_eq!(tests[1]["message"], "AssertionError: beat 3: out_data 0xfffe, wanted 0x1");
    // The moments are accumulated, not recorded, and the answer says so.
    assert_eq!(tests[1]["start_ns"], 300.0);
    assert_eq!(tests[1]["end_ns"], 400.0);
    assert!(run["basis"].as_str().is_some_and(|text| text.contains("added up")), "{run}");

    let ticks: Vec<i64> =
        run["ticks"].as_array().unwrap().iter().filter_map(|t| t.as_i64()).collect();
    assert_eq!(ticks, vec![300, 400], "one tick per test, in this dump's own units");
}

/// The one answer here that does not come from RTLScope reading the source.
#[test]
fn the_second_opinion_comes_back_as_a_verdict() {
    let mut server = Server::start();
    let report = server.result(
        "yosys_check",
        serde_json::json!({
            "files": [fixture("hier.sv")],
            "top": "hier_top",
            "netlist": rtlscope_fixtures::netlist("hier.json").display().to_string(),
        }),
    );

    assert!(report["creator"].as_str().is_some_and(|c| c.starts_with("Yosys")), "{report}");
    assert_eq!(report["differences"].as_array().map(Vec::len), Some(0));
    assert_eq!(report["modules"].as_array().map(Vec::len), Some(4));
    assert!(report["checked"].as_u64().is_some_and(|n| n > 50), "{report}");
    // What could not be compared is part of the answer, not a footnote.
    assert!(report["notes"].as_array().is_some_and(|n| !n.is_empty()), "{report}");
}
