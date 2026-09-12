//! AXI4-Stream, as video carries it.
//!
//! The difference from a plain valid-with-markers stream is `tready`: the sink
//! can refuse, and a beat only happens when both sides agree. That makes one
//! number worth having that the simpler protocol has no way to produce —
//! **how often the source was ready and the sink was not** — which is where a
//! pipeline that cannot keep up shows itself.
//!
//! ```text
//! tdata[23:0]  tvalid  tready  tlast  tuser[0:0]  tkeep[3:0]
//! ```
//!
//! Video on AXI-Stream uses two of those fields by convention rather than by
//! the specification: `tuser` bit 0 marks the start of a frame, and `tlast`
//! marks the end of a *line* rather than the end of the frame. That is the
//! Xilinx convention this design follows, and it is a convention — so it is
//! stated in the report rather than assumed silently.

use std::collections::BTreeMap;

use super::{
    Annotation, ChannelSpec, DecodeReport, Decoder, Level, ResolvedBindings, Sampler, Transaction,
};
use crate::dump::Dump;

pub struct AxiStream;

const CHANNELS: &[ChannelSpec] = &[
    ChannelSpec { role: "clock", required: true, doc: "the clock the stream runs on" },
    ChannelSpec { role: "tvalid", required: true, doc: "the source has data" },
    ChannelSpec { role: "tready", required: true, doc: "the sink can take it" },
    ChannelSpec { role: "tdata", required: false, doc: "the payload" },
    ChannelSpec { role: "tlast", required: false, doc: "end of line, by video convention" },
    ChannelSpec { role: "tuser", required: false, doc: "bit 0 marks the start of a frame" },
    ChannelSpec { role: "tkeep", required: false, doc: "which bytes of the beat count" },
];

struct Line {
    start: u64,
    beats: u64,
}

struct Frame {
    index: u64,
    start: u64,
    lines: u64,
    beats: u64,
}

