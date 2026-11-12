//! The `brix` binary against the reference `nix`.
//!
//! Every test needs `BRIX_NIX_BIN` (the devshell sets it). The fixture
//! flake under `tests/fixture` exercises most output types without any
//! input, so the comparisons run offline.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use assert_cmd::prelude::*;
use brix_testutil::{
    copy_dir, nix_bin, nixpkgs, require_env, Oracle, OracleOutput, NETWORK, ORACLE_NIX_CONFIG,
    REAL_DAEMON,
};

/// What the reference `nix` evaluates `hello.drvPath` to in the checkout,
/// computed once per test binary.
fn hello_drv() -> &'static str {
    static DRV: OnceLock<String> = OnceLock::new();
    DRV.get_or_init(|| {
        let installable = format!("path:{}#hello.drvPath", nixpkgs());
        ok(nix(&["eval", "--raw", &installable]))
    })
}

/// A copy of the fixture flake in a temporary directory (so that `nix`
/// and `brix` see the same absolute path and lock files can be written).
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture");
    copy_dir(&src, dir.path());
    dir
}

fn brix() -> Command {
    let mut cmd = Command::cargo_bin("brix").expect("brix binary");
    // Evaluation-only tests must not need a daemon.
    cmd.arg("--store").arg("dummy://");
    cmd
}

/// `brix` with the default store (the daemon), for build-shaped tests.
fn brix_with_store() -> Command {
    Command::cargo_bin("brix").expect("brix binary")
}

struct Output {
    status: i32,
    stdout: String,
    stderr: String,
}

fn run(cmd: &mut Command) -> Output {
    let out = cmd.output().expect("run command");
    Output {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// [`run`] with `input` on standard input.
fn run_with_stdin(cmd: &mut Command, input: &str) -> Output {
    use std::io::Write;
    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start command");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("run command");
    Output {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

impl From<OracleOutput> for Output {
    fn from(out: OracleOutput) -> Output {
        Output {
            status: out.code(),
            stdout: out.stdout_lossy(),
            stderr: out.stderr_lossy(),
        }
    }
}

/// The reference `nix` (brix-testutil's [`Oracle`]: a pinned
/// configuration, and stderr without what Nix says about the machine it
/// runs on, which brix does not say), in the test's own directory.
fn nix(args: &[&str]) -> Output {
    Oracle::new().args(args).output().into()
}

/// The oracle run in `dir`: for relative installables, out-links and
/// paths under the current directory.
fn nix_in(dir: &Path, args: &[&str]) -> Output {
    Oracle::new().dir(dir).args(args).output().into()
}

fn ok(out: Output) -> String {
    assert_eq!(out.status, 0, "command failed:\n{}", out.stderr);
    out.stdout
}

/// Nix's progress messages are not part of the comparison.
fn without_progress(stderr: &str) -> Vec<String> {
    let mut lines: Vec<String> = stderr
        .lines()
        .filter(|l| !l.starts_with("evaluating '") && !l.starts_with("running flake check '"))
        // Source excerpts from the schema definitions are left out: brix's
        // rendition of the built-in schemas is laid out differently from
        // Nix's file, so positions inside it differ while messages agree.
        .filter(|l| {
            let t = l.trim_start();
            !(t.starts_with("at «")
                || t.starts_with('|')
                || (t.chars().next().is_some_and(|c| c.is_ascii_digit()) && t.contains('|')))
        })
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

// ---------------------------------------------------------------------
// Structure
// ---------------------------------------------------------------------

/// The command tree (names, flags, arities, labels, categories) equals the
/// one `nix __dump-cli` reports, descriptions aside.
#[test]
fn command_tree_matches_nix() {
    let mut ours: serde_json::Value =
        serde_json::from_str(&ok(run(brix().args(["__dump-cli"])))).unwrap();
    let theirs: serde_json::Value = serde_json::from_str(&ok(nix(&["__dump-cli"]))).unwrap();
    // brix's own additions for compiled flake-input images are not nix's;
    // everything else must be.
    let root_flags = ours["args"]["flags"].as_object_mut().unwrap();
    for extension in ["compiled-input", "no-compiled-inputs"] {
        assert!(
            root_flags.remove(extension).is_some(),
            "{extension} is registered"
        );
    }
    let flake_commands = ours["args"]["commands"]["flake"]["commands"]
        .as_object_mut()
        .unwrap();
    assert!(flake_commands.remove("compile").is_some());
    fn strip(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(m) => serde_json::Value::Object(
                m.iter()
                    .filter(|(k, _)| k.as_str() != "description" && k.as_str() != "doc")
                    .map(|(k, v)| (k.clone(), strip(v)))
                    .collect(),
            ),
            serde_json::Value::Array(a) => serde_json::Value::Array(a.iter().map(strip).collect()),
            other => other.clone(),
        }
    }
    assert_eq!(strip(&ours["args"]), strip(&theirs["args"]));
}

/// The parts of a `--help` page that Nix generates from the command tree
/// (`generate-manpage.nix`), which brix lays out the same way: the
/// synopsis, the subcommands and their categories, and the options and
/// their categories. Descriptions and documentation are left out (brix's
/// wording is its own), and Nix's line wrapping is undone.
fn help_outline(page: &str, program: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut section = "";
    let mut synopsis: Vec<&str> = Vec::new();
    let mut header = String::new();
    for line in page.lines() {
        match line {
            "Synopsis" => section = "synopsis",
            "Name" | "Description" | "Examples" => section = "",
            "Options" => section = "options",
            l if l.starts_with("where subcommand is one of") => section = "subcommands",
            l if section == "synopsis" => {
                if l.is_empty() {
                    if !synopsis.is_empty() {
                        section = "";
                    }
                } else {
                    synopsis.push(l);
                }
            }
            l if section == "subcommands" => {
                if let Some(item) = l.strip_prefix("  · ") {
                    let name = item.split(" - ").next().unwrap_or(item);
                    out.push(format!("subcommand {}", name.rsplit(' ').next().unwrap()));
                } else if !l.is_empty() && !l.starts_with(' ') {
                    // A category, which Nix may wrap.
                    header = if header.is_empty() {
                        l.to_owned()
                    } else {
                        format!("{header} {l}")
                    };
                    if header.ends_with(':') {
                        out.push(format!("category {header}"));
                        header.clear();
                    }
                }
            }
            l if section == "options" => {
                if let Some(category) = l.strip_prefix("## ") {
                    out.push(format!("option category {category}"));
                } else if let Some(item) = l.strip_prefix("  · --") {
                    out.push(format!("option --{}", item.trim_end()));
                }
            }
            _ => {}
        }
    }
    let synopsis = synopsis.join(" ");
    let synopsis = synopsis.strip_prefix(program).unwrap_or(&synopsis);
    out.insert(0, format!("synopsis{synopsis}"));
    out
}

#[test]
fn help_works_for_every_command() {
    let tree: serde_json::Value =
        serde_json::from_str(&ok(run(brix().args(["__dump-cli"])))).unwrap();
    fn walk(cmd: &serde_json::Value, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
        out.push(path.clone());
        if let Some(cmds) = cmd.get("commands").and_then(|c| c.as_object()) {
            for (name, sub) in cmds {
                path.push(name.clone());
                walk(sub, path, out);
                path.pop();
            }
        }
    }
    let mut all = Vec::new();
    walk(&tree["args"], &mut Vec::new(), &mut all);
    assert!(all.len() > 100, "only {} commands", all.len());
    for path in all {
        let mut cmd = brix();
        cmd.args(&path).arg("--help");
        let out = run(&mut cmd);
        assert_eq!(out.status, 0, "--help failed for {path:?}:\n{}", out.stderr);
        // brix's own command and flags aside, the page has Nix's outline.
        if path.iter().any(|p| p == "compile") {
            continue;
        }
        let ours: Vec<String> = help_outline(&out.stdout, "brix")
            .into_iter()
            .filter(|l| !l.contains("compiled-input") && l != "subcommand compile")
            .collect();
        let mut args: Vec<&str> = path.iter().map(String::as_str).collect();
        args.push("--help");
        let theirs = help_outline(&ok(nix(&args)), "nix");
        assert_eq!(ours, theirs, "{path:?}");
    }
}

/// `help` shows the page `--help` shows, `help-stores` brix's own page,
/// and `--version` the version of Nix brix is compatible with.
#[test]
fn help_and_version() {
    for args in [
        &["help"][..],
        &["help", "flake", "show"],
        &["help", "dev-shell"],
    ] {
        let ours = help_outline(&ok(run(brix().args(args))), "brix");
        let theirs = help_outline(&ok(nix(args)), "nix");
        let ours: Vec<String> = ours
            .into_iter()
            .filter(|l| !l.contains("compiled-input"))
            .collect();
        assert_eq!(ours, theirs, "{args:?}");
    }
    let stores = ok(run(brix().args(["help-stores"])));
    assert!(stores.contains("ssh-ng://"), "{stores}");
    let version = |out: String| out.trim_end().rsplit(' ').next().unwrap().to_owned();
    assert_eq!(
        version(ok(run(brix().args(["--version"])))),
        version(ok(nix(&["--version"])))
    );
}

/// `__parse`, a development aid, prints what `nix-instantiate --parse`
/// prints, errors included.
#[test]
fn parse_matches_nix_instantiate() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("ok.nix"),
        "let x = 1; in { a = x + 2; b = \"s${toString x}\"; c = ./p; }",
    )
    .unwrap();
    std::fs::write(dir.path().join("bad.nix"), "1 +").unwrap();
    for file in ["ok.nix", "bad.nix"] {
        let ours = run(brix().current_dir(dir.path()).args(["__parse", file]));
        let theirs: Output = Oracle::nix_instantiate()
            .dir(dir.path())
            .args(["--parse", file])
            .output()
            .into();
        assert_eq!(
            (ours.status, &ours.stdout, &ours.stderr),
            (theirs.status, &theirs.stdout, &theirs.stderr),
            "{file}"
        );
    }
}

/// `--arg-from-file` and `--arg-from-stdin` pass text to the function
/// `--file` evaluates to; a file that cannot be read is Nix's error.
#[test]
fn args_from_files_and_stdin_match_nix() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.nix"), "{ x, y ? \"-\" }: x + y").unwrap();
    std::fs::write(dir.path().join("arg"), "from a file\n").unwrap();
    for (args, stdin) in [
        (
            vec!["eval", "-f", "f.nix", "--arg-from-file", "x", "arg"],
            "",
        ),
        (
            vec![
                "eval",
                "-f",
                "f.nix",
                "--arg-from-stdin",
                "x",
                "--argstr",
                "y",
                "!",
            ],
            "piped",
        ),
        (
            vec!["eval", "-f", "f.nix", "--arg-from-file", "x", "missing"],
            "",
        ),
    ] {
        let ours = run_with_stdin(brix().current_dir(dir.path()).args(&args), stdin);
        let theirs: Output = Oracle::new()
            .dir(dir.path())
            .args(&args)
            .stdin(stdin)
            .output()
            .into();
        assert_same_as_nix(&ours, &theirs, &format!("{args:?}"));
    }
}

/// A `--reference-lock-file` that does not exist is an empty lock file.
#[test]
fn a_missing_reference_lock_file_is_empty() {
    let f = fixture();
    let installable = format!("path:{}#somethingElse", f.path().display());
    let args = [
        "eval",
        "--reference-lock-file",
        "missing.lock",
        &installable,
    ];
    let dir = tempfile::tempdir().unwrap();
    let ours = run(brix().current_dir(dir.path()).args(args));
    assert_eq!(ours.stdout, "42\n", "{}", ours.stderr);
    assert_same_as_nix(
        &ours,
        &nix_in(dir.path(), &args),
        "a missing reference lock file",
    );
}

// ---------------------------------------------------------------------
// Commands brix hands to Nix
// ---------------------------------------------------------------------

/// The variable naming the `nix` that runs the commands brix does not
/// implement.
const DELEGATE_NIX: &str = "BRIX_DELEGATE_NIX";

/// A stand-in `nix` that prints its arguments, one per line, and exits
/// with status 3.
fn fake_nix() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = script(dir.path(), "printf '%s\\n' \"$@\"\nexit 3");
    (dir, path)
}

/// An executable shell script `nix` in `dir` running `body`.
fn script(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("nix");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn commands_brix_does_not_implement_run_nix() {
    let (_dir, fake) = fake_nix();
    for argv in [
        // `--impure`, which brix refuses, is Nix's business here.
        vec!["log", ".#shell", "--impure"],
        // Nix warns about an unknown setting itself; brix stays quiet.
        vec!["--option", "bogus", "1", "copy", "--to", "x"],
        vec!["profile", "list"],
        vec!["store", "gc", "--dry-run"],
        vec!["why-depends", "a", "b"],
        vec!["repl"],
        // A deprecated alias reaches Nix as typed, and Nix warns about it.
        vec!["optimise-store"],
    ] {
        let out = run(brix_with_store().env(DELEGATE_NIX, &fake).args(&argv));
        assert_eq!(out.status, 3, "{argv:?}: {}", out.stderr);
        let expected: String = argv.iter().map(|a| format!("{a}\n")).collect();
        assert_eq!(out.stdout, expected, "{argv:?}");
        assert_eq!(out.stderr, "", "{argv:?}");
    }
}

#[test]
fn delegated_commands_run_the_real_nix() {
    let out = run(brix().env(DELEGATE_NIX, nix_bin()).args([
        "key",
        "generate-secret",
        "--key-name",
        "brix-test",
    ]));
    assert_eq!(out.status, 0, "{}", out.stderr);
    assert!(out.stdout.starts_with("brix-test:"), "{}", out.stdout);
}

#[test]
fn brix_options_are_not_passed_to_nix() {
    let (_dir, fake) = fake_nix();
    for (argv, message) in [
        (
            vec!["--no-compiled-inputs", "log"],
            "'--no-compiled-inputs' is a brix option, but 'brix log' is run by Nix",
        ),
        (
            vec!["profile", "list", "--compiled-input", "nixpkgs=/x"],
            "'--compiled-input' is a brix option, but 'brix profile list' is run by Nix",
        ),
    ] {
        let out = run(brix_with_store().env(DELEGATE_NIX, &fake).args(&argv));
        assert_eq!(out.status, 1, "{argv:?}");
        assert!(out.stderr.contains(message), "{argv:?}: {}", out.stderr);
        assert_eq!(out.stdout, "", "{argv:?}: Nix must not run");
    }
}

#[test]
fn delegation_needs_a_nix_that_is_not_brix() {
    let out = run(brix_with_store()
        .env_remove(DELEGATE_NIX)
        .args(["profile", "list"]));
    assert_eq!(out.status, 1);
    assert!(
        out.stderr.contains(
            "'brix profile list' is run by Nix, but no Nix is configured: set BRIX_DELEGATE_NIX"
        ),
        "{}",
        out.stderr
    );
    let out = run(brix_with_store()
        .env(DELEGATE_NIX, "nix")
        .args(["profile", "list"]));
    assert!(
        out.stderr
            .contains("BRIX_DELEGATE_NIX must be an absolute path, not 'nix'"),
        "{}",
        out.stderr
    );
    // brix behind a symlink or a wrapper called `nix` would run itself
    // forever.
    let brix_bin = env!("CARGO_BIN_EXE_brix");
    let link_dir = tempfile::tempdir().unwrap();
    let link = link_dir.path().join("nix");
    std::os::unix::fs::symlink(brix_bin, &link).unwrap();
    let wrapper_dir = tempfile::tempdir().unwrap();
    let wrapper = script(wrapper_dir.path(), &format!("exec {brix_bin} \"$@\""));
    for nix in [&link, &wrapper] {
        let out = run(brix_with_store()
            .env(DELEGATE_NIX, nix)
            .args(["profile", "list"]));
        assert_eq!(out.status, 1, "{}", out.stderr);
        assert!(
            out.stderr.contains(&format!(
                "BRIX_DELEGATE_NIX ('{}') runs brix again, not Nix",
                nix.display()
            )),
            "{}",
            out.stderr
        );
    }
}

/// Arguments that are not UTF-8 never make brix panic: a command Nix runs
/// gets them byte for byte; a flake reference still needs text in brix,
/// and its refusal quotes the original bytes. Other byte arguments are
/// covered by `byte_arguments.rs`.
#[test]
fn arguments_that_are_not_utf8() {
    use std::os::unix::ffi::OsStrExt;
    let raw = std::ffi::OsStr::from_bytes(b"./a\xffb");
    let (_dir, fake) = fake_nix();
    let out = brix_with_store()
        .env(DELEGATE_NIX, &fake)
        .arg("log")
        .arg(raw)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{:?}", out);
    assert_eq!(out.stdout, b"log\n./a\xffb\n");
    let out = brix().arg("eval").arg(raw).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(
        out.stderr,
        b"error: brix does not support flake references that are not valid UTF-8, such as './a\xffb'\n"
    );
}

/// brix run by the Nix it delegated to (say, inside `nix develop`) can
/// delegate in turn.
#[test]
fn brix_inside_delegated_nix_still_delegates() {
    let (_inner_dir, inner) = fake_nix();
    let outer_dir = tempfile::tempdir().unwrap();
    // Runs brix as a child, as a `nix develop` shell would; the `exit`
    // keeps shells such as busybox ash from `exec`ing the last command.
    let outer = script(
        outer_dir.path(),
        &format!(
            "{DELEGATE_NIX}={} {} \"$@\"\nexit $?",
            inner.display(),
            env!("CARGO_BIN_EXE_brix")
        ),
    );
    let out = run(brix_with_store()
        .env(DELEGATE_NIX, &outer)
        .args(["profile", "list"]));
    assert_eq!(out.status, 3, "{}", out.stderr);
    assert_eq!(out.stdout, "profile\nlist\n");
}

/// brix takes the store from nix.conf, like the `nix` it hands commands
/// to, and the command line still wins.
#[test]
fn the_store_comes_from_nix_conf() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("nix.conf"),
        "store = unix:///nonexistent/brix-test.sock\n",
    )
    .unwrap();
    let expr = format!(
        "derivation {{ name = \"d\"; system = \"{}\"; builder = \"/bin/sh\"; }}",
        brix::host_system()
    );
    let brix_conf = || {
        let mut cmd = brix_with_store();
        cmd.env("NIX_CONF_DIR", dir.path())
            .env("NIX_USER_CONF_FILES", "")
            .env_remove("NIX_CONFIG");
        cmd
    };
    // The daemon client's error is Nix's own, as it is.
    let args = ["build", "--dry-run", "--expr", &expr];
    let out = run(brix_conf().args(args));
    let theirs: Output = Oracle::new()
        .env("NIX_CONF_DIR", dir.path())
        .env("NIX_USER_CONF_FILES", "")
        .args(args)
        .output()
        .into();
    assert_same_as_nix(&out, &theirs, "an unreachable store");
    assert_eq!(
        out.stderr,
        "error: cannot connect to socket at '/nonexistent/brix-test.sock': No such file or directory\n"
    );
    let out = run(brix_conf().args(["--store", "dummy://", "build", "--dry-run", "--expr", &expr]));
    assert_eq!(out.status, 1);
    assert!(
        out.stderr
            .contains("needs a Nix store, but none is configured"),
        "{}",
        out.stderr
    );
}

