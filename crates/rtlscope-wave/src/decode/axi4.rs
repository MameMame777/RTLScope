//! AXI4 — the memory-mapped kind — folded into transactions.
//!
//! Five channels carry one of two things. A write is one AW, then AWLEN+1
//! beats of W with WLAST on the last, then one B. A read is one AR, then
//! ARLEN+1 beats of R with RLAST on the last, each beat carrying its own
//! RRESP. The dump has the handshakes, one signal at a time; what a reader
//! wants is the sentence — *write 0x10, four beats, OKAY* — and the moments
//! where a rule was broken.
//!
//! ```text
//! AW  awvalid awready  awaddr awlen awsize awburst
//! W   wvalid  wready   wdata wstrb wlast
//! B   bvalid  bready   bresp
//! AR  arvalid arready  araddr arlen arsize arburst
//! R   rvalid  rready   rdata rresp rlast
//! ```
//!
//! No IDs. One master against one slave, which is the common case and the
//! teachable one, fixes the order: responses come back in the order the
//! addresses went out, and W bursts follow AW in order — though a burst's data
//! may start before its address is accepted, which AXI4 allows and this keeps
//! straight with a queue per direction. A bus that uses AWID/ARID to
//! interleave transactions is a different, larger job, and is not modelled.
//!
//! Two rules are checked, because they are the ones a simulator will not flag
//! and a slave may silently tolerate: once VALID is high it stays high until
//! the transfer, and the payload beside it does not change while it waits.
//! Both are reported where they happened, on their own lane.

use std::collections::VecDeque;

use super::{
    Annotation, ChannelSpec, DecodeReport, Decoder, Level, ResolvedBindings, Sample, Sampler,
    Transaction,
};
use crate::dump::{Dump, WaveValue};

pub struct Axi4;

const CHANNELS: &[ChannelSpec] = &[
    ChannelSpec { role: "clock", required: true, doc: "ACLK, which every channel is sampled on" },
    ChannelSpec { role: "awvalid", required: false, doc: "write address offered" },
    ChannelSpec { role: "awready", required: false, doc: "write address taken" },
    ChannelSpec { role: "awaddr", required: false, doc: "where the write goes" },
    ChannelSpec { role: "awlen", required: false, doc: "beats in the write, minus one" },
    ChannelSpec { role: "awsize", required: false, doc: "log2 of the bytes per beat" },
    ChannelSpec { role: "awburst", required: false, doc: "FIXED, INCR or WRAP" },
    ChannelSpec { role: "wvalid", required: false, doc: "write data offered" },
    ChannelSpec { role: "wready", required: false, doc: "write data taken" },
    ChannelSpec { role: "wdata", required: false, doc: "the beat" },
    ChannelSpec { role: "wstrb", required: false, doc: "which bytes of the beat count" },
    ChannelSpec { role: "wlast", required: false, doc: "the last beat of the burst" },
    ChannelSpec { role: "bvalid", required: false, doc: "write response offered" },
    ChannelSpec { role: "bready", required: false, doc: "write response taken" },
    ChannelSpec { role: "bresp", required: false, doc: "how the write went" },
    ChannelSpec { role: "arvalid", required: false, doc: "read address offered" },
    ChannelSpec { role: "arready", required: false, doc: "read address taken" },
    ChannelSpec { role: "araddr", required: false, doc: "where the read comes from" },
    ChannelSpec { role: "arlen", required: false, doc: "beats in the read, minus one" },
    ChannelSpec { role: "arsize", required: false, doc: "log2 of the bytes per beat" },
    ChannelSpec { role: "arburst", required: false, doc: "FIXED, INCR or WRAP" },
    ChannelSpec { role: "rvalid", required: false, doc: "read data offered" },
    ChannelSpec { role: "rready", required: false, doc: "read data taken" },
    ChannelSpec { role: "rdata", required: false, doc: "the beat" },
    ChannelSpec { role: "rresp", required: false, doc: "how this beat went" },
    ChannelSpec { role: "rlast", required: false, doc: "the last beat of the burst" },
];

