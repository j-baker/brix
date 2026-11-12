//! Git inputs whose commits are verified (`verifyCommit`, `publicKey`,
//! `keytype`, `publicKeys`; the `verified-fetches` experimental feature)
//! against the reference `nix`: `builtins.fetchGit` and `fetchTree` of
//! signed and unsigned commits, keys that did not sign, Nix's errors, and
//! flake inputs with keys (`flake lock`).
//!
//! Every test needs `BRIX_NIX_BIN` (the devshell sets it), `git` and
//! `ssh-keygen`, which git runs to sign commits and to check signatures.
//! Both programs run git with no user or system configuration, and with
//! a `TMPDIR` of the test's, whose `nix.XXXXXX` files (the allowed-signers
//! file git's output can name) are masked.

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::prelude::*;
use brix_testutil::{require_env, strip_noise, GitRepo, Oracle, SshKey, REAL_STORE};

/// A repository whose commits are, oldest first: an unsigned one, one
/// signed by `ed` (ed25519), one by `rsa` and one by `other` (ed25519),
/// which is checked out.
struct Fixture {
    repo: GitRepo,
    ed: SshKey,
    rsa: SshKey,
    other: SshKey,
    unsigned: String,
    by_ed: String,
    by_rsa: String,
    by_other: String,
    /// `TMPDIR` for both programs.
    tmp: PathBuf,
}

/// What a run printed, and how it ended.
#[derive(Debug, PartialEq)]
struct Outcome {
    status: i32,
    stdout: String,
    stderr: String,
}

impl Fixture {
    fn new() -> Fixture {
        let repo = GitRepo::new();
        let ed = SshKey::generate(repo.scratch(), "ed", "ed25519");
        let rsa = SshKey::generate(repo.scratch(), "rsa", "rsa");
        let other = SshKey::generate(repo.scratch(), "other", "ed25519");
        repo.write("a", "a\n");
        let unsigned = repo.commit();
        repo.write("b", "b\n");
        let by_ed = repo.commit_signed(&ed);
        repo.write("c", "c\n");
        let by_rsa = repo.commit_signed(&rsa);
        repo.write("d", "d\n");
        let by_other = repo.commit_signed(&other);
        let tmp = repo.scratch().join("tmp");
        std::fs::create_dir(&tmp).unwrap();
        Fixture {
            repo,
            ed,
            rsa,
            other,
            unsigned,
            by_ed,
            by_rsa,
            by_other,
            tmp,
        }
    }

    fn url(&self) -> String {
        format!("file://{}", self.repo.path().display())
    }

    /// The environment both programs run in; `who` names the cache.
    fn environment<'c>(&self, cmd: &'c mut Command, who: &str) -> &'c mut Command {
        cmd.env(
            "XDG_CACHE_HOME",
            self.repo.scratch().join(format!("{who}-cache")),
        )
        .env("TMPDIR", &self.tmp)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
    }

    /// `brix <args>`, with a `dummy://` store.
    fn brix(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("brix").expect("brix binary");
        cmd.args(["--store", "dummy://"]).args(args);
        self.environment(&mut cmd, "brix");
        cmd
    }

    /// The reference `nix <args>`, with a `dummy://` store when `dummy`
    /// (`flake lock` needs a real one).
    fn nix(&self, dummy: bool, args: &[&str]) -> Command {
        let mut oracle = Oracle::new();
        if dummy {
            oracle.dummy_store();
        }
        let mut cmd = oracle.args(args).command();
        self.environment(&mut cmd, "nix");
        cmd
    }

    fn run(&self, cmd: &mut Command) -> Outcome {
        let out = cmd.output().expect("run command");
        let text = |bytes: &[u8]| mask_temp_files(&String::from_utf8_lossy(bytes), &self.tmp);
        Outcome {
            status: out.status.code().unwrap_or(-1),
            stdout: text(&out.stdout),
            stderr: text(&strip_noise(&out.stderr)),
        }
    }

    /// `brix` and `nix` evaluate `expr` alike (with `verified-fetches`
    /// unless `without_feature`): status, stdout, and, when nix fails, its
    /// whole stderr, the error with its traces ("… while fetching the
    /// input …"). A success is not compared by what it prints there.
    fn eval_like_nix(&self, expr: &str, without_feature: bool) -> Outcome {
        let mut args = Vec::new();
        if !without_feature {
            args.extend(["--extra-experimental-features", "verified-fetches"]);
        }
        args.extend(["eval", "--json", "--expr", expr]);
        let mut ours = self.run(&mut self.brix(&args));
        let mut theirs = self.run(&mut self.nix(true, &args));
        if theirs.status == 0 {
            ours.stderr = error_message(&ours.stderr);
            theirs.stderr = error_message(&theirs.stderr);
        }
        assert_eq!(ours, theirs, "{expr}: brix (left) and nix (right) differ");
        ours
    }
}

