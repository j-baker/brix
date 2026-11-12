//! Commit signature verification for `git` inputs: `verifyCommit`,
//! `publicKey` (with `keytype`) and `publicKeys`, all behind the
//! `verified-fetches` experimental feature (`GitInputScheme::verifyCommit`
//! in `libfetchers/git.cc`, `GitRepoImpl::verifyCommit` in
//! `libfetchers/git-utils.cc`).
//!
//! Like Nix, brix has `git verify-commit` check the commit against an
//! allowed-signers file that lists the keys (`gpg.ssh.allowedSignersFile`,
//! so only SSH signatures can pass), and then looks for git's "Good
//! signature" line in what it printed. Nix's quirks are kept, as they
//! decide what passes and what the errors say:
//!
//! - its key types map to OpenSSH's names, except `ssh-ecdsa` and
//!   `ssh-dsa`, which stay as they are and which OpenSSH does not accept
//!   ("invalid key" in git's output);
//! - the keys' fingerprints go into the regular expression between `[`
//!   and `]`, a character class: git's "Good signature" line passes when
//!   the first character of the fingerprint it shows is one of theirs (or
//!   a parenthesis);
//! - git's output, stdout and stderr together, is the error message, and
//!   a `git` that cannot be run fails with the message Nix's child process
//!   prints then.
//!
//! When: on every fetch of a commit, once its revision is known and before
//! its tree is read, and on every fetch of a clean working tree (its
//! `HEAD`); a dirty working tree, which has no commit, fails. Nix skips it
//! only when it does not ask the fetcher at all (`getAccessorUnchecked`):
//! for an input with its own `narHash` whose store path is valid. So brix's
//! fetch cache never answers for such an input
//! ([`super::NativeFetcher`]'s `fetch` and `resolve_cached`): its commit is
//! read from the local checkout, or from the clone at the revision the
//! cache resolved its `ref` to, and verified again.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use brix_store::{Hash, HashAlgo, HashFormat};

use crate::attrs::maybe_get_bool_attr;
use crate::hashes::GitRev;
use crate::input::{git_public_keys, Input, PublicKey, Scheme};
use crate::{err, Result};

/// The key types Nix accepts, each with the name OpenSSH's allowed-signers
/// file gets for it, in the order Nix lists them when a key has another
/// type (the iteration order of its `boost::unordered_flat_map`, as the
/// reference Nix prints it).
const KEY_TYPES: [(&str, &str); 6] = [
    ("ssh-ed25519-sk", "sk-ssh-ed25519@openssh.com"),
    ("ssh-rsa", "ssh-rsa"),
    ("ssh-dsa", "ssh-dsa"),
    ("ssh-ecdsa", "ssh-ecdsa"),
    ("ssh-ecdsa-sk", "sk-ecdsa-sha2-nistp256@openssh.com"),
    ("ssh-ed25519", "ssh-ed25519"),
];

/// Whether fetching `input` verifies its commit: a Git input's
/// `verifyCommit`, which defaults to whether the input gives any keys.
pub fn wants_verification(input: &Input) -> Result<bool> {
    if input.scheme != Some(Scheme::Git) {
        return Ok(false);
    }
    let keys = git_public_keys(&input.attrs)?;
    Ok(maybe_get_bool_attr(&input.attrs, "verifyCommit")?.unwrap_or(!keys.is_empty()))
}

/// `GitInputScheme::verifyCommit(input, repo)` for a commit: the one
/// `input`'s `rev` names, in the repository at `repo` (a local checkout,
/// or the clone a remote one was fetched into). Nothing to do unless the
/// input [`wants_verification`].
pub fn verify(input: &Input, repo: &Path) -> Result<()> {
    if !wants_verification(input)? {
        return Ok(());
    }
    match input.rev()? {
        Some(rev) => verify_commit(
            OsStr::new("git"),
            repo,
            &rev,
            &git_public_keys(&input.attrs)?,
        ),
        None => Err(dirty_error(input)),
    }
}

