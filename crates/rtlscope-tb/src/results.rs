//! Reading back what happened when the testbench ran.
//!
//! cocotb writes a JUnit XML file when a regression finishes, and it is the one
//! thing that closes the loop: the harness was generated here, the waveform was
//! produced by it, and this says which test failed — so the waveform can be
//! opened *at the moment it went wrong* rather than at the beginning.
//!
//! One thing about the file has to be stated rather than assumed. **It records
//! how long each test ran, not when.** The attribute is `sim_time_ns`, and
//! cocotb computes it as the simulation time at the end of a test minus the
//! time at its start. There is no absolute moment anywhere in the file. So the
//! moments here are those durations added up in the order the file lists them,
//! which is exact when the tests ran back to back in one simulation — which is
//! how cocotb runs them — and which every report says out loud rather than
//! presenting an accumulated number as a recorded one.
//!
//! The parser reads the shape cocotb writes rather than XML in general: no
//! namespaces, no comments, no CDATA, no nesting past `<testcase>`. Anything it
//! cannot account for goes into [`Run::problems`] instead of being dropped.

use std::path::Path;

use serde::Serialize;

use crate::cocotb::TbError;

/// Why the moments in a [`TestOutcome`] are what they are.
pub const BASIS: &str = "The file records how long each test ran, not when it started, so the \
     moments above are those durations added up in the order the file lists them. That holds \
     when the tests ran back to back in one simulation, which is how cocotb runs them.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Passed,
    Failed,
    Skipped,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Passed => "passed",
            Outcome::Failed => "FAILED",
            Outcome::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TestOutcome {
    pub name: String,
    /// The Python module the test came from — cocotb's `classname`.
    pub module: String,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// Wall-clock seconds.
    pub wall_s: f64,
    /// How long it ran, in nanoseconds of simulation — what the file records.
    pub sim_ns: f64,
    /// When it started and ended, accumulated. See [`BASIS`].
    pub start_ns: f64,
    pub end_ns: f64,
}

impl TestOutcome {
    pub fn failed(&self) -> bool {
        self.outcome == Outcome::Failed
    }