/// `text` with the name of every `nix.XXXXXX` file in `tmp` masked.
fn mask_temp_files(text: &str, tmp: &Path) -> String {
    let prefix = format!("{}/nix.", tmp.display());
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(&prefix) {
        let end = at + prefix.len();
        out.push_str(&rest[..end]);
        out.push_str("XXXXXX");
        // The random part is six ASCII letters and digits.
        rest = rest.get(end + 6..).unwrap_or("");
    }
    out.push_str(rest);
    out
}

/// From the last line that starts with `error: ` (after indentation) to
/// the end: the error's message, lines of git's output included.
fn error_message(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().collect();
    match lines
        .iter()
        .rposition(|l| l.trim_start().starts_with("error: "))
    {
        Some(at) => lines[at..].join("\n"),
        None => String::new(),
    }
}

/// A key as a URL query parameter.
fn param(key: &str) -> String {
    key.replace('+', "%2B").replace('/', "%2F")
}

fn without_out_path(call: &str) -> String {
    format!("builtins.removeAttrs ({call}) [ \"outPath\" ]")
}

/// Commits signed by a key the input gives are fetched, with each way of
/// giving keys; with `verifyCommit = false` nothing is checked.
#[test]
fn signed_commits_are_fetched_like_nix() {
    let f = Fixture::new();
    let url = f.url();
    let (ed, rsa, other) = (&f.ed.key, &f.rsa.key, &f.other.key);
    for call in [
        format!(
            r#"builtins.fetchGit {{ url = "{url}"; rev = "{}"; exportIgnore = false; publicKey = "{ed}"; }}"#,
            f.by_ed
        ),
        format!(
            r#"builtins.fetchGit {{ url = "{url}"; rev = "{}"; exportIgnore = false; keytype = "ssh-rsa"; publicKey = "{rsa}"; }}"#,
            f.by_rsa
        ),
        format!(
            r#"builtins.fetchGit {{ url = "{url}"; rev = "{}"; exportIgnore = false; verifyCommit = true; publicKeys = [ {{ key = "{ed}"; }} {{ type = "ssh-rsa"; key = "{rsa}"; }} ]; }}"#,
            f.by_rsa
        ),
        format!(
            r#"builtins.fetchTree {{ type = "git"; url = "{url}"; rev = "{}"; publicKeys = [ {{ type = "ssh-ed25519"; key = "{other}"; }} ]; }}"#,
            f.by_other
        ),
        format!(
            r#"builtins.fetchTree "git+{url}?rev={}&publicKey={}""#,
            f.by_ed,
            param(ed)
        ),
        format!(
            r#"builtins.fetchGit {{ url = "{url}"; rev = "{}"; exportIgnore = false; publicKey = "{ed}"; verifyCommit = false; }}"#,
            f.unsigned
        ),
        format!(
            r#"builtins.fetchTree {{ type = "git"; url = "{url}"; rev = "{}"; publicKeys = "[]"; }}"#,
            f.unsigned
        ),
    ] {
        let outcome = f.eval_like_nix(&without_out_path(&call), false);
        assert_eq!(outcome.status, 0, "{call}: {}", outcome.stderr);
    }
}

