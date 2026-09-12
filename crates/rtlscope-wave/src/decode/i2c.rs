//! I²C, and the SCCB dialect a camera speaks.
//!
//! Two wires, and everything is in when they move relative to each other:
//!
//! ```text
//! START     SDA falls while SCL is high
//! bit       SDA is read on SCL's rising edge
//! ACK       the ninth bit: low is an acknowledgement, high is not
//! STOP      SDA rises while SCL is high
//! ```
//!
//! Which makes this decoder event-driven rather than clock-sampled, and that
//! settles two things that would otherwise need care. The bus clock's duty
//! cycle does not matter — the design measured here holds SCL low for two
//! thirds of every bit, and nothing below depends on it. And there is no
//! sample clock to be in phase with.
//!
//! Two details come from the design rather than from the specification:
//!
//! - **The lines are usually recorded inverted.** An open-drain bus is modelled
//!   as a `drive_low` enable, which is the opposite of the wire. Bind it as
//!   `scl=!scl_drive_low` and everything below is right; bind it uninverted and
//!   every START reads as a STOP.
//! - **A STOP may release both wires on one edge.** The design does, and a
//!   decoder that insisted on seeing SCL rise strictly before SDA would miss
//!   every one. So when both change at the same instant, the clock is applied
//!   first — which turns that simultaneous release back into the sequence it
//!   means.

use super::{
    Annotation, ChannelSpec, DecodeReport, Decoder, Level, ResolvedBindings, Transaction, edges,
};
use crate::dump::Dump;

pub struct I2c;

const CHANNELS: &[ChannelSpec] = &[
    ChannelSpec {
        role: "scl",
        required: true,
        doc: "the clock line; bind as `!..._drive_low` for an open-drain model",
    },
    ChannelSpec {
        role: "sda",
        required: true,
        doc: "the data line; bind as `!..._drive_low` for an open-drain model",
    },
];

/// A byte as it went past, with what the receiver said about it.
struct Byte {
    end: u64,
    value: u8,
    acked: bool,
}

/// What is being assembled between a START and a STOP.
struct Transfer {
    start: u64,
    bytes: Vec<Byte>,
    /// A repeated START came in the middle, which is how a read is addressed.
    restarted: bool,
}

impl Decoder for I2c {
    fn protocol(&self) -> &'static str {
        "i2c"
    }

    fn doc(&self) -> &'static str {
        "I²C, and the SCCB dialect a camera speaks: START, bytes with their \
         acknowledgements, repeated START and STOP. Event-driven, so the bus \
         clock's duty cycle does not matter. Bind an open-drain model inverted, \
         as `scl=!scl_drive_low`."
    }

    fn channels(&self) -> &'static [ChannelSpec] {
        CHANNELS
    }

    fn decode(&self, dump: &Dump, bindings: &ResolvedBindings) -> DecodeReport {
        let mut report = DecodeReport::new(self.protocol());
        report.bindings = bindings.listing(dump);

        // SCL first, so that a STOP releasing both wires at one instant reads
        // as the clock rising and then the data.
        let timeline = match edges(dump, bindings, "scl", "sda") {
            Ok(timeline) => timeline,
            Err(error) => {
                report.problem(error.to_string());
                return report;
            }
        };

        let mut transfer: Option<Transfer> = None;
        let mut bits: Vec<bool> = Vec::new();
        let mut byte_start = 0u64;
        let mut last_bit_at = 0u64;

        let mut was_scl: Option<bool> = None;
        let mut was_sda: Option<bool> = None;

        let mut starts = 0u64;
        let mut stops = 0u64;
        let mut nacks = 0u64;
        let mut undriven = 0u64;

        for (time, scl, sda) in timeline {
            let (Some(scl), Some(sda)) = (scl, sda) else {
                undriven += 1;
                was_scl = scl;
                was_sda = sda;
                continue;
            };
            let (prev_scl, prev_sda) = (was_scl, was_sda);
            was_scl = Some(scl);
            was_sda = Some(sda);

            let sda_fell = prev_sda == Some(true) && !sda;
            let sda_rose = prev_sda == Some(false) && sda;
            let scl_rose = prev_scl == Some(false) && scl;

            // ---- START, and the repeated START that addresses a read ----
            if scl && sda_fell {
                starts += 1;
                match transfer.take() {
                    Some(mut open) => {
                        // A START inside a transfer is a repeated one.
                        open.restarted = true;
                        report.annotations.push(Annotation::new(
                            time,
                            time,
                            1,
                            "restart",
                            "Sr".into(),
                        ));
                        transfer = Some(open);
                    }
                    None => {
                        report.annotations.push(Annotation::new(
                            time,
                            time,
                            1,
                            "start",
                            "S".into(),
                        ));
                        transfer =
                            Some(Transfer { start: time, bytes: Vec::new(), restarted: false });
                    }
                }
                bits.clear();
                continue;
            }

            // ---- STOP ----
            if scl && sda_rose {
                stops += 1;
                report.annotations.push(Annotation::new(time, time, 1, "stop", "P".into()));
                match transfer.take() {
                    Some(open) => finish(&mut report, open, time),
                    None => report.problem(format!("a STOP at {time} with no transfer open")),
                }
                bits.clear();
                continue;
            }

            // ---- a bit, sampled where the receiver samples it ----
            if scl_rose && transfer.is_some() {
                if bits.is_empty() {
                    byte_start = time;
                }
                bits.push(sda);
                last_bit_at = time;

                if bits.len() == 9 {
                    let value = bits[..8].iter().fold(0u8, |acc, bit| acc << 1 | u8::from(*bit));
                    // The ninth bit is the receiver pulling SDA low to say it
                    // heard: low means acknowledged.
                    let acked = !bits[8];
                    if !acked {
                        nacks += 1;
                    }
                    if let Some(open) = transfer.as_mut() {
                        open.bytes.push(Byte { end: time, value, acked });
                    }
                    report.annotations.push(
                        Annotation::new(
                            byte_start,
                            time,
                            2,
                            "byte",
                            format!("0x{value:02x} {}", if acked { "A" } else { "N" }),
                        )
                        .at(if acked { Level::Info } else { Level::Warning })
                        .with("value", format!("0x{value:02x}")),
                    );
                    bits.clear();
                }
            }
        }

        if let Some(open) = transfer {
            report.problem(format!(
                "the dump ends inside a transfer that started at {} and never stopped",
                open.start
            ));
            let end = open.bytes.last().map_or(open.start, |b| b.end);
            finish(&mut report, open, end);
        }
        if !bits.is_empty() {
            report.problem(format!(
                "{} bit(s) after the last complete byte, ending at {last_bit_at}",
                bits.len()
            ));
        }
        if undriven > 0 {
            report.problem(format!("a line was undriven at {undriven} change(s)"));
        }
        // Bound the wrong way up, an open-drain bus produces either no START at
        // all or a scatter of transfers with nothing in them. Both are worth one
        // hint rather than a page of nonsense.
        let empty = report.problems.iter().filter(|p| p.contains("carried no bytes")).count();
        if starts == 0 || empty > 1 {
            report.problem(
                "this does not read like a bus — an open-drain one is usually recorded as a                  `drive_low` enable, which is the opposite of the wire; try `scl=!<signal>`                  and `sda=!<signal>`",
            );
        }

        report.stat("transfers", report.transactions.len());
        report.stat("starts", starts);
        report.stat("stops", stops);
        report.stat("bytes", report.annotations.iter().filter(|a| a.kind == "byte").count());
        if nacks > 0 {
            report.stat("not acknowledged", nacks);
        }
        report
    }
}