#[test]
fn unknown_commands_say_so() {
    let out = run(brix().args(["parse", "x"]));
    assert!(
        out.stderr.contains("'parse' is not a recognised command"),
        "{}",
        out.stderr
    );
}

/// A missing attribute of an installable or of a value suggests the names
/// that are close, as Nix does (`Suggestions`).
#[test]
fn missing_attributes_suggest_names_like_nix() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        "{ outputs = { self }: { foo = { hello = 1; help = 2; }; }; }",
    )
    .unwrap();
    for args in [
        &["eval", "path:.#foo.helo"][..],
        &["eval", "path:.#fo"],
        &["eval", "--expr", "{ abc = 1; }.abd"],
    ] {
        let ours = run(brix().current_dir(dir.path()).args(args));
        let theirs = nix_in(dir.path(), args);
        assert_same_as_nix(&ours, &theirs, &format!("{args:?}"));
    }
}

/// The suggestions of every attribute path an installable tries are
/// joined as Nix joins them (`Suggestions`): closest first, then by name.
#[test]
fn suggestions_across_attribute_paths_are_ordered_like_nix() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        "{ outputs = { self }: { packages.x86_64-linux.xbcd = 1; legacyPackages.x86_64-linux = { abxx = 2; abcc = 3; }; }; }",
    )
    .unwrap();
    let args = ["eval", "--system", "x86_64-linux", "path:.#abcd"];
    let ours = run(brix().current_dir(dir.path()).args(args));
    assert_same_as_nix(&ours, &nix_in(dir.path(), &args), "suggestions");
    assert!(ours.stderr.contains("abcc"), "{}", ours.stderr);
}

#[test]
fn usage_errors_match_nix() {
    // Suggestions are laid out under the message, as `logError` does.
    for args in [
        &["frobnicate"][..],
        &["evl"],
        &["buidl"],
        &["hash"],
        &["store"],
    ] {
        let ours = run(brix().args(args));
        let mut theirs = nix(args);
        // The program name is the only difference.
        theirs.stderr = theirs
            .stderr
            .replace("nix store requires", "brix store requires");
        theirs.stderr = theirs
            .stderr
            .replace("nix hash requires", "brix hash requires");
        assert_same_as_nix(&ours, &theirs, &format!("{args:?}"));
    }
    let ours = run(brix().args(["flake"]));
    let theirs = nix(&["flake"]);
    // The program name is the only difference.
    assert_eq!(
        ours.stderr
            .lines()
            .next()
            .map(|l| l.replace("brix flake", "nix flake")),
        theirs.stderr.lines().next().map(str::to_owned)
    );
    let ours = run(brix().args(["eval", "--bogus"]));
    assert!(
        ours.stderr
            .starts_with("error: unrecognised flag '--bogus'"),
        "{}",
        ours.stderr
    );
    assert!(ours.stderr.contains("Try '"), "{}", ours.stderr);
}

/// Nix applies each flag as it reads it: a setting flag's bad value is
/// reported before a bad flag after it, an `--option` warning comes
/// before it, and once `--help` was read a usage error is ignored.
#[test]
fn flag_errors_come_in_command_line_order_like_nix() {
    for args in [
        &["eval", "--max-jobs", "foo", "--bogus", "--expr", "1"][..],
        &["eval", "--bogus", "--max-jobs", "foo", "--expr", "1"],
        &["eval", "--cores", "x", "--expr"],
        &["eval", "--option", "cores", "x", "--bogus", "--expr", "1"],
        &["--max-jobs", "foo", "bogus"],
        &["eval", "--max-jobs", "foo", "--help"],
    ] {
        let ours = run(brix().args(args));
        assert_same_as_nix(&ours, &nix(args), &format!("{args:?}"));
    }
    let args = ["eval", "--help", "--bogus"];
    let (ours, theirs) = (run(brix().args(args)), nix(&args));
    assert_eq!((ours.status, theirs.status), (0, 0), "{}", ours.stderr);
    assert_eq!(
        ours.stdout.lines().next(),
        theirs.stdout.lines().next(),
        "the help of 'eval'"
    );
}

/// Nix's `UsageError`s carry the "Try --help" hint and its other errors do
/// not, wherever the command throws them.
#[test]
fn errors_have_the_hint_exactly_when_nix_gives_it() {
    // `InstallableValueCommand`'s `UsageError`. Nix opens its store
    // first: a dummy one, rather than whatever the environment has (in the
    // build sandbox, a local store under $HOME whose database the tests
    // running at the same time keep busy).
    let args = [
        "--store",
        "dummy://",
        "eval",
        "/nix/store/00000000000000000000000000000000-x",
    ];
    assert_same_as_nix(&run(brix().args(args)), &nix(&args), "eval of a store path");
    // `flake update` warns about a bad input and rethrows it as a plain
    // `Error`, the zero-length one included.
    let flake = flake_with_outputs("");
    for input in ["", "a/\""] {
        let args = ["flake", "update", input];
        let ours = run(brix().current_dir(flake.path()).args(args));
        let theirs = nix_in(flake.path(), &args);
        assert_eq!(theirs.status, 1, "{input:?}: {}", theirs.stderr);
        assert_same_as_nix(&ours, &theirs, &format!("flake update {input:?}"));
    }
}

// ---------------------------------------------------------------------
// eval
// ---------------------------------------------------------------------

#[test]
fn eval_forms() {
    assert_eq!(ok(run(brix().args(["eval", "--expr", "1 + 1"]))), "2\n");
    assert_eq!(
        ok(run(brix().args([
            "eval",
            "--json",
            "--expr",
            "{ a = [ 1 2 ]; }"
        ]))),
        "{\"a\":[1,2]}\n"
    );
    assert_eq!(
        ok(run(brix().args([
            "eval",
            "--raw",
            "--expr",
            "\"a\" + \"b\""
        ]))),
        "ab"
    );
    assert_eq!(
        ok(run(
            brix().args(["eval", "--apply", "x: x + 1", "--expr", "1"])
        )),
        ok(nix(&["eval", "--apply", "x: x + 1", "--expr", "1"]))
    );
    let out = run(brix().args(["eval", "--raw", "--json", "--expr", "1"]));
    assert_eq!(out.status, 1);
    assert!(out
        .stderr
        .contains("--raw and --json are mutually exclusive"));
    // The last of `--pretty` and `--no-pretty` decides.
    for flags in [["--pretty", "--no-pretty"], ["--no-pretty", "--pretty"]] {
        let mut args = vec!["eval", "--json", "--expr", "{ a = [ 1 ]; }"];
        args.extend(flags);
        assert_eq!(ok(run(brix().args(&args))), ok(nix(&args)), "{flags:?}");
    }
}

/// `nix eval` writes strings' bytes as they are, UTF-8 or not: in the
/// value (`--raw` too), in `trace` output and in an error quoting a
/// value. The outputs are compared as bytes.
#[test]
fn eval_writes_bytes_as_they_are() {
    let lead = r#"(builtins.substring 0 1 "é")"#;
    for expr in [
        format!("[ {lead} ]"),
        format!("builtins.trace {lead} 1"),
        format!("throw {lead}"),
        format!("{{ a = {lead}; }} + 1"),
    ] {
        let args = ["eval", "--expr", &expr];
        let ours = brix().args(args).output().expect("run brix");
        let theirs = Oracle::new().args(args).output();
        assert_eq!(
            (ours.status.code(), ours.stdout, ours.stderr),
            (theirs.status.code(), theirs.stdout, theirs.stderr),
            "{expr}"
        );
    }
    let raw_args = ["eval", "--raw", "--expr", lead];
    assert_eq!(
        brix().args(raw_args).output().unwrap().stdout,
        Oracle::new().args(raw_args).output().stdout
    );
}

#[test]
fn eval_write_to() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out");
    ok(run(brix().args([
        "eval",
        "--write-to",
        dest.to_str().unwrap(),
        "--expr",
        "{ a = \"x\"; b = { c = \"y\"; }; }",
    ])));
    assert_eq!(std::fs::read_to_string(dest.join("a")).unwrap(), "x");
    assert_eq!(std::fs::read_to_string(dest.join("b/c")).unwrap(), "y");
    let out = run(brix().args([
        "eval",
        "--write-to",
        dest.to_str().unwrap(),
        "--expr",
        "\"z\"",
    ]));
    assert_eq!(out.status, 1);
    assert!(out.stderr.contains("already exists"), "{}", out.stderr);
    // The path is used as given, relative to the current directory.
    for dest in ["./out", "./missing/../new"] {
        let args = ["eval", "--write-to", dest, "--expr", "\"z\""];
        let ours = run(brix().current_dir(dir.path()).args(args));
        let theirs = nix_in(dir.path(), &args);
        assert_eq!(theirs.status, 1, "{dest}: {}", theirs.stderr);
        assert_same_as_nix(&ours, &theirs, dest);
    }
}

/// `--stdin` adds the words of standard input to the installables given,
/// and replaces only the default `.`.
#[test]
fn stdin_adds_installables() {
    let f = fixture();
    let flake = format!("path:{}", f.path().display());
    let (tool, default) = (format!("{flake}#tool"), format!("{flake}#default"));
    let stdin = format!("{default}\x0b{flake}#packages.x86_64-linux.script\n");
    for args in [
        vec!["derivation", "show", "--stdin", tool.as_str()],
        vec!["derivation", "show", "--stdin"],
    ] {
        let mut ours_args = args.clone();
        ours_args.extend(["--system", "x86_64-linux"]);
        let ours = run_with_stdin(brix().args(&ours_args), &stdin);
        let theirs: Output = Oracle::new()
            .args(&args)
            .stdin(stdin.as_str())
            .output()
            .into();
        assert_eq!(ours.status, 0, "{}", ours.stderr);
        assert_eq!(ours.stdout, theirs.stdout, "{args:?}");
    }
}

/// `--drv-link` makes its links as every out-link is made
/// (`addPermRoot`): never over something that is not a link into the
/// store, and only in a store on this machine.
#[test]
fn eval_drv_link_does_not_replace_files() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain");
    std::fs::write(&plain, "mine\n").unwrap();
    let args = [
        "eval",
        "--raw",
        "--drv-link",
        "plain",
        "--expr",
        r#"builtins.toFile "brix-drv-link" "x""#,
    ];
    // With the daemon's store. Nix wrote the path while evaluating, so
    // the link is refused without the store being asked anything; brix
    // refuses it before it writes the path, so a daemon it cannot reach
    // (as in the build sandbox, where Nix's store is a local one) makes
    // no difference.
    let ours = run(brix_with_store()
        .current_dir(dir.path())
        .env("NIX_DAEMON_SOCKET_PATH", dir.path().join("no-daemon"))
        .args(args));
    let theirs = nix_in(dir.path(), &args);
    assert_eq!(ours.status, 1, "{}", ours.stderr);
    assert!(ours.stderr.contains("; already exists"), "{}", ours.stderr);
    assert_same_as_nix(&ours, &theirs, "--drv-link over a file");
    assert_eq!(std::fs::read_to_string(&plain).unwrap(), "mine\n");
    // A store that is not on this machine gets no links, and no error.
    let args = [
        "--store",
        "dummy://",
        "eval",
        "--raw",
        "--drv-link",
        "link",
        "--expr",
        r#"(derivation { name = "x"; system = "x"; builder = "/bin/sh"; }).drvPath"#,
    ];
    let ours = run(brix_with_store().current_dir(dir.path()).args(args));
    assert_same_as_nix(&ours, &nix_in(dir.path(), &args), "--drv-link, dummy store");
    assert_eq!(ours.status, 0, "{}", ours.stderr);
    assert!(!dir.path().join("link").exists());
}

#[test]
fn eval_file_and_args() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("f.nix"),
        "{ x ? 1 }: { v = x * 2; t = builtins.typeOf x; }",
    )
    .unwrap();
    let file = dir.path().join("f.nix");
    let file = file.to_str().unwrap();
    assert_eq!(ok(run(brix().args(["eval", "-f", file, "v"]))), "2\n");
    assert_eq!(
        ok(run(
            brix().args(["eval", "-f", file, "--arg", "x", "5", "v"])
        )),
        "10\n"
    );
    assert_eq!(
        ok(run(
            brix().args(["eval", "-f", file, "--argstr", "x", "s", "t"])
        )),
        ok(nix(&["eval", "-f", file, "--argstr", "x", "s", "t"]))
    );
}

#[test]
fn eval_hello_drv_path_from_nixpkgs_flake() {
    let installable = format!("path:{}#hello.drvPath", nixpkgs());
    let got = ok(run(brix().args([
        "eval",
        "--raw",
        "--system",
        "x86_64-linux",
        &installable,
    ])));
    assert_eq!(got, hello_drv());
}

#[test]
fn eval_lib_version_matches_nix() {
    let installable = format!("path:{}#lib.version", nixpkgs());
    assert_eq!(
        ok(run(brix().args(["eval", &installable]))),
        ok(nix(&["eval", &installable]))
    );
}

#[test]
fn eval_missing_attribute_message_matches_nix() {
    let f = fixture();
    let installable = format!("path:{}#tol", f.path().display());
    let ours = run(brix().args(["eval", "--system", "x86_64-linux", &installable]));
    let theirs = nix(&["eval", &installable]);
    assert_eq!(ours.status, theirs.status);
    assert_eq!(ours.stderr, theirs.stderr);
}

/// The parser's warnings and the `lint-*` settings, as flags and as
/// `--option`, print and fail as in Nix.
#[test]
fn eval_parser_warnings_and_lint_settings_match_nix() {
    for args in [
        &["--lint-url-literals", "warn", "--expr", "http://x"][..],
        &["--lint-url-literals", "fatal", "--expr", "http://x"],
        &["--lint-short-path-literals", "fatal", "--expr", "foo/bar"],
        &[
            "--option",
            "lint-absolute-path-literals",
            "warn",
            "--expr",
            "builtins.isPath /abs",
        ],
        &[
            "--warn-short-path-literals",
            "--expr",
            "builtins.isPath foo/bar",
        ],
        &["--expr", "let or = 1; in builtins.length [ (x: x) or ]"],
    ] {
        let ours = run(brix().arg("eval").args(args));
        let theirs = nix(&[&["--store", "dummy://", "eval"][..], args].concat());
        assert_eq!(ours.status, theirs.status, "{args:?}");
        assert_eq!(ours.stdout, theirs.stdout, "{args:?}");
        assert_eq!(ours.stderr, theirs.stderr, "{args:?}");
    }
    // A value that is not a level is a usage error.
    let ours = run(brix().args(["eval", "--lint-url-literals", "bogus", "--expr", "1"]));
    let theirs = nix(&[
        "--store",
        "dummy://",
        "eval",
        "--lint-url-literals",
        "bogus",
        "--expr",
        "1",
    ]);
    assert_eq!(ours.status, theirs.status);
    assert_eq!(ours.stderr.lines().next(), theirs.stderr.lines().next());
}

/// Setting values are checked as Nix's `Setting::set` checks them: a
/// setting flag with a bad value is a usage error that ends the parse,
/// `--option` with one warns and leaves the setting alone, and the
/// warnings come in Nix's order (set-time ones as the flags are read,
/// unknown settings at the end).
#[test]
fn setting_values_are_checked_like_nix() {
    let expr = "let f = n: if n == 0 then 0 else f (n - 1); in f 50";
    for flags in [
        &["--max-call-depth", "abc"][..],
        &["--max-call-depth", "4294967296"],
        &["--max-call-depth", "-1"],
        &["--max-call-depth", "+5"],
        &["--max-call-depth", "1K"],
        &["--eval-cores", "x"],
        &["--lint-url-literals", "bogus"],
        &["--option", "max-call-depth", "abc"],
        &[
            "--max-call-depth",
            "10",
            "--option",
            "max-call-depth",
            "abc",
        ],
        &["--option", "pure-eval", "maybe"],
        &["--option", "lint-url-literals", "bogus"],
        &["--option", "keep-going", "maybe"],
        &["--option", "warn-short-path-literals", "maybe"],
        &["--option", "max-jobs", "2K"],
        &["--max-jobs", "2K"],
        &["--option", "extra-max-call-depth", "x"],
        // Aliases: known settings, and flags, checked under the name of
        // the setting they set.
        &["--option", "download-attempts", "3"],
        &["--option", "download-attempts", "x"],
        &["--download-attempts", "x"],
        &["--option", "extra-download-attempts", "3"],
        &["--option", "extra-binary-caches", "https://x"],
        &["--build-max-jobs", "2K"],
        &["--build-fallback", "--build-use-sandbox"],
        &["--option", "build-use-sandbox", "bogus"],
        &["--option", "commit-lockfile-summary", "x"],
        &[
            "--option",
            "bogus",
            "1",
            "--warn-short-path-literals",
            "--option",
            "eval-cores",
            "x",
        ],
        &[
            "--option",
            "bogus",
            "1",
            "--option",
            "eval-cores",
            "x",
            "--max-call-depth",
            "abc",
            "--option",
            "keep-going",
            "maybe",
        ],
    ] {
        let args = [&["eval"][..], flags, &["--expr", expr]].concat();
        let ours = run(brix().args(&args));
        let theirs = nix(&[&["--store", "dummy://"][..], &args].concat());
        assert_same_as_nix(&ours, &theirs, &format!("{flags:?}"));
    }
}

