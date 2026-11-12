//! `brix flake lock` against the reference `nix flake lock`, on local
//! flakes, comparing the lock files byte for byte and what both print.
//!
//! Every test needs `BRIX_NIX_BIN` (the devshell sets it); all flakes are
//! `path:` or `git+file:` ones, so the comparisons run offline.

use std::path::Path;
use std::process::Command;

use assert_cmd::prelude::*;
use brix_testutil::{git, is_noise, Oracle};

#[derive(Debug, PartialEq)]
struct Outcome {
    status: i32,
    stderr: String,
    /// `flake.lock` afterwards, if there is one.
    lock: Option<String>,
}

/// `cmd` run in `dir`, with the caches (Git clones, fetched trees) in the
/// test's temporary directory rather than the user's.
fn in_dir<'c>(cmd: &'c mut Command, dir: &Path) -> &'c mut Command {
    let tmp = dir
        .parent()
        .expect("test flakes live in a temporary directory");
    cmd.current_dir(dir)
        .env("XDG_CACHE_HOME", tmp.join(".cache"))
}

/// Runs `cmd` in `dir` and collects the outcome, then removes the lock
/// file it wrote so that the next run starts from the same state.
fn outcome(mut cmd: Command, dir: &Path) -> Outcome {
    let out = in_dir(&mut cmd, dir).output().expect("run command");
    let lock_path = dir.join("flake.lock");
    let lock = match std::fs::read_to_string(&lock_path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => panic!("reading {}: {e}", lock_path.display()),
    };
    if lock.is_some() {
        std::fs::remove_file(&lock_path).unwrap();
        // Both add a lock file they write in a Git checkout to the index
        // (`git add --intent-to-add`); it is taken out again.
        if dir.ancestors().any(|d| d.join(".git").exists()) {
            git(
                dir,
                &["rm", "-q", "--cached", "--ignore-unmatch", "flake.lock"],
            );
        }
    }
    Outcome {
        status: out.status.code().unwrap_or(-1),
        stderr: without_noise(&String::from_utf8_lossy(&out.stderr)),
        lock,
    }
}

/// The reference `nix` with brix-testutil's pinned configuration; the
/// tests run it in a directory of their own (see [`outcome`], which drops
/// the noise from its stderr).
fn nix(args: &[&str]) -> Command {
    Oracle::new().args(args).command()
}

fn brix(args: &[&str]) -> Command {
    let mut cmd = Command::cargo_bin("brix").expect("brix binary");
    // Locking local flakes needs no store.
    cmd.arg("--store").arg("dummy://").args(args);
    cmd
}

/// Nix's remarks about the machine it runs on ([`is_noise`]), and the
/// output of the programs it runs, are not part of the comparison.
fn without_noise(stderr: &str) -> String {
    stderr
        .lines()
        .filter(|l| {
            !(is_noise(l.as_bytes())
                // What the `git fetch` Nix runs prints.
                || l.starts_with("remote: ")
                || l.starts_with("From ")
                || l.starts_with(" * ["))
        })
        .map(|l| format!("{l}\n"))
        .collect()
}

/// `nix` and `brix` run with `args` in `dir` (a flake without a lock
/// file) must leave the same lock file, print the same and exit alike.
fn same_as_nix(dir: &Path, args: &[&str]) -> Outcome {
    assert!(!dir.join("flake.lock").exists());
    let ours = outcome(brix(args), dir);
    let theirs = outcome(nix(args), dir);
    assert_eq!(ours, theirs, "brix (left) and nix (right) differ");
    ours
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A flake with no inputs at `dir`.
fn leaf(dir: &Path) {
    write(&dir.join("flake.nix"), "{ outputs = _: { }; }\n");
}

/// The case `flake lock --override-input` got wrong: the root says
/// `hm`'s nixpkgs follows its own, the command line overrides it, and
/// the command line wins.
#[test]
fn command_line_overrides_win_over_follows() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path();
    for name in ["np1", "np2", "np3"] {
        leaf(&p.join(name));
    }
    write(
        &p.join("hm/flake.nix"),
        &format!(
            "{{ inputs.nixpkgs.url = \"path:{}/np1\"; outputs = _: {{ }}; }}\n",
            p.display()
        ),
    );
    write(
        &p.join("root/flake.nix"),
        &format!(
            "{{
  inputs.nixpkgs.url = \"path:{0}/np2\";
  inputs.hm.url = \"path:{0}/hm\";
  inputs.hm.inputs.nixpkgs.follows = \"nixpkgs\";
  outputs = _: {{ }};
}}
",
            p.display()
        ),
    );
    let np3 = format!("path:{}/np3", p.display());
    let res = same_as_nix(
        &p.join("root"),
        &["flake", "lock", "--override-input", "hm/nixpkgs", &np3],
    );
    assert!(res.lock.unwrap().contains("/np3\""));
    // And without the override the follows holds.
    same_as_nix(&p.join("root"), &["flake", "lock"]);
}