/// `GitInputScheme::verifyCommit(input, nullptr)` for a dirty working tree
/// at `path`, which has no commit: an error when the input
/// [`wants_verification`].
///
/// The caller has just warned that the tree is dirty (`warnDirty`). Nix
/// warns twice before this error: `Input::getAccessorUnchecked` asks the
/// fetcher first in `fastOnly` mode, drops whatever that call throws, and
/// then asks again, and a working tree is read the same way both times;
/// `native::git::fetch_workdir` reads it twice too.
pub fn verify_dirty(input: &Input) -> Result<()> {
    if !wants_verification(input)? {
        return Ok(());
    }
    Err(dirty_error(input))
}

fn dirty_error(input: &Input) -> crate::Error {
    err!(
        "commit verification is required for Git repository '{}', but it's dirty",
        input
    )
}

/// `GitRepoImpl::verifyCommit`: `program` (`git`) checks that `rev`, in
/// the repository at `repo`, is signed by one of `keys`.
fn verify_commit(program: &OsStr, repo: &Path, rev: &GitRev, keys: &[PublicKey]) -> Result<()> {
    // Nix makes the file before it looks at the keys.
    let allowed_signers = allowed_signers_file()?;
    let mut lines = String::new();
    for key in keys {
        let Some((_, openssh_type)) = KEY_TYPES.iter().find(|(name, _)| *name == key.key_type)
        else {
            let supported: String = KEY_TYPES
                .iter()
                .map(|(name, _)| format!("  {name}\n"))
                .collect();
            return Err(err!(
                "Invalid SSH key type '{}' in publicKeys.\nPlease use one of:\n{}",
                key.key_type,
                magenta(&supported)
            ));
        };
        lines.push_str(&format!("* {openssh_type} {}\n", key.key));
    }
    allowed_signers
        .as_file()
        .write_all(lines.as_bytes())
        .map_err(|e| {
            sys_error(
                &format!("writing to file '{}'", allowed_signers.path().display()),
                &e,
            )
        })?;

    let (succeeded, output) = run_verify_commit(program, allowed_signers.path(), repo, rev)?;
    // The keys are decoded after git has run, so an undecodable key is
    // the error whatever git said.
    let good = good_signature(keys)?;
    if succeeded && good.is_some_and(|re| re.is_match(&output)) {
        // Nix says so at the `talkative` level only.
        Ok(())
    } else {
        Err(err!(
            "Commit signature verification on commit {} failed: {}",
            rev,
            magenta(&String::from_utf8_lossy(&output))
        ))
    }
}

/// `createTempFile()`: an empty file `nix.XXXXXX` in `$TMPDIR` (or
/// `/tmp`), whose name git's output can quote ("…/nix.XXXXXX:1: invalid
/// key"). Nix leaves it behind; brix removes it when done.
fn allowed_signers_file() -> Result<tempfile::NamedTempFile> {
    let dir = default_temp_dir();
    tempfile::Builder::new()
        .prefix("nix.")
        .rand_bytes(6)
        .tempfile_in(&dir)
        .map_err(|e| {
            sys_error(
                &format!(
                    "creating temporary file '{}'",
                    dir.join("nix.XXXXXX").display()
                ),
                &e,
            )
        })
}

/// `defaultTempDir`: `$TMPDIR` unless unset or empty, else `/tmp`.
fn default_temp_dir() -> PathBuf {
    match std::env::var_os("TMPDIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from("/tmp"),
    }
}