/// What `nix.conf` and `NIX_CONFIG` give a setting is checked when it is
/// read, failing before the command runs, with the file named as
/// `PathFmt` does.
#[test]
fn configuration_errors_match_nix() {
    let dir = tempfile::tempdir().unwrap();
    let conf = dir.path().join("nix.conf");
    for text in [
        "keep-going = maybe\n",
        "tarball-ttl = x\n",
        "netrc-file\n",
        "include nowhere.conf\n",
    ] {
        std::fs::write(&conf, text).unwrap();
        let args = ["eval", "--expr", "1"];
        let ours = run(brix()
            .env("NIX_USER_CONF_FILES", &conf)
            .env("NIX_CONFIG", "")
            .args(args));
        let theirs: Output = Oracle::new()
            .env("NIX_USER_CONF_FILES", &conf)
            .args(["--store", "dummy://"])
            .args(args)
            .output()
            .into();
        assert_same_as_nix(&ours, &theirs, text);
    }
}

/// brix's own switches in the environment are `1` or `0`; any other value
/// is refused rather than read as "off".
#[test]
fn brix_switches_are_one_or_zero() {
    for var in ["BRIX_TIMINGS", "BRIX_FETCH_CACHE_VERIFY"] {
        let out = run(brix().env(var, "yes").args(["eval", "--expr", "1"]));
        assert_eq!(out.status, 1, "{var}: {}", out.stdout);
        assert_eq!(
            out.stderr,
            format!("error: {var} must be 1 or 0, not 'yes'\n")
        );
        let out = run(brix().env(var, "0").args(["eval", "--expr", "1"]));
        assert_eq!((out.status, out.stdout.as_str()), (0, "1\n"), "{var}");
        assert_eq!(out.stderr, "", "{var}");
    }
    // `BRIX_TIMINGS=1` reaches the evaluator, which reports its counters.
    let out = run(brix()
        .env("BRIX_TIMINGS", "1")
        .args(["eval", "--expr", "1"]));
    assert_eq!(out.stdout, "1\n", "{}", out.stderr);
    assert!(out.stderr.starts_with("brix timings:\n"), "{}", out.stderr);
    assert!(out.stderr.contains("gc: 0 collections,"), "{}", out.stderr);
}

#[test]
fn gc_threshold_override_requires_a_positive_byte_count() {
    for variable in ["BRIX_GC_THRESHOLD_BYTES", "BRIX_GC_STEADY_THRESHOLD_BYTES"] {
        for value in ["0", "-1", "yes", "184467440737095516160"] {
            let out = run(brix().env(variable, value).args(["eval", "--expr", "1"]));
            assert_eq!(out.status, 1);
            assert_eq!(
                out.stderr,
                format!("error: {variable} must be a positive number of bytes\n")
            );
        }
    }
    let out = run(brix()
        .env("BRIX_GC_THRESHOLD_BYTES", "1024")
        .env("BRIX_GC_STEADY_THRESHOLD_BYTES", "256")
        .args(["eval", "--expr", "1 + 2"]));
    assert_eq!(
        (out.status, out.stdout.as_str(), out.stderr.as_str()),
        (0, "3\n", "")
    );
}

// ---------------------------------------------------------------------
// flake
// ---------------------------------------------------------------------

#[test]
fn flake_metadata_matches_nix() {
    let f = fixture();
    let flake_ref = format!("path:{}", f.path().display());
    assert_eq!(
        ok(run(brix().args(["flake", "metadata", &flake_ref]))),
        ok(nix(&["flake", "metadata", &flake_ref]))
    );
    // The deprecated alias warns, then does the same.
    let args = ["flake", "info", &flake_ref];
    assert_same_as_nix(&run(brix().args(args)), &nix(&args), "flake info");
    assert_eq!(
        ok(run(brix().args(["flake", "metadata", "--json", &flake_ref]))),
        ok(nix(&["flake", "metadata", "--json", &flake_ref]))
    );
}

#[test]
fn flake_metadata_json_of_nixpkgs_matches_nix() {
    let flake_ref = format!("path:{}", nixpkgs());
    let got: serde_json::Value = serde_json::from_str(&ok(run(
        brix().args(["flake", "metadata", "--json", &flake_ref])
    )))
    .unwrap();
    let mut expected: serde_json::Value =
        serde_json::from_str(&ok(nix(&["flake", "metadata", "--json", &flake_ref]))).unwrap();
    // `fingerprint` is derived from Nix's local store state and `path` needs
    // the store to vouch for the tree, which the dummy store cannot.
    let obj = expected.as_object_mut().unwrap();
    obj.remove("fingerprint");
    obj.remove("path");
    assert_eq!(got, expected);
}

/// `flake metadata` asks the store about the flake's store path, so a
/// store that cannot be reached fails it: for a flake in the store with
/// "previously failed" under the fetch's trace (Nix's fetcher swallowed
/// its first attempt), for any other with the connection error.
#[test]
fn flake_metadata_with_an_unreachable_store_fails_like_nix() {
    let f = fixture();
    for flake_ref in [
        format!("path:{}", nixpkgs()),
        format!("path:{}", f.path().display()),
    ] {
        let args = [
            "--store",
            "unix:///nonexistent/brix-metadata-test",
            "flake",
            "metadata",
            flake_ref.as_str(),
        ];
        let ours = run(brix_with_store().args(args));
        assert_ne!(ours.status, 0, "{flake_ref}");
        assert_same_as_nix(&ours, &nix(&args), &flake_ref);
    }
}

#[test]
fn flake_show_matches_nix() {
    let f = fixture();
    let flake_ref = format!("path:{}", f.path().display());
    for extra in [
        vec![],
        vec!["--json"],
        vec!["--all-systems", "--legacy"],
        vec![
            "--json",
            "--all-systems",
            "--legacy",
            "--drv-paths",
            "--output-paths",
        ],
    ] {
        let mut args = vec!["flake", "show", "--system", "x86_64-linux", &flake_ref];
        args.extend(extra.iter());
        let mut theirs: Vec<&str> = vec!["flake", "show", &flake_ref];
        theirs.extend(extra.iter());
        assert_eq!(
            ok(run(brix().args(&args))),
            ok(nix(&theirs)),
            "args {extra:?}"
        );
    }
    let out = run(brix().args(["flake", "show", "--output-paths", &flake_ref]));
    assert_eq!(out.status, 1);
    assert!(out
        .stderr
        .contains("The '--output-paths' flag requires '--json'."));
}

#[test]
fn flake_check_no_build_matches_nix() {
    let f = fixture();
    let flake_ref = format!("path:{}", f.path().display());
    for extra in [vec![], vec!["--all-systems"]] {
        let mut args = vec![
            "flake",
            "check",
            "--no-build",
            "--system",
            "x86_64-linux",
            &flake_ref,
        ];
        args.extend(extra.iter());
        let mut theirs: Vec<&str> = vec!["flake", "check", "--no-build", &flake_ref];
        theirs.extend(extra.iter());
        let ours = run(brix().args(&args));
        let nix_out = nix(&theirs);
        assert_eq!(ours.status, nix_out.status);
        assert_eq!(
            without_progress(&ours.stderr),
            without_progress(&nix_out.stderr)
        );
    }
}

#[test]
fn flake_check_reports_a_broken_output_like_nix() {
    let f = fixture();
    std::fs::write(
        f.path().join("flake.nix"),
        r#"{ outputs = { self }: { overlays.default = 42; nixosModules.bad = "nope"; }; }"#,
    )
    .unwrap();
    let flake_ref = format!("path:{}", f.path().display());
    let ours = run(brix().args(["flake", "check", "--no-build", "--keep-going", &flake_ref]));
    let theirs = nix(&["flake", "check", "--no-build", "--keep-going", &flake_ref]);
    assert_eq!(ours.status, 1);
    assert_eq!(theirs.status, 1);
    assert_eq!(
        without_progress(&ours.stderr),
        without_progress(&theirs.stderr)
    );
}

/// `flake prefetch-inputs` fetches every locked input, saying so at
/// `info`.
#[test]
fn flake_prefetch_inputs_matches_nix() {
    let f = fixture();
    let flake = tempfile::tempdir().unwrap();
    std::fs::write(
        flake.path().join("flake.nix"),
        format!(
            "{{ inputs.fx.url = \"path:{}\"; outputs = {{ self, fx }}: {{ }}; }}\n",
            f.path().display()
        ),
    )
    .unwrap();
    ok(nix_in(flake.path(), &["flake", "lock"]));
    let args = ["flake", "prefetch-inputs"];
    let ours = run(brix().current_dir(flake.path()).args(args));
    assert!(
        ours.stderr.starts_with("fetching 'path:"),
        "{}",
        ours.stderr
    );
    assert_same_as_nix(&ours, &nix_in(flake.path(), &args), "prefetch-inputs");
}

#[test]
fn flake_prefetch_matches_nix() {
    let f = fixture();
    let flake_ref = format!("path:{}", f.path().display());
    assert_eq!(
        ok(run(brix().args(["flake", "prefetch", "--json", &flake_ref]))),
        ok(nix(&["flake", "prefetch", "--json", &flake_ref]))
    );
}

#[test]
fn flake_lock_and_update_write_what_nix_writes() {
    let f = fixture();
    let dep = format!("path:{}", f.path().display());
    let ours = tempfile::tempdir().unwrap();
    let theirs = tempfile::tempdir().unwrap();
    let flake_nix = format!(
        "{{ inputs.fx.url = \"{dep}\"; outputs = {{ self, fx }}: {{ v = fx.somethingElse; }}; }}\n"
    );
    std::fs::write(ours.path().join("flake.nix"), &flake_nix).unwrap();
    std::fs::write(theirs.path().join("flake.nix"), &flake_nix).unwrap();
    let out = run(brix().current_dir(ours.path()).args(["flake", "lock"]));
    assert_eq!(out.status, 0, "{}", out.stderr);
    assert!(out.stderr.contains("creating lock file"), "{}", out.stderr);
    ok(nix_in(theirs.path(), &["flake", "lock"]));
    assert_eq!(
        std::fs::read_to_string(ours.path().join("flake.lock")).unwrap(),
        std::fs::read_to_string(theirs.path().join("flake.lock")).unwrap()
    );
    // With the lock in place the flake evaluates; a second lock is a no-op.
    assert_eq!(
        ok(run(brix().current_dir(ours.path()).args(["eval", ".#v"]))),
        "42\n"
    );
    let out = run(brix().current_dir(ours.path()).args(["flake", "lock"]));
    assert_eq!(out.status, 0);
    assert!(out.stderr.is_empty(), "{}", out.stderr);
    // `flake update` re-locks from scratch; same inputs, same lock.
    let before = std::fs::read_to_string(ours.path().join("flake.lock")).unwrap();
    ok(run(brix()
        .current_dir(ours.path())
        .args(["flake", "update"])));
    assert_eq!(
        std::fs::read_to_string(ours.path().join("flake.lock")).unwrap(),
        before
    );
}

/// `flake new` and `flake init` copy a template as Nix does: the
/// destination created, files written without their executable bit,
/// symlinks recreated, identical files skipped and a different one
/// refused, with Nix's messages, which name the template's files where
/// the flake has them (`coerceToPath`, no copy to the store).
#[test]
fn flake_new_and_init_match_nix() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let f = fixture();
    let tmpl = f.path().join("tmpl");
    std::fs::write(tmpl.join("run.sh"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(tmpl.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("flake.nix", tmpl.join("link")).unwrap();
    let template = format!("path:{}", f.path().display());
    let root = tempfile::tempdir().unwrap();
    let mut outcomes = Vec::new();
    for who in ["brix", "nix"] {
        let dir = root.path().join(who);
        std::fs::create_dir(&dir).unwrap();
        let tool = |cwd: &Path, args: &[&str]| -> Output {
            let mut out = if who == "brix" {
                run(brix().current_dir(cwd).args(args))
            } else {
                nix_in(cwd, args)
            };
            out.stderr = out.stderr.replace(&dir.display().to_string(), "DIR");
            out
        };
        let new = tool(&dir, &["flake", "new", "-t", &template, "new"]);
        std::fs::write(dir.join("new/flake.nix"), "# changed\n").unwrap();
        let init = tool(&dir.join("new"), &["flake", "init", "-t", &template]);
        let mut tree = Vec::new();
        for name in ["flake.nix", "run.sh", "link"] {
            let path = dir.join("new").join(name);
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let target = std::fs::read_link(&path).ok();
            tree.push((name, meta.mode(), target));
        }
        outcomes.push((
            (new.status, new.stdout, new.stderr),
            (init.status, init.stdout, init.stderr),
            tree,
        ));
    }
    assert_eq!(outcomes[0].0, outcomes[1].0, "flake new");
    assert_eq!(outcomes[0].1, outcomes[1].1, "flake init");
    assert_eq!(outcomes[0].2, outcomes[1].2, "the copied tree");
    assert_eq!(outcomes[0].1 .0, 1, "a conflict fails");
}

// ---------------------------------------------------------------------
// derivation, hash, search
// ---------------------------------------------------------------------

#[test]
fn derivation_show_matches_nix() {
    let installable = format!("path:{}#hello", nixpkgs());
    assert_eq!(
        ok(run(brix().args([
            "derivation",
            "show",
            "--system",
            "x86_64-linux",
            &installable
        ]))),
        ok(nix(&["derivation", "show", &installable]))
    );
    let f = fixture();
    let installable = format!("path:{}#tool", f.path().display());
    assert_eq!(
        ok(run(brix().args([
            "derivation",
            "show",
            "--system",
            "x86_64-linux",
            &installable
        ]))),
        ok(nix(&["derivation", "show", &installable]))
    );
}

/// An attribute nixpkgs does not have: every default attribute path the
/// schemas give for the command's roles is named, as Nix names them.
#[test]
fn missing_nixpkgs_attributes_name_every_default_path_like_nix() {
    let installable = format!("path:{}#doesNotExist", nixpkgs());
    let args = ["derivation", "show", installable.as_str()];
    let ours = run(brix().args(args));
    assert_same_as_nix(&ours, &nix(&args), "a missing attribute");
    assert!(
        ours.stderr.contains("does not provide attribute 'packages.x86_64-linux.doesNotExist', 'defaultPackage.x86_64-linux.doesNotExist', 'legacyPackages.x86_64-linux.doesNotExist' or 'doesNotExist'"),
        "{}",
        ours.stderr
    );
}

#[test]
fn derivation_show_recursive_matches_nix() {
    let installable = format!("path:{}#hello", nixpkgs());
    let ours = ok(run(brix().args([
        "derivation",
        "show",
        "--recursive",
        "--system",
        "x86_64-linux",
        &installable,
    ])));
    assert_eq!(
        ours,
        ok(nix(&["derivation", "show", "--recursive", &installable]))
    );
    let got: serde_json::Value = serde_json::from_str(&ours).unwrap();
    assert!(got["derivations"]
        .as_object()
        .unwrap()
        .contains_key(&hello_drv()["/nix/store/".len()..]));
}

#[test]
fn hash_commands_match_nix() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "hello\n").unwrap();
    std::fs::create_dir(dir.path().join("d")).unwrap();
    std::fs::write(dir.path().join("d/x"), "x").unwrap();
    let f = dir.path().join("f");
    let d = dir.path().join("d");
    let f = f.to_str().unwrap();
    let d = d.to_str().unwrap();
    let cases: Vec<Vec<&str>> = vec![
        vec!["hash", "file", f],
        vec!["hash", "file", "--base16", f],
        vec!["hash", "file", "--type", "sha1", "--base32", f],
        vec!["hash", "path", d],
        vec!["hash", "path", "--algo", "sha512", "--format", "base64", d],
        vec!["hash", "path", "--mode", "flat", f],
        vec![
            "hash",
            "convert",
            "--hash-algo",
            "sha256",
            "--to",
            "base16",
            "sha256-f1w9CI4HdZrOwQI/7ZwZhUAMFuNFdnE8PQdTl+tTgRA=",
        ],
        vec![
            "hash",
            "convert",
            "--to",
            "nix32",
            "sha256-f1w9CI4HdZrOwQI/7ZwZhUAMFuNFdnE8PQdTl+tTgRA=",
        ],
        vec![
            "hash",
            "to-base32",
            "--type",
            "sha256",
            "f1w9CI4HdZrOwQI/7ZwZhUAMFuNFdnE8PQdTl+tTgRA=",
        ],
        vec![
            "hash",
            "to-sri",
            "--type",
            "sha256",
            "0d1a6d9bd69e32d6c7bd9c33d5e3a6ab7f92b4b4b7a8a83e5b2a0bd7dc7f1a5e",
        ],
    ];
    for case in cases {
        assert_eq!(ok(run(brix().args(&case))), ok(nix(&case)), "{case:?}");
    }
}

#[test]
fn search_matches_nix() {
    let f = fixture();
    let flake_ref = format!("path:{}", f.path().display());
    assert_eq!(
        ok(run(brix().args([
            "search",
            "--json",
            "--system",
            "x86_64-linux",
            &flake_ref,
            "hello"
        ]))),
        ok(nix(&["search", "--json", &flake_ref, "hello"]))
    );
    // Nix prints results as its parallel evaluator finds them, so the
    // order and the number of blank separator lines vary between runs;
    // compare the set of non-empty lines.
    let sorted = |s: String| {
        let mut v: Vec<&str> = s.lines().filter(|l| !l.is_empty()).collect();
        v.sort();
        v.join("\n")
    };
    let ours = run(brix().args([
        "search",
        "--system",
        "x86_64-linux",
        &flake_ref,
        "-e",
        "alpha",
        "^",
    ]));
    let theirs = nix(&["search", &flake_ref, "-e", "alpha", "^"]);
    assert_eq!(sorted(ours.stdout), sorted(theirs.stdout));
    assert_eq!(ours.stderr, theirs.stderr);
    let out = run(brix().args(["search", &flake_ref, "nothingmatchesthis"]));
    assert_eq!(out.status, 1);
    assert!(out
        .stderr
        .contains("no results for the given search term(s)!"));
}

// ---------------------------------------------------------------------
// Through the real daemon
// ---------------------------------------------------------------------

// The tests that build go through the real Nix daemon. They are
// `#[ignore]`d by default and refuse to run without
// `BRIX_TEST_REAL_DAEMON=1` (`require_env`), so that running them is a
// decision.