/// `inputs.a.inputs.b.inputs.nixpkgs.follows` in the root and
/// `inputs.b.inputs.nixpkgs.follows` in `a`: the root's wins.
#[test]
fn the_outermost_follows_wins() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path();
    for name in ["np1", "np2", "npa"] {
        leaf(&p.join(name));
    }
    let d = p.display();
    write(
        &p.join("b/flake.nix"),
        &format!("{{ inputs.nixpkgs.url = \"path:{d}/np1\"; outputs = _: {{ }}; }}\n"),
    );
    write(
        &p.join("a/flake.nix"),
        &format!(
            "{{ inputs.nixpkgs.url = \"path:{d}/npa\"; inputs.b.url = \"path:{d}/b\"; inputs.b.inputs.nixpkgs.follows = \"nixpkgs\"; outputs = _: {{ }}; }}\n"
        ),
    );
    write(
        &p.join("root/flake.nix"),
        &format!(
            "{{ inputs.nixpkgs.url = \"path:{d}/np2\"; inputs.a.url = \"path:{d}/a\"; inputs.a.inputs.b.inputs.nixpkgs.follows = \"nixpkgs\"; outputs = _: {{ }}; }}\n"
        ),
    );
    same_as_nix(&p.join("root"), &["flake", "lock"]);
}

/// Nested `url` overrides, overrides of inputs that do not exist and
/// command-line flags that match nothing, with Nix's warnings.
#[test]
fn overrides_and_their_warnings() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path();
    let d = p.display();
    leaf(&p.join("np1"));
    leaf(&p.join("np2"));
    write(
        &p.join("dep/flake.nix"),
        &format!("{{ inputs.np.url = \"path:{d}/np1\"; outputs = _: {{ }}; }}\n"),
    );
    write(
        &p.join("root/flake.nix"),
        &format!(
            "{{
  inputs.dep.url = \"path:{d}/dep\";
  inputs.dep.inputs.np.url = \"path:{d}/np2\";
  inputs.dep.inputs.gone.follows = \"dep\";
  outputs = _: {{ }};
}}
"
        ),
    );
    let np1 = format!("path:{d}/np1");
    let res = same_as_nix(
        &p.join("root"),
        &[
            "flake",
            "lock",
            "--override-input",
            "zzz",
            &np1,
            "--override-input",
            "dep/x",
            &np1,
        ],
    );
    assert!(
        res.stderr.contains("does not match any input"),
        "{}",
        res.stderr
    );
    same_as_nix(&p.join("root"), &["flake", "update", "nosuch"]);
}

/// `flake update`'s positionals are input paths (the flake is
/// `--flake`'s), read while the command line is parsed: one that is not
/// an input path, a flake reference for one, is refused with Nix's hint
/// to use `--flake` before the flake is looked for (in a directory that
/// has none here); a good one given twice is warned about first. The
/// flags are read before the positionals wherever they stand, so an input
/// the deprecated `--update-input` names is one a positional repeats,
/// while the flag given twice is not warned about.
#[test]
fn update_takes_input_paths_not_flakes() {
    let t = tempfile::tempdir().unwrap();
    let empty = t.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    let refused: [&[&str]; 6] = [
        &["flake", "update", "path:/x"],
        &["flake", "update", "a", "path:/x"],
        &["flake", "update", ""],
        &["flake", "update", "x x"],
        &["flake", "update", "path:/x", "--update-input", "a"],
        &["flake", "update", "--update-input", "a", "path:/x"],
    ];
    for args in refused {
        let res = same_as_nix(&empty, args);
        assert_eq!(res.status, 1, "{args:?}: {}", res.stderr);
        assert!(
            res.stderr.contains("warning: Invalid flake input"),
            "{}",
            res.stderr
        );
    }
    let flake = t.path().join("flake");
    leaf(&flake);
    let repeated: [&[&str]; 4] = [
        &["flake", "update", "a", "a"],
        &["flake", "update", "--update-input", "a", "a"],
        &["flake", "update", "a", "--update-input", "a"],
        &["flake", "update", "a", "b", "--update-input", "a"],
    ];
    for args in repeated {
        let res = same_as_nix(&flake, args);
        assert!(
            res.stderr
                .contains("warning: Input 'a' was specified multiple times."),
            "{args:?}: {}",
            res.stderr
        );
    }
    let res = same_as_nix(
        &flake,
        &[
            "flake",
            "update",
            "--update-input",
            "a",
            "--update-input",
            "a",
        ],
    );
    assert!(
        !res.stderr.contains("specified multiple times"),
        "{}",
        res.stderr
    );
}