/// `runProgram` of `<program> -c gpg.ssh.allowedSignersFile=<file> -C
/// <repo> verify-commit <rev>`, stderr merged into stdout: whether it
/// exited with status 0, and what it printed.
///
/// A program that cannot be started is a failure too, with the output
/// Nix's forked child prints when its `exec` fails.
fn run_verify_commit(
    program: &OsStr,
    allowed_signers: &Path,
    repo: &Path,
    rev: &GitRev,
) -> Result<(bool, Vec<u8>)> {
    // Both streams go to one file, through one open file description and so
    // at one offset: they interleave as in Nix's pipe, and the output is
    // read once git has exited, with no pipe to drain meanwhile.
    let mut output = tempfile::tempfile().map_err(|e| sys_error("creating temporary file", &e))?;
    let stream = || {
        output
            .try_clone()
            .map_err(|e| sys_error("duplicating a file descriptor", &e))
    };
    let mut setting = OsString::from("gpg.ssh.allowedSignersFile=");
    setting.push(allowed_signers);
    let status = Command::new(program)
        .arg("-c")
        .arg(&setting)
        .arg("-C")
        .arg(repo)
        .arg("verify-commit")
        .arg(rev.to_hex())
        .stdin(Stdio::null())
        .stdout(stream()?)
        .stderr(stream()?)
        .status();
    let status = match status {
        Ok(status) => status,
        // `SysError("executing %s", PathFmt(program))` (the name in double
        // quotes, unescaped), as the child prints it before exiting with
        // status 1.
        Err(e) => {
            let message = format!(
                "error: executing \"{}\": {}\n",
                program.to_string_lossy(),
                strerror(&e)
            );
            return Ok((false, message.into_bytes()));
        }
    };
    let mut printed = Vec::new();
    output
        .rewind()
        .and_then(|()| output.read_to_end(&mut printed))
        .map_err(|e| sys_error("reading the output of 'git verify-commit'", &e))?;
    Ok((status.success(), printed))
}

