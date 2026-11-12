//! Shared test support: running `nix-instantiate --parse` as the oracle,
//! and brix's parser the same way.
#![allow(dead_code)]

use std::path::Path;

use brix_syntax::{parse, show_bytes, Origin, ParseOptions};

/// What a parse produced, as `nix-instantiate --parse` shows it.
#[derive(Debug, PartialEq, Eq)]
pub enum Oracle {
    /// The printed expression, and the warnings logged on the way.
    Parsed { out: String, warnings: String },
    /// Everything on stderr: the warnings logged before the error, then
    /// the error.
    Failed(String),
}

impl Oracle {
    /// Whether Nix reported an undefined variable. brix finds those when
    /// it compiles the expression, not when it parses it; the comparisons
    /// that include them are in `brix-eval/tests/parse_oracle.rs`.
    pub fn needs_scoping(&self) -> bool {
        match self {
            Oracle::Failed(stderr) => stderr
                .lines()
                .any(|l| l.starts_with("error: undefined variable '")),
            Oracle::Parsed { .. } => false,
        }
    }
}

fn run_nix_instantiate(args: &[&str], stdin: Option<&[u8]>, cwd: &Path) -> Oracle {
    let mut cmd = brix_testutil::Oracle::nix_instantiate();
    cmd.args(["--parse", "--pure-eval"])
        .args(args)
        .dir(cwd)
        .dummy_store()
        .config("warn-dirty = false");
    if let Some(input) = stdin {
        cmd.stdin(input);
    }
    let out = cmd.output();
    let stderr = out.stderr_lossy();
    if out.success() {
        Oracle::Parsed {
            out: out.stdout_lossy(),
            warnings: stderr,
        }
    } else {
        Oracle::Failed(stderr)
    }
}

/// Parses `src` with `nix-instantiate --parse -` from `cwd`.
pub fn nix_parse_stdin(src: &[u8], cwd: &Path) -> Oracle {
    nix_parse_stdin_with(src, cwd, &[])
}

/// Like [`nix_parse_stdin`], with more `nix-instantiate` flags
/// (`--extra-experimental-features pipe-operators`, `--lint-…`).
pub fn nix_parse_stdin_with(src: &[u8], cwd: &Path, flags: &[&str]) -> Oracle {
    let mut args = flags.to_vec();
    args.push("-");
    run_nix_instantiate(&args, Some(src), cwd)
}

/// Parses a file the way the corpus does: on stdin, from its directory.
/// (Pure evaluation forbids `nix-instantiate` from reading the file itself.)
pub fn nix_parse_file(file: &Path) -> Oracle {
    let dir = file.parent().expect("file has a directory");
    nix_parse_stdin(&std::fs::read(file).expect("read file"), dir)
}

/// Parses `src` with brix the way `nix-instantiate --parse` would from
/// `origin`, except that undefined variables are not looked for (see
/// [`Oracle::needs_scoping`]).
pub fn brix_parse(src: &[u8], base_dir: &(impl AsRef<[u8]> + ?Sized), origin: &Origin) -> Oracle {
    brix_parse_with(src, base_dir, origin, &ParseOptions::default())
}

/// [`brix_parse`] with parse options.
pub fn brix_parse_with(
    src: &[u8],
    base_dir: &(impl AsRef<[u8]> + ?Sized),
    origin: &Origin,
    options: &ParseOptions,
) -> Oracle {
    let mut warnings = Vec::new();
    let result = parse(src, base_dir, options, &mut warnings);
    let mut logged = String::new();
    for w in &warnings {
        logged.push_str(&String::from_utf8_lossy(&w.render(origin, src)));
        logged.push('\n');
    }
    match result {
        Ok(expr) => {
            let mut out = show_bytes(&expr);
            out.push(b'\n');
            Oracle::Parsed {
                out: String::from_utf8_lossy(&out).into_owned(),
                warnings: logged,
            }
        }
        Err(err) => {
            logged.push_str(&String::from_utf8_lossy(&err.render(origin, src)));
            Oracle::Failed(logged)
        }
    }
}