/// An unsigned commit, a commit another key signed, a key of the wrong
/// type (git's output names the allowed-signers file), a type Nix does
/// not know, a key that is not base64, `verifyCommit` without keys, and
/// keys given as JSON of the wrong shape.
#[test]
fn failed_verifications_fail_like_nix() {
    let f = Fixture::new();
    let url = f.url();
    let (ed, rsa) = (&f.ed.key, &f.rsa.key);
    let fetch = |rev: &str, rest: &str| {
        format!(
            r#"(builtins.fetchGit {{ url = "{url}"; rev = "{rev}"; exportIgnore = false; {rest} }}).rev"#
        )
    };
    for expr in [
        fetch(&f.unsigned, &format!(r#"publicKey = "{ed}";"#)),
        fetch(&f.by_other, &format!(r#"publicKey = "{ed}";"#)),
        fetch(&f.by_rsa, &format!(r#"publicKey = "{rsa}";"#)),
        fetch(
            &f.by_rsa,
            &format!(r#"keytype = "foo"; publicKey = "{rsa}";"#),
        ),
        fetch(
            &f.by_rsa,
            r#"keytype = "ssh-rsa"; publicKey = "AB!!notbase64";"#,
        ),
        fetch(&f.by_ed, "verifyCommit = true;"),
        fetch(&f.by_ed, r#"publicKeys = [ { type = "ssh-rsa"; } ];"#),
        fetch(&f.by_ed, r#"publicKeys = [ "foo" ];"#),
        fetch(&f.by_ed, r#"publicKeys = { key = "x"; };"#),
        fetch(&f.by_ed, "publicKeys = [ { key = 1; } ];"),
        // Not JSON at all: nlohmann's exception, printed without traces.
        fetch(&f.by_ed, r#"publicKeys = "[1, tru]";"#),
        fetch(&f.by_ed, r#"publicKeys = "";"#),
    ] {
        let outcome = f.eval_like_nix(&expr, false);
        assert_eq!(outcome.status, 1, "{expr}");
    }
}

/// A commit is verified on every fetch, also when brix's fetch cache
/// has its tree from an earlier one: once `git` is gone, a fetch that
/// passed before fails, as in Nix (which, with no store path to find the
/// tree by, asks its Git fetcher again).
#[test]
fn commits_are_verified_on_every_fetch_like_nix() {
    let f = Fixture::new();
    let expr = format!(
        r#"(builtins.fetchGit {{ url = "{}"; rev = "{}"; exportIgnore = false; publicKey = "{}"; }}).rev"#,
        f.url(),
        f.by_ed,
        f.ed.key
    );
    assert_eq!(f.eval_like_nix(&expr, false).status, 0);
    let empty = f.repo.scratch().join("empty-path");
    std::fs::create_dir(&empty).unwrap();
    let args = [
        "--extra-experimental-features",
        "verified-fetches",
        "eval",
        "--json",
        "--expr",
        &expr,
    ];
    let ours = f.run(f.brix(&args).env("PATH", &empty));
    let theirs = f.run(f.nix(true, &args).env("PATH", &empty));
    assert_eq!(ours, theirs, "brix (left) and nix (right) differ");
    assert_eq!(ours.status, 1, "{}", ours.stderr);
}

/// The attributes need `verified-fetches`, a list of keys too.
#[test]
fn keys_need_verified_fetches_like_nix() {
    let f = Fixture::new();
    let url = f.url();
    let ed = &f.ed.key;
    for rest in [
        format!(r#"publicKey = "{ed}";"#),
        format!(r#"publicKeys = [ {{ key = "{ed}"; }} ];"#),
        "verifyCommit = false;".to_owned(),
    ] {
        let expr = format!(
            r#"(builtins.fetchGit {{ url = "{url}"; rev = "{}"; exportIgnore = false; {rest} }}).rev"#,
            f.by_ed
        );
        let outcome = f.eval_like_nix(&expr, true);
        assert_eq!(outcome.status, 1, "{expr}");
        assert!(
            outcome.stderr.contains("'verified-fetches' is disabled"),
            "{expr}: {}",
            outcome.stderr
        );
    }
}

/// Without `git` on `PATH`, verification fails with what Nix's child
/// process prints when it cannot run git.
#[test]
fn verification_without_git_fails_like_nix() {
    let f = Fixture::new();
    let empty = f.repo.scratch().join("empty-path");
    std::fs::create_dir(&empty).unwrap();
    let expr = format!(
        r#"(builtins.fetchGit {{ url = "{}"; rev = "{}"; exportIgnore = false; publicKey = "{}"; }}).rev"#,
        f.url(),
        f.by_ed,
        f.ed.key
    );
    let args = [
        "--extra-experimental-features",
        "verified-fetches",
        "eval",
        "--json",
        "--expr",
        &expr,
    ];
    // The whole of stderr, the error's traces included.
    let ours = f.run(f.brix(&args).env("PATH", &empty));
    let theirs = f.run(f.nix(true, &args).env("PATH", &empty));
    assert_eq!(ours, theirs, "brix (left) and nix (right) differ");
    assert!(
        ours.stderr
            .contains("error: executing \"git\": No such file or directory"),
        "{}",
        ours.stderr
    );
}

/// A flake whose inputs give keys: `flake lock` in a directory of its
/// own, the lock file it writes (removed afterwards) and all it prints.
struct Locking<'f> {
    fixture: &'f Fixture,
    dir: PathBuf,
}

impl Locking<'_> {
    fn new<'f>(fixture: &'f Fixture, name: &str, inputs: &str) -> Locking<'f> {
        let dir = fixture.repo.scratch().join(name);
        std::fs::create_dir(&dir).unwrap();
        let names: Vec<&str> = inputs
            .lines()
            .filter_map(|l| l.trim().strip_prefix("inputs."))
            .filter_map(|l| l.split([' ', '.']).next())
            .collect();
        std::fs::write(
            dir.join("flake.nix"),
            format!(
                "{{\n{inputs}\n  outputs = {{ self, {} }}: {{ }};\n}}\n",
                names.join(", ")
            ),
        )
        .unwrap();
        Locking { fixture, dir }
    }

    /// `flake lock` by `cmd`: the outcome and the lock file it wrote.
    fn lock(&self, mut cmd: Command) -> (Outcome, Option<String>) {
        let outcome = self.fixture.run(cmd.current_dir(&self.dir));
        let path = self.dir.join("flake.lock");
        let lock = match std::fs::read_to_string(&path) {
            Ok(text) => {
                std::fs::remove_file(&path).unwrap();
                Some(text)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => panic!("reading {}: {e}", path.display()),
        };
        (outcome, lock)
    }

    /// `brix flake lock` and `nix flake lock` do the same.
    fn same_as_nix(&self) -> (Outcome, Option<String>) {
        let args = [
            "--extra-experimental-features",
            "verified-fetches",
            "flake",
            "lock",
        ];
        let ours = self.lock(self.fixture.brix(&args));
        let theirs = self.lock(self.fixture.nix(false, &args));
        assert_eq!(ours, theirs, "brix (left) and nix (right) differ");
        ours
    }
}

/// Inputs that give keys as a list, as one key, or with `verifyCommit =
/// false` lock like Nix's: the keys as given go into the lock file.
#[test]
fn flake_inputs_with_keys_lock_like_nix() {
    let f = Fixture::new();
    let inputs = format!(
        r#"  inputs.signed = {{
    type = "git";
    url = "{url}";
    rev = "{by_rsa}";
    flake = false;
    publicKeys = [ {{ key = "{ed}"; }} {{ type = "ssh-rsa"; key = "{rsa}"; }} ];
  }};
  inputs.head = {{
    type = "git";
    url = "{url}";
    flake = false;
    publicKey = "{other}";
  }};
  inputs.unchecked = {{
    type = "git";
    url = "{url}";
    ref = "main";
    rev = "{unsigned}";
    flake = false;
    publicKey = "{ed}";
    verifyCommit = false;
  }};"#,
        url = f.url(),
        by_rsa = f.by_rsa,
        unsigned = f.unsigned,
        ed = f.ed.key,
        rsa = f.rsa.key,
        other = f.other.key,
    );
    let (outcome, lock) = Locking::new(&f, "keys", &inputs).same_as_nix();
    assert_eq!(outcome.status, 0, "{}", outcome.stderr);
    assert!(lock.is_some_and(|l| l.contains("publicKeys")));
}

/// A working tree whose `HEAD` another key signed, and a dirty one (which
/// Nix warns about twice before refusing it).
#[test]
fn flake_inputs_that_fail_verification_fail_like_nix() {
    let f = Fixture::new();
    let input = |key: &str| {
        format!(
            r#"  inputs.work = {{
    type = "git";
    url = "{}";
    flake = false;
    publicKey = "{key}";
  }};"#,
            f.url()
        )
    };
    let (outcome, lock) = Locking::new(&f, "wrong-key", &input(&f.ed.key)).same_as_nix();
    assert_eq!(outcome.status, 1, "{}", outcome.stderr);
    assert!(lock.is_none());
    assert!(
        outcome.stderr.contains("No principal matched."),
        "{}",
        outcome.stderr
    );

    f.repo.write("a", "changed\n");
    let (outcome, lock) = Locking::new(&f, "dirty", &input(&f.other.key)).same_as_nix();
    assert_eq!(outcome.status, 1, "{}", outcome.stderr);
    assert!(lock.is_none());
    assert!(
        outcome.stderr.contains("but it's dirty"),
        "{}",
        outcome.stderr
    );
    assert_eq!(
        outcome.stderr.matches("has uncommitted changes").count(),
        2,
        "{}",
        outcome.stderr
    );
}

/// An input of `flake.nix` is not final, so even with the `narHash` of a
/// tree the store has, Nix asks its Git fetcher, which verifies the
/// commit; and keys that are not JSON fail with nlohmann's exception
/// alone. The tree is put in the user's store first.
#[test]
#[ignore = "adds a source tree to the user's Nix store; set BRIX_TEST_REAL_STORE=1"]
fn flake_inputs_with_a_nar_hash_are_verified_like_nix() {
    require_env(REAL_STORE);
    let f = Fixture::new();
    // The tree's `narHash`, and the tree in the oracle's store.
    let nar_hash = Oracle::new()
        .args([
            "eval",
            "--raw",
            "--option",
            "lazy-trees",
            "false",
            "--expr",
            &format!(
                r#"(builtins.fetchTree {{ type = "git"; url = "{}"; rev = "{}"; }}).narHash"#,
                f.url(),
                f.unsigned
            ),
        ])
        .stdout();
    let input = |rest: &str| {
        format!(
            r#"  inputs.x = {{
    type = "git";
    url = "{}";
    rev = "{}";
    flake = false;
    {rest}
  }};"#,
            f.url(),
            f.unsigned
        )
    };
    let with_hash = input(&format!(
        r#"narHash = "{nar_hash}"; publicKey = "{}";"#,
        f.ed.key
    ));
    let (outcome, lock) = Locking::new(&f, "nar-hash", &with_hash).same_as_nix();
    assert_eq!(outcome.status, 1, "{}", outcome.stderr);
    assert!(lock.is_none());
    assert!(
        outcome.stderr.contains("Commit signature verification"),
        "{}",
        outcome.stderr
    );

    let not_json = input(r#"publicKeys = "[1, tru]";"#);
    let (outcome, lock) = Locking::new(&f, "not-json", &not_json).same_as_nix();
    assert_eq!(outcome.status, 1, "{}", outcome.stderr);
    assert!(lock.is_none());
    assert!(
        outcome
            .stderr
            .starts_with("error: [json.exception.parse_error.101]"),
        "{}",
        outcome.stderr
    );
}
