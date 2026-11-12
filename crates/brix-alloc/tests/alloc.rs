//! The pre-faulting wrapper hands out usable memory for every size class
//! and policy, and `init` tunes mimalloc and selects the policy from the
//! environment, rejecting bad settings loudly.

use std::alloc::{GlobalAlloc, Layout, System};
use std::ffi::OsStr;
use std::process::Command;

use brix_alloc::{policy, set_policy, Policy, Prefaulting, THRESHOLD};

fn exercise(policy: Policy) {
    let alloc = Prefaulting::new(System);
    for &size in &[64usize, THRESHOLD - 1, THRESHOLD, 3 * THRESHOLD + 17] {
        let layout = Layout::from_size_align(size, 16).unwrap();
        // SAFETY: a valid, non-zero-sized layout; the block is written
        // within bounds, then freed with the same layout.
        unsafe {
            let p = alloc.alloc(layout);
            assert!(
                !p.is_null(),
                "{policy:?}: allocation of {size} bytes failed"
            );
            std::ptr::write_bytes(p, 0xA5, size);
            assert_eq!(*p.add(size - 1), 0xA5);
            let grown = alloc.realloc(p, layout, size * 2);
            assert!(!grown.is_null());
            assert_eq!(*grown, 0xA5);
            assert_eq!(*grown.add(size - 1), 0xA5);
            alloc.dealloc(grown, Layout::from_size_align(size * 2, 16).unwrap());
            let z = alloc.alloc_zeroed(layout);
            assert_eq!(*z.add(size / 2), 0);
            alloc.dealloc(z, layout);
        }
    }
}

/// The only test in this binary that changes the process-wide policy:
/// tests run on parallel threads.
#[test]
fn every_policy_allocates() {
    assert_eq!(
        policy(),
        Policy::Off,
        "nothing is populated before a policy is set"
    );
    for p in [Policy::Off, Policy::Sync, Policy::Background, Policy::Off] {
        set_policy(p).unwrap_or_else(|e| panic!("{p:?}: {e}"));
        assert_eq!(policy(), p);
        exercise(p);
    }
}

#[test]
fn policy_spellings() {
    for p in [Policy::Off, Policy::Sync, Policy::Background] {
        assert_eq!(Policy::parse(p.name()), Some(p));
    }
    assert_eq!(Policy::parse("off"), Some(Policy::Off));
    assert_eq!(Policy::parse("sync"), Some(Policy::Sync));
    assert_eq!(Policy::parse("background"), Some(Policy::Background));
    assert_eq!(Policy::parse("Background"), None);
}

/// Set in the environment of the child process the `init` tests run.
const CHILD: &str = "BRIX_ALLOC_TEST_CHILD";

/// `init` reads the process environment and changes process-wide state,
/// so each case runs it in a fresh copy of this test binary, with `env`
/// applied (`None` removes a variable), and returns its report.
fn init_in_child(env: &[(&str, Option<&OsStr>)]) -> String {
    let mut cmd = Command::new(std::env::current_exe().expect("the test binary's path"));
    cmd.args([
        "child_runs_init",
        "--exact",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(CHILD, "1");
    for (name, value) in env {
        match value {
            Some(value) => cmd.env(name, value),
            None => cmd.env_remove(name),
        };
    }
    let out = cmd.output().expect("run the test binary");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "the child failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
        .lines()
        // The harness prints `test child_runs_init ... ` before it.
        .find_map(|l| l.split_once("init: ").map(|(_, report)| report))
        .unwrap_or_else(|| panic!("no report from the child in:\n{stdout}"))
        .to_owned()
}

#[test]
#[ignore = "run in a child process by the init_* tests"]
fn child_runs_init() {
    assert!(
        std::env::var_os(CHILD).is_some(),
        "run by the init_* tests, which set {CHILD}"
    );
    let result = brix_alloc::init();
    println!(
        "init: {result:?} eager_commit={} env={:?} policy={:?}",
        brix_alloc::arena_eager_commit(),
        std::env::var_os("MIMALLOC_ARENA_EAGER_COMMIT"),
        policy()
    );
}

const EAGER: &str = "MIMALLOC_ARENA_EAGER_COMMIT";

#[test]
fn init_turns_eager_commit_off_without_touching_the_environment() {
    // Child processes of brix (`brix run`, delegated `nix`) must not
    // inherit the setting.
    assert_eq!(
        init_in_child(&[(EAGER, None), ("BRIX_PREFAULT", Some("sync".as_ref()))]),
        "Ok(()) eager_commit=0 env=None policy=Sync"
    );
}

#[test]
fn init_keeps_the_eager_commit_the_user_set() {
    assert_eq!(
        init_in_child(&[
            (EAGER, Some("1".as_ref())),
            ("BRIX_PREFAULT", Some("off".as_ref()))
        ]),
        "Ok(()) eager_commit=1 env=Some(\"1\") policy=Off"
    );
}

#[test]
fn init_defaults_to_background() {
    assert_eq!(
        init_in_child(&[(EAGER, None), ("BRIX_PREFAULT", None)]),
        "Ok(()) eager_commit=0 env=None policy=Background"
    );
}

#[test]
fn init_rejects_unknown_policies() {
    assert_eq!(
        init_in_child(&[(EAGER, None), ("BRIX_PREFAULT", Some("sometimes".as_ref()))]),
        r#"Err("BRIX_PREFAULT must be 'off', 'sync' or 'background', not \"sometimes\"") eager_commit=0 env=None policy=Off"#
    );
    // Not UTF-8: an error too, not the default.
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(
            init_in_child(&[
                (EAGER, None),
                ("BRIX_PREFAULT", Some(OsStr::from_bytes(b"sync\xff"))),
            ]),
            r#"Err("BRIX_PREFAULT must be 'off', 'sync' or 'background', not \"sync\\xFF\"") eager_commit=0 env=None policy=Off"#
        );
    }
}