/// A derivation brix did not evaluate, named by its store path, is read
/// from the daemon's store, and with `--recursive` so is its whole input
/// closure (`Store::readDerivation`).
#[test]
#[ignore = "reads derivations through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn derivation_show_of_a_store_derivation_matches_nix() {
    require_env(REAL_DAEMON);
    // `nix eval` of the `drvPath` writes the derivations to the store.
    let drv = hello_drv();
    for args in [
        vec!["derivation", "show", drv],
        vec!["derivation", "show", "--recursive", drv],
    ] {
        assert_eq!(ok(run(brix_with_store().args(&args))), ok(nix(&args)));
    }
}

/// Outputs of a store path the store does not have: the daemon reports
/// what it cannot build, and each command then fails where Nix's does,
/// in its words (`readInvalidDerivation`, `readDerivation`, the
/// daemon's output map), or skips what is no derivation.
#[test]
#[ignore = "queries the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn outputs_of_paths_the_store_lacks_match_nix() {
    require_env(REAL_DAEMON);
    let missing = "/nix/store/00000000000000000000000000000000-brix-missing";
    for path in [missing.to_owned(), format!("{missing}.drv")] {
        let built = format!("{path}^out");
        for args in [
            vec!["path-info", built.as_str()],
            vec!["build", "--dry-run", built.as_str()],
            vec!["build", "--dry-run", "--json", built.as_str()],
            vec!["derivation", "show", built.as_str()],
        ] {
            let ours = run(brix_with_store().args(&args));
            assert_same_as_nix(&ours, &nix(&args), &format!("{args:?}"));
        }
    }
}

/// A locked `path:` input the store already has is taken from the store
/// (`makeStoreAccessor`), and its files are shown as `«path:…»/file`,
/// not at the directory it was locked from.
#[test]
#[ignore = "copies to the store through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn path_inputs_served_from_the_store_are_shown_like_nix() {
    require_env(REAL_DAEMON);
    let dir = tempfile::tempdir().unwrap();
    let dep = dir.path().join("dep");
    let root = dir.path().join("root");
    std::fs::create_dir_all(&dep).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    // A unique tree, so that it is in the store only once archived below.
    std::fs::write(
        dep.join("flake.nix"),
        format!(
            "{{ outputs = _: {{ bad = throw \"{}\"; }}; }}\n",
            dir.path().display()
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("flake.nix"),
        format!(
            "{{ inputs.dep.url = \"path:{}\"; outputs = {{ self, dep }}: {{ bad = dep.bad; }}; }}\n",
            dep.display()
        ),
    )
    .unwrap();
    let flake = format!("path:{}", root.display());
    ok(nix(&["flake", "lock", &flake]));
    let installable = format!("{flake}#bad");
    let args = ["eval", installable.as_str()];
    // Read from its directory first, then from the store.
    for archived in [false, true] {
        if archived {
            ok(nix(&["flake", "archive", &flake]));
        }
        let ours = run(brix_with_store().args(args));
        assert_same_as_nix(&ours, &nix(&args), &format!("archived: {archived}"));
        let shown_at = format!("«path:{}»/flake.nix", dep.display());
        assert_eq!(ours.stderr.contains(&shown_at), archived, "{}", ours.stderr);
    }
}

/// A path value as an installable is copied to the store under the name
/// `source` (`fetchToStore`), there before anything is built; `^outputs`
/// on a store path that is no derivation fails when the derivation is
/// read, as in Nix.
#[test]
#[ignore = "adds to the store through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn path_values_and_outputs_of_non_derivations_match_nix() {
    require_env(REAL_DAEMON);
    let dir = tempfile::tempdir().unwrap();
    // New content, so that the path is not in the store yet.
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    std::fs::write(dir.path().join("data.txt"), &nonce).unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        "{ outputs = _: { p = ./data.txt; }; }\n",
    )
    .unwrap();
    let installable = format!("path:{}#p", dir.path().display());
    let args = ["build", "--no-link", "--print-out-paths", &installable];
    let ours = ok(run(brix_with_store().args(args)));
    assert!(ours.trim_end().ends_with("-source"), "{ours}");
    assert_eq!(
        std::fs::read_to_string(ours.trim_end()).unwrap(),
        nonce,
        "brix put the file in the store"
    );
    assert_eq!(ours, ok(nix(&args)));
    let args = ["path-info", &installable];
    assert_eq!(ok(run(brix_with_store().args(args))), ok(nix(&args)));

    let not_a_drv = format!("{}^out", nixpkgs());
    for args in [
        vec!["build", "--no-link", &not_a_drv],
        vec!["path-info", &not_a_drv],
    ] {
        assert_same_as_nix(&run(brix_with_store().args(&args)), &nix(&args), args[0]);
    }
}

/// `path-info --derivation` of an output path asks the store for its
/// derivers (`queryValidDerivers`).
#[test]
#[ignore = "builds through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn path_info_derivation_of_an_output_matches_nix() {
    require_env(REAL_DAEMON);
    let f = fixture();
    let installable = format!("path:{}#tool", f.path().display());
    let out = ok(run(brix_with_store().args([
        "build",
        "--no-link",
        "--print-out-paths",
        &installable,
    ])));
    // The reference nix's own package was most likely substituted: its
    // derivation is not in the store.
    let package = nix_bin()
        .canonicalize()
        .unwrap()
        .ancestors()
        .find(|p| p.parent() == Some(Path::new("/nix/store")))
        .expect("the reference nix lives in /nix/store")
        .display()
        .to_string();
    for path in [out.trim_end(), package.as_str()] {
        let args = ["path-info", "--derivation", path];
        assert_same_as_nix(&run(brix_with_store().args(args)), &nix(&args), path);
    }
}

/// A locked `path:` input whose source is gone is read from the store,
/// where Nix put it, as Nix reads it (`getAccessorUnchecked`).
#[test]
#[ignore = "copies to the store through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn a_locked_path_input_without_its_source_comes_from_the_store() {
    require_env(REAL_DAEMON);
    let t = tempfile::tempdir().unwrap();
    let dep = t.path().join("dep");
    let root = t.path().join("root");
    std::fs::create_dir_all(&dep).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    // New content, so that the tree is not in the store already.
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    std::fs::write(dep.join("a.txt"), &nonce).unwrap();
    std::fs::write(
        root.join("flake.nix"),
        format!(
            "{{ inputs.dep = {{ url = \"path:{}\"; flake = false; }}; outputs = {{ self, dep }}: {{ x = builtins.readFile \"${{dep}}/a.txt\"; p = dep.outPath; }}; }}\n",
            dep.display()
        ),
    )
    .unwrap();
    ok(nix_in(&root, &["flake", "lock"]));
    // Realising the input's path copies it to the store.
    let store_path = ok(nix_in(&root, &["eval", "--raw", ".#p"]));
    ok(nix_in(&root, &["path-info", &store_path]));
    std::fs::remove_dir_all(&dep).unwrap();
    let args = ["eval", "--raw", ".#x"];
    let ours = run(brix_with_store().current_dir(&root).args(args));
    assert_same_as_nix(
        &ours,
        &nix_in(&root, &args),
        "a path input without its source",
    );
    assert_eq!(ours.stdout, nonce);
}

/// `env shell` runs its command with Nix's environment in Nix's order:
/// the variables in name order (`replaceEnv` from a sorted map), `PATH`
/// in its place or, when the flags dropped it, at the end (`setenv`).
#[test]
#[ignore = "runs a program from the real Nix store; set BRIX_TEST_REAL_DAEMON=1"]
fn env_shell_passes_the_environment_in_nixs_order() {
    require_env(REAL_DAEMON);
    // The reference nix's own package: valid in the store.
    let package = nix_bin()
        .canonicalize()
        .unwrap()
        .ancestors()
        .find(|p| p.parent() == Some(Path::new("/nix/store")))
        .expect("the reference nix lives in /nix/store")
        .display()
        .to_string();
    // `env` by its absolute path: the flags may leave it off `PATH`.
    let env_program = std::env::split_paths(&std::env::var_os("PATH").expect("PATH is set"))
        .map(|dir| dir.join("env"))
        .find(|p| p.is_file())
        .expect("env is on PATH")
        .display()
        .to_string();
    let env = |program: PathBuf, flags: &[&str]| {
        let mut cmd = Command::new(program);
        cmd.env_clear()
            .env("ZB", "1")
            .env("HOME", std::env::var_os("HOME").expect("HOME is set"))
            .env("PATH", std::env::var_os("PATH").expect("PATH is set"))
            .env("NIX_CONFIG", ORACLE_NIX_CONFIG)
            .env("AB", "2")
            .args(["shell", &package])
            .args(flags)
            .args(["--command", &env_program]);
        ok(run(&mut cmd))
    };
    for flags in [
        &[][..],
        &[
            "--ignore-env",
            "--keep-env-var",
            "HOME",
            "--keep-env-var",
            "ZB",
            "--set-env-var",
            "CC",
            "3",
        ],
    ] {
        let brix_bin = PathBuf::from(env!("CARGO_BIN_EXE_brix"));
        assert_eq!(env(brix_bin, flags), env(nix_bin(), flags), "{flags:?}");
    }
}

#[test]
#[ignore = "builds through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn build_matches_nix() {
    require_env(REAL_DAEMON);
    let f = fixture();
    let installable = format!("path:{}#tool", f.path().display());
    let ours = tempfile::tempdir().unwrap();
    let theirs = tempfile::tempdir().unwrap();
    let out = run(brix_with_store().current_dir(ours.path()).args([
        "build",
        "--print-out-paths",
        &installable,
    ]));
    assert_eq!(out.status, 0, "{}", out.stderr);
    let expected = ok(nix_in(
        theirs.path(),
        &["build", "--print-out-paths", &installable],
    ));
    assert_eq!(out.stdout, expected);
    assert_eq!(
        std::fs::read_link(ours.path().join("result")).unwrap(),
        std::fs::read_link(theirs.path().join("result")).unwrap()
    );
    // `--json` is Nix's, byte for byte (keys sorted, never pretty), but
    // for the times of the build.
    let times = regex::Regex::new(r#""(cpuSystem|cpuUser|startTime|stopTime)":[0-9.e-]+"#).unwrap();
    let json = |out: Output| times.replace_all(&ok(out), "\"$1\":T").into_owned();
    let args = ["build", "--no-link", "--json", "--pretty", &installable];
    let ours_json = json(run(brix_with_store().args(args)));
    assert_eq!(ours_json, json(nix(&args)));
    let v: serde_json::Value = serde_json::from_str(&ours_json).unwrap();
    assert_eq!(v[0]["outputs"]["out"].as_str().unwrap(), expected.trim());
    // A store path is its own entry.
    let args = ["build", "--no-link", "--json", expected.trim()];
    assert_eq!(json(run(brix_with_store().args(args))), json(nix(&args)));
    // --dry-run of something already built says nothing.
    let out = run(brix_with_store().args(["build", "--dry-run", &installable]));
    assert_eq!(out.status, 0, "{}", out.stderr);
    assert!(!out.stderr.contains("will be built"), "{}", out.stderr);
    // An out-link replaces a link into the store, but nothing else.
    std::fs::write(ours.path().join("plain"), "mine\n").unwrap();
    for (link, what) in [("result", "an old result"), ("plain", "a file")] {
        let args = ["build", "-o", link, &installable];
        let o = brix_store_in(ours.path(), &args);
        let t = nix_in(ours.path(), &args);
        assert_same_as_nix(&o, &t, what);
    }
    assert_eq!(
        std::fs::read_to_string(ours.path().join("plain")).unwrap(),
        "mine\n"
    );
    // The last of `--out-link` and `--no-link` decides.
    for (flags, linked) in [
        (["-o", "LINK", "--no-link"], false),
        (["--no-link", "-o", "LINK"], true),
    ] {
        for who in ["brix", "nix"] {
            let link = format!("{who}-{linked}");
            let mut args = vec!["build"];
            args.extend(
                flags
                    .iter()
                    .map(|&f| if f == "LINK" { link.as_str() } else { f }),
            );
            args.push(&installable);
            let out = if who == "brix" {
                brix_store_in(ours.path(), &args)
            } else {
                nix_in(ours.path(), &args)
            };
            assert_eq!(out.status, 0, "{who} {args:?}: {}", out.stderr);
            assert_eq!(
                ours.path().join(&link).is_symlink(),
                linked,
                "{who} {args:?}"
            );
        }
    }
    // The link is made canonical, its directory created.
    let args = ["build", "-o", "sub/../new/dir/link", &installable];
    ok(brix_store_in(ours.path(), &args));
    assert_eq!(
        std::fs::read_link(ours.path().join("new/dir/link")).unwrap(),
        std::fs::read_link(ours.path().join("result")).unwrap()
    );
    assert!(!ours.path().join("sub").exists());
}

/// `--drv-link` writes the derivations of the result's context to the
/// store and links to them, `-1`, `-2`, … after the first, the link made
/// canonical and its directory created.
#[test]
#[ignore = "builds through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn eval_drv_link_links_like_nix() {
    require_env(REAL_DAEMON);
    let expr = format!(
        r#"let d = n: derivation {{ name = "brix-drv-link-${{n}}"; system = "{}"; builder = "/bin/sh"; }}; in "${{(d "a").drvPath}} ${{(d "b").outPath}}""#,
        brix::host_system()
    );
    let (ours, theirs) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let args = [
        "eval",
        "--raw",
        "--drv-link",
        "sub/../new/link",
        "--expr",
        &expr,
    ];
    let o = brix_store_in(ours.path(), &args);
    let t = nix_in(theirs.path(), &args);
    assert_eq!((o.status, &o.stdout), (t.status, &t.stdout), "{}", o.stderr);
    for link in ["new/link", "new/link-1"] {
        assert_eq!(
            std::fs::read_link(ours.path().join(link)).unwrap(),
            std::fs::read_link(theirs.path().join(link)).unwrap(),
            "{link}"
        );
    }
    assert!(!ours.path().join("new/link-2").exists());
}

/// A failed build is reported as Nix's `throwBuildErrors` does: the
/// installable as Nix names it (`path:…#broken`, not `.#broken` as typed),
/// then the daemon's error laid out under `error: `, and exit status 1.
#[test]
#[ignore = "builds through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn build_failures_are_reported_like_nix() {
    require_env(REAL_DAEMON);
    let flake = flake_with_outputs(&format!(
        r#"packages.{{system}}.broken = derivation {{
      name = "brix-broken-{}";
      system = "{{system}}";
      builder = "/bin/sh";
      args = [ "-c" "echo first line; echo second line; exit 3" ];
    }};"#,
        nonce()
    ));
    // A failed build is not remembered: both build it and fail alike.
    let args = ["build", "--no-link", ".#broken"];
    let ours = brix_store_in(flake.path(), &args);
    let theirs = nix_in(flake.path(), &args);
    assert_eq!(theirs.status, 1, "{}", theirs.stderr);
    assert!(
        ours.stderr.contains(&format!(
            "❌ path:{}#broken\nerror: Cannot build",
            flake.path().display()
        )),
        "{}",
        ours.stderr
    );
    assert_same_as_nix(&ours, &theirs, "a failing build");
}

#[test]
#[ignore = "builds through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn build_dry_run_and_logs() {
    require_env(REAL_DAEMON);
    let nonce = nonce();
    let expr = format!(
        "derivation {{ name = \"brix-cli-test-{nonce}\"; system = \"{}\"; builder = \"/bin/sh\"; args = [ \"-c\" \"echo hello-from-build; echo x > $out\" ]; }}",
        brix::host_system()
    );
    let out = run(brix_with_store().args(["build", "--dry-run", "--expr", &expr]));
    assert_eq!(out.status, 0, "{}", out.stderr);
    assert!(
        out.stderr.contains("this derivation will be built:"),
        "{}",
        out.stderr
    );
    let out = run(brix_with_store().args(["build", "--no-link", "-L", "--expr", &expr]));
    assert_eq!(out.status, 0, "{}", out.stderr);
    // Lines are prefixed with the `DrvName` of the derivation, as by nix.
    assert!(
        out.stderr.contains("brix-cli-test> hello-from-build"),
        "{}",
        out.stderr
    );
}

#[test]
#[ignore = "builds through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn flake_check_builds_checks() {
    require_env(REAL_DAEMON);
    let f = fixture();
    let flake_ref = format!("path:{}", f.path().display());
    let out = run(brix_with_store().args(["flake", "check", &flake_ref]));
    assert_eq!(out.status, 0, "{}", out.stderr);
    assert!(
        out.stderr.contains("✅ checks.x86_64-linux.smoke"),
        "{}",
        out.stderr
    );
}

/// A fresh filtered source must be uploaded before querying missing builds.
/// Run brix first so an oracle evaluation cannot populate the store for it.
#[test]
#[ignore = "builds through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn flake_check_builds_with_filtered_sources() {
    require_env(REAL_DAEMON);
    for source in [
        r#"builtins.path { path = ./src; name = "filtered-NONCE"; filter = p: t: baseNameOf p != "skip"; }"#,
        r#"builtins.filterSource (p: t: baseNameOf p != "skip") ./src"#,
    ] {
        let unique = nonce();
        let outputs = r#"
          checks.{system}.filtered = derivation {
            name = "brix-filtered-check-NONCE";
            system = "{system}";
            builder = "/bin/sh";
            src = SOURCE;
            args = [ "-ec" ''
              test -f "$src/keep"
              test -f "$src/sub/inner"
              test ! -e "$src/skip"
              test ! -e "$src/sub/skip"
              read -r content < "$src/keep"
              test "$content" = "NONCE"
              printf '%s\n' "$content" > "$out"
            '' ];
          };
        "#
        .replace("SOURCE", source)
        .replace("NONCE", &unique);
        let flake = flake_with_outputs(&outputs);
        let src = flake.path().join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("keep"), format!("{unique}\n")).unwrap();
        std::fs::write(src.join("skip"), "excluded").unwrap();
        std::fs::write(src.join("sub/inner"), "included").unwrap();
        std::fs::write(src.join("sub/skip"), "excluded").unwrap();
        let flake_ref = format!("path:{}", flake.path().display());
        let out = run(brix_with_store().args(["flake", "check", &flake_ref]));
        assert_eq!(out.status, 0, "{}", out.stderr);
        assert!(
            out.stderr
                .contains(&format!("✅ checks.{}.filtered", brix::host_system())),
            "{}",
            out.stderr
        );
        let installable = format!(
            "{flake_ref}#checks.{}.filtered.outPath",
            brix::host_system()
        );
        let path = ok(run(brix().args(["eval", "--raw", &installable])));
        assert_eq!(
            std::fs::read_to_string(path.trim()).unwrap(),
            format!("{unique}\n")
        );
    }
}