    /// `module.name`, which is what cocotb's own log calls a test.
    pub fn full_name(&self) -> String {
        if self.module.is_empty() {
            self.name.clone()
        } else {
            format!("{}.{}", self.module, self.name)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Run {
    pub tests: Vec<TestOutcome>,
    /// Everything the file said that could not be made sense of.
    pub problems: Vec<String>,
}

impl Run {
    /// Passed, failed, skipped.
    pub fn counts(&self) -> (usize, usize, usize) {
        let count = |want: Outcome| self.tests.iter().filter(|t| t.outcome == want).count();
        (count(Outcome::Passed), count(Outcome::Failed), count(Outcome::Skipped))
    }

    pub fn failures(&self) -> impl Iterator<Item = &TestOutcome> {
        self.tests.iter().filter(|test| test.failed())
    }

    /// How long the whole regression ran, in nanoseconds of simulation.
    pub fn sim_ns(&self) -> f64 {
        self.tests.last().map_or(0.0, |test| test.end_ns)
    }
}

/// Reads a `results.xml`.
pub fn read(path: &Path) -> Result<Run, TbError> {
    let text = std::fs::read_to_string(path)
        .map_err(|source| TbError::Io { path: path.display().to_string(), source })?;
    let run = parse(&text);
    if run.tests.is_empty() {
        return Err(TbError::NoTests { path: path.display().to_string() });
    }
    Ok(run)
}

/// Looks for a results file beside a dump.
///
/// cocotb writes the waveform into its build directory and the results into the
/// test directory above it, so both are worth trying — as is the directory
/// above that, for a layout that keeps them apart.
pub fn beside(dump: &Path) -> Option<std::path::PathBuf> {
    let mut here = dump.parent()?;
    for _ in 0..3 {
        let candidate = here.join("results.xml");
        if candidate.is_file() {
            return Some(candidate);
        }
        here = here.parent()?;
    }
    None
}

pub fn parse(xml: &str) -> Run {
    let mut tests: Vec<TestOutcome> = Vec::new();
    let mut problems = Vec::new();
    let mut rest = xml;
    let mut running = 0.0f64;

    while let Some(at) = rest.find("<testcase") {
        rest = &rest[at + "<testcase".len()..];
        let Some(close) = rest.find('>') else {
            problems.push("a <testcase> was never closed; the file is cut short".to_string());
            break;
        };
        let head = rest[..close].trim_end();
        let self_closing = head.ends_with('/');
        let attrs = attributes(head.trim_end_matches('/'));

        // Whatever sits between this tag and its end tag: the failure or the
        // skip, when there is one.
        let body = &rest[close + 1..];
        let inner = if self_closing {
            rest = body;
            ""
        } else {
            match body.find("</testcase>") {
                Some(end) => {
                    rest = &body[end..];
                    &body[..end]
                }
                None => {
                    problems.push(format!(
                        "`{}` has no </testcase>; the file is cut short",
                        attribute(&attrs, "name").unwrap_or_default()
                    ));
                    rest = "";
                    body
                }
            }
        };

        let name = attribute(&attrs, "name").unwrap_or_else(|| "(unnamed)".to_string());
        let (outcome, message) = judge(inner);

        let sim_ns = match attribute(&attrs, "sim_time_ns") {
            Some(text) => match text.parse::<f64>() {
                Ok(value) if value.is_finite() && value >= 0.0 => value,
                _ => {
                    problems.push(format!(
                        "`{name}` says it ran for `{text}` ns, which is not a length of time; \
                         it is counted as zero and everything after it is that much early"
                    ));
                    0.0
                }
            },
            None => {
                problems.push(format!(
                    "`{name}` has no sim_time_ns, so the moments after it are that much early"
                ));
                0.0
            }
        };

        let start_ns = running;
        running += sim_ns;

        tests.push(TestOutcome {
            name,
            module: attribute(&attrs, "classname").unwrap_or_default(),
            outcome,
            message,
            file: attribute(&attrs, "file"),
            line: attribute(&attrs, "lineno").and_then(|text| text.parse().ok()),
            wall_s: attribute(&attrs, "time").and_then(|t| t.parse().ok()).unwrap_or(0.0),
            sim_ns,
            start_ns,
            end_ns: running,
        });
    }

    Run { tests, problems }
}

/// What the tags inside a `<testcase>` say became of it.
///
/// Three spellings of the same thing are in the wild and all three are read:
/// cocotb 2.x writes `error_type` with `error_msg`, its own initialisation
/// failure writes `msg`, and JUnit itself — which cocotb 1.x followed — writes
/// `message`. Reading only one of them would drop the reason for the failure
/// and leave a report that says a test failed and not why.
fn judge(inner: &str) -> (Outcome, Option<String>) {
    for (tag, outcome) in
        [("<failure", Outcome::Failed), ("<error", Outcome::Failed), ("<skipped", Outcome::Skipped)]
    {
        let Some(at) = inner.find(tag) else { continue };
        let after = &inner[at + tag.len()..];
        let head = after.find('>').map(|end| &after[..end]).unwrap_or(after);
        let attrs = attributes(head.trim_end().trim_end_matches('/'));

        let text = attribute(&attrs, "error_msg")
            .or_else(|| attribute(&attrs, "message"))
            .or_else(|| attribute(&attrs, "msg"));
        let message = match (attribute(&attrs, "error_type"), text) {
            (Some(kind), Some(text)) => Some(format!("{kind}: {text}")),
            (Some(kind), None) => Some(kind),
            (None, text) => text,
        };
        return (outcome, message);
    }
    (Outcome::Passed, None)
}

fn attribute(attrs: &[(String, String)], name: &str) -> Option<String> {
    attrs.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone())
}

/// The `name="value"` pairs in a tag, given everything after the tag's name.
fn attributes(body: &str) -> Vec<(String, String)> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = Vec::new();
    let mut at = 0usize;

    while at < chars.len() {
        while at < chars.len() && chars[at].is_whitespace() {
            at += 1;
        }
        let start = at;
        while at < chars.len() && chars[at] != '=' && !chars[at].is_whitespace() {
            at += 1;
        }
        if start == at {
            at += 1;
            continue;
        }
        let name: String = chars[start..at].iter().collect();

        while at < chars.len() && chars[at].is_whitespace() {
            at += 1;
        }
        if chars.get(at) != Some(&'=') {
            continue;
        }
        at += 1;
        while at < chars.len() && chars[at].is_whitespace() {
            at += 1;
        }
        let Some(quote) = chars.get(at).copied() else { break };
        if quote != '"' && quote != '\'' {
            continue;
        }
        at += 1;
        let start = at;
        while at < chars.len() && chars[at] != quote {
            at += 1;
        }
        let value: String = chars[start..at].iter().collect();
        at += 1;
        out.push((name, unescape(&value)));
    }
    out
}

/// The five XML entities and the numeric ones, which is all cocotb writes.
///
/// Anything else is left exactly as it was written rather than dropped: an
/// assertion message with a mangled `&` in it still reads, where one with a
/// hole in it does not.
fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest.find(';') else {
            out.push_str(rest);
            return out;
        };
        let entity = &rest[1..end];
        match entity {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => {
                let code = entity
                    .strip_prefix("#x")
                    .or_else(|| entity.strip_prefix("#X"))
                    .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                    .or_else(|| {
                        entity.strip_prefix('#').and_then(|decimal| decimal.parse::<u32>().ok())
                    })
                    .and_then(char::from_u32);
                match code {
                    Some(character) => out.push(character),
                    None => {
                        out.push('&');
                        out.push_str(entity);
                        out.push(';');
                    }
                }
            }
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape cocotb 2.x writes, cut down.
    const SAMPLE: &str = r#"<?xml version='1.0' encoding='UTF-8'?>
<testsuites name="results">
  <testsuite name="all" package="all">
    <property name="random_seed" value="1756000000" />
    <testcase name="runs" classname="test_counter" file="tb\test_counter.py" lineno="41"
              time="0.71" sim_time_ns="20010.0" ratio_time="28183.0" />
    <testcase name="checks_crc" classname="test_counter" file="tb\test_counter.py" lineno="60"
              time="0.12" sim_time_ns="4000.0" ratio_time="33333.0">
      <failure error_type="AssertionError" error_msg="crc 0x1a2b &lt;&gt; 0x0000 at line 3" />
    </testcase>
    <testcase name="later" classname="test_counter" time="0.0" sim_time_ns="500.0">
      <skipped />
    </testcase>
  </testsuite>
</testsuites>
"#;

    #[test]
    fn every_test_and_its_verdict_comes_back() {
        let run = parse(SAMPLE);
        assert_eq!(run.tests.len(), 3);
        assert_eq!(run.counts(), (1, 1, 1));
        assert_eq!(run.tests[0].outcome, Outcome::Passed);
        assert_eq!(run.tests[1].outcome, Outcome::Failed);
        assert_eq!(run.tests[2].outcome, Outcome::Skipped);
        assert_eq!(run.tests[1].full_name(), "test_counter.checks_crc");
        assert_eq!(run.tests[1].line, Some(60));
        assert!(run.problems.is_empty(), "{:?}", run.problems);
    }

    /// The whole point: the file says how long, and this says when.
    #[test]
    fn moments_are_the_durations_added_up_in_order() {
        let run = parse(SAMPLE);
        assert_eq!(run.tests[0].start_ns, 0.0);
        assert_eq!(run.tests[0].end_ns, 20010.0);
        assert_eq!(run.tests[1].start_ns, 20010.0);
        assert_eq!(run.tests[1].end_ns, 24010.0);
        assert_eq!(run.sim_ns(), 24510.0);
    }

    #[test]
    fn a_failure_message_comes_back_unescaped() {
        let run = parse(SAMPLE);
        assert_eq!(
            run.tests[1].message.as_deref(),
            Some("AssertionError: crc 0x1a2b <> 0x0000 at line 3")
        );
    }

    /// The three spellings a run may use, all of which mean the same thing.
    #[test]
    fn a_failure_reads_however_the_runner_spelled_it() {
        let junit =
            r#"<testcase name="a" sim_time_ns="1.0"><failure message="plain" /></testcase>"#;
        assert_eq!(parse(junit).tests[0].message.as_deref(), Some("plain"));

        let init = r#"<testcase name="a" sim_time_ns="1.0"><failure msg="init" /></testcase>"#;
        assert_eq!(parse(init).tests[0].message.as_deref(), Some("init"));

        let typed = r#"<testcase name="a" sim_time_ns="1.0"><failure error_type="SimFailure" /></testcase>"#;
        assert_eq!(parse(typed).tests[0].message.as_deref(), Some("SimFailure"));
    }

    /// A missing duration would silently shift every later test, so it is
    /// named rather than treated as zero in silence.
    #[test]
    fn a_test_without_a_duration_is_reported_not_assumed() {
        let run = parse(
            r#"<testsuites><testcase name="a" time="0.1" /><testcase name="b" sim_time_ns="10.0" /></testsuites>"#,
        );
        assert_eq!(run.tests.len(), 2);
        assert_eq!(run.tests[0].sim_ns, 0.0);
        assert_eq!(run.tests[1].start_ns, 0.0);
        assert!(run.problems.iter().any(|p| p.contains("no sim_time_ns")), "{:?}", run.problems);
    }

    #[test]
    fn a_file_cut_short_says_so_rather_than_looping() {
        let run = parse(r#"<testsuites><testcase name="a" sim_time_ns="1.0">"#);
        assert_eq!(run.tests.len(), 1);
        assert!(run.problems.iter().any(|p| p.contains("cut short")), "{:?}", run.problems);
    }

    #[test]
    fn nothing_at_all_is_not_a_panic() {
        let run = parse("");
        assert!(run.tests.is_empty());
        assert!(run.problems.is_empty());
    }

    #[test]
    fn entities_this_does_not_know_survive_rather_than_vanishing() {
        assert_eq!(unescape("a &amp; b"), "a & b");
        assert_eq!(unescape("&#x41;&#66;"), "AB");
        assert_eq!(unescape("&nosuch; tail"), "&nosuch; tail");
        assert_eq!(unescape("bare & ampersand"), "bare & ampersand");
    }
}