/// Turns a completed transfer into a transaction, naming the SCCB shapes.
fn finish(report: &mut DecodeReport, transfer: Transfer, end: u64) {
    let Some(address) = transfer.bytes.first() else {
        report.problem(format!("a transfer at {} carried no bytes", transfer.start));
        return;
    };

    let device = address.value >> 1;
    let reading = address.value & 1 == 1;
    let payload: Vec<&Byte> = transfer.bytes[1..].iter().collect();

    // A camera register is at a 16-bit address, so the write that sets one is
    // three bytes and the read that follows it restarts. Anything else is
    // reported as plain I²C rather than forced into the shape.
    let register = |bytes: &[&Byte]| {
        format!("0x{:04x}", u16::from(bytes[0].value) << 8 | u16::from(bytes[1].value))
    };
    let (kind, mut fields) = match (transfer.restarted, reading, payload.len()) {
        // `S addr regHi regLo value P`
        (false, false, 3) => (
            "sccb-write",
            vec![
                ("register".to_string(), register(&payload)),
                ("value".to_string(), format!("0x{:02x}", payload[2].value)),
            ],
        ),
        // `S addr regHi regLo Sr addr|1 value P` — the read address arrives as
        // a payload byte, since a repeated START does not end the transfer.
        (true, _, 4) if payload[2].value & 1 == 1 => (
            "sccb-read",
            vec![
                ("register".to_string(), register(&payload)),
                ("value".to_string(), format!("0x{:02x}", payload[3].value)),
            ],
        ),
        _ => (
            if reading { "i2c-read" } else { "i2c-write" },
            vec![(
                "bytes".to_string(),
                payload.iter().map(|b| format!("0x{:02x}", b.value)).collect::<Vec<_>>().join(" "),
            )],
        ),
    };
    fields.insert(0, ("device".to_string(), format!("0x{device:02x}")));

    let nacked = transfer.bytes.iter().any(|b| !b.acked);
    let label = match kind {
        "sccb-write" => format!("write {} = {}", fields[1].1, fields[2].1),
        "sccb-read" => format!("read {} = {}", fields[1].1, fields[2].1),
        _ => format!("{kind} 0x{device:02x}"),
    };
    report.annotations.push(
        Annotation::new(transfer.start, end, 0, kind, label)
            .at(if nacked { Level::Warning } else { Level::Info })
            .with("device", format!("0x{device:02x}")),
    );
    report.transactions.push(Transaction {
        t_start: transfer.start,
        t_end: end,
        kind: kind.to_string(),
        fields,
    });
}
