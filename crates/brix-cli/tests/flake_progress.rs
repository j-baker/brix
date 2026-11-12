//! Progress must be written before evaluation moves to the next output.
//! Traces mark evaluation boundaries without sleeps or timing assumptions.

use std::io::{Read, Seek};
use std::process::{Command, Stdio};

use assert_cmd::prelude::*;

fn run(command: &str, children: &str, extra: &[&str]) -> (bool, String) {
    run_output(command, "example", children, extra)
}

fn run_output(command: &str, name: &str, children: &str, extra: &[&str]) -> (bool, String) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        format!(
            r#"{{
              outputs = {{ self }}: {{
                schemas.{name} = {{
                  version = 1;
                  doc = "";
                  inventory = output: {{ children = output; }};
                }};
                {name} = {{ {children} }};
              }};
            }}"#
        ),
    )
    .unwrap();
    // Sharing a file description preserves stdout/stderr write order and
    // exercises flushing when stdout isn't a terminal.
    let mut output = tempfile::tempfile().unwrap();
    let status = Command::cargo_bin("brix")
        .unwrap()
        .args(["--store", "dummy://", "flake", command])
        .args(extra)
        .arg(format!("path:{}", dir.path().display()))
        .stdout(Stdio::from(output.try_clone().unwrap()))
        .stderr(Stdio::from(output.try_clone().unwrap()))
        .status()
        .unwrap();
    output.rewind().unwrap();
    let mut text = String::new();
    output.read_to_string(&mut text).unwrap();
    (status.success(), text)
}

fn assert_order(text: &str, markers: &[&str]) {
    let mut remaining = text;
    for marker in markers {
        let index = remaining
            .find(marker)
            .unwrap_or_else(|| panic!("missing or out-of-order {marker:?}:\n{text}"));
        remaining = &remaining[index + marker.len()..];
    }
}

#[test]
fn show_streams_parents_and_leaves_before_evaluating_later_children() {
    let (success, text) = run(
        "show",
        r#"
          a.children = builtins.trace "enumerating children" {
            first.what = "first leaf";
            second = builtins.trace "evaluating second" { what = "second leaf"; };
          };
          z = builtins.trace "evaluating z" { what = "last leaf"; };
        "#,
        &[],
    );
    assert!(success, "{text}");
    assert_order(
        &text,
        &[
            "a\u{1b}[0m\n",
            "trace: enumerating children",
            ": first leaf",
            "trace: evaluating second",
            ": second leaf",
            "trace: evaluating z",
            ": last leaf",
        ],
    );
}

#[test]
fn show_keeps_completed_output_when_later_evaluation_fails() {
    let (success, text) = run(
        "show",
        r#"a.what = "completed leaf"; z = throw "later failure";"#,
        &[],
    );
    assert!(!success, "{text}");
    assert_order(&text, &[": completed leaf", "error:", "later failure"]);
}

#[test]
fn show_continues_past_broken_legacy_children() {
    let (success, text) = run_output(
        "show",
        "legacyPackages",
        r#"
          a = throw "broken leaf";
          b.children = throw "broken children";
          c.what = "remaining leaf";
        "#,
        &["--legacy"],
    );
    assert!(success, "{text}");
    assert_order(&text, &["a\u{1b}[0m\n", "b\u{1b}[0m\n", ": remaining leaf"]);
    assert_eq!(text.matches("b\u{1b}[0m\n").count(), 1, "{text}");
    assert!(!text.contains("broken"), "{text}");
}

#[test]
fn check_streams_successes_and_keep_going_failures() {
    let (success, text) = run(
        "check",
        r#"
          a.evalChecks.ok = true;
          b.evalChecks.ok = builtins.trace "checking b" false;
          c.evalChecks.ok = builtins.trace "checking c" true;
        "#,
        &["--no-build", "--keep-going"],
    );
    assert!(!success, "{text}");
    assert_order(
        &text,
        &[
            "✅ example.a",
            "trace: checking b",
            "❌ example.b",
            "Evaluation check 'example.b.ok' failed.",
            "trace: checking c",
            "✅ example.c",
        ],
    );
}

#[test]
fn check_keeps_successes_when_later_inventory_evaluation_fails() {
    let (success, text) = run(
        "check",
        r#"a.evalChecks.ok = true; z = throw "later failure";"#,
        &["--no-build"],
    );
    assert!(!success, "{text}");
    assert_order(&text, &["✅ example.a", "error:", "later failure"]);
}

#[test]
fn check_stops_streaming_at_first_failure_without_keep_going() {
    let (success, text) = run(
        "check",
        r#"
          a.evalChecks.ok = true;
          b.evalChecks.ok = false;
          c.evalChecks.ok = builtins.trace "should not be evaluated" true;
        "#,
        &["--no-build"],
    );
    assert!(!success, "{text}");
    assert_order(&text, &["✅ example.a", "❌ example.b"]);
    assert!(!text.contains("should not be evaluated"), "{text}");
    assert!(!text.contains("unchecked"), "{text}");
}