/// Relative inputs, as `path:./…` strings and as path literals that
/// point into the flake's own directory or above it in the repository.
#[test]
fn relative_and_path_literal_inputs() {
    let t = tempfile::tempdir().unwrap();
    let repo = t.path();
    leaf(&repo.join("other"));
    leaf(&repo.join("sub/nested"));
    write(
        &repo.join("sub/flake.nix"),
        "{
  inputs.parent.url = ../other;
  inputs.me = { url = ./.; flake = false; };
  inputs.nested.url = \"path:./nested\";
  outputs = _: { };
}
",
    );
    git(repo, &["init", "-q"]);
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "init"]);
    let res = same_as_nix(&repo.join("sub"), &["flake", "lock"]);
    let lock = res.lock.unwrap();
    assert!(lock.contains("\"path\": \"../other\""), "{lock}");
    assert!(lock.contains("\"path\": \".\""), "{lock}");
}

/// A relative input that climbs out of the tree its flake is in is
/// resolved against where the tree is mounted, as in Nix: a flake there
/// cannot be read in pure mode, and a non-flake input is locked, then
/// fails when `callFlake` takes its store path.
#[test]
fn relative_inputs_that_leave_their_tree() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path();
    leaf(&p.join("c"));
    let flake_ref = format!("path:{}/tree?dir=a#v", p.display());
    for (inputs, value) in [
        (
            "inputs.c = { url = \"path:../../c\"; flake = false; };",
            "c.outPath",
        ),
        (
            "inputs.c = { url = \"path:../../../x/c\"; flake = false; };",
            "c.outPath",
        ),
        ("inputs.c.url = \"path:../../c\";", "c"),
    ] {
        write(
            &p.join("tree/a/flake.nix"),
            &format!("{{ {inputs} outputs = {{ self, c }}: {{ v = {value}; }}; }}\n"),
        );
        let res = same_as_nix(&p.join("tree/a"), &["eval", &flake_ref]);
        assert_eq!(res.status, 1, "{inputs}: {}", res.stderr);
    }
}

/// A failing lock prints the same error, traces outermost first.
#[test]
fn a_missing_input_fails_like_nix() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path();
    write(
        &p.join("root/flake.nix"),
        &format!(
            "{{ inputs.gone.url = \"path:{}/nonexistent\"; outputs = _: {{ }}; }}\n",
            p.display()
        ),
    );
    let res = same_as_nix(&p.join("root"), &["flake", "lock"]);
    assert_eq!(res.status, 1);
    assert!(
        res.stderr
            .starts_with("error:\n       … while updating the lock file of flake"),
        "{}",
        res.stderr
    );
}

/// A Git input with uncommitted changes is unlocked: `flake lock`
/// refuses to write it, `flake update` warns, and `allow-dirty-locks`
/// (read from the settings) lets its `narHash` lock it.
#[test]
fn dirty_inputs_and_allow_dirty_locks() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path();
    leaf(&p.join("dep"));
    git(&p.join("dep"), &["init", "-q"]);
    git(&p.join("dep"), &["add", "-A"]);
    git(&p.join("dep"), &["commit", "-q", "-m", "init"]);
    write(
        &p.join("dep/flake.nix"),
        "{ outputs = _: { dirty = true; }; }\n",
    );
    write(
        &p.join("root/flake.nix"),
        &format!(
            "{{ inputs.dep.url = \"git+file://{}/dep\"; outputs = _: {{ }}; }}\n",
            p.display()
        ),
    );
    let root = p.join("root");
    let res = same_as_nix(&root, &["flake", "lock"]);
    assert_eq!((res.status, res.lock), (1, None));
    let res = same_as_nix(&root, &["flake", "update"]);
    assert_eq!((res.status, res.lock), (0, None));
    let res = same_as_nix(
        &root,
        &["flake", "lock", "--option", "allow-dirty-locks", "true"],
    );
    assert!(res.lock.unwrap().contains("dirtyRev"));
}