/// One handshake channel: its VALID/READY pair, and the payload beside them
/// that has to hold while VALID waits.
struct Channel {
    name: &'static str,
    valid: &'static str,
    ready: &'static str,
    payload: &'static [&'static str],
}

const AW: Channel = Channel {
    name: "AW",
    valid: "awvalid",
    ready: "awready",
    payload: &["awaddr", "awlen", "awsize", "awburst"],
};
const W: Channel =
    Channel { name: "W", valid: "wvalid", ready: "wready", payload: &["wdata", "wstrb", "wlast"] };
const B: Channel = Channel { name: "B", valid: "bvalid", ready: "bready", payload: &["bresp"] };
const AR: Channel = Channel {
    name: "AR",
    valid: "arvalid",
    ready: "arready",
    payload: &["araddr", "arlen", "arsize", "arburst"],
};
const R: Channel =
    Channel { name: "R", valid: "rvalid", ready: "rready", payload: &["rdata", "rresp", "rlast"] };

/// What a channel did at one clock edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handshake {
    Idle,
    /// VALID up, READY down: the source is being made to wait.
    Waiting,
    Transfer,
    /// VALID or READY undriven, so the edge cannot be read either way.
    Undriven,
}

/// Watches one channel across edges, for the two rules a dump can break and
/// for how long the other side was kept waiting.
#[derive(Default)]
struct Watch {
    waiting: bool,
    held: Vec<Option<WaveValue>>,
    waits: u64,
    run: u64,
    longest: u64,
    undriven: u64,
}

impl Watch {
    fn look(
        &mut self,
        channel: &Channel,
        sample: &Sample<'_>,
        report: &mut DecodeReport,
        violations: &mut u64,
    ) -> Handshake {
        let time = sample.time;
        let (Some(valid), Some(ready)) = (sample.high(channel.valid), sample.high(channel.ready))
        else {
            self.undriven += 1;
            self.waiting = false;
            return Handshake::Undriven;
        };
        let payload: Vec<Option<WaveValue>> =
            channel.payload.iter().map(|role| sample.value(role).cloned()).collect();

        if self.waiting {
            if !valid {
                *violations += 1;
                let text = format!(
                    "`{}` dropped at {time} before `{}` came: VALID holds until the transfer",
                    channel.valid, channel.ready
                );
                report.annotations.push(
                    Annotation::new(
                        time,
                        time,
                        2,
                        "violation",
                        format!("{} dropped", channel.valid),
                    )
                    .at(Level::Error)
                    .with("rule", "VALID, once high, stays high until READY"),
                );
                report.problem(text);
            } else if payload != self.held {
                let changed: Vec<&str> = channel
                    .payload
                    .iter()
                    .zip(payload.iter().zip(self.held.iter()))
                    .filter(|(_, (now, then))| now != then)
                    .map(|(role, _)| *role)
                    .collect();
                *violations += 1;
                let text = format!(
                    "`{}` changed at {time} while `{}` was waiting: the payload holds with VALID",
                    changed.join("`, `"),
                    channel.valid
                );
                report.annotations.push(
                    Annotation::new(
                        time,
                        time,
                        2,
                        "violation",
                        format!("{} changed", changed.join(", ")),
                    )
                    .at(Level::Error)
                    .with("rule", "the payload holds while VALID waits"),
                );
                report.problem(text);
            }
        }

        let outcome = match (valid, ready) {
            (false, _) => Handshake::Idle,
            (true, false) => Handshake::Waiting,
            (true, true) => Handshake::Transfer,
        };
        if outcome == Handshake::Waiting {
            self.waits += 1;
            self.run += 1;
            self.longest = self.longest.max(self.run);
        } else {
            self.run = 0;
        }
        self.waiting = outcome == Handshake::Waiting;
        self.held = payload;
        outcome
    }
}

/// An address accepted, waiting for its data or its response.
struct Pending {
    time: u64,
    /// The address as text, as wide as the bus, ready for a label.
    addr_text: String,
    /// Beats promised: AxLEN + 1.
    beats: Option<u64>,
    size: Option<u64>,
    burst: Option<u64>,
}