#[test]
#[ignore = "builds through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn run_and_shell_and_path_info() {
    require_env(REAL_DAEMON);
    // The build sandbox has only `/bin/sh`, so a runnable program comes from
    // nixpkgs (`writeShellScriptBin`, substituted from the binary cache).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        format!(
            r#"{{
  inputs.nixpkgs.url = "path:{}";
  outputs = {{ self, nixpkgs }}: {{
    packages.x86_64-linux.script =
      nixpkgs.legacyPackages.x86_64-linux.writeShellScriptBin "script" "echo script ran with \"$@\"";
  }};
}}
"#,
            nixpkgs()
        ),
    )
    .unwrap();
    let out = run(brix_with_store()
        .current_dir(dir.path())
        .args(["flake", "lock"]));
    assert_eq!(out.status, 0, "{}", out.stderr);
    let out = run(brix_with_store()
        .current_dir(dir.path())
        .args(["run", ".#script", "--", "a", "b"]));
    assert_eq!(out.status, 0, "{}", out.stderr);
    assert_eq!(out.stdout, "script ran with a b\n");
    let out = run(brix_with_store()
        .current_dir(dir.path())
        .args(["shell", ".#script", "-c", "script", "x"]));
    assert_eq!(out.status, 0, "{}", out.stderr);
    assert_eq!(out.stdout, "script ran with x\n");
    let path = ok(run(brix_with_store().current_dir(dir.path()).args([
        "build",
        "--no-link",
        "--print-out-paths",
        ".#script",
    ])));
    let path = path.trim();
    assert_eq!(
        ok(run(brix_with_store().args([
            "path-info",
            "--json",
            "--json-format",
            "2",
            path
        ]))),
        ok(nix(&["path-info", "--json", "--json-format", "2", path]))
    );
    assert_eq!(
        ok(run(brix_with_store().args([
            "path-info",
            "-sSh",
            "--sigs",
            path
        ]))),
        ok(nix(&["path-info", "-sSh", "--sigs", path]))
    );
}

/// The environment options' own mistakes are reported as Nix's flag
/// handlers report them, while the command line is parsed: before any
/// evaluation, so a store path that does not exist is never looked at.
#[test]
fn environment_option_errors_match_nix() {
    let missing = "/nix/store/00000000000000000000000000000000-x";
    for args in [
        &[
            "env",
            "shell",
            "--unset-env-var",
            "A",
            "--set-env-var",
            "A",
            "1",
            missing,
        ][..],
        &[
            "env",
            "shell",
            "--set-env-var",
            "A",
            "1",
            "--unset-env-var",
            "A",
            missing,
        ],
        &[
            "run",
            "--set-env-var",
            "A",
            "1",
            "--set-env-var",
            "A",
            "2",
            missing,
        ],
        &["develop", "-s", "A", "1", "-u", "A", missing],
        &["env", "shell", missing, "--command"],
    ] {
        let ours = run(brix().args(args));
        let theirs = nix(args);
        assert_eq!(theirs.status, 1, "{args:?}: {}", theirs.stderr);
        assert_same_as_nix(&ours, &theirs, &format!("{args:?}"));
    }
}

/// `--ignore-env`, `--keep-env-var`, `--set-env-var` and `--unset-env-var`
/// give `run` and `shell` the environment Nix gives them, and `shell`
/// builds `PATH` from that environment.
#[test]
#[ignore = "builds through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn run_and_shell_environment_match_nix() {
    require_env(REAL_DAEMON);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        format!(
            r#"{{
  inputs.nixpkgs.url = "path:{}";
  outputs = {{ self, nixpkgs }}: let pkgs = nixpkgs.legacyPackages.x86_64-linux; in {{
    packages.x86_64-linux.coreutils = pkgs.coreutils;
    # The same derivation, run as `env`.
    packages.x86_64-linux.env = pkgs.coreutils // {{
      meta = pkgs.coreutils.meta // {{ mainProgram = "env"; }};
    }};
  }};
}}
"#,
            nixpkgs()
        ),
    )
    .unwrap();
    ok(brix_store_in(dir.path(), &["flake", "lock"]));
    // The oracle runs with its own `NIX_CONFIG`; without it the two pass on
    // the same environment.
    let shell = |extra: &[&'static str]| -> Vec<&'static str> {
        let mut args = vec!["shell", ".#coreutils", "--unset-env-var", "NIX_CONFIG"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["-c", "env"]);
        args
    };
    for args in [
        vec![
            "shell",
            ".#coreutils",
            "-i",
            "-k",
            "HOME",
            "-s",
            "A",
            "1",
            "-c",
            "env",
        ],
        // `PATH` comes from the new environment, an empty entry as `.`.
        vec![
            "shell",
            ".#coreutils",
            "-i",
            "-s",
            "PATH",
            "/x::/y",
            "-c",
            "env",
        ],
        shell(&["--unset-env-var", "PATH"]),
        shell(&["--set-env-var", "B", "it's"]),
        vec!["run", ".#env", "--ignore-env", "--set-env-var", "A", "1"],
        vec![
            "run",
            ".#env",
            "--unset-env-var",
            "NIX_CONFIG",
            "--unset-env-var",
            "HOME",
        ],
        // Checked once the installables are built.
        vec![
            "shell",
            ".#coreutils",
            "--keep-env-var",
            "HOME",
            "-c",
            "env",
        ],
        vec![
            "shell",
            ".#coreutils",
            "-i",
            "--unset-env-var",
            "HOME",
            "-c",
            "env",
        ],
    ] {
        // What `env` lists, in order: brix passes the environment in the
        // order Nix's `setenv` calls leave it.
        let ours = brix_store_in(dir.path(), &args);
        let theirs = nix_in(dir.path(), &args);
        assert_same_as_nix(&ours, &theirs, &format!("{args:?}"));
    }
    // A variable that is not UTF-8 is passed on as it is.
    use std::os::unix::ffi::OsStrExt;
    let bytes = std::ffi::OsStr::from_bytes(b"caf\xe9");
    let args = ["shell", ".#coreutils", "-i", "-k", "LATIN1", "-c", "env"];
    let ours = run(brix_with_store()
        .current_dir(dir.path())
        .env("LATIN1", bytes)
        .args(args));
    let theirs: Output = Oracle::new()
        .dir(dir.path())
        .env("LATIN1", bytes)
        .args(args)
        .output()
        .into();
    assert_eq!(ours.status, 0, "{}", ours.stderr);
    assert!(
        ours.stdout.starts_with("LATIN1=caf\u{fffd}\n"),
        "{}",
        ours.stdout
    );
    assert_eq!(ours.stdout, theirs.stdout);
}

/// `path-info`'s listings and every JSON format, for a content-addressed
/// path and for a substituted one with signatures and a closure.
#[test]
#[ignore = "builds through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn path_info_matches_nix() {
    require_env(REAL_DAEMON);
    let text = ok(nix(&[
        "eval",
        "--raw",
        "--expr",
        r#"builtins.toFile "brix-path-info" "x""#,
    ]));
    let coreutils = ok(nix(&[
        "build",
        "--no-link",
        "--print-out-paths",
        &format!("path:{}#coreutils", nixpkgs()),
    ]));
    let coreutils = coreutils.trim();
    let glibc = ok(nix(&["path-info", "-r", coreutils]));
    let glibc = glibc.lines().find(|p| p.contains("-glibc-")).unwrap();
    let f = fixture();
    let tool = format!("path:{}#tool", f.path().display());
    let mut cases: Vec<Vec<&str>> = Vec::new();
    for format in ["1", "2", "3"] {
        for path in [text.as_str(), coreutils] {
            cases.push(vec!["path-info", "--json", "--json-format", format, path]);
        }
        cases.push(vec![
            "path-info",
            "-rS",
            "--json",
            "--json-format",
            format,
            coreutils,
        ]);
    }
    cases.extend([
        // Deprecated, and format 1.
        vec!["path-info", "--json", &text],
        // Each path after those it refers to, whatever the order given.
        vec!["path-info", coreutils, glibc],
        vec!["path-info", "-r", coreutils],
        vec!["path-info", "-rsSh", "--sigs", coreutils],
        vec!["path-info", "--sigs", &text],
        vec![
            "path-info",
            "--derivation",
            "--system",
            "x86_64-linux",
            &tool,
        ],
        vec!["path-info", "--json-format", "4", &text],
        vec!["path-info", "--json-format", "x", &text],
    ]);
    for args in cases {
        // `--system` is for brix, which evaluates for the host otherwise.
        let theirs_args: Vec<&str> = args
            .iter()
            .copied()
            .filter(|a| *a != "--system" && *a != "x86_64-linux")
            .collect();
        let ours = run(brix_with_store().args(&args));
        let theirs = nix(&theirs_args);
        assert_same_as_nix(&ours, &theirs, &format!("{args:?}"));
    }
}

#[test]
#[ignore = "fetches this repository's GitHub inputs; set BRIX_TEST_NETWORK=1"]
fn own_flake_metadata() {
    require_env(NETWORK);
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let got = ok(run(brix().args([
        "flake",
        "metadata",
        "--json",
        repo.to_str().unwrap(),
    ])));
    let json: serde_json::Value = serde_json::from_str(&got).unwrap();
    assert!(json["locks"]["nodes"]["nixpkgs"].is_object());
}

// ---------------------------------------------------------------------
// develop and print-dev-env
// ---------------------------------------------------------------------

/// `brix` with the default store (the daemon) run in `dir`.
fn brix_store_in(dir: &Path, args: &[&str]) -> Output {
    run(brix_with_store().current_dir(dir).args(args))
}

/// A locked flake with a `mkShell` dev shell, a package whose build phase
/// says where it runs, and a derivation with structured attributes, all
/// from the locked nixpkgs. `nonce` makes every derivation new to the
/// store, so that what building prints can be compared.
fn dev_flake(nonce: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        format!(
            r#"{{
  inputs.nixpkgs.url = "path:{nixpkgs}";
  outputs = {{ self, nixpkgs }}: let pkgs = nixpkgs.legacyPackages.x86_64-linux; in {{
    devShells.x86_64-linux.default = pkgs.mkShell {{
      packages = [ pkgs.hello ];
      shellHook = "echo shell hook ran";
      QUOTED = "it's {nonce}";
    }};
    packages.x86_64-linux.where = pkgs.stdenv.mkDerivation {{
      name = "where-{nonce}";
      dontUnpack = true;
      buildPhase = "echo build phase in $PWD";
    }};
    packages.x86_64-linux.structured = pkgs.stdenv.mkDerivation {{
      name = "structured-{nonce}";
      __structuredAttrs = true;
      dontUnpack = true;
      outputs = [ "out" "dev" ];
      list = [ "a" "b c" ];
      outputChecks.out.maxSize = 1000;
    }};
  }};
}}
"#,
            nixpkgs = nixpkgs()
        ),
    )
    .unwrap();
    ok(brix_store_in(dir.path(), &["flake", "lock"]));
    dir
}

/// Unique enough to make a derivation that has never been built, even
/// across tests running at once.
fn nonce() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{nanos}-{}", std::process::id())
}

/// Store hashes and the nonces of [`dev_flake`]s replaced, so that runs
/// on different flakes compare.
fn normalise_build_log(stderr: &str, nonces: &[&str]) -> String {
    let hashes = regex::Regex::new(r"/nix/store/[0-9a-z]{32}-").unwrap();
    let mut s = hashes.replace_all(stderr, "/nix/store/HASH-").into_owned();
    for n in nonces {
        s = s.replace(n, "NONCE");
    }
    s
}

#[test]
#[ignore = "builds through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn print_dev_env_matches_nix() {
    require_env(REAL_DAEMON);
    // Build what the environments need once, so that each tool's first
    // run below builds only its own `-env` derivation.
    let warm = dev_flake(&nonce());
    ok(nix_in(warm.path(), &["print-dev-env", "--json", "."]));
    ok(nix_in(
        warm.path(),
        &["print-dev-env", "--json", ".#structured"],
    ));

    let (ours_nonce, theirs_nonce) = (nonce(), format!("{}b", nonce()));
    let ours = dev_flake(&ours_nonce);
    let theirs = dev_flake(&theirs_nonce);
    let b = brix_store_in(ours.path(), &["print-dev-env", "--json", "."]);
    let n = nix_in(theirs.path(), &["print-dev-env", "--json", "."]);
    assert_eq!(b.status, 0, "{}", b.stderr);
    assert_eq!(
        normalise_build_log(&b.stderr, &[&ours_nonce]),
        normalise_build_log(&n.stderr, &[&theirs_nonce])
    );
    assert!(b.stderr.contains("-nix-shell-env.drv'..."), "{}", b.stderr);

    // On the same flake the output is the same, byte for byte.
    let dir = ours.path();
    for args in [
        vec!["print-dev-env", "--json", "."],
        vec!["print-dev-env", "."],
        vec!["print-dev-env", "--json", ".#structured"],
    ] {
        assert_eq!(
            ok(brix_store_in(dir, &args)),
            ok(nix_in(dir, &args)),
            "{args:?}"
        );
    }
    // The structured attributes go to files in a fresh temporary directory.
    let tmp = regex::Regex::new(r"/[^'\x22]*/nix-dev-env-[^/]+/").unwrap();
    let script = |out: Output| tmp.replace_all(&ok(out), "TMP/").into_owned();
    let args = ["print-dev-env", ".#structured"];
    let ours_script = script(brix_store_in(dir, &args));
    assert!(ours_script.contains("TMP/.attrs.json"), "{ours_script}");
    assert_eq!(ours_script, script(nix_in(dir, &args)));

    // `--redirect` replaces a store path the environment uses.
    let hello = format!("path:{}#hello", nixpkgs());
    let args = ["print-dev-env", ".", "--redirect", &hello, "./hello-dev"];
    let (b, n) = (brix_store_in(dir, &args), nix_in(dir, &args));
    assert_eq!(
        (b.status, &b.stdout, &b.stderr),
        (n.status, &n.stdout, &n.stderr)
    );
    assert!(b.stderr.starts_with("redirecting '"), "{}", b.stderr);
    assert!(b.stdout.contains("/hello-dev"));
}

#[test]
#[ignore = "builds through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn develop_matches_nix() {
    require_env(REAL_DAEMON);
    let flake = dev_flake(&nonce());
    let dir = flake.path();
    // A command sees the environment, its outputs moved under the
    // current directory, and nixpkgs' interactive bash as SHELL.
    let report = r#"echo "$QUOTED|$SHELL|$out|$IN_NIX_SHELL|$NIX_GCROOT"; type -p hello"#;
    let args = ["develop", ".", "--command", "bash", "-c", report];
    let ours = ok(brix_store_in(dir, &args));
    assert_eq!(ours, ok(nix_in(dir, &args)));
    assert!(ours.starts_with("shell hook ran\nit's "), "{ours}");
    assert!(
        ours.contains(&format!("{}/outputs/out|impure|", dir.display())),
        "{ours}"
    );

    // The environment's own store path works as the installable.
    let gcroot = ours.split('|').nth(4).unwrap().lines().next().unwrap();
    assert!(gcroot.ends_with("-nix-shell-env"), "{gcroot}");
    let args = ["print-dev-env", "--json", gcroot];
    assert_eq!(ok(brix_store_in(dir, &args)), ok(nix_in(dir, &args)));

    // A phase runs in the flake's directory, wherever brix was started.
    let sub = dir.join("sub");
    std::fs::create_dir(&sub).unwrap();
    let args = ["develop", "..#where", "--build"];
    let ours = ok(brix_store_in(&sub, &args));
    assert_eq!(ours, ok(nix_in(&sub, &args)));
    assert!(
        ours.contains(&format!("build phase in {}\n", dir.display())),
        "{ours}"
    );

    // `--command` and a phase do not go together (found after building).
    let args = ["develop", ".#where", "--build", "--command", "true"];
    let (b, n) = (brix_store_in(dir, &args), nix_in(dir, &args));
    assert_eq!(b.status, n.status);
    assert_eq!(b.stderr.lines().next(), n.stderr.lines().next());
    assert!(b
        .stderr
        .contains("you cannot use both '--command' and '--phase'"));
}

/// Errors found before anything is built, against the oracle.
#[test]
fn develop_errors_match_nix() {
    let not_bash = r#"derivation { name = "x"; system = "x86_64-linux"; builder = "/bin/sh"; }"#;
    let two = r#"let d = n: derivation { name = n; system = "x86_64-linux"; builder = "/bin/bash"; }; in [ (d "a") (d "b") ]"#;
    for args in [
        vec!["print-dev-env", "--expr", not_bash],
        vec!["develop", "--expr", not_bash],
        vec!["develop", "--expr", two],
    ] {
        let ours = run(brix().args(&args));
        let theirs = nix(&args);
        assert_eq!(ours.status, theirs.status, "{args:?}: {}", ours.stderr);
        assert_eq!(ours.stderr, theirs.stderr, "{args:?}");
    }
    let out = run(brix().args(["develop", "--profile", "p", "--expr", not_bash]));
    assert_eq!(out.status, 1);
    assert!(
        out.stderr
            .contains("'--profile' is not supported: brix does not manage profiles"),
        "{}",
        out.stderr
    );
}

// ---------------------------------------------------------------------
// Compiled flake-input images (brix extension)
// ---------------------------------------------------------------------

