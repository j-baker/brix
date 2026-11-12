//! Splitting a string into words as a shell would: Nix's
//! `shellSplitString` (`src/libutil/strings.cc`) and `getNixSshOpts`
//! (`src/libstore/ssh.cc`), which splits `$NIX_SSHOPTS` with it. Both the
//! SSH store transport and the Git LFS fetcher (`git-lfs-authenticate`
//! over `ssh`) pass those options on.

use std::fmt;

/// `shellSplitString`'s failure: an unterminated quote, with Nix's
/// message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellSplitError(pub &'static str);

impl fmt::Display for ShellSplitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for ShellSplitError {}

/// `shellSplitString`: split into words like a POSIX shell, honouring
/// single quotes (literal), double quotes (where a backslash escapes only
/// `$`, `` ` ``, `"` and `\` and is kept before anything else) and
/// backslash escapes outside quotes. Words are separated by spaces and
/// tabs only. One `escaping` flag is shared by both states, as in Nix. The
/// words are bytes: nothing needs to be UTF-8.
pub fn shell_split(mut s: &[u8]) -> Result<Vec<Vec<u8>>, ShellSplitError> {
    let mut result = Vec::new();
    let mut current = Vec::new();
    let mut started = false;
    let mut escaping = false;
    while let Some((&c, rest)) = s.split_first() {
        s = rest;
        if escaping {
            current.push(c);
            started = true;
            escaping = false;
        } else if c == b'\\' {
            escaping = true;
        } else if c == b' ' || c == b'\t' {
            if started {
                result.push(std::mem::take(&mut current));
                started = false;
            }
        } else if c == b'"' {
            started = true;
            let mut closed = false;
            while let Some((&c, rest)) = s.split_first() {
                s = rest;
                if escaping {
                    if !matches!(c, b'$' | b'`' | b'"' | b'\\') {
                        current.push(b'\\');
                    }
                    current.push(c);
                    escaping = false;
                } else if c == b'\\' {
                    escaping = true;
                } else if c == b'"' {
                    closed = true;
                    break;
                } else {
                    current.push(c);
                }
            }
            if !closed {
                return Err(ShellSplitError("unterminated double quote"));
            }
        } else if c == b'\'' {
            started = true;
            let mut closed = false;
            while let Some((&c, rest)) = s.split_first() {
                s = rest;
                if c == b'\'' {
                    closed = true;
                    break;
                }
                current.push(c);
            }
            if !closed {
                return Err(ShellSplitError("unterminated single quote"));
            }
        } else {
            current.push(c);
            started = true;
        }
    }
    if started {
        result.push(current);
    }
    Ok(result)
}

/// `getNixSshOpts`'s failure: the split's message, and the trace Nix adds
/// to it (`while splitting NIX_SSHOPTS '…'`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshOptsError {
    pub message: String,
    pub trace: String,
}

impl fmt::Display for SshOptsError {
    /// The message alone, as Nix's `msg()`; the trace is the caller's to
    /// render.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SshOptsError {}

/// `getNixSshOpts`: `$NIX_SSHOPTS` (unset is empty) split by
/// [`shell_split`], as the arguments `ssh` is given.
#[cfg(unix)]
pub fn nix_ssh_opts() -> Result<Vec<std::ffi::OsString>, SshOptsError> {
    let opts = std::env::var_os("NIX_SSHOPTS").unwrap_or_default();
    split_ssh_opts(&opts)
}

/// [`nix_ssh_opts`] for the value `opts`.
#[cfg(unix)]
pub fn split_ssh_opts(opts: &std::ffi::OsStr) -> Result<Vec<std::ffi::OsString>, SshOptsError> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    match shell_split(opts.as_bytes()) {
        Ok(words) => Ok(words
            .into_iter()
            .map(std::ffi::OsString::from_vec)
            .collect()),
        Err(e) => Err(SshOptsError {
            message: e.0.to_owned(),
            trace: format!("while splitting NIX_SSHOPTS '{}'", opts.to_string_lossy()),
        }),
    }
}

