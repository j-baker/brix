//! [`brix::check::check`] from a library caller: a flake's checks
//! evaluated and reported as events, their derivations found but not
//! built. The last test adds to a real store, so it only runs on request:
//!
//! ```text
//! BRIX_TEST_REAL_DAEMON=1 cargo test -p brix --test check -- --ignored
//! ```

use brix::check::{check, CheckEvent, CheckReport};
use brix::{Config, LockFlags, Session};
use brix_testutil::{require_env, REAL_DAEMON};

/// A flake whose one output, `example`, is `{ <children> }`, each child a
/// leaf of the inventory.
fn flake_dir(children: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        format!(
            r#"{{
              outputs = {{ self }}: {{
                schemas.example = {{
                  version = 1;
                  doc = "";
                  inventory = output: {{ children = output; }};
                }};
                example = {{ {children} }};
              }};
            }}"#
        ),
    )
    .unwrap();
    dir
}

/// The events and outcome of checking the flake in `dir`, every system,
/// going on past failures.
fn checked(
    session: &Session,
    dir: &std::path::Path,
) -> (Vec<CheckEvent>, brix::Result<CheckReport>) {
    let flake_ref = session
        .parse_flake_ref(&format!("path:{}", dir.display()), "/")
        .unwrap();
    let flags = LockFlags {
        allow_unlocked: true,
        ..LockFlags::pure()
    };
    let flake = session.flake_with_flags(&flake_ref, &flags, false).unwrap();
    let root = brix::schemas::schema_root(session, &flake, None).unwrap();
    let mut events = Vec::new();
    let report = check(&root, None, true, false, true, |event| events.push(event));
    (events, report)
}

/// A check that fails to evaluate is an event of its own and the checks
/// after it are still evaluated; none of it needs a store.
#[test]
fn evaluation_failures_are_events_and_do_not_stop_the_rest() {
    let dir = flake_dir(
        r#"
          a.evalChecks.ok = true;
          b.evalChecks.broken = throw "b is broken";
          c.evalChecks.ok = true;
        "#,
    );
    let session = Session::new(Config::default()).unwrap();
    let (mut events, report) = checked(&session, dir.path());
    let report = report.unwrap();
    assert!(report.has_errors && !report.aborted, "{report:?}");
    assert!(report.to_build.is_empty(), "{report:?}");
    assert!(report.unchecked_outputs.contains("schemas"), "{report:?}");
    // Parallel evaluation reports in completion order.
    events.sort_by_key(|e| match e {
        CheckEvent::Passed { attr_path, .. } | CheckEvent::Failed { attr_path, .. } => {
            attr_path.clone()
        }
    });
    let [CheckEvent::Passed { attr_path: a, .. }, CheckEvent::Failed {
        attr_path: b,
        error,
    }, CheckEvent::Passed { attr_path: c, .. }] = events.as_slice()
    else {
        panic!("{events:?}");
    };
    assert_eq!([a, b, c], ["example.a", "example.b", "example.c"]);
    assert!(error.contains("b is broken"), "{error}");
}

/// A flake that cannot be walked to the end is an error, not a report.
#[test]
fn a_flake_that_cannot_be_walked_gives_no_report() {
    let dir = flake_dir(r#"a = throw "no inventory";"#);
    let session = Session::new(Config::default()).unwrap();
    let (events, report) = checked(&session, dir.path());
    assert!(report.is_err(), "{events:?}");
}

/// Finding a check's derivation needs no store: it is evaluated, and
/// reported as something to build, not as passed.
#[test]
fn a_derivation_is_found_without_a_store() {
    let dir = flake_dir(&derivation_check("no-store"));
    let session = Session::new(Config::default()).unwrap();
    let (events, report) = checked(&session, dir.path());
    assert!(events.is_empty(), "{events:?}");
    let report = report.unwrap();
    assert!(!report.has_errors, "{report:?}");
    let attrs: Vec<_> = report.to_build.values().collect();
    assert_eq!(attrs, [&vec!["example.a".to_owned()]]);
}

/// A leaf `a` that `flake check` builds: a derivation named `name` whose
/// builder fails, so that a check waiting for its build would show.
fn derivation_check(name: &str) -> String {
    format!(
        r#"a = {{
             isFlakeCheck = true;
             derivation = derivation {{
               name = "brix-detached-{name}";
               system = "{}";
               builder = "/bin/sh";
               args = [ "-c" "exit 1" ];
             }};
           }};"#,
        brix::host_system()
    )
}

/// The caller's half: the derivation the check found is made valid in
/// the store, and nothing waited for (or noticed) the build that would
/// fail.
#[test]
#[ignore = "adds to the store through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn a_found_derivation_can_be_handed_to_the_store_unbuilt() {
    require_env(REAL_DAEMON);
    let dir = flake_dir(&derivation_check("handed-over"));
    let config = Config {
        daemon: Some("daemon".into()),
        ..Config::default()
    };
    let session = Session::new(config).unwrap();
    let (events, report) = checked(&session, dir.path());
    assert!(events.is_empty(), "{events:?}");
    let report = report.unwrap();
    assert!(!report.has_errors, "{report:?}");
    let [drv_path] = report.to_build.keys().collect::<Vec<_>>()[..] else {
        panic!("{report:?}");
    };
    session
        .store_upload(std::slice::from_ref(drv_path))
        .unwrap();
    assert!(session.store_is_valid_path(drv_path).unwrap());
}