/// `--output-lock-file` is written even when nothing changed, and
/// `--no-update-lock-file` refuses a needed change.
#[test]
fn output_lock_file_and_no_update_lock_file() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path();
    leaf(&p.join("dep"));
    write(
        &p.join("root/flake.nix"),
        &format!(
            "{{ inputs.dep.url = \"path:{}/dep\"; outputs = _: {{ }}; }}\n",
            p.display()
        ),
    );
    let root = p.join("root");
    let res = same_as_nix(&root, &["flake", "lock", "--no-update-lock-file"]);
    assert_eq!(res.status, 1);

    // With the lock file in place, `--output-lock-file` still writes.
    let lock = outcome(brix(&["flake", "lock"]), &root).lock.unwrap();
    let outputs: Vec<_> = [nix as fn(&[&str]) -> Command, brix]
        .into_iter()
        .enumerate()
        .map(|(i, run)| {
            std::fs::write(root.join("flake.lock"), &lock).unwrap();
            let out_path = p.join(format!("out{i}.lock"));
            let out = out_path.to_str().unwrap();
            let res = outcome(run(&["flake", "lock", "--output-lock-file", out]), &root);
            assert_eq!(res.status, 0, "{}", res.stderr);
            std::fs::read_to_string(&out_path).unwrap()
        })
        .collect();
    assert_eq!(outputs[0], outputs[1]);
    assert_eq!(outputs[1], lock);
}

/// A build-time input is locked like any other and evaluates to the
/// `builtin:fetch-tree` derivation of `call-flake.nix`.
#[test]
fn build_time_inputs() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path();
    write(&p.join("dep/file"), "hello\n");
    write(
        &p.join("root/flake.nix"),
        &format!(
            "{{
  inputs.dep = {{ url = \"path:{}/dep\"; flake = false; buildTime = true; }};
  outputs = {{ dep, ... }}: {{
    out = dep.outPath;
    drv = dep.sourceInfo.drvPath;
    names = builtins.attrNames dep;
  }};
}}
",
            p.display()
        ),
    );
    let root = p.join("root");
    let feature = ["--extra-experimental-features", "build-time-fetch-tree"];
    // Without the feature, `buildTime` is refused. (Nix's trace also
    // shows the attribute's position, which brix's does not have.)
    let ours = outcome(brix(&["flake", "lock"]), &root);
    let theirs = outcome(nix(&["flake", "lock"]), &root);
    assert_eq!((ours.status, theirs.status), (1, 1));
    assert_eq!(ours.stderr.lines().last(), theirs.stderr.lines().last());
    assert!(
        ours.stderr.contains("build-time-fetch-tree"),
        "{}",
        ours.stderr
    );
    let lock_args = [&["flake", "lock"][..], &feature].concat();
    let res = same_as_nix(&root, &lock_args);
    let lock = res.lock.unwrap();
    assert!(lock.contains("\"buildTime\": true"), "{lock}");
    std::fs::write(root.join("flake.lock"), &lock).unwrap();
    for attr in ["out", "drv", "names"] {
        let installable = format!(".#{attr}");
        let args = [&["eval", "--json", installable.as_str()][..], &feature].concat();
        let theirs = in_dir(&mut nix(&args), &root).output().unwrap();
        let ours = in_dir(&mut brix(&args), &root).output().unwrap();
        assert!(
            theirs.status.success(),
            "{}",
            String::from_utf8_lossy(&theirs.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&ours.stdout),
            String::from_utf8_lossy(&theirs.stdout),
            "{}",
            String::from_utf8_lossy(&ours.stderr)
        );
    }
}

/// Nix always records a `ref` for a Git input: the one given, else the
/// remote's `HEAD` branch (a bare repository is fetched like a remote),
/// else the branch a local checkout has checked out.
#[test]
fn git_inputs_record_the_ref_nix_records() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path();
    let work = p.join("work");
    leaf(&work);
    git(&work, &["init", "-q", "-b", "trunk"]);
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "init"]);
    git(p, &["clone", "-q", "--bare", "work", "bare.git"]);
    let rev = git(&work, &["rev-parse", "HEAD"]);
    let d = p.display();
    write(
        &p.join("root/flake.nix"),
        &format!(
            "{{
  inputs.head.url = \"git+file://{d}/bare.git\";
  inputs.byrev.url = \"git+file://{d}/bare.git?rev={rev}\";
  inputs.local.url = \"git+file://{d}/work?rev={rev}\";
  outputs = _: {{ }};
}}
"
        ),
    );
    let res = same_as_nix(&p.join("root"), &["flake", "lock"]);
    let lock = res.lock.unwrap();
    assert_eq!(
        lock.matches("\"ref\": \"refs/heads/trunk\"").count(),
        3,
        "{lock}"
    );
}

