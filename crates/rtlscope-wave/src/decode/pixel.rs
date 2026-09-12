//! The valid-with-markers pixel stream.
//!
//! The commonest interface in the design this was measured against, and the
//! simplest: a `valid`, some data, and four one-cycle markers saying where the
//! frame and the lines begin and end. There is no `ready`, so a beat is any
//! cycle where `valid` is high, and everything else is bookkeeping.
//!
//! ```text
//! in_pixel[23:0]  in_valid  in_sof  in_eol  in_eof  in_err
//! ```
//!
//! What makes it worth decoding is the counting. A frame that should be 480
//! lines and is 495, or a line that is short by two pixels, is invisible in a
//! trace and obvious in a summary — and is the bug this particular design kept
//! hitting. So lines whose length disagrees with the rest are reported
//! individually as well as counted.
//!
//! One rule about markers: they are honoured only while `valid` is high. A
//! `sof` pulse on a cycle with no beat is either a different convention or a
//! mistake, and either way guessing which would be worse than saying so.

use std::collections::BTreeMap;

use super::{
    Annotation, ChannelSpec, DecodeReport, Decoder, Level, ResolvedBindings, Sampler, Transaction,
};
use crate::dump::Dump;

pub struct PixelStream;

const CHANNELS: &[ChannelSpec] = &[
    ChannelSpec { role: "clock", required: true, doc: "the clock the stream runs on" },
    ChannelSpec { role: "valid", required: true, doc: "high on a cycle carrying a pixel" },
    ChannelSpec { role: "data", required: false, doc: "the pixel itself" },
    ChannelSpec { role: "sof", required: false, doc: "start of frame, with the first pixel" },
    ChannelSpec { role: "eol", required: false, doc: "end of line, with the last pixel of it" },
    ChannelSpec { role: "eof", required: false, doc: "end of frame, with the last pixel" },
    ChannelSpec { role: "err", required: false, doc: "something went wrong on this beat" },
];

/// A line being counted.
struct Line {
    start: u64,
    pixels: u64,
}

/// A frame being counted.
struct Frame {
    index: u64,
    start: u64,
    lines: u64,
    pixels: u64,
}