/// `escapeShellArgAlways`: `s` in single quotes, each `'` in it written
/// `'\''`.
pub fn escape_shell_arg_always(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(s: &str) -> Vec<String> {
        shell_split(s.as_bytes())
            .unwrap()
            .into_iter()
            .map(|w| String::from_utf8(w).unwrap())
            .collect()
    }

    // The cases of `TEST(shellSplitString, …)` in libutil-tests/strings.cc.
    #[test]
    fn shell_split_string_nix_cases() {
        assert_eq!(split(""), Vec::<String>::new());
        assert_eq!(split("foo"), ["foo"]);
        assert_eq!(split("'foo bar'"), ["foo bar"]);
        assert_eq!(split("'foo bar\"'"), ["foo bar\""]);
        assert_eq!(split("\"foo bar\""), ["foo bar"]);
        assert_eq!(split("foo bar"), ["foo", "bar"]);
        assert_eq!(split("\"foo bar'\" 'baz\"'"), ["foo bar'", "baz\""]);
        assert_eq!(split("foo '' bar baz ''"), ["foo", "", "bar", "baz", ""]);
        assert_eq!(
            split("foo \"\" bar baz \"\""),
            ["foo", "", "bar", "baz", ""]
        );
        assert_eq!(split("'foo\\\"bar'"), ["foo\\\"bar"]);
        assert_eq!(split("\"foo\\\"bar\""), ["foo\"bar"]);
        assert_eq!(
            split("foo\\ bar baz qux\\ quux"),
            ["foo bar", "baz", "qux quux"]
        );
        assert_eq!(
            split("foo\\\"bar baz qux\\'quux"),
            ["foo\"bar", "baz", "qux'quux"]
        );
        for unbalanced in ["foo'", "foo\"", "foo'bar", "foo\"bar", "foo\"bar\\\""] {
            assert!(shell_split(unbalanced.as_bytes()).is_err(), "{unbalanced}");
        }
    }

    #[test]
    fn shell_split_details() {
        // In double quotes a backslash only escapes $ ` " and itself.
        assert_eq!(split(r#""a\$b\`c\\d\e""#), [r"a$b`c\d\e"]);
        // Only spaces and tabs separate words.
        assert_eq!(split("a\tb\nc"), ["a", "b\nc"]);
        // A trailing backslash escapes nothing and is dropped.
        assert_eq!(split("a\\"), ["a"]);
        assert_eq!(
            shell_split(b"'x").unwrap_err(),
            ShellSplitError("unterminated single quote")
        );
        assert_eq!(
            shell_split(b"\"x").unwrap_err().to_string(),
            "unterminated double quote"
        );
        // Bytes that are not UTF-8 pass through.
        assert_eq!(
            shell_split(b"-o \xff").unwrap(),
            [b"-o".to_vec(), vec![0xff]]
        );
        assert_eq!(
            split(r#"-oFoo=bar  -i "my key" 'a b'\ c"#),
            ["-oFoo=bar", "-i", "my key", "a b c"]
        );
    }

    /// `NIX_SSHOPTS='-o "unterminated' nix store info --store
    /// ssh-ng://host`: the message and the trace Nix adds.
    #[cfg(unix)]
    #[test]
    fn nix_sshopts_errors_carry_nix_trace() {
        let e = split_ssh_opts(std::ffi::OsStr::new("-o \"unterminated")).unwrap_err();
        assert_eq!(e.message, "unterminated double quote");
        assert_eq!(e.trace, "while splitting NIX_SSHOPTS '-o \"unterminated'");
        assert_eq!(
            split_ssh_opts(std::ffi::OsStr::new("-o Foo=1 'a b'")).unwrap(),
            ["-o", "Foo=1", "a b"].map(std::ffi::OsString::from)
        );
    }

    #[test]
    fn shell_args_are_quoted_always() {
        assert_eq!(escape_shell_arg_always("it's"), r"'it'\''s'");
        assert_eq!(escape_shell_arg_always(""), "''");
    }
}