/// The regular expression Nix's `verifyCommit` looks for in git's output:
/// `Good "git" signature for \* with .* key SHA256:[<fingerprints>]`, each
/// fingerprint (the key's SHA-256 in base64, without padding) in
/// parentheses. Between the brackets that is one character class, which
/// is kept. `None` for no keys: ECMAScript's `[]` matches nothing.
///
/// A key that is not base64 is an error (`base64::decode`), traced as Nix
/// traces it.
fn good_signature(keys: &[PublicKey]) -> Result<Option<regex::bytes::Regex>> {
    let mut class = BTreeSet::new();
    for key in keys {
        let decoded = brix_store::hash::base64::decode(key.key.as_str()).ok_or_else(|| {
            // `FormatError("invalid character in Base64 string: '%c'", c)`:
            // `%c` keeps the first character of the argument as Nix
            // formats it, the escape that starts its colour, and printing
            // drops that.
            err!("invalid character in Base64 string: ''").trace(format!(
                "while decoding public key '{}' used for git signature",
                key.key
            ))
        })?;
        let fingerprint = Hash::of(HashAlgo::Sha256, &decoded).to_string(HashFormat::Base64, false);
        class.insert('(');
        class.insert(')');
        // Nix escapes `+` as `\+`, which in a class is `+`.
        class.extend(fingerprint.trim_matches('=').chars());
    }
    if class.is_empty() {
        return Ok(None);
    }
    let class: String = class
        .iter()
        .map(|c| regex::escape(&c.to_string()))
        .collect();
    // ECMAScript's `.` matches neither `\n` nor `\r`.
    let pattern = format!(r#"Good "git" signature for \* with [^\n\r]* key SHA256:[{class}]"#);
    Ok(Some(
        regex::bytes::Regex::new(&pattern).expect("the signature pattern is a valid regex"),
    ))
}

/// What Nix's error formatting (`HintFmt`) makes of an argument:
/// `ANSI_MAGENTA` before it and `ANSI_NORMAL` after. The codes are dropped
/// when the error is printed, but the second one keeps the line it is on
/// from being chomped: an argument that ends in a newline leaves a line of
/// indentation after it.
fn magenta(text: &str) -> String {
    format!("\x1b[35;1m{text}\x1b[0m")
}

/// An OS error as Nix's `SysError` shows it: the context, then
/// `strerror`.
fn sys_error(context: &str, e: &std::io::Error) -> crate::Error {
    err!("{}: {}", context, strerror(e))
}

/// `strerror`: the error's text without the ` (os error N)` Rust adds.
fn strerror(e: &std::io::Error) -> String {
    let text = e.to_string();
    match e.raw_os_error() {
        Some(n) => text
            .strip_suffix(&format!(" (os error {n})"))
            .unwrap_or(&text)
            .to_owned(),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::Fetcher;
    use crate::input::FetchSettings;
    use crate::native::NativeFetcher;
    use brix_testutil::{git, GitRepo, SshKey};
    use std::sync::{Arc, Mutex};

    fn key(key_type: &str, key: &str) -> PublicKey {
        PublicKey {
            key_type: key_type.to_owned(),
            key: key.to_owned(),
        }
    }

    fn public(k: &SshKey) -> PublicKey {
        key(&k.openssh_type, &k.key)
    }

    fn settings() -> FetchSettings {
        let mut settings = FetchSettings::default();
        settings
            .experimental_features
            .insert("verified-fetches".into());
        settings
    }

    /// A repository with an unsigned commit, then one signed by `ed`
    /// (ed25519), one by `rsa` and one by `other` (ed25519), which is
    /// checked out.
    struct Signed {
        repo: GitRepo,
        ed: SshKey,
        rsa: SshKey,
        other: SshKey,
        unsigned: String,
        by_ed: String,
        by_rsa: String,
        by_other: String,
    }

    impl Signed {
        fn new() -> Signed {
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
            Signed {
                repo,
                ed,
                rsa,
                other,
                unsigned,
                by_ed,
                by_rsa,
                by_other,
            }
        }

        fn verify(&self, rev: &str, keys: &[PublicKey]) -> Result<()> {
            let rev = GitRev::parse(rev, Default::default()).unwrap();
            verify_commit(OsStr::new("git"), self.repo.path(), &rev, keys)
        }

        /// A fetcher with a cache of its own, which records its warnings.
        fn fetcher(&self) -> (NativeFetcher, Arc<Mutex<Vec<String>>>) {
            let warnings = Arc::new(Mutex::new(Vec::new()));
            let sink = warnings.clone();
            let mut fetcher = NativeFetcher::new()
                .unwrap()
                .with_cache_dir(self.repo.scratch().join("cache"));
            fetcher.warn = Box::new(move |msg| sink.lock().unwrap().push(msg.to_owned()));
            (fetcher, warnings)
        }

        fn input(&self, url: &str) -> Input {
            Input::from_url_str(&settings(), url, true).unwrap()
        }
    }

    /// The query parameter of a key (`+` and `/` are base64's).
    fn param(key: &str) -> String {
        key.replace('+', "%2B").replace('/', "%2F")
    }

    #[test]
    fn commits_signed_by_a_given_key_pass() {
        let s = Signed::new();
        s.verify(&s.by_ed, &[public(&s.ed)]).unwrap();
        s.verify(&s.by_rsa, &[public(&s.rsa)]).unwrap();
        // Any of several keys.
        s.verify(&s.by_rsa, &[public(&s.ed), public(&s.rsa)])
            .unwrap();
        s.verify(&s.by_other, &[public(&s.ed), public(&s.other)])
            .unwrap();
    }

    #[test]
    fn unsigned_commits_and_other_signers_fail_with_gits_output() {
        let s = Signed::new();
        // git prints nothing about an unsigned commit.
        let err = s.verify(&s.unsigned, &[public(&s.ed)]).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "Commit signature verification on commit {} failed: \x1b[35;1m\x1b[0m",
                s.unsigned
            )
        );
        let err = s.verify(&s.by_other, &[public(&s.ed)]).unwrap_err();
        let text = err.to_string();
        assert!(
            text.starts_with(&format!(
                "Commit signature verification on commit {} failed: \x1b[35;1mGood \"git\" signature with ED25519 key SHA256:",
                s.by_other
            )),
            "{text}"
        );
        assert!(text.ends_with("\nNo principal matched.\n\x1b[0m"), "{text}");
        // An RSA key given the default type `ssh-ed25519`: OpenSSH refuses
        // the allowed-signers line, in a line `ssh-keygen` ends with CRLF.
        // The message keeps git's output as it is, as Nix's does; printing
        // it drops the `\r` (`filterANSIEscapes`).
        let err = s
            .verify(&s.by_rsa, &[key("ssh-ed25519", &s.rsa.key)])
            .unwrap_err()
            .to_string();
        assert!(err.contains(":1: invalid key\r\n"), "{err:?}");
        assert!(err.contains("/nix."), "{err}");
    }

    /// `verifyCommit = true` without keys: nothing can match.
    #[test]
    fn verification_without_keys_always_fails() {
        let s = Signed::new();
        let err = s.verify(&s.by_ed, &[]).unwrap_err().to_string();
        assert!(
            err.starts_with(&format!(
                "Commit signature verification on commit {} failed: \x1b[35;1m",
                s.by_ed
            )),
            "{err}"
        );
    }

    #[test]
    fn unknown_key_types_are_refused_with_nixs_list() {
        let err = verify_commit(
            OsStr::new("git"),
            Path::new("/nonexistent"),
            &GitRev::NULL,
            &[key("ssh-ed25519", "AAAA"), key("foo", "AAAA")],
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Invalid SSH key type 'foo' in publicKeys.\nPlease use one of:\n\x1b[35;1m  ssh-ed25519-sk\n  ssh-rsa\n  ssh-dsa\n  ssh-ecdsa\n  ssh-ecdsa-sk\n  ssh-ed25519\n\x1b[0m"
        );
    }

    #[test]
    fn keys_that_are_not_base64_are_refused_after_git_ran() {
        let s = Signed::new();
        let err = s
            .verify(&s.by_ed, &[public(&s.ed), key("ssh-rsa", "AB!!x")])
            .unwrap_err();
        assert_eq!(err.message(), "invalid character in Base64 string: ''");
        assert_eq!(
            err.traces(),
            ["while decoding public key 'AB!!x' used for git signature"]
        );
    }

    /// The fingerprints sit in a character class, so a "Good" line passes
    /// on the first character of the fingerprint it shows.
    #[test]
    fn the_fingerprints_form_a_character_class() {
        let ed = "AAAAC3NzaC1lZDI1NTE5AAAAIGP9hbFfdMHI8lPuehQjBnh/xrdBkPAwyNF63YzMgcd2";
        let re = good_signature(&[key("ssh-ed25519", ed)]).unwrap().unwrap();
        let fingerprint = Hash::of(
            HashAlgo::Sha256,
            &brix_store::hash::base64::decode(ed).unwrap(),
        )
        .to_string(HashFormat::Base64, false);
        let fingerprint = fingerprint.trim_end_matches('=');
        let line =
            |fp: &str| format!("Good \"git\" signature for * with ED25519 key SHA256:{fp}\n");
        assert!(re.is_match(line(fingerprint).as_bytes()));
        let first = fingerprint.chars().next().unwrap();
        assert!(re.is_match(line(&format!("{first}somethingelse")).as_bytes()));
        assert!(re.is_match(line("(").as_bytes()));
        let absent = ('A'..='Z')
            .chain('a'..='z')
            .find(|c| !fingerprint.contains(*c))
            .expect("a 43-character fingerprint misses a letter");
        assert!(!re.is_match(line(&absent.to_string()).as_bytes()));
        // Git's line without "for *" (no principal) never matches.
        let no_principal =
            format!("Good \"git\" signature with ED25519 key SHA256:{fingerprint}\n");
        assert!(!re.is_match(no_principal.as_bytes()));
        assert!(good_signature(&[]).unwrap().is_none());
    }

    /// A `git` that cannot be run fails the verification with what Nix's
    /// child process prints.
    #[test]
    fn a_missing_git_fails_as_in_nix() {
        let s = Signed::new();
        let rev = GitRev::parse(&s.by_ed, Default::default()).unwrap();
        let err = verify_commit(
            OsStr::new("brix-test-no-such-git"),
            s.repo.path(),
            &rev,
            &[public(&s.ed)],
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "Commit signature verification on commit {} failed: \x1b[35;1merror: executing \"brix-test-no-such-git\": No such file or directory\n\x1b[0m",
                s.by_ed
            )
        );
    }

    /// Fetches of commits, from a local checkout and from a clone, verify
    /// them; `verifyCommit = false` turns verification off.
    #[test]
    fn fetched_commits_are_verified() {
        let s = Signed::new();
        let bare = s.repo.scratch().join("bare.git");
        git(
            s.repo.scratch(),
            &[
                "clone",
                "-q",
                "--bare",
                s.repo.path().to_str().unwrap(),
                "bare.git",
            ],
        );
        let (fetcher, _) = s.fetcher();
        let settings = settings();
        for base in [s.repo.path(), bare.as_path()] {
            let url =
                |rev: &str, query: &str| format!("git+file://{}?rev={rev}&{query}", base.display());
            let ok = s.input(&url(&s.by_ed, &format!("publicKey={}", param(&s.ed.key))));
            let fetched = fetcher.fetch(&settings, &ok).unwrap();
            assert_eq!(fetched.input.rev().unwrap().unwrap().to_hex(), s.by_ed);
            let rsa = s.input(&url(
                &s.by_rsa,
                &format!("keytype=ssh-rsa&publicKey={}", param(&s.rsa.key)),
            ));
            fetcher.fetch(&settings, &rsa).unwrap();

            let unsigned = s.input(&url(
                &s.unsigned,
                &format!("publicKey={}", param(&s.ed.key)),
            ));
            let err = fetcher.fetch(&settings, &unsigned).unwrap_err().to_string();
            assert!(
                err.starts_with("Commit signature verification on commit"),
                "{err}"
            );
            let off = s.input(&url(
                &s.unsigned,
                &format!("publicKey={}&verifyCommit=0", param(&s.ed.key)),
            ));
            fetcher.fetch(&settings, &off).unwrap();
            let no_keys = s.input(&url(&s.by_ed, "verifyCommit=1"));
            assert!(fetcher.fetch(&settings, &no_keys).is_err());
        }
    }

    /// The real fetchers, counting the fetches that reach them and noting
    /// the revision each was asked for.
    struct Counting(Arc<Mutex<Vec<Option<String>>>>);

    impl crate::native::Source for Counting {
        fn fetch(
            &self,
            fetcher: &NativeFetcher,
            settings: &FetchSettings,
            input: &Input,
        ) -> Result<crate::fetch::FetchedTree> {
            let rev = input.rev()?.map(|r| r.to_hex());
            self.0.lock().unwrap().push(rev);
            fetcher.fetch_from_scheme(settings, input)
        }
    }

    impl Signed {
        /// [`Signed::fetcher`], counting what reaches the fetchers.
        fn counting_fetcher(&self) -> (NativeFetcher, Arc<Mutex<Vec<Option<String>>>>) {
            let (mut fetcher, _) = self.fetcher();
            let fetches = Arc::new(Mutex::new(Vec::new()));
            fetcher.source = Box::new(Counting(fetches.clone()));
            (fetcher, fetches)
        }
    }

    /// A commit to verify is fetched from its repository, and verified, on
    /// every fetch: the fetch cache does not answer for it, whatever it
    /// remembers of the commit (Nix's `verifyCommit` runs whenever the
    /// fetcher is asked).
    #[test]
    fn commits_to_verify_are_verified_on_every_fetch() {
        let s = Signed::new();
        let (fetcher, fetches) = s.counting_fetcher();
        let settings = settings();
        let count = || fetches.lock().unwrap().len();
        let base = format!("git+file://{}?rev={}", s.repo.path().display(), s.by_other);
        // Without verification, a second fetch comes from the cache.
        let plain = s.input(&base);
        let first = fetcher.fetch(&settings, &plain).unwrap();
        fetcher.fetch(&settings, &plain).unwrap();
        assert_eq!(count(), 1);
        // With it, each fetch reaches the repository, also with the tree's
        // own `narHash`, which the fetch cache has.
        let right = s.input(&format!("{base}&publicKey={}", param(&s.other.key)));
        fetcher.fetch(&settings, &right).unwrap();
        fetcher.fetch(&settings, &right).unwrap();
        assert_eq!(count(), 3);
        let nar_hash = first.input.nar_hash().unwrap().unwrap().to_sri();
        let with_hash = s.input(&format!(
            "{base}&publicKey={}&narHash={}",
            param(&s.other.key),
            nar_hash.replace('+', "%2B").replace('/', "%2F")
        ));
        fetcher.fetch(&settings, &with_hash).unwrap();
        assert_eq!(count(), 4);
        assert!(fetcher.resolve_cached(&settings, &right).unwrap().is_none());
        // A key that did not sign it fails, however often the commit was
        // fetched before.
        let wrong = s.input(&format!("{base}&publicKey={}", param(&s.ed.key)));
        let err = fetcher.fetch(&settings, &wrong).unwrap_err().to_string();
        assert!(err.contains("No principal matched."), "{err}");
    }

    /// A remote's `ref` is resolved from the fetch cache within
    /// `tarball-ttl`, so a commit to verify is read from the clone at that
    /// revision instead of being fetched again.
    #[test]
    fn a_verified_remote_ref_is_read_at_its_cached_revision() {
        let s = Signed::new();
        git(
            s.repo.scratch(),
            &[
                "clone",
                "-q",
                "--bare",
                s.repo.path().to_str().unwrap(),
                "bare.git",
            ],
        );
        let (fetcher, fetches) = s.counting_fetcher();
        let settings = settings();
        let url = format!(
            "git+file://{}?ref=main&publicKey={}",
            s.repo.scratch().join("bare.git").display(),
            param(&s.other.key)
        );
        let input = s.input(&url);
        let first = fetcher.fetch(&settings, &input).unwrap();
        assert_eq!(first.input.rev().unwrap().unwrap().to_hex(), s.by_other);
        let second = fetcher.fetch(&settings, &input).unwrap();
        assert_eq!(second.input.rev().unwrap().unwrap().to_hex(), s.by_other);
        assert_eq!(
            *fetches.lock().unwrap(),
            [None, Some(s.by_other.clone())],
            "the second fetch asks for the cached revision"
        );
    }

    /// A clean working tree has its `HEAD` verified; a dirty one cannot be,
    /// and Nix warns about it twice before saying so.
    #[test]
    fn working_trees_are_verified_unless_dirty() {
        let s = Signed::new();
        let settings = settings();
        let url = |key: &str| {
            format!(
                "git+file://{}?publicKey={}",
                s.repo.path().display(),
                param(key)
            )
        };
        let (fetcher, warnings) = s.fetcher();
        // `HEAD` is the commit `other` signed.
        let fetched = fetcher
            .fetch(&settings, &s.input(&url(&s.other.key)))
            .unwrap();
        assert_eq!(fetched.input.rev().unwrap().unwrap().to_hex(), s.by_other);
        let err = fetcher
            .fetch(&settings, &s.input(&url(&s.ed.key)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("No principal matched."), "{err}");
        assert!(warnings.lock().unwrap().is_empty());

        s.repo.write("a", "changed\n");
        let input = s.input(&url(&s.other.key));
        let err = fetcher.fetch(&settings, &input).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "commit verification is required for Git repository '{}', but it's dirty",
                input
            )
        );
        let warning = format!(
            "Git tree '{}' has uncommitted changes",
            s.repo.path().display()
        );
        assert_eq!(*warnings.lock().unwrap(), [warning.clone(), warning]);
        // Without verification a dirty tree is fine (and warned about once).
        warnings.lock().unwrap().clear();
        let off = s.input(&format!("{}&verifyCommit=0", url(&s.other.key)));
        fetcher.fetch(&settings, &off).unwrap();
        assert_eq!(warnings.lock().unwrap().len(), 1);
    }
}