/// `flake compile` writes an image per input; evaluating with it gives
/// the same result, and a bad explicit image is a loud error.
#[test]
fn flake_compile_and_use_images() {
    let f = fixture();
    let dep = format!("path:{}", f.path().display());
    let flake = tempfile::tempdir().unwrap();
    std::fs::write(
        flake.path().join("flake.nix"),
        format!("{{ inputs.fx.url = \"{dep}\"; outputs = {{ self, fx }}: {{ v = fx.somethingElse; }}; }}\n"),
    )
    .unwrap();
    ok(run(brix()
        .current_dir(flake.path())
        .args(["flake", "lock"])));
    let plain = ok(run(brix().current_dir(flake.path()).args(["eval", ".#v"])));
    assert_eq!(plain, "42\n");

    let out_dir = tempfile::tempdir().unwrap();
    let out = run(brix().current_dir(flake.path()).args([
        "flake",
        "compile",
        "--json",
        "--out",
        out_dir.path().to_str().unwrap(),
    ]));
    assert_eq!(out.status, 0, "{}", out.stderr);
    let report: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let entries = report.as_array().unwrap();
    let names: Vec<&str> = entries
        .iter()
        .map(|e| e["input"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["self", "fx"], "{report}");
    for e in entries {
        let path = std::path::Path::new(e["path"].as_str().unwrap());
        assert!(path.starts_with(out_dir.path()), "{path:?}");
        assert!(path.exists(), "{path:?}");
        assert!(e["modules"].as_u64().unwrap() >= 1, "{e}");
    }
    let fx_image = entries[1]["path"].as_str().unwrap().to_owned();
    let fx_hash = entries[1]["narHash"].as_str().unwrap().to_owned();

    // Use the image by input name and by hash: same answer, and
    // BRIX_TIMINGS reports that the input came from it.
    for key in ["fx", fx_hash.as_str()] {
        let out = run(brix()
            .current_dir(flake.path())
            .env("BRIX_TIMINGS", "1")
            .args([
                "--compiled-input",
                &format!("{key}={fx_image}"),
                "eval",
                ".#v",
            ]));
        assert_eq!(out.status, 0, "{}", out.stderr);
        assert_eq!(out.stdout, plain);
        assert!(
            out.stderr.contains("served from the compiled image"),
            "{key}: {}",
            out.stderr
        );
    }
    // `--no-compiled-inputs` parses from source.
    let out = run(brix()
        .current_dir(flake.path())
        .env("BRIX_TIMINGS", "1")
        .args(["--no-compiled-inputs", "eval", ".#v"]));
    assert_eq!(out.status, 0, "{}", out.stderr);
    assert_eq!(out.stdout, plain);
    assert!(!out.stderr.contains("compiled image"), "{}", out.stderr);
    // An explicit image that is not an image fails the evaluation.
    let bogus = out_dir.path().join("bogus.brix");
    std::fs::write(&bogus, b"not an image").unwrap();
    let out = run(brix().current_dir(flake.path()).args([
        "--compiled-input",
        &format!("fx={}", bogus.display()),
        "eval",
        ".#v",
    ]));
    assert_eq!(out.status, 1, "{}", out.stdout);
    assert!(
        out.stderr.contains("cannot use the compiled image"),
        "{}",
        out.stderr
    );
    // A malformed flag is a usage error.
    let out = run(brix().args(["--compiled-input", "nixpkgs", "eval", "--expr", "1"]));
    assert_eq!(out.status, 1);
    assert!(out.stderr.contains("NAME=PATH"), "{}", out.stderr);
    // `--input` restricts to named inputs and rejects unknown ones.
    let out = run(brix().current_dir(flake.path()).args([
        "flake",
        "compile",
        "--input",
        "nothere",
        "--out",
        out_dir.path().to_str().unwrap(),
    ]));
    assert_eq!(out.status, 1);
    assert!(
        out.stderr.contains("no locked input 'nothere'"),
        "{}",
        out.stderr
    );
}

/// Without `--keep-going`, `nix flake check` prints `❌ <attr>` for the
/// first failing output, then the error, and stops: no summary warnings
/// and nothing further checked. (One failing output keeps the test
/// independent of nix's parallel visiting order.)
#[test]
fn flake_check_stops_at_first_failure_like_nix() {
    for flake in [
        r#"{ outputs = { self }: { overlays.default = 42; somethingElse = 1; }; }"#,
        // A failure deep inside the evaluation of a derivation output.
        r#"{ outputs = { self }: { packages.x86_64-linux.bad = let f = x: builtins.throw "boom ${x}"; in f "deep"; apps.x86_64-linux.unused = { type = "app"; program = "/bin/true"; }; }; }"#,
    ] {
        let f = fixture();
        std::fs::write(f.path().join("flake.nix"), flake).unwrap();
        let flake_ref = format!("path:{}", f.path().display());
        let ours = run(brix().args(["flake", "check", "--no-build", &flake_ref]));
        let theirs = nix(&["flake", "check", "--no-build", &flake_ref]);
        assert_eq!(ours.status, 1, "{}", ours.stderr);
        assert_eq!(theirs.status, 1, "{}", theirs.stderr);
        assert_eq!(
            without_progress(&ours.stderr),
            without_progress(&theirs.stderr)
        );
    }
}

// ---------------------------------------------------------------------
// Git working trees
// ---------------------------------------------------------------------

/// What the Git tests' flake shows of its own source: everything a
/// working tree's state changes.
const GIT_FLAKE: &str = r#"{
  outputs = { self }: {
    info = {
      path = self.outPath;
      inherit (self) lastModified narHash;
      rev = self.rev or null;
      revCount = self.revCount or null;
      dirtyRev = self.dirtyRev or null;
      dirtyShortRev = self.dirtyShortRev or null;
      files = builtins.readDir self;
      sub = if builtins.pathExists (self + "/sub") then builtins.readDir (self + "/sub") else null;
    };
    untracked = builtins.readFile (self + "/untracked");
    missing = builtins.readFileType (self + "/missing");
  };
}
"#;

/// A Git repository for flake tests ([`brix_testutil::GitRepo`]), with a
/// brix fetch cache of its own next to it (`cache()`) so that what other
/// tests cached cannot leak in.
struct GitRepo(brix_testutil::GitRepo);

impl std::ops::Deref for GitRepo {
    type Target = brix_testutil::GitRepo;
    fn deref(&self) -> &brix_testutil::GitRepo {
        &self.0
    }
}

impl GitRepo {
    fn new() -> GitRepo {
        GitRepo(brix_testutil::GitRepo::new())
    }

    /// The Git test flake with a few files, committed.
    fn flake() -> GitRepo {
        let repo = GitRepo::new();
        repo.write("flake.nix", GIT_FLAKE);
        repo.write("a", "a\n");
        repo.write("sub/b", "b\n");
        repo.write("run.sh", "#!/bin/sh\n");
        repo.commit();
        repo
    }

    /// brix's fetch cache for this repository's tests.
    fn cache(&self) -> PathBuf {
        self.scratch().join("cache")
    }

    /// `brix` and `nix` run `args` (with `{flake}` standing for the
    /// repository) to the same output, messages and exit status.
    fn assert_matches_nix(&self, state: &str, args: &[&str]) {
        let flake = self.path().display().to_string();
        let args: Vec<String> = args.iter().map(|a| a.replace("{flake}", &flake)).collect();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let ours = run(brix().env("XDG_CACHE_HOME", self.cache()).args(&args));
        assert_same_as_nix(&ours, &nix(&args), &format!("{state}: {args:?}"));
    }
}

/// A local checkout fetched by revision is locked with the branch checked
/// out (`getDefaultRef`), whether its tree comes from brix's fetch cache
/// (filled by reading the working tree) or not.
#[test]
fn a_local_checkout_by_rev_records_its_branch_like_nix() {
    let repo = GitRepo::flake();
    let rev = repo.git(&["rev-parse", "HEAD"]);
    let by_rev = format!("git+file://{{flake}}?rev={rev}");
    repo.assert_matches_nix("by rev", &["flake", "metadata", "--json", &by_rev]);
    repo.assert_matches_nix("working tree", &["flake", "metadata", "--json", "{flake}"]);
    repo.assert_matches_nix("by rev, cached", &["flake", "metadata", "--json", &by_rev]);
}

/// A flake in a Git working tree is read as it is on disk, tracked files
/// only, with uncommitted changes included and warned about. Each state
/// is evaluated after the previous one has filled brix's fetch cache, so
/// this also checks that nothing stale comes back from it.
#[test]
fn git_working_tree_states_match_nix() {
    let repo = GitRepo::flake();
    let eval = ["eval", "--json", "{flake}#info"];
    repo.assert_matches_nix("clean", &eval);
    repo.assert_matches_nix("clean, cached", &eval);

    repo.write("a", "edited\n");
    repo.assert_matches_nix("modified", &eval);
    repo.assert_matches_nix(
        "modified, no warning",
        &["eval", "--no-warn-dirty", "--json", "{flake}#info"],
    );
    repo.assert_matches_nix(
        "modified, not allowed",
        &[
            "eval",
            "--option",
            "allow-dirty",
            "false",
            "--json",
            "{flake}#info",
        ],
    );
    repo.commit();
    repo.assert_matches_nix("committed", &eval);

    repo.write("untracked", "u\n");
    repo.assert_matches_nix("untracked file", &eval);
    repo.assert_matches_nix(
        "reading an untracked file",
        &["eval", "--json", "{flake}#untracked"],
    );
    repo.assert_matches_nix(
        "reading a missing file",
        &["eval", "--json", "{flake}#missing"],
    );
    repo.git(&["add", "untracked"]);
    repo.assert_matches_nix("staged new file", &eval);
    repo.git(&["rm", "-q", "--cached", "untracked"]);

    repo.git(&["rm", "-q", "sub/b"]);
    repo.assert_matches_nix("staged deletion of a directory's only file", &eval);
    repo.git(&["reset", "-q"]);
    repo.git(&["checkout", "-q", "sub/b"]);

    std::fs::remove_file(repo.path().join("a")).unwrap();
    repo.assert_matches_nix("deleted from the working tree", &eval);
    repo.git(&["checkout", "-q", "a"]);

    let script = repo.path().join("run.sh");
    let mut mode = std::fs::metadata(&script).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
    std::fs::set_permissions(&script, mode).unwrap();
    repo.assert_matches_nix("made executable", &eval);
}

/// A `flake.nix` Git does not track, or one that is not there, is reported
/// with the Git tree's own message, as Nix 2.35 reports it.
#[test]
fn untracked_or_missing_flake_nix_matches_nix() {
    let repo = GitRepo::new();
    repo.write("flake.nix", GIT_FLAKE);
    repo.assert_matches_nix("no commits", &["eval", "--json", "{flake}#info"]);

    let repo = GitRepo::new();
    repo.write("a", "a\n");
    repo.commit();
    repo.assert_matches_nix("no flake.nix", &["flake", "metadata", "git+file://{flake}"]);
    repo.assert_matches_nix(
        "no flake.nix in dir",
        &["flake", "metadata", "git+file://{flake}?dir=sub"],
    );
}

/// `nix flake metadata` of a flake in a Git working tree: the time of its
/// last commit in the local time zone and its inputs' in UTC, its
/// fingerprint (clean, then with uncommitted changes, whose contents it
/// hashes), and a `locked` reference without a `narHash`, since the
/// command does not need a lockable result. Commands that evaluate still
/// see `self.narHash`.
#[test]
fn git_flake_metadata_matches_nix() {
    // A POSIX time zone string, which needs no time zone database.
    const TZ: &str = "XST-5:30";
    let dep = GitRepo::flake();
    let repo = GitRepo::new();
    repo.write(
        "flake.nix",
        format!(
            r#"{{
  inputs.dep.url = "git+file://{}";
  outputs = {{ self, dep }}: {{
    checks.x86_64-linux.${{if self ? narHash then "hasNarHash" else "noNarHash"}} =
      derivation {{ name = "x"; builder = "/bin/sh"; system = "x86_64-linux"; }};
  }};
}}
"#,
            dep.path().display()
        ),
    );
    repo.write("a", "a\n");
    repo.write("sub/b", "b\n");
    repo.commit();
    // Locking first: both would otherwise write the lock file, and add it
    // to the index, making the tree the other sees dirty.
    let flake = repo.path().display().to_string();
    let status = Command::new(nix_bin())
        .env("NIX_CONFIG", ORACLE_NIX_CONFIG)
        .args(["flake", "lock", &flake])
        .output()
        .expect("run nix flake lock")
        .status;
    assert!(status.success());
    repo.commit();
    let both = |args: &[&str]| {
        let ours = run(brix()
            .env("TZ", TZ)
            .env("XDG_CACHE_HOME", repo.path().join("cache"))
            .args(args));
        let mut theirs = run(Command::new(nix_bin())
            .env("NIX_CONFIG", ORACLE_NIX_CONFIG)
            .env("TZ", TZ)
            .args(args));
        theirs.stderr =
            String::from_utf8_lossy(&brix_testutil::strip_noise(theirs.stderr.as_bytes()))
                .into_owned();
        (ours, theirs)
    };
    let same = |state: &str, args: &[&str]| {
        let (ours, theirs) = both(args);
        assert_same_as_nix(&ours, &theirs, &format!("{state}: {args:?}"));
    };
    let states = |state: &str| {
        same(state, &["flake", "metadata", &flake]);
        same(state, &["flake", "metadata", "--json", &flake]);
        // Nix's progress messages aside.
        let (ours, theirs) = both(&["flake", "show", "--json", &flake]);
        assert_eq!(
            (ours.status, ours.stdout),
            (theirs.status, theirs.stdout),
            "{state}"
        );
    };
    states("clean");
    repo.write("a", "edited\n");
    repo.write("new", "staged\n");
    repo.git(&["add", "new"]);
    std::fs::remove_file(repo.path().join("sub/b")).unwrap();
    states("modified, added and deleted files");
}

/// A gitlink (submodule) is an empty directory when submodules are not
/// asked for, in a working tree (where the submodule's checkout is on
/// disk) and in an exported commit alike, NAR hash included.
#[test]
fn git_submodules_are_empty_directories_as_in_nix() {
    let sub = GitRepo::new();
    sub.write("a", "a\n");
    sub.commit();
    let repo = GitRepo::new();
    repo.write(
        "flake.nix",
        r#"{
  outputs = { self }: {
    info = {
      inherit (self) narHash;
      root = builtins.readDir self;
      sub = builtins.readDir "${self}/sub";
      inside = builtins.pathExists "${self}/sub/a";
    };
  };
}
"#,
    );
    let sub_path = sub.path().display().to_string();
    repo.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "--quiet",
        "add",
        &sub_path,
        "sub",
    ]);
    repo.commit();
    assert!(
        repo.path().join("sub/a").exists(),
        "the submodule is checked out"
    );
    repo.assert_matches_nix("working tree", &["eval", "--json", "{flake}#info"]);
    let rev = repo.git(&["rev-parse", "HEAD"]);
    repo.assert_matches_nix(
        "commit",
        &[
            "eval",
            "--json",
            &format!("git+file://{{flake}}?rev={rev}#info"),
        ],
    );
}

/// A flake reference that is a bare path: a directory without a
/// `flake.nix` is searched upwards with a notice (which `--quiet` twice
/// silences on a terminal and keeps otherwise), `…/flake.nix` names its
/// directory with a warning, and the errors quote paths as `PathFmt` does.
#[test]
fn bare_path_flake_refs_match_nix() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("flake/sub/deeper")).unwrap();
    std::fs::write(
        root.join("flake/flake.nix"),
        "{ outputs = { self }: { }; }\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("none/sub")).unwrap();
    std::fs::create_dir_all(root.join("repo/sub")).unwrap();
    std::fs::create_dir(root.join("repo/.git")).unwrap();
    std::fs::write(root.join("file"), "").unwrap();
    let at = |rel: &str| root.join(rel).display().to_string();
    for args in [
        vec!["flake", "metadata", "--json", &at("flake/sub/deeper")],
        vec!["flake", "metadata", "--json", &at("flake/flake.nix")],
        vec!["flake", "metadata", &at("none/sub")],
        vec!["flake", "metadata", "--quiet", &at("none/sub")],
        vec!["flake", "metadata", &at("repo/sub")],
        vec!["flake", "metadata", &at("file")],
        vec!["flake", "metadata", &at("missing")],
    ] {
        let ours = run(brix().args(&args));
        assert_same_as_nix(&ours, &nix(&args), &format!("{args:?}"));
    }
}

/// A flake that is itself a valid store path: with lazy trees (the
/// oracle's configuration, and Determinate's) it is mounted at a virtual
/// store path, which is never valid, so there is no `Path:` line and no
/// `path` field, though the store is asked.
#[test]
#[ignore = "asks the real Nix daemon about a store path; set BRIX_TEST_REAL_DAEMON=1"]
fn a_flake_in_the_store_has_no_path_with_lazy_trees() {
    require_env(REAL_DAEMON);
    let flake = format!("path:{}", nixpkgs());
    let args = ["flake", "metadata", &flake];
    let ours = run(brix_with_store().args(args));
    assert_same_as_nix(&ours, &nix(&args), "text");
    assert!(!ours.stdout.contains("Path:"), "{}", ours.stdout);
    let args = ["flake", "metadata", "--json", &flake];
    let ours = run(brix_with_store().args(args));
    assert_same_as_nix(&ours, &nix(&args), "JSON");
    let json: serde_json::Value = serde_json::from_str(&ours.stdout).unwrap();
    assert_eq!(json.get("path"), None, "{json}");
}

/// A flake from a tarball: Nix unpacks it into its Git-based tarball cache,
/// so its fingerprint names the tree by its Git object id (executable
/// files, symlinks and empty directories included).
#[test]
fn tarball_flake_metadata_matches_nix() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path().join("src/top");
    std::fs::create_dir_all(top.join("sub")).unwrap();
    std::fs::create_dir_all(top.join("empty")).unwrap();
    std::fs::write(top.join("flake.nix"), "{ outputs = { self }: { }; }\n").unwrap();
    std::fs::write(top.join("sub/f"), "hi\n").unwrap();
    std::fs::write(top.join("run.sh"), "#!/bin/sh\n").unwrap();
    let mut mode = std::fs::metadata(top.join("run.sh")).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
    std::fs::set_permissions(top.join("run.sh"), mode).unwrap();
    std::os::unix::fs::symlink("sub/f", top.join("link")).unwrap();
    let tarball = dir.path().join("t.tar.gz");
    let status = Command::new("tar")
        .arg("-C")
        .arg(dir.path().join("src"))
        .arg("-czf")
        .arg(&tarball)
        .arg("top")
        .status()
        .expect("the `tar` command must be on PATH");
    assert!(status.success());
    let flake_ref = format!("tarball+file://{}", tarball.display());
    // The first run downloads and says so ("unpacking '…' into the Git
    // cache..."), at the `info` level; later ones find the tree in the
    // cache, until `--refresh`; `--quiet` hides the message.
    for args in [
        vec!["flake", "metadata", "--json", &flake_ref],
        vec!["flake", "metadata", &flake_ref],
        vec!["flake", "metadata", "--refresh", &flake_ref],
        vec!["flake", "metadata", "--refresh", "--quiet", &flake_ref],
    ] {
        let ours = run(brix()
            .env("TZ", "UTC")
            .env("XDG_CACHE_HOME", dir.path().join("cache"))
            .args(&args));
        let theirs: Output = Oracle::new()
            .env("TZ", "UTC")
            .env("XDG_CACHE_HOME", dir.path().join("nix-cache"))
            .args(&args)
            .output()
            .into();
        assert_same_as_nix(&ours, &theirs, &format!("{args:?}"));
    }
}