impl Decoder for PixelStream {
    fn protocol(&self) -> &'static str {
        "pixel"
    }

    fn doc(&self) -> &'static str {
        "A valid-with-markers pixel stream: `valid` with `sof`/`eol`/`eof`/`err` \
         and no back-pressure. Counts frames, lines and pixels, and reports any \
         line whose length disagrees with the rest."
    }

    fn channels(&self) -> &'static [ChannelSpec] {
        CHANNELS
    }

    fn decode(&self, dump: &Dump, bindings: &ResolvedBindings) -> DecodeReport {
        let mut report = DecodeReport::new(self.protocol());
        report.bindings = bindings.listing(dump);

        let roles = ["valid", "data", "sof", "eol", "eof", "err"];
        let mut sampler = match Sampler::new(dump, bindings, "clock", &roles) {
            Ok(sampler) => sampler,
            Err(error) => {
                report.problem(error.to_string());
                return report;
            }
        };
        report.stat("clock edges", sampler.edges());

        let mut beats = 0u64;
        let mut errors = 0u64;
        let mut undriven = 0u64;
        let mut markers_without_a_beat = 0u64;
        let mut line: Option<Line> = None;
        let mut frame: Option<Frame> = None;
        let mut frames_done = 0u64;
        let mut line_lengths: BTreeMap<u64, u64> = BTreeMap::new();
        let mut lines: Vec<(u64, u64, u64)> = Vec::new(); // start, end, pixels

        while let Some(sample) = sampler.advance() {
            let time = sample.time;
            let marker = |name: &str| sample.high(name).unwrap_or(false);

            let valid = match sample.value("valid") {
                Some(value) => match value.as_bool() {
                    Some(high) => high,
                    None => {
                        // An undriven `valid` is not a beat and not the absence
                        // of one: it is a cycle nobody can read.
                        undriven += 1;
                        continue;
                    }
                },
                None => continue,
            };

            if !valid {
                if marker("sof") || marker("eol") || marker("eof") {
                    markers_without_a_beat += 1;
                }
                continue;
            }

            beats += 1;
            if marker("err") {
                errors += 1;
                report
                    .annotations
                    .push(Annotation::new(time, time, 2, "err", "err".into()).at(Level::Error));
            }

            if marker("sof") {
                if let Some(open) = frame.take() {
                    // A new frame starting before the last one ended.
                    report.problem(format!(
                        "a frame started at {time} while the one from {} had not ended",
                        open.start
                    ));
                }
                frames_done += 1;
                frame = Some(Frame { index: frames_done, start: time, lines: 0, pixels: 0 });
            }

            let entry = line.get_or_insert(Line { start: time, pixels: 0 });
            entry.pixels += 1;
            if let Some(open) = frame.as_mut() {
                open.pixels += 1;
            }

            if marker("eol") {
                let done = line.take().expect("a line was open");
                *line_lengths.entry(done.pixels).or_default() += 1;
                lines.push((done.start, time, done.pixels));
                if let Some(open) = frame.as_mut() {
                    open.lines += 1;
                }
            }

            if marker("eof") {
                // A frame may end on the same beat as its last line.
                if let Some(done) = line.take() {
                    *line_lengths.entry(done.pixels).or_default() += 1;
                    lines.push((done.start, time, done.pixels));
                    if let Some(open) = frame.as_mut() {
                        open.lines += 1;
                    }
                }
                match frame.take() {
                    Some(open) => {
                        report.annotations.push(
                            Annotation::new(
                                open.start,
                                time,
                                0,
                                "frame",
                                format!("frame #{} — {} lines", open.index, open.lines),
                            )
                            .with("lines", open.lines)
                            .with("pixels", open.pixels),
                        );
                        report.transactions.push(Transaction {
                            t_start: open.start,
                            t_end: time,
                            kind: "frame".into(),
                            fields: vec![
                                ("index".into(), open.index.to_string()),
                                ("lines".into(), open.lines.to_string()),
                                ("pixels".into(), open.pixels.to_string()),
                            ],
                        });
                    }
                    None => report.problem(format!("a frame ended at {time} without starting")),
                }
            }
        }

        // The commonest line length is what the design meant; anything else is
        // worth pointing at rather than averaging away.
        let usual = line_lengths.iter().max_by_key(|(_, count)| **count).map(|(len, _)| *len);
        for (start, end, pixels) in &lines {
            let odd = usual.is_some_and(|usual| *pixels != usual);
            let label = format!("{pixels} px");
            let annotation = Annotation::new(*start, *end, 1, "line", label).with("pixels", pixels);
            report.annotations.push(if odd {
                annotation.at(Level::Error).with("expected", usual.unwrap_or_default())
            } else {
                annotation
            });
        }

        if let Some(open) = frame {
            report.problem(format!(
                "the dump ends inside frame #{}, which started at {}",
                open.index, open.start
            ));
        }
        if line.is_some() {
            report.problem("the dump ends inside a line that never reached its `eol`");
        }
        if markers_without_a_beat > 0 {
            report.problem(format!(
                "{markers_without_a_beat} marker(s) pulsed on a cycle where `valid` was low, \
                 and were not counted"
            ));
        }
        if undriven > 0 {
            report.problem(format!("`valid` was undriven on {undriven} clock edge(s)"));
        }
        if !bindings.has("eol") {
            report.problem("no `eol` was bound, so lines could not be counted");
        }
        if !bindings.has("eof") {
            report.problem("no `eof` was bound, so frames could not be counted");
        }

        report.stat("beats", beats);
        report.stat("frames", frames_done);
        report.stat("lines", lines.len());
        if let Some(usual) = usual {
            report.stat("pixels per line", usual);
            let odd = lines.iter().filter(|(_, _, px)| *px != usual).count();
            if odd > 0 {
                report.stat("lines of another length", odd);
            }
        }
        if errors > 0 {
            report.stat("err beats", errors);
        }
        report
    }
}