impl Pending {
    fn of(sample: &Sample<'_>, addr: &str, len: &str, size: &str, burst: &str) -> Self {
        Pending {
            time: sample.time,
            addr_text: hex(sample.value(addr)),
            beats: sample.number(len).map(|len| len + 1),
            size: sample.number(size),
            burst: sample.number(burst),
        }
    }
}

/// Data beats accumulating until the last one.
struct Burst {
    first: u64,
    last: u64,
    beats: u64,
    /// The worst response seen on the beats, for a read.
    worst: Option<u64>,
    /// Beats with some byte lanes switched off, for a write.
    partial: u64,
}

impl Burst {
    fn new(time: u64) -> Self {
        Burst { first: time, last: time, beats: 0, worst: None, partial: 0 }
    }
}

fn resp_name(resp: Option<u64>) -> &'static str {
    match resp {
        Some(0) => "OKAY",
        Some(1) => "EXOKAY",
        Some(2) => "SLVERR",
        Some(3) => "DECERR",
        Some(_) => "?",
        None => "unbound",
    }
}

fn resp_is_error(resp: Option<u64>) -> bool {
    matches!(resp, Some(2 | 3))
}

fn burst_name(burst: Option<u64>) -> Option<&'static str> {
    match burst? {
        0 => Some("FIXED"),
        1 => Some("INCR"),
        2 => Some("WRAP"),
        _ => Some("?"),
    }
}

/// A value as hex, as wide as the signal, so `0x10` on a 32-bit address reads
/// as `0x00000010` and beats line up under each other.
fn hex(value: Option<&WaveValue>) -> String {
    match value {
        Some(WaveValue::Bits { value, width }) => {
            format!("0x{value:0width$x}", width = (*width as usize).div_ceil(4))
        }
        Some(WaveValue::Wide { bits, .. }) => {
            let mut out = String::from("0x");
            let padded = format!("{}{bits}", "0".repeat((4 - bits.len() % 4) % 4));
            for nibble in padded.as_bytes().chunks(4) {
                let digit = nibble.iter().fold(0u8, |acc, b| acc << 1 | (b == &b'1') as u8);
                out.push(char::from_digit(u32::from(digit), 16).unwrap_or('?'));
            }
            out
        }
        Some(WaveValue::Unknown { .. }) => "x".to_string(),
        None => "?".to_string(),
    }
}