// ---------------------------------------------------------------------
// Lazy trees
// ---------------------------------------------------------------------

/// What a lazily mounted top-level flake shows of its own location: every
/// way a path into it becomes text, is compared or copies the root.
const LAZY_FLAKE: &str = r#"{
  outputs = { self }: {
    t = {
      toStringRoot = toString ./.;
      toStringSub = toString ./sub;
      interpRoot = "${./.}";
      interpSub = "${./sub}";
      outPath = self.outPath;
      narHash = self.narHash;
      eqOutPath = ./. == /. + self.outPath;
      eqSub = ./sub == /. + "${self}/sub";
      dirOfRoot = toString (dirOf ./.);
      baseRoot = baseNameOf ./.;
      up = toString ./..;
      pathDefault = builtins.path { path = ./.; };
      filtered = builtins.path { path = ./sub; name = "f"; filter = p: t: builtins.substring 0 11 p == "/nix/store/"; };
      json = builtins.toJSON { a = ./sub; b = toString ./sub; };
      curPos = __curPos.file;
      ctxOut = builtins.getContext self.outPath;
      viaString = import "${self}/sub/v.nix";
      lt = [ (./a < ./sub) (./. < /tmp) ];
      drvSub = (derivation { name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; src = ./sub; }).drvPath;
      drvSelf = (derivation { name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; src = self; }).drvPath;
    };
  };
}
"#;

/// `nix` without lazy trees: Nix's own lazy trees put a random store path
/// wherever a path into the tree becomes text, so they are no oracle for
/// that; brix computes the real one, as Nix does without them.
fn nix_eager(args: &[&str]) -> Output {
    Oracle::new()
        .config("lazy-trees = false")
        .args(args)
        .output()
        .into()
}

/// A top-level flake whose hash brix does not know (a `path:` directory,
/// a Git tree with changes) is mounted lazily; the results are exactly
/// those of hashing it up front.
#[test]
fn lazily_mounted_flake_matches_nix() {
    let repo = GitRepo::new();
    repo.write("flake.nix", LAZY_FLAKE);
    repo.write("a", "a\n");
    repo.write("sub/v.nix", "\"v\"\n");
    repo.commit();
    repo.write("a", "edited\n");
    let path_flake = format!("path:{}", repo.path().display());
    let git_flake = repo.path().display().to_string();
    let attrs = [
        "toStringRoot",
        "toStringSub",
        "interpRoot",
        "interpSub",
        "outPath",
        "narHash",
        "eqOutPath",
        "eqSub",
        "dirOfRoot",
        "baseRoot",
        "up",
        "pathDefault",
        "filtered",
        "json",
        "curPos",
        "ctxOut",
        "viaString",
        "lt",
        "drvSub",
        "drvSelf",
    ];
    for flake in [&path_flake, &git_flake] {
        for attr in attrs {
            let installable = format!("{flake}#t.{attr}");
            let args = ["eval", "--json", installable.as_str()];
            let ours = run(brix().env("XDG_CACHE_HOME", repo.cache()).args(args));
            let theirs = nix_eager(&args);
            assert_eq!(
                (ours.status, &ours.stdout, &ours.stderr),
                (theirs.status, &theirs.stdout, &theirs.stderr),
                "{installable}"
            );
        }
    }
}

/// Laziness itself: a file nothing asks for is never read. The tree holds
/// a file nobody may read; copying `./app` works without it, as with Nix's
/// lazy trees, while the tree's own store path needs every file.
#[test]
fn lazily_mounted_flake_reads_only_what_it_needs() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let flake = dir.path().join("flake");
    std::fs::create_dir_all(flake.join("app")).unwrap();
    std::fs::write(
        flake.join("flake.nix"),
        r#"{ outputs = { self }: {
             drv = (derivation { name = "app"; system = "x86_64-linux"; builder = "/bin/sh"; src = ./app; }).drvPath;
             out = self.outPath;
           }; }"#,
    )
    .unwrap();
    std::fs::write(flake.join("app/run.sh"), "echo hi\n").unwrap();
    let secret = flake.join("secret");
    std::fs::write(&secret, "no\n").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
    assert!(
        std::fs::read(&secret).is_err(),
        "the test needs a file it cannot read (is it running as root?)"
    );

    let installable = |attr: &str| format!("path:{}#{attr}", flake.display());
    let ours = run(brix()
        .env("XDG_CACHE_HOME", dir.path().join("cache"))
        .args(["eval", "--raw", &installable("drv")]));
    let theirs = nix(&["eval", "--raw", &installable("drv")]);
    assert_eq!(ours.status, 0, "{}", ours.stderr);
    assert_eq!(ours.stdout, ok(theirs));

    let out = run(brix()
        .env("XDG_CACHE_HOME", dir.path().join("cache"))
        .args(["eval", "--raw", &installable("out")]));
    assert_ne!(
        out.status, 0,
        "hashing the tree must read the unreadable file"
    );
    assert!(out.stderr.contains("secret"), "{}", out.stderr);
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).unwrap();
}

/// Paths into a lazy tree that become text keep the tree's hash lazy: the
/// string operations that only need its length, its alphabet or its
/// identity (prefix tests, base names, slicing around it, ordering, turning
/// the text back into a path, path filters) give exactly Nix's results
/// without the tree being hashed, and everything else hashes it.
#[test]
fn lazy_hashes_in_strings_match_nix_and_stay_lazy() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let flake = dir.path().join("flake");
    std::fs::create_dir_all(flake.join("sub")).unwrap();
    std::fs::write(
        flake.join("flake.nix"),
        r#"{
  outputs = { self }: let
    root = toString ./.;
    sub = toString ./sub;
    hasPrefix = pref: str: builtins.substring 0 (builtins.stringLength pref) str == pref;
    removePrefix = pref: str:
      if hasPrefix pref str then builtins.substring (builtins.stringLength pref) (-1) str else str;
  in {
    t = {
      inherit root sub;
      len = builtins.stringLength root;
      remove = removePrefix (root + "/") sub;
      viaString = builtins.readFile "${root}/sub/v.nix";
      exists = builtins.pathExists "${root}/sub/";
      baseSub = baseNameOf sub;
      dir = dirOf sub;
      cut = builtins.substring 5 20 root;
      tail = builtins.substring 44 100 root;
      eqSelf = root == self.outPath;
      eqSame = root == toString ./.;
      lt = root < sub;
      sorted = builtins.sort builtins.lessThan [ sub "/tmp" root "/zzz" ];
      joined = builtins.concatStringsSep ":" [ root sub ];
      hasCtx = builtins.hasContext root;
      ctx = builtins.getContext root;
      discardLen = builtins.stringLength (builtins.unsafeDiscardStringContext root);
      json = builtins.toJSON [ root ];
      hash = builtins.hashString "sha256" root;
      match = builtins.match ".*/(sub)" sub;
      attrName = builtins.attrNames { ${root} = 1; };
      filtered = builtins.path { path = ./.; name = "f"; filter = p: t: baseNameOf p != "secret"; };
      posLen = builtins.stringLength __curPos.file;
      drv = (derivation { name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; src = ./sub; inherit root; }).drvPath;
    };
  };
}"#,
    )
    .unwrap();
    std::fs::write(flake.join("sub/v.nix"), "\"v\"\n").unwrap();
    let secret = flake.join("secret");
    std::fs::write(&secret, "no\n").unwrap();

    let installable = |attr: &str| format!("path:{}#t.{attr}", flake.display());
    let brix_eval = |attr: &str| {
        run(brix()
            .env("XDG_CACHE_HOME", dir.path().join("cache"))
            .args(["eval", "--json", &installable(attr)]))
    };
    let all = [
        "root",
        "sub",
        "len",
        "remove",
        "viaString",
        "exists",
        "baseSub",
        "dir",
        "cut",
        "tail",
        "eqSelf",
        "eqSame",
        "lt",
        "sorted",
        "joined",
        "hasCtx",
        "ctx",
        "discardLen",
        "json",
        "hash",
        "match",
        "attrName",
        "filtered",
        "posLen",
        "drv",
    ];
    let mut expected = std::collections::HashMap::new();
    for attr in all {
        let ours = brix_eval(attr);
        let theirs = nix_eager(&["eval", "--json", &installable(attr)]);
        assert_eq!(
            (ours.status, &ours.stdout, &ours.stderr),
            (theirs.status, &theirs.stdout, &theirs.stderr),
            "{attr}"
        );
        expected.insert(attr, theirs.stdout);
    }

    // The results that do not show the hash need no file the filter or the
    // path skip: an unreadable one does not stop them.
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
    assert!(
        std::fs::read(&secret).is_err(),
        "the test needs a file it cannot read (is it running as root?)"
    );
    for attr in [
        "len",
        "remove",
        "viaString",
        "exists",
        "baseSub",
        "tail",
        "eqSame",
        "lt",
        "hasCtx",
        "discardLen",
        "filtered",
        "posLen",
    ] {
        let ours = brix_eval(attr);
        assert_eq!(ours.status, 0, "{attr} hashed the tree: {}", ours.stderr);
        assert_eq!(ours.stdout, expected[attr], "{attr}");
    }
    let shown = brix_eval("root");
    assert_ne!(shown.status, 0, "showing the root's path needs its hash");
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).unwrap();
}

// ---------------------------------------------------------------------
// fmt, formatter run/build and bundle
// ---------------------------------------------------------------------

/// Writes a flake to `dir` whose outputs are the attribute definitions
/// `outputs`, with `{system}` standing for the host system.
fn write_flake(dir: &Path, outputs: &str) {
    let outputs = outputs.replace("{system}", &brix::host_system());
    std::fs::write(
        dir.join("flake.nix"),
        format!("{{\n  outputs = {{ self }}: {{\n    {outputs}\n  }};\n}}\n"),
    )
    .unwrap();
}

/// A flake in a temporary directory (see [`write_flake`]).
fn flake_with_outputs(outputs: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_flake(dir.path(), outputs);
    dir
}

/// Makes `dir` a flake whose formatter prints `PRJ_ROOT`, its `argv[0]`
/// and its arguments, or with `--env` the environment it was started with,
/// in order (`/proc/$$/environ`: the shell reorders what it exports), but
/// for `NIX_CONFIG`, which the oracle sets for itself. The build
/// sandbox's `/bin/sh` is a bare busybox that
/// can neither create directories nor set the executable bit, so the
/// script lives in the flake's source (whose copy in the store keeps the
/// bit) and Nix's `builtin:buildenv` builder links it into the output.
/// `pname` names the program; `name` can make the derivation new.
fn write_echo_formatter(dir: &Path, name: &str) {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("fmt/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("fmt");
    std::fs::write(
        &script,
        "#!/bin/sh\nif [ \"$1\" = --env ]; then grep -zv '^NIX_CONFIG=' /proc/$$/environ | tr '\\0' '\\n'; exit 0; fi\necho \"PRJ_ROOT=$PRJ_ROOT\"\necho \"argv0=$0\"\nfor a in \"$@\"; do echo \"arg=$a\"; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    write_flake(
        dir,
        &format!(
            r#"formatter.{{system}} = derivation {{
      name = "{name}";
      pname = "fmt";
      system = "{{system}}";
      builder = "builtin:buildenv";
      derivations = [ "true" "5" "1" "${{./fmt}}" ];
      manifest = "/dev/null";
    }};"#
        ),
    );
}

