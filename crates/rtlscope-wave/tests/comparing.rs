//! Two recordings, held against each other — through the reader, not around it.
//!
//! The merge itself is unit-tested beside the code. What is worth an
//! integration test is everything between two files and an answer: the paths
//! lining up, the values coming back out of wellen, and the two timebases
//! being reconciled rather than assumed equal.

use rtlscope_wave::Dump;

/// A recording, written out the way a simulator would.
fn vcd(timescale: &str, body: &str) -> Dump {
    let text = format!(
        "$timescale {timescale} $end\n\
         $scope module tb $end\n\
         $var wire 1 ! clk $end\n\
         $var wire 8 \" data $end\n\
         $upscope $end\n\
         $enddefinitions $end\n\
         {body}"
    );
    Dump::open_vcd_bytes(text.into_bytes()).expect("this is a VCD")
}

#[test]
fn two_recordings_of_the_same_run_agree() {
    let body = "#0\n0!\nb0 \"\n#10\n1!\nb101 \"\n#20\n0!\n";
    let (mut a, mut b) = (vcd("1ns", body), vcd("1ns", body));

    let report = rtlscope_wave::compare(&mut a, &mut b);

    assert_eq!(report.shared, 2, "clk and data");
    assert!(report.agrees(), "{:?}", report.differing);
    assert!(report.only_in_a.is_empty() && report.only_in_b.is_empty());
    assert!(report.problems.is_empty(), "{:?}", report.problems);
}

/// The answer is the first moment and the two values, because that is what a
/// reader takes back to the waveform.
#[test]
fn the_signal_that_changed_is_named_with_the_moment_it_did() {
    let mut a = vcd("1ns", "#0\n0!\nb0 \"\n#10\n1!\nb101 \"\n#20\n0!\n");
    // Same clock, but `data` takes a different value at 10.
    let mut b = vcd("1ns", "#0\n0!\nb0 \"\n#10\n1!\nb110 \"\n#20\n0!\n");

    let report = rtlscope_wave::compare(&mut a, &mut b);

    assert_eq!(report.differing.len(), 1, "{:?}", report.differing);
    let first = report.first().expect("they part");
    assert_eq!(first.path, "tb.data");
    assert_eq!(first.at, 10);
    assert_eq!((first.a.as_str(), first.b.as_str()), ("0x5", "0x6"));
    assert!(!report.agrees());
}

/// A signal one of them did not record is not a difference in behaviour, and a
/// report that read it as one would send the reader after nothing.
#[test]
fn a_signal_only_one_recording_has_is_listed_rather_than_compared() {
    let mut a = vcd("1ns", "#0\n0!\nb0 \"\n");
    let text = "$timescale 1ns $end\n\
                $scope module tb $end\n\
                $var wire 1 ! clk $end\n\
                $upscope $end\n\
                $enddefinitions $end\n\
                #0\n0!\n";
    let mut b = Dump::open_vcd_bytes(text.as_bytes().to_vec()).expect("a VCD");

    let report = rtlscope_wave::compare(&mut a, &mut b);

    assert_eq!(report.shared, 1, "only clk is in both");
    assert_eq!(report.only_in_a, ["tb.data"]);
    assert!(report.only_in_b.is_empty());
    assert!(report.agrees(), "the shared one is the same: {:?}", report.differing);
}

/// Two simulators counting in different units are still recording the same
/// moments. Comparing tick against tick would call every signal different.
#[test]
fn two_timebases_are_reconciled_and_the_answer_is_in_the_first_ones_ticks() {
    // 10 ns in, `data` becomes 5. Both say so; they count differently.
    let mut a = vcd("1ns", "#0\n0!\nb0 \"\n#10\n1!\nb101 \"\n");
    let mut b = vcd("1ps", "#0\n0!\nb0 \"\n#10000\n1!\nb101 \"\n");

    let report = rtlscope_wave::compare(&mut a, &mut b);
    assert!(report.agrees(), "same run, different units: {:?}", report.differing);

    // And when they really do differ, the moment is reported in A's ticks.
    let mut b = vcd("1ps", "#0\n0!\nb0 \"\n#10000\n1!\nb110 \"\n");
    let report = rtlscope_wave::compare(&mut a, &mut b);
    let first = report.first().expect("they part");
    assert_eq!(first.at, 10, "ticks of the first dump, which is 1ns");
}