impl Decoder for AxiStream {
    fn protocol(&self) -> &'static str {
        "axis"
    }

    fn doc(&self) -> &'static str {
        "AXI4-Stream as video carries it: a beat is `tvalid && tready`, `tuser` \
         bit 0 starts a frame and `tlast` ends a line. Counts frames, lines and \
         beats, and measures how often the sink held the source up."
    }

    fn channels(&self) -> &'static [ChannelSpec] {
        CHANNELS
    }

    fn decode(&self, dump: &Dump, bindings: &ResolvedBindings) -> DecodeReport {
        let mut report = DecodeReport::new(self.protocol());
        report.bindings = bindings.listing(dump);

        let roles = ["tvalid", "tready", "tdata", "tlast", "tuser", "tkeep"];
        let mut sampler = match Sampler::new(dump, bindings, "clock", &roles) {
            Ok(sampler) => sampler,
            Err(error) => {
                report.problem(error.to_string());
                return report;
            }
        };
        report.stat("clock edges", sampler.edges());

        let mut beats = 0u64;
        let mut stalls = 0u64;
        let mut idle = 0u64;
        let mut undriven = 0u64;
        let mut partial = 0u64;
        let mut longest_stall = 0u64;
        let mut current_stall = 0u64;

        let mut line: Option<Line> = None;
        let mut frame: Option<Frame> = None;
        let mut frames_done = 0u64;
        let mut lines: Vec<(u64, u64, u64)> = Vec::new();
        let mut line_lengths: BTreeMap<u64, u64> = BTreeMap::new();

        while let Some(sample) = sampler.advance() {
            let time = sample.time;
            let (Some(valid), Some(ready)) = (sample.high("tvalid"), sample.high("tready")) else {
                // Either side undriven means this cycle cannot be read as a
                // beat or as its absence.
                undriven += 1;
                continue;
            };

            if !valid {
                idle += 1;
                current_stall = 0;
                continue;
            }
            if !ready {
                stalls += 1;
                current_stall += 1;
                longest_stall = longest_stall.max(current_stall);
                continue;
            }
            current_stall = 0;
            beats += 1;

            // `tkeep` below all-ones means some byte lanes carry nothing.
            if let Some(keep) = sample.value("tkeep")
                && let Some(bits) = keep.as_u64()
            {
                let all = if keep.width() >= 64 { u64::MAX } else { (1u64 << keep.width()) - 1 };
                if bits != all {
                    partial += 1;
                }
            }

            // By convention, not by specification: bit 0 of `tuser`.
            let sof = sample.number("tuser").is_some_and(|user| user & 1 == 1);
            let eol = sample.high("tlast").unwrap_or(false);

            if sof {
                if let Some(open) = frame.take() {
                    report.problem(format!(
                        "a frame started at {time} while the one from {} had not ended",
                        open.start
                    ));
                }
                frames_done += 1;
                frame = Some(Frame { index: frames_done, start: time, lines: 0, beats: 0 });
            }

            let open_line = line.get_or_insert(Line { start: time, beats: 0 });
            open_line.beats += 1;
            if let Some(open) = frame.as_mut() {
                open.beats += 1;
            }

            if eol {
                let done = line.take().expect("a line was open");
                *line_lengths.entry(done.beats).or_default() += 1;
                lines.push((done.start, time, done.beats));
                if let Some(open) = frame.as_mut() {
                    open.lines += 1;
                }
            }
        }

        // A frame ends where the next one starts, since `tlast` marks lines
        // rather than frames — so the last one is closed by the dump ending.
        if let Some(open) = frame.take() {
            let end = lines.last().map_or(open.start, |(_, end, _)| *end);
            report.annotations.push(
                Annotation::new(
                    open.start,
                    end,
                    0,
                    "frame",
                    format!("frame #{} — {} lines", open.index, open.lines),
                )
                .with("lines", open.lines)
                .with("beats", open.beats),
            );
            report.transactions.push(Transaction {
                t_start: open.start,
                t_end: end,
                kind: "frame".into(),
                fields: vec![
                    ("index".into(), open.index.to_string()),
                    ("lines".into(), open.lines.to_string()),
                    ("beats".into(), open.beats.to_string()),
                ],
            });
        }

        let usual = line_lengths.iter().max_by_key(|(_, count)| **count).map(|(len, _)| *len);
        for (start, end, count) in &lines {
            let odd = usual.is_some_and(|usual| *count != usual);
            let annotation = Annotation::new(*start, *end, 1, "line", format!("{count} beats"))
                .with("beats", count);
            report.annotations.push(if odd {
                annotation.at(Level::Error).with("expected", usual.unwrap_or_default())
            } else {
                annotation
            });
        }

        if line.is_some() {
            report.problem("the dump ends inside a line that never reached its `tlast`");
        }
        if undriven > 0 {
            report
                .problem(format!("`tvalid` or `tready` was undriven on {undriven} clock edge(s)"));
        }
        if !bindings.has("tuser") {
            report.problem("no `tuser` was bound, so frames could not be told apart");
        }
        if !bindings.has("tlast") {
            report.problem("no `tlast` was bound, so lines could not be counted");
        }

        report.stat("beats", beats);
        report.stat("frames", frames_done);
        report.stat("lines", lines.len());
        if let Some(usual) = usual {
            report.stat("beats per line", usual);
            let odd = lines.iter().filter(|(_, _, n)| *n != usual).count();
            if odd > 0 {
                report.stat("lines of another length", odd);
            }
        }
        // What the source offered, and how much of it the sink took.
        let offered = beats + stalls;
        report.stat("stall cycles", stalls);
        if offered > 0 {
            report.stat("stalled", format!("{:.1}%", stalls as f64 * 100.0 / offered as f64));
        }
        if longest_stall > 0 {
            report.stat("longest stall", format!("{longest_stall} cycles"));
        }
        report.stat("idle cycles", idle);
        if partial > 0 {
            report.stat("partial beats (tkeep)", partial);
        }
        report
    }
}