/// `stderr` with the program in usage errors' "Try '<program> --help'"
/// hint made `nix`: the one place where brix's messages name itself.
fn with_nix_as_program(stderr: &str) -> String {
    stderr
        .split('\n')
        .map(|line| match line.strip_prefix("Try '") {
            Some(rest) if rest.ends_with(" --help' for more information.") => {
                "Try 'nix --help' for more information."
            }
            _ => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Of two runs that need the same derivation, only the first builds it and
/// prints `building '…'...`, so comparisons of messages around builds
/// leave those lines out.
fn without_building_lines(stderr: &str) -> String {
    stderr
        .split('\n')
        .filter(|line| !line.starts_with("building '"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Same exit status, output and messages.
fn assert_same_as_nix(ours: &Output, theirs: &Output, what: &str) {
    assert_eq!(
        (
            ours.status,
            ours.stdout.as_str(),
            with_nix_as_program(&ours.stderr)
        ),
        (
            theirs.status,
            theirs.stdout.as_str(),
            with_nix_as_program(&theirs.stderr)
        ),
        "{what}: brix differs from nix"
    );
}

/// Every way of getting the formatter wrong is reported as Nix does, by
/// `fmt`, `formatter run` and `formatter build` alike.
#[test]
fn formatter_errors_match_nix() {
    for (case, outputs) in [
        ("no formatter", ""),
        ("not a set", r#"formatter.{system} = "hi";"#),
        (
            "an app",
            r#"formatter.{system} = { type = "app"; program = "/bin/sh"; };"#,
        ),
        (
            "a failing evaluation",
            r#"formatter.{system} = throw "boom";"#,
        ),
        (
            "a derivation without drvPath",
            r#"formatter.{system} = { type = "derivation"; };"#,
        ),
    ] {
        let dir = flake_with_outputs(outputs);
        for command in [&["fmt"][..], &["formatter", "run"], &["formatter", "build"]] {
            let ours = run(brix().current_dir(dir.path()).args(command));
            let theirs = nix_in(dir.path(), command);
            assert_eq!(theirs.status, 1, "{case}: {}", theirs.stderr);
            assert_same_as_nix(&ours, &theirs, &format!("{case}, {command:?}"));
        }
    }
    // With `--expr`, `.` is an attribute path into the expression: Nix's
    // `fmt` cannot cast that to a flake, `formatter build` evaluates it.
    let dir = flake_with_outputs("");
    for args in [
        &["fmt", "--expr", "{}"][..],
        &["formatter", "run", "--expr", "{}"],
        &["fmt", "--expr", r#"throw "early""#],
        &["formatter", "build", "--expr", "{}"],
        &[
            "formatter",
            "build",
            "--expr",
            r#"{ type = "derivation"; }"#,
        ],
    ] {
        let ours = run(brix().current_dir(dir.path()).args(args));
        let theirs = nix_in(dir.path(), args);
        assert_eq!(theirs.status, 1, "{args:?}: {}", theirs.stderr);
        assert_same_as_nix(&ours, &theirs, &format!("{args:?}"));
    }
}

/// What `nix bundle` packages in these tests.
const BUNDLE_APP: &str = r#"packages.{system}.default = derivation {
      name = "bundle-app";
      system = "{system}";
      builder = "/bin/sh";
      args = [ "-c" "echo app > $out" ];
    };"#;

/// A bundler that wraps the app in a derivation named after it.
const BUNDLER: &str = r#"bundlers.{system}.default = drv: derivation {
      name = "bundled-${drv.name}";
      system = "{system}";
      builder = "/bin/sh";
      args = [ "-c" "echo ${drv} > $out" ];
    };"#;

/// A bundler that is not one, a result that is not a derivation, the
/// installable it is given and the `--bundler` reference are all reported
/// as Nix does, before anything is built.
#[test]
fn bundle_errors_match_nix() {
    let app = flake_with_outputs(BUNDLE_APP);
    let app_ref = format!("path:{}", app.path().display());
    let cwd = tempfile::tempdir().unwrap();
    let check = |args: &[&str], what: &str| {
        let ours = run(brix().current_dir(cwd.path()).args(args));
        let theirs = nix_in(cwd.path(), args);
        assert_eq!(theirs.status, 1, "{what}: {}", theirs.stderr);
        assert_same_as_nix(&ours, &theirs, what);
    };
    for (case, outputs) in [
        (
            "a string",
            r#"bundlers.{system}.default = drv: "not a drv";"#,
        ),
        (
            "no drvPath",
            r#"bundlers.{system}.default = drv: { type = "derivation"; };"#,
        ),
        (
            "a drvPath outside the store",
            r#"bundlers.{system}.default = drv: { type = "derivation"; drvPath = "/tmp/x"; };"#,
        ),
        (
            "a drvPath that is not a derivation",
            r#"bundlers.{system}.default = drv: { type = "derivation"; drvPath = builtins.toFile "x" "y"; };"#,
        ),
        (
            "no outPath",
            r#"bundlers.{system}.default = drv: { type = "derivation"; inherit (drv) drvPath; };"#,
        ),
        ("not a function", r#"bundlers.{system}.default = 42;"#),
        ("no bundlers", ""),
        (
            "a failing bundler",
            r#"bundlers.{system}.default = drv: throw "bundler boom";"#,
        ),
    ] {
        let bundler = flake_with_outputs(outputs);
        let bundler_ref = format!("path:{}", bundler.path().display());
        check(&["bundle", "--bundler", &bundler_ref, &app_ref], case);
    }

    let bundler = flake_with_outputs(r#"bundlers.{system}.default = drv: "not a drv";"#);
    let bundler_ref = format!("path:{}", bundler.path().display());
    // The bundler is named by its fragment as written; an outputs
    // specifier is accepted and ignored.
    for bundler_arg in [
        format!("{bundler_ref}#default"),
        format!("{bundler_ref}#.bundlers.{}.default", brix::host_system()),
        format!("{bundler_ref}^out"),
    ] {
        check(
            &["bundle", "--bundler", &bundler_arg, &app_ref],
            &bundler_arg,
        );
    }
    // The installable: an attribute the flake lacks, store paths (which
    // have no value), and an expression whose broken attribute the bundler
    // never looks at.
    let missing = format!("{app_ref}#nope");
    for (what, installable) in [
        ("a missing attribute", &["--", missing.as_str()][..]),
        (
            "a store path",
            &["/nix/store/00000000000000000000000000000000-x"],
        ),
        (
            "outputs of a store derivation",
            &["/nix/store/00000000000000000000000000000000-x.drv^out,dev"],
        ),
        (
            "an expression",
            &["--expr", r#"{ a = throw "unused"; }"#, "a"],
        ),
    ] {
        let mut args = vec!["bundle", "--bundler", &bundler_ref];
        args.extend_from_slice(installable);
        check(&args, what);
    }
    // The bundler's flake is locked Nix's way whatever the command's own
    // lock options say: a missing lock entry is added in memory, with
    // Nix's warning, and the lock file is not written.
    let dep = flake_with_outputs(r#"value = "from dep";"#);
    let with_input = tempfile::tempdir().unwrap();
    std::fs::write(
        with_input.path().join("flake.nix"),
        format!(
            "{{\n  inputs.dep.url = \"path:{}\";\n  outputs = {{ self, dep }}: {{ bundlers.{}.default = drv: dep.value; }};\n}}\n",
            dep.path().display(),
            brix::host_system()
        ),
    )
    .unwrap();
    let with_input_ref = format!("path:{}", with_input.path().display());
    for flags in [&[][..], &["--no-update-lock-file"]] {
        let mut args = vec!["bundle"];
        args.extend_from_slice(flags);
        args.extend_from_slice(&["--bundler", &with_input_ref, &app_ref]);
        check(&args, &format!("an unlocked bundler input, {flags:?}"));
    }
    assert!(!with_input.path().join("flake.lock").exists());
}

/// `fmt` and `formatter run` build the formatter and execute it with the
/// arguments, `argv[0]` the program and `PRJ_ROOT` the flake's directory.
#[test]
#[ignore = "builds through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn fmt_runs_the_formatter_like_nix() {
    require_env(REAL_DAEMON);
    let dir = tempfile::tempdir().unwrap();
    let flake = dir.path();
    let name = format!("brix-fmt-{}", nonce());
    write_echo_formatter(flake, &name);
    // A new formatter: brix builds it, then runs it.
    let ours = run(brix_with_store()
        .current_dir(flake)
        .args(["fmt", "--", "a", "b c"]));
    assert_eq!(ours.status, 0, "{}", ours.stderr);
    assert!(
        ours.stderr.contains("this derivation will be built:"),
        "{}",
        ours.stderr
    );
    let program = ok(run(brix_with_store().current_dir(flake).args([
        "formatter",
        "build",
        "--no-link",
    ])));
    assert_eq!(
        ours.stdout,
        format!(
            "PRJ_ROOT={}\nargv0={}\narg=a\narg=b c\n",
            flake.display(),
            program.trim_end()
        )
    );
    // Built now, so nothing differs from Nix, messages included.
    // The environment comes in name order (`StringMap`), `PRJ_ROOT` in its
    // place.
    for args in [
        &["fmt", "--", "a", "b c"][..],
        &["fmt"],
        &["formatter", "run", "x", "--", "--check"],
        &["fmt", "--", "--env"],
    ] {
        let ours = run(brix_with_store().current_dir(flake).args(args));
        assert_same_as_nix(&ours, &nix_in(flake, args), &format!("{args:?}"));
    }
    // From a subdirectory PRJ_ROOT is still the flake's directory. (Nix
    // also notes that it searched up for flake.nix, which brix does not.)
    let sub = flake.join("sub");
    std::fs::create_dir(&sub).unwrap();
    let ours = run(brix_with_store()
        .current_dir(&sub)
        .args(["formatter", "run"]));
    let theirs = nix_in(&sub, &["formatter", "run"]);
    assert_eq!(ours.status, 0, "{}", ours.stderr);
    assert_eq!((ours.status, &ours.stdout), (theirs.status, &theirs.stdout));
    assert!(
        ours.stdout
            .starts_with(&format!("PRJ_ROOT={}\n", flake.display())),
        "{}",
        ours.stdout
    );
    // In a Git repository `.` is a `git+file:` reference with `?dir=`, and
    // PRJ_ROOT is the top of the working tree (`Input::getSourcePath`).
    let repo = GitRepo::new();
    let in_repo = repo.path().join("project");
    std::fs::create_dir(&in_repo).unwrap();
    write_echo_formatter(&in_repo, &name);
    repo.commit();
    let ours = run(brix_with_store()
        .env("XDG_CACHE_HOME", repo.cache())
        .current_dir(&in_repo)
        .args(["fmt"]));
    assert_same_as_nix(&ours, &nix_in(&in_repo, &["fmt"]), "a Git repository");
    assert!(
        ours.stdout
            .starts_with(&format!("PRJ_ROOT={}\n", repo.path().display())),
        "{}",
        ours.stdout
    );
    // A formatter that fails to build gets Nix's report: the ❌ line, then
    // the daemon's multi-line error laid out under `error: `. A failed
    // build is not remembered, so both build it and say the same.
    let broken = flake_with_outputs(&format!(
        r#"formatter.{{system}} = derivation {{
      name = "brix-fmt-broken-{}";
      system = "{{system}}";
      builder = "/bin/sh";
      args = [ "-c" "echo first line; echo second line; exit 1" ];
    }};"#,
        nonce()
    ));
    let ours = run(brix_with_store().current_dir(broken.path()).args(["fmt"]));
    let theirs = nix_in(broken.path(), &["fmt"]);
    assert_eq!(theirs.status, 1, "{}", theirs.stderr);
    assert!(
        ours.stderr.contains("\n       > second line\n"),
        "{}",
        ours.stderr
    );
    assert_same_as_nix(&ours, &theirs, "a formatter that fails to build");
}

/// `formatter build` prints the program and links the formatter's output
/// like `nix build`: `result` unless `--out-link` or `--no-link`, the
/// last of which wins.
#[test]
#[ignore = "builds through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn formatter_build_links_like_nix() {
    require_env(REAL_DAEMON);
    let dir = tempfile::tempdir().unwrap();
    let flake = dir.path();
    write_echo_formatter(flake, "brix-fmt");
    let links = tempfile::tempdir().unwrap();
    let link = |name: &str| links.path().join(name).display().to_string();
    let ours =
        run(brix_with_store()
            .current_dir(flake)
            .args(["formatter", "build", "-o", &link("ours")]));
    let theirs = nix_in(flake, &["formatter", "build", "-o", &link("theirs")]);
    assert_eq!(ours.status, 0, "{}", ours.stderr);
    assert_eq!((ours.status, &ours.stdout), (theirs.status, &theirs.stdout));
    assert!(ours.stdout.ends_with("/bin/fmt\n"), "{}", ours.stdout);
    assert_eq!(
        std::fs::read_link(link("ours")).unwrap(),
        std::fs::read_link(link("theirs")).unwrap()
    );
    // Built now, so the messages agree too.
    let ours = run(brix_with_store()
        .current_dir(flake)
        .args(["formatter", "build", "--no-link"]));
    assert_same_as_nix(
        &ours,
        &nix_in(flake, &["formatter", "build", "--no-link"]),
        "--no-link",
    );
    // `result` in the current directory by default.
    let result = flake.join("result");
    assert_eq!(
        ok(run(brix_with_store()
            .current_dir(flake)
            .args(["formatter", "build"]))),
        ours.stdout
    );
    let ours_target = std::fs::read_link(&result).unwrap();
    std::fs::remove_file(&result).unwrap();
    ok(nix_in(flake, &["formatter", "build"]));
    assert_eq!(ours_target, std::fs::read_link(&result).unwrap());
    // The last of `--out-link` and `--no-link` decides.
    for (flags, linked) in [
        (["-o", "LINK", "--no-link"], false),
        (["--no-link", "-o", "LINK"], true),
    ] {
        for who in ["brix", "nix"] {
            let path = link(&format!("{who}-{linked}"));
            let mut args = vec!["formatter", "build"];
            args.extend(
                flags
                    .iter()
                    .map(|&f| if f == "LINK" { path.as_str() } else { f }),
            );
            let out = if who == "brix" {
                run(brix_with_store().current_dir(flake).args(&args))
            } else {
                nix_in(flake, &args)
            };
            assert_eq!(out.status, 0, "{who} {args:?}: {}", out.stderr);
            assert_eq!(Path::new(&path).is_symlink(), linked, "{who} {args:?}");
        }
    }
}

/// `bundle` builds what the bundler returns and links its output under the
/// bundle's name, or `--out-link`, made absolute and canonical; it does not
/// replace a file that is not a link into the store.
#[test]
#[ignore = "builds through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn bundle_builds_and_links_like_nix() {
    require_env(REAL_DAEMON);
    let app = flake_with_outputs(BUNDLE_APP);
    let app_ref = format!("path:{}", app.path().display());
    let bundler = flake_with_outputs(BUNDLER);
    let bundler_ref = format!("path:{}", bundler.path().display());
    let ours = tempfile::tempdir().unwrap();
    let theirs = tempfile::tempdir().unwrap();
    // Whichever runs first builds; brix's `building '…'` lines differ.
    let check_links = |extra: &[&str], link: &str| {
        let mut args = vec!["bundle", "--bundler", &bundler_ref];
        args.extend_from_slice(extra);
        args.push(&app_ref);
        let o = run(brix_with_store().current_dir(ours.path()).args(&args));
        let t = nix_in(theirs.path(), &args);
        assert_eq!(o.status, 0, "{args:?}: {}", o.stderr);
        assert_eq!(
            (o.status, &o.stdout, without_building_lines(&o.stderr)),
            (t.status, &t.stdout, without_building_lines(&t.stderr)),
            "{args:?}"
        );
        let target = std::fs::read_link(ours.path().join(link)).unwrap();
        assert_eq!(
            target,
            std::fs::read_link(theirs.path().join(link)).unwrap()
        );
        assert!(target.starts_with("/nix/store/"), "{}", target.display());
    };
    check_links(&[], "bundled-bundle-app");
    // Again: a link into the store is replaced.
    check_links(&[], "bundled-bundle-app");
    check_links(&["-o", "sub/../mylink"], "mylink");

    // What fails after evaluation, in a directory of its own.
    let clash = tempfile::tempdir().unwrap();
    std::fs::write(clash.path().join("plain"), "mine\n").unwrap();
    let check_failure = |bundler_ref: &str, extra: &[&str], what: &str| {
        let mut args = vec!["bundle", "--bundler", bundler_ref];
        args.extend_from_slice(extra);
        args.push(&app_ref);
        let o = run(brix_with_store().current_dir(clash.path()).args(&args));
        let t = nix_in(clash.path(), &args);
        assert_eq!(t.status, 1, "{what}: {}", t.stderr);
        assert_eq!(
            (o.status, &o.stdout, without_building_lines(&o.stderr)),
            (t.status, &t.stdout, without_building_lines(&t.stderr)),
            "{what}"
        );
    };
    check_failure(&bundler_ref, &["-o", "plain"], "an existing file");
    assert_eq!(
        std::fs::read_to_string(clash.path().join("plain")).unwrap(),
        "mine\n"
    );
    for (what, outputs) in [
        (
            "a failing build",
            format!(
                r#"bundlers.{{system}}.default = drv: derivation {{
      name = "brix-bundle-fails-{}";
      system = "{{system}}";
      builder = "/bin/sh";
      args = [ "-c" "echo failing on purpose; exit 3" ];
    }};"#,
                nonce()
            ),
        ),
        (
            "no name",
            r#"bundlers.{system}.default = drv: { type = "derivation"; inherit (drv) drvPath outPath; };"#
                .to_owned(),
        ),
        (
            "a name with context",
            r#"bundlers.{system}.default = drv: { type = "derivation"; inherit (drv) drvPath outPath; name = "x${drv}"; };"#
                .to_owned(),
        ),
    ] {
        let failing = flake_with_outputs(&outputs);
        check_failure(
            &format!("path:{}", failing.path().display()),
            &[],
            what,
        );
    }
}

#[test]
fn flake_check_parallel_matches_serial_reports() {
    for source in [
        None,
        Some(
            r#"{ outputs = { self }: {
          schemas.custom = { version = 1; doc = "test"; inventory = value: { children = value; }; };
          custom = {
            a.evalChecks.passes = true;
            b.evalChecks.fails = false;
            c.evalChecks.later = throw "later failure";
            other = { forSystems = [ "another-system" ]; evalChecks.unused = throw "filtered"; };
            legacy = { isLegacy = true; evalChecks.unused = throw "legacy"; };
          };
        }; }"#,
        ),
        Some(
            r#"{ outputs = { self }: let
          shared = builtins.foldl' (a: b: a + b) 0 (builtins.genList (x: x) 100);
        in {
          checks.x86_64-linux = builtins.listToAttrs (builtins.genList (i: {
            name = "check-${toString i}";
            value = derivation { name = "check-${toString i}-${toString shared}"; system = "x86_64-linux"; builder = "/bin/true"; };
          }) 20);
        }; }"#,
        ),
        Some(r#"{ outputs = { self }: { overlays.default = 42; nixosModules.bad = "nope"; }; }"#),
    ] {
        let fixture = fixture();
        if let Some(source) = source {
            std::fs::write(fixture.path().join("flake.nix"), source).unwrap();
        }
        let flake_ref = format!("path:{}", fixture.path().display());
        for keep_going in [false, true] {
            let mut args = vec![
                "flake",
                "check",
                "--no-build",
                "--system",
                "x86_64-linux",
                &flake_ref,
            ];
            if keep_going {
                args.push("--keep-going");
            }
            let serial = run(brix().args(&args).args(["--eval-cores", "1"]));
            for cores in ["2", "4"] {
                let parallel = run(brix()
                    .args(&args)
                    .args(["--eval-cores", cores])
                    .env("BRIX_GC_THRESHOLD_BYTES", "4096")
                    .env("BRIX_GC_STEADY_THRESHOLD_BYTES", "4096"));
                assert_eq!(
                    parallel.status, serial.status,
                    "cores={cores}: {}",
                    parallel.stderr
                );
                assert_eq!(parallel.stdout, serial.stdout);
                if keep_going || serial.status == 0 {
                    assert_eq!(
                        without_progress(&parallel.stderr),
                        without_progress(&serial.stderr),
                        "cores={cores}, source={source:?}"
                    );
                } else {
                    // Completion order decides which failure is seen first.
                    // Exactly one failure is reported without --keep-going;
                    // every reported line must belong to the complete report.
                    let complete =
                        run(brix()
                            .args(&args)
                            .args(["--eval-cores", "1", "--keep-going"]));
                    let complete = without_progress(&complete.stderr);
                    let lines = without_progress(&parallel.stderr);
                    assert_eq!(
                        lines.iter().filter(|line| line.starts_with("❌")).count(),
                        1
                    );
                    for line in lines {
                        assert!(complete.contains(&line), "unexpected report line: {line}");
                    }
                }
            }
        }
    }
}

/// `flake check` evaluates on as many threads as Nix's `eval-cores`
/// setting says, wherever it is set, and on the performance cores'
/// threads when it is unset or 0.
#[test]
fn flake_check_takes_its_threads_from_the_eval_cores_setting() {
    let fixture = fixture();
    let flake_ref = format!("path:{}", fixture.path().display());
    let conf_dir = tempfile::tempdir().unwrap();
    // Whether the evaluator with several threads ran: only it reports
    // background evaluation.
    let parallel = |nix_config: &str, flags: &[&str]| {
        let out = run(brix()
            .args(["flake", "check", "--no-build", "--system", "x86_64-linux"])
            .arg(&flake_ref)
            .args(flags)
            .env("NIX_CONF_DIR", conf_dir.path())
            .env("NIX_USER_CONF_FILES", "")
            .env("NIX_CONFIG", nix_config)
            .env("BRIX_TIMINGS", "1"));
        assert_eq!(out.status, 0, "{nix_config:?} {flags:?}: {}", out.stderr);
        out.stderr.contains("background sparks:")
    };
    let several_by_default = brix::performance_threads() > 1;
    assert_eq!(parallel("", &[]), several_by_default);
    assert_eq!(parallel("eval-cores = 0", &[]), several_by_default);
    assert!(parallel("eval-cores = 3", &[]));
    assert!(!parallel("eval-cores = 1", &[]));
    // The command line wins, in either spelling.
    assert!(!parallel(
        "eval-cores = 3",
        &["--option", "eval-cores", "1"]
    ));
    assert!(!parallel("eval-cores = 3", &["--eval-cores", "1"]));
    assert!(parallel("eval-cores = 1", &["--option", "eval-cores", "2"]));
    assert_eq!(
        parallel("eval-cores = 1", &["--eval-cores", "0"]),
        several_by_default
    );
}

#[test]
fn flake_check_parallel_prints_completed_checks_and_keeps_refilling() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::process::{Child, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    // The first check waits for an HTTP response that the test releases only
    // after the last quick check is printed. This proves both incremental
    // reporting and refill beyond the old 4 * workers batch boundary.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let body = format!("released-{}", nonce());
    let hash = brix_store::Hash::of(brix_store::HashAlgo::Sha256, body.as_bytes())
        .to_string(brix_store::HashFormat::Sri, true);
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (release, released) = mpsc::channel();
    let server = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let _ = stream.read(&mut [0; 4096]);
        let _ = released.recv_timeout(Duration::from_secs(20));
        let _ = stream.write_all(response.as_bytes());
    });
    let fixture = fixture();
    let checks: String = (0..12)
        .map(|i| format!("z{i:02}.evalChecks.pass = true;\n"))
        .collect();
    std::fs::write(fixture.path().join("flake.nix"), format!(r#"{{ outputs = {{ self }}: {{
      schemas.custom = {{ version = 1; doc = "streaming"; inventory = value: {{ children = value; }}; }};
      custom = {{
        aSlow.evalChecks.pass = builtins.readFile (builtins.fetchurl {{ url = "http://{address}/slow"; sha256 = "{hash}"; }}) == "{body}";
        {checks}
      }};
    }}; }}"#)).unwrap();
    struct Running(Child);
    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Running(
        brix()
            .args([
                "flake",
                "check",
                "--no-build",
                "--eval-cores",
                "2",
                &format!("path:{}", fixture.path().display()),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stderr = child.0.stderr.take().unwrap();
    let (lines, received) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            if lines.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let mut output = Vec::new();
    loop {
        let line = received
            .recv_timeout(Duration::from_secs(15))
            .unwrap_or_else(|error| {
                panic!(
                    "checks must print before the slow check is released: {error}\n{}",
                    output.join("\n")
                )
            });
        let last = line.contains("✅ custom.z11");
        output.push(line);
        if last {
            break;
        }
    }
    assert!(!output.iter().any(|line| line.contains("✅ custom.aSlow")));
    assert!(child.0.try_wait().unwrap().is_none());
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "check did not finish after releasing the fetch"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    reader.join().unwrap();
    output.extend(received.try_iter());
    server.join().unwrap();
    assert!(status.success(), "{}", output.join("\n"));
    assert!(output.iter().any(|line| line.contains("✅ custom.aSlow")));
}