impl Decoder for Axi4 {
    fn protocol(&self) -> &'static str {
        "axi4"
    }

    fn doc(&self) -> &'static str {
        "AXI4, the memory-mapped kind, without IDs: AW+W+B folded into one write and \
         AR+R into one read, each with its address, beat count and response. Reports a \
         VALID that dropped before READY, or a payload that changed while it waited, \
         and how long each channel was kept waiting."
    }

    fn channels(&self) -> &'static [ChannelSpec] {
        CHANNELS
    }

    fn decode(&self, dump: &Dump, bindings: &ResolvedBindings) -> DecodeReport {
        let mut report = DecodeReport::new(self.protocol());
        report.bindings = bindings.listing(dump);

        let pair = |channel: &Channel| bindings.has(channel.valid) && bindings.has(channel.ready);
        let writes = pair(&AW) && pair(&W) && pair(&B);
        let reads = pair(&AR) && pair(&R);
        if !writes && !reads {
            report.problem(
                "no channel pair is bound: writes need awvalid/awready, wvalid/wready and \
                 bvalid/bready; reads need arvalid/arready and rvalid/rready",
            );
            return report;
        }

        let roles: Vec<&str> = CHANNELS.iter().map(|c| c.role).filter(|r| *r != "clock").collect();
        let mut sampler = match Sampler::new(dump, bindings, "clock", &roles) {
            Ok(sampler) => sampler,
            Err(error) => {
                report.problem(error.to_string());
                return report;
            }
        };
        report.stat("clock edges", sampler.edges());

        let (mut aw, mut w, mut b, mut ar, mut r) = (
            Watch::default(),
            Watch::default(),
            Watch::default(),
            Watch::default(),
            Watch::default(),
        );
        let mut violations = 0u64;

        let mut aw_queue: VecDeque<Pending> = VecDeque::new();
        let mut w_open: Option<Burst> = None;
        let mut w_done: VecDeque<Burst> = VecDeque::new();
        let mut ar_queue: VecDeque<Pending> = VecDeque::new();
        let mut r_open: Option<Burst> = None;

        let mut written = 0u64;
        let mut read = 0u64;
        let mut responses = [0u64; 4];
        let mut transactions: Vec<Transaction> = Vec::new();
        // The clock period, for drawing a beat as one cycle wide rather than
        // as an instant; learnt from the first two edges.
        let mut previous: Option<u64> = None;
        let mut period = 0u64;
        let mut beat_marks: Vec<(u64, &'static str, String, bool)> = Vec::new();

        while let Some(sample) = sampler.advance() {
            let time = sample.time;
            if let Some(before) = previous
                && period == 0
            {
                period = time.saturating_sub(before).max(1);
            }
            previous = Some(time);

            if writes {
                if aw.look(&AW, &sample, &mut report, &mut violations) == Handshake::Transfer {
                    aw_queue
                        .push_back(Pending::of(&sample, "awaddr", "awlen", "awsize", "awburst"));
                }
                if w.look(&W, &sample, &mut report, &mut violations) == Handshake::Transfer {
                    let burst = w_open.get_or_insert_with(|| Burst::new(time));
                    burst.beats += 1;
                    burst.last = time;
                    written += 1;
                    if let Some(strobe) = sample.value("wstrb")
                        && let Some(bits) = strobe.as_u64()
                    {
                        let all = if strobe.width() >= 64 {
                            u64::MAX
                        } else {
                            (1u64 << strobe.width()) - 1
                        };
                        if bits != all {
                            burst.partial += 1;
                        }
                    }
                    beat_marks.push((time, "wbeat", hex(sample.value("wdata")), false));
                    // Without `wlast` the burst is cut where the address said
                    // it would end; with it, where the master said.
                    let last = sample.high("wlast").unwrap_or_else(|| {
                        aw_queue.front().and_then(|aw| aw.beats).is_some_and(|n| burst.beats >= n)
                    });
                    if last {
                        w_done.push_back(w_open.take().expect("a burst was open"));
                    }
                }
                if b.look(&B, &sample, &mut report, &mut violations) == Handshake::Transfer {
                    let resp = sample.number("bresp");
                    if let Some(code) = resp
                        && code < 4
                    {
                        responses[code as usize] += 1;
                    }
                    let address = aw_queue.pop_front();
                    let data = w_done.pop_front();
                    match (&address, &data) {
                        (None, None) => {
                            report.problem(format!(
                                "a write response at {time} with no write outstanding"
                            ));
                            continue;
                        }
                        (None, Some(_)) => report.problem(format!(
                            "a write response at {time} for data whose address was never seen"
                        )),
                        (Some(_), None) => report.problem(format!(
                            "a write response at {time} before its data finished: no burst had \
                             reached `wlast`"
                        )),
                        (Some(_), Some(_)) => {}
                    }
                    let start = match (&address, &data) {
                        (Some(a), Some(d)) => a.time.min(d.first),
                        (Some(a), None) => a.time,
                        (None, Some(d)) => d.first,
                        (None, None) => time,
                    };
                    let beats = data.as_ref().map_or(0, |d| d.beats);
                    let addr_text =
                        address.as_ref().map_or_else(|| "?".to_string(), |a| a.addr_text.clone());
                    let mut fields = vec![
                        ("addr".to_string(), addr_text.clone()),
                        ("beats".to_string(), beats.to_string()),
                        ("resp".to_string(), resp_name(resp).to_string()),
                    ];
                    let mut level = if resp_is_error(resp) { Level::Error } else { Level::Info };
                    if let Some(a) = &address {
                        if let Some(promised) = a.beats {
                            fields.push(("len".to_string(), promised.to_string()));
                            if data.is_some() && promised != beats {
                                report.problem(format!(
                                    "write to {addr_text} at {}: AWLEN promised {promised} beat(s), \
                                     {beats} arrived",
                                    a.time
                                ));
                                if level == Level::Info {
                                    level = Level::Warning;
                                }
                            }
                        }
                        if let Some(size) = a.size {
                            fields.push(("bytes/beat".to_string(), (1u64 << size).to_string()));
                        }
                        if let Some(name) = burst_name(a.burst) {
                            fields.push(("burst".to_string(), name.to_string()));
                        }
                    }
                    if let Some(d) = &data
                        && d.partial > 0
                    {
                        fields.push(("partial beats".to_string(), d.partial.to_string()));
                    }
                    let label = format!("write {addr_text} ×{beats} {}", resp_name(resp));
                    let mut annotation = Annotation::new(start, time, 0, "write", label).at(level);
                    for (name, value) in &fields {
                        annotation = annotation.with(name, value);
                    }
                    report.annotations.push(annotation);
                    transactions.push(Transaction {
                        t_start: start,
                        t_end: time,
                        kind: "write".into(),
                        fields,
                    });
                }
            }

            if reads {
                if ar.look(&AR, &sample, &mut report, &mut violations) == Handshake::Transfer {
                    ar_queue
                        .push_back(Pending::of(&sample, "araddr", "arlen", "arsize", "arburst"));
                }
                if r.look(&R, &sample, &mut report, &mut violations) == Handshake::Transfer {
                    let burst = r_open.get_or_insert_with(|| Burst::new(time));
                    burst.beats += 1;
                    burst.last = time;
                    read += 1;
                    let resp = sample.number("rresp");
                    if let Some(code) = resp
                        && code < 4
                    {
                        responses[code as usize] += 1;
                    }
                    burst.worst = match (burst.worst, resp) {
                        (Some(a), Some(b)) => Some(a.max(b)),
                        (a, b) => a.or(b),
                    };
                    // A refused beat says so on its label: `0xdeadbeef` alone
                    // reads as data, and it is a refusal.
                    let label = match resp_is_error(resp) {
                        true => format!("{} {}", hex(sample.value("rdata")), resp_name(resp)),
                        false => hex(sample.value("rdata")),
                    };
                    beat_marks.push((time, "rbeat", label, resp_is_error(resp)));
                    let last = sample.high("rlast").unwrap_or_else(|| {
                        ar_queue.front().and_then(|ar| ar.beats).is_some_and(|n| burst.beats >= n)
                    });
                    if last {
                        let done = r_open.take().expect("a burst was open");
                        let address = ar_queue.pop_front();
                        if address.is_none() {
                            report.problem(format!(
                                "read data ending at {time} with no read outstanding"
                            ));
                        }
                        let start = address.as_ref().map_or(done.first, |a| a.time.min(done.first));
                        let addr_text = address
                            .as_ref()
                            .map_or_else(|| "?".to_string(), |a| a.addr_text.clone());
                        let mut fields = vec![
                            ("addr".to_string(), addr_text.clone()),
                            ("beats".to_string(), done.beats.to_string()),
                            ("resp".to_string(), resp_name(done.worst).to_string()),
                        ];
                        let mut level =
                            if resp_is_error(done.worst) { Level::Error } else { Level::Info };
                        if let Some(a) = &address {
                            if let Some(promised) = a.beats {
                                fields.push(("len".to_string(), promised.to_string()));
                                if promised != done.beats {
                                    report.problem(format!(
                                        "read from {addr_text} at {}: ARLEN promised {promised} \
                                         beat(s), {} arrived",
                                        a.time, done.beats
                                    ));
                                    if level == Level::Info {
                                        level = Level::Warning;
                                    }
                                }
                            }
                            if let Some(size) = a.size {
                                fields.push(("bytes/beat".to_string(), (1u64 << size).to_string()));
                            }
                            if let Some(name) = burst_name(a.burst) {
                                fields.push(("burst".to_string(), name.to_string()));
                            }
                        }
                        let label =
                            format!("read {addr_text} ×{} {}", done.beats, resp_name(done.worst));
                        let mut annotation =
                            Annotation::new(start, time, 0, "read", label).at(level);
                        for (name, value) in &fields {
                            annotation = annotation.with(name, value);
                        }
                        report.annotations.push(annotation);
                        transactions.push(Transaction {
                            t_start: start,
                            t_end: time,
                            kind: "read".into(),
                            fields,
                        });
                    }
                }
            }
        }

        // The beats, one cycle wide each, on the lane under the transactions.
        let period = period.max(1);
        for (time, kind, label, error) in beat_marks {
            let annotation = Annotation::new(time, time + period, 1, kind, label);
            report.annotations.push(if error { annotation.at(Level::Error) } else { annotation });
        }

        // What the dump ended in the middle of.
        for pending in &aw_queue {
            report.problem(format!(
                "a write to {} accepted at {} never got its response",
                pending.addr_text, pending.time
            ));
        }
        if w_open.is_some() {
            report.problem("the dump ends inside a write burst that never reached `wlast`");
        }
        for burst in &w_done {
            if aw_queue.is_empty() {
                report.problem(format!(
                    "a write burst finished at {} never got its response",
                    burst.last
                ));
            }
        }
        for pending in &ar_queue {
            report.problem(format!(
                "a read from {} accepted at {} never returned",
                pending.addr_text, pending.time
            ));
        }
        if r_open.is_some() {
            report.problem("the dump ends inside a read burst that never reached `rlast`");
        }

        // Halves and payloads that were not bound, so the reader knows why a
        // column is blank rather than wondering.
        if !writes {
            report.problem("the write channels were not all bound, so writes were not decoded");
        }
        if !reads {
            report.problem("the read channels were not all bound, so reads were not decoded");
        }
        let unbound: &[(&str, bool, &str)] = &[
            ("awaddr", writes, "writes have no address"),
            ("wdata", writes, "write beats show no data"),
            ("wlast", writes, "write bursts were cut by AWLEN instead"),
            ("bresp", writes, "writes have no response"),
            ("araddr", reads, "reads have no address"),
            ("rdata", reads, "read beats show no data"),
            ("rlast", reads, "read bursts were cut by ARLEN instead"),
            ("rresp", reads, "reads have no response"),
        ];
        for (role, wanted, consequence) in unbound {
            if *wanted && !bindings.has(role) {
                report.problem(format!("no `{role}` was bound, so {consequence}"));
            }
        }
        for (channel, watch) in [(&AW, &aw), (&W, &w), (&B, &b), (&AR, &ar), (&R, &r)]
            .into_iter()
            .filter(|(c, _)| pair(c))
        {
            if watch.undriven > 0 {
                report.problem(format!(
                    "`{}` or `{}` was undriven on {} clock edge(s)",
                    channel.valid, channel.ready, watch.undriven
                ));
            }
        }

        let writes_done = transactions.iter().filter(|t| t.kind == "write").count();
        let reads_done = transactions.iter().filter(|t| t.kind == "read").count();
        report.stat("writes", writes_done);
        report.stat("reads", reads_done);
        report.stat("beats written", written);
        report.stat("beats read", read);
        for (code, count) in responses.iter().enumerate() {
            if *count > 0 {
                report.stat(&format!("{} responses", resp_name(Some(code as u64))), count);
            }
        }
        for (channel, watch) in [(&AW, &aw), (&W, &w), (&B, &b), (&AR, &ar), (&R, &r)]
            .into_iter()
            .filter(|(c, _)| pair(c))
        {
            if watch.waits > 0 {
                report.stat(
                    &format!("{} waited", channel.name),
                    format!("{} cycle(s), longest {}", watch.waits, watch.longest),
                );
            }
        }
        if violations > 0 {
            report.stat("handshake violations", violations);
        }
        report.transactions = transactions;
        report
    }
}