/// A lock file written in a Git checkout is added to the index (`git add
/// --intent-to-add`, `GitInputScheme::putFile`) unless Git ignores it.
#[test]
fn lock_files_written_in_git_checkouts_are_added() {
    for ignored in [false, true] {
        let mut statuses = Vec::new();
        for tool in ["brix", "nix"] {
            let t = tempfile::tempdir().unwrap();
            let repo = t.path().join("repo");
            leaf(&t.path().join("dep"));
            write(
                &repo.join("flake.nix"),
                &format!(
                    "{{ inputs.dep.url = \"path:{}/dep\"; outputs = _: {{ }}; }}\n",
                    t.path().display()
                ),
            );
            if ignored {
                write(&repo.join(".gitignore"), "flake.lock\n");
            }
            git(&repo, &["init", "-q"]);
            git(&repo, &["add", "-A"]);
            git(&repo, &["commit", "-q", "-m", "init"]);
            let mut cmd = if tool == "brix" {
                brix(&["flake", "lock"])
            } else {
                nix(&["flake", "lock"])
            };
            let out = in_dir(&mut cmd, &repo).output().unwrap();
            assert!(out.status.success(), "{tool}: {out:?}");
            assert!(repo.join("flake.lock").exists(), "{tool}");
            statuses.push(git(&repo, &["status", "--porcelain", "--", "flake.lock"]));
        }
        assert_eq!(statuses[0], statuses[1], "ignored: {ignored}");
        // (Trimmed: ` A flake.lock`.)
        let expected = if ignored { "" } else { "A flake.lock" };
        assert_eq!(statuses[0], expected, "ignored: {ignored}");
    }
}

/// Commands that evaluate a flake lock it as Nix's CLI does (Nix's default
/// lock flags, not pure evaluation's): a lock file that lacks an input is
/// completed and written, announced; with `--no-write-lock-file` the lock
/// is kept in memory, with Nix's warning; `--update-input` (deprecated)
/// updates the input it names.
#[test]
fn evaluating_commands_lock_like_nix() {
    let t = tempfile::tempdir().unwrap();
    leaf(&t.path().join("dep"));
    let root = t.path().join("root");
    write(
        &root.join("flake.nix"),
        &format!(
            "{{ inputs.dep.url = \"path:{}/dep\"; outputs = {{ self, dep }}: {{ x = 1; }}; }}\n",
            t.path().display()
        ),
    );
    for args in [
        &["eval", ".#x"][..],
        &["eval", "--no-write-lock-file", ".#x"],
        &["eval", "--update-input", "dep", ".#x"],
        &["flake", "metadata"],
    ] {
        let res = same_as_nix(&root, args);
        assert_eq!(res.status, 0, "{args:?}: {}", res.stderr);
        assert_eq!(
            res.lock.is_some(),
            !args.contains(&"--no-write-lock-file"),
            "{args:?}"
        );
    }
}

/// A Git input's `publicKeys` given as a list in `flake.nix` is its JSON
/// (with `verified-fetches`), which the lock file records; with
/// `verifyCommit = false` nothing is verified, so both fetch the commit.
#[test]
fn public_keys_given_as_a_list_lock_like_nix() {
    let t = tempfile::tempdir().unwrap();
    let repo = t.path().join("repo");
    leaf(&repo);
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let root = t.path().join("root");
    write(
        &root.join("flake.nix"),
        &format!(
            r#"{{
  inputs.r = {{
    type = "git";
    url = "file://{}";
    verifyCommit = false;
    publicKeys = [ {{ type = "ssh-ed25519"; key = "AAAAC3"; }} {{ key = "AAAAC4"; }} ];
  }};
  outputs = _: {{ }};
}}
"#,
            repo.display()
        ),
    );
    let res = same_as_nix(
        &root,
        &[
            "--extra-experimental-features",
            "verified-fetches",
            "flake",
            "lock",
        ],
    );
    assert_eq!(res.status, 0, "{}", res.stderr);
    assert!(
        res.lock
            .as_deref()
            .unwrap_or_default()
            .contains("publicKeys"),
        "{:?}",
        res.lock
    );
}
