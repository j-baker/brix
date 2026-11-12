//! Short expressions parsed by brix and by `nix-instantiate --parse`, with
//! the settings that change what the parser does: the `pipe-operators`
//! experimental feature and the `lint-*` settings. Output, errors and
//! warnings must be identical.

mod common;

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use brix_syntax::{parse, show_bytes, Diagnose, Lints, Origin, ParseOptions};
use brix_testutil::{join_bytes, scratch_store, write_bytes};
use common::{brix_parse_with, nix_parse_stdin_with, Oracle};

/// Parses each source with both and lists the differences. Every case
/// must be one the parser decides alone (no undefined variables).
fn compare(cases: &[&str], flags: &[&str], options: &ParseOptions) {
    let mut failures = Vec::new();
    for src in cases {
        let expected = nix_parse_stdin_with(src.as_bytes(), Path::new("/"), flags);
        assert!(
            !expected.needs_scoping(),
            "{src:?} has an undefined variable, which the parser does not look for"
        );
        let got = brix_parse_with(src.as_bytes(), "/", &Origin::Stdin, options);
        if expected != got {
            failures.push(format!("{src:?}:\n  nix:  {expected:?}\n  brix: {got:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn pipe_operators() {
    let options = ParseOptions {
        pipe_operators: true,
        ..ParseOptions::default()
    };
    let cases: Vec<String> = [
        // `<|` nests to the right, `|>` to the left; both merge into an
        // application they are given, as `makeCall` does.
        "f <| g <| x",
        "x |> f |> g",
        "x |> f a |> g b",
        "f a <| g b <| x",
        "a |> (b <| c)",
        // They bind more loosely than every other operator.
        "a + b |> f",
        "f <| a + b",
        "a |> b == c",
        "a -> b |> c",
        "a |> b -> c",
        "- a |> f",
        "f <| a.b or c",
        "if a then b else c |> f",
        // Mixing the two without parentheses, and other errors.
        "a |> b |> c <| d",
        "f <| a <| b |> c",
        "f <| x: x",
        "a <|",
    ]
    .iter()
    .map(|e| format!("{{ a, b, c, d, f, g, x }}: {e}"))
    .collect();
    let cases: Vec<&str> = cases.iter().map(String::as_str).collect();
    compare(
        &cases,
        &["--extra-experimental-features", "pipe-operators"],
        &options,
    );
    // Without the feature, the operators are an error.
    compare(&["a: a |> a", "a: a <| a"], &[], &ParseOptions::default());
}

/// Errors that Nix finds when it reduces a whole rule come after errors
/// in the rest of the rule.
#[test]
fn errors_in_the_order_bison_finds_them() {
    compare(
        &[
            "{ a, a }: (",
            "a@{ a }: (",
            "{ a }@a: (",
            "{ a, a }: { b = 1; b = 2; }",
            "{ a, a }: 1",
            "{ a = 1; inherit a }",
            "{ a = 1; inherit (a) a }",
            "let a = 1; inherit a in a",
            "{ a = 1; inherit a a; }",
            "{ inherit a ${b}; a = 1; }",
            "x: let ${x} = 1; in (",
            "x: let ${x} = 1; in x",
            "x: let ${x} = 1; a = 1; a = 2; in x",
        ],
        &[],
        &ParseOptions::default(),
    );
}

/// Empty sources, and sources with a NUL: outside a string or comment a
/// NUL ends the input (the lexer returns it as token 0, Bison's end of
/// file), and Nix reads the lines it shows (and counts positions in) up
/// to the first NUL of a source it got on stdin, showing a missing line
/// as `(empty file)`.
#[test]
fn errors_in_empty_sources_and_after_a_nul() {
    compare(
        &[
            "",
            " ",
            "\n",
            "\0",
            "\0 1",
            "1 \0",
            "1 \0 2",
            "(\n\0",
            "(1\n\0\n(",
            "\"a\0b\"",
            "\"a\" + \0",
            "#a\0b\n1",
            "/*\0*/ 1",
        ],
        &[],
        &ParseOptions::default(),
    );
}

#[test]
fn cursed_or_warnings() {
    compare(
        &[
            "let or = 1; in [ (x: x) or ]",
            "let or = 1; in (x: x * 2) (x: x + 1) or",
            "let or = 1; in { a = 2; }.a or (x: x) or",
            "let or = 1; in [ (x:\n  x) or ]",
            "let or = 1; in [ (x: x)\r\n  or ]",
            // Not cursed: the first expression of an application.
            "let or = (x: x); in map or []",
            "let f = (x: x); or = f; in f (f or)",
        ],
        &[],
        &ParseOptions::default(),
    );
}

#[test]
fn lint_settings() {
    let all = |level: Diagnose| ParseOptions {
        lints: Lints {
            url_literals: level,
            short_path_literals: level,
            absolute_path_literals: level,
        },
        ..ParseOptions::default()
    };
    let flags = |level: &'static str| {
        [
            "--lint-url-literals",
            level,
            "--lint-short-path-literals",
            level,
            "--lint-absolute-path-literals",
            level,
        ]
    };
    let cases = [
        "[ http://x foo/bar /abs ./rel ../up \"http://y\" ]",
        "http://x ~",
        "foo/bar/",
        "foo/${\"x\"}",
        "/a/${\"x\"}/b",
        "[ a/b (",
        "[\n  1\n  a.b/c\n]",
    ];
    for (level, name) in [
        (Diagnose::Ignore, "ignore"),
        (Diagnose::Warn, "warn"),
        (Diagnose::Fatal, "fatal"),
    ] {
        compare(&cases, &flags(name), &all(level));
    }
    // One setting at a time.
    let only_urls = ParseOptions {
        lints: Lints {
            url_literals: Diagnose::Fatal,
            ..Lints::default()
        },
        ..ParseOptions::default()
    };
    compare(
        &["[ foo/bar /abs http://x ]"],
        &["--lint-url-literals", "fatal"],
        &only_urls,
    );
}

#[test]
fn indentation_is_capped_at_a_million_spaces() {
    let src = format!("''\n{}\n''", " ".repeat(1_000_003));
    compare(&[src.as_str()], &[], &ParseOptions::default());
    // A sanity check that the case means something: Nix keeps 3 spaces.
    let Oracle::Parsed { out, .. } = nix_parse_stdin_with(src.as_bytes(), Path::new("/"), &[])
    else {
        panic!("parses")
    };
    assert_eq!(out, "\"   \\n\"\n");
}

/// The cases above mean something only if Nix does warn for them.
#[test]
fn the_oracle_warns_for_these_cases() {
    let warnings = |src: &str, flags: &[&str]| match nix_parse_stdin_with(
        src.as_bytes(),
        Path::new("/"),
        flags,
    ) {
        Oracle::Parsed { warnings, .. } => warnings,
        other => panic!("{src:?}: {other:?}"),
    };
    assert_eq!(
        warnings("let or = 1; in [ (x: x) or ]", &[])
            .matches("warning: at «stdin»:1:18: This expression uses `or`")
            .count(),
        1
    );
    assert!(warnings(
        "[ http://x foo/bar /abs ]",
        &[
            "--lint-url-literals",
            "warn",
            "--lint-short-path-literals",
            "warn",
            "--lint-absolute-path-literals",
            "warn"
        ]
    )
    .contains("(lint-absolute-path-literals)\n         at «stdin»:1:20:"));
}

/// A directory named `d\xe9` (not UTF-8) in a throwaway directory, by
/// its canonical path (the working directory Nix finds with `getcwd`).
/// The throwaway directory is removed when the first value is dropped.
fn byte_named_dir() -> (impl Sized, PathBuf) {
    let scratch = scratch_store();
    let dir = join_bytes(scratch.path(), b"d\xe9");
    std::fs::create_dir(&dir).unwrap_or_else(|e| panic!("creating {}: {e}", dir.display()));
    let dir = std::fs::canonicalize(&dir).expect("canonical path of the directory");
    (scratch, dir)
}

/// Relative path literals resolve against a base directory that is not
/// UTF-8 and keep its bytes, as Nix's do: `nix-instantiate --parse` run
/// in a directory named `d\xe9` prints the same bytes as brix's parser
/// given that directory.
#[test]
fn path_literals_keep_the_bytes_of_their_directory() {
    let (_scratch, dir) = byte_named_dir();
    let src =
        br#"{ a = ./.; b = ../z; c = /abs/x/..; d = ./x/../y; e = ./a/${"b"}; f = ./x/${"y"}/z; }"#;
    let out = brix_testutil::Oracle::nix_instantiate()
        .args(["--parse", "--pure-eval", "-"])
        .dir(&dir)
        .dummy_store()
        .stdin(src.to_vec())
        .output();
    assert!(out.success(), "{}", out.stderr_lossy());
    assert_eq!(out.stderr, b"");
    let mut warnings = Vec::new();
    let expr = parse(
        src,
        dir.as_os_str().as_bytes(),
        &ParseOptions::default(),
        &mut warnings,
    )
    .expect("parses");
    assert!(warnings.is_empty());
    let mut ours = show_bytes(&expr);
    ours.push(b'\n');
    assert_eq!(bstr::BStr::new(&ours), bstr::BStr::new(&out.stdout));
    // The bytes are there to be compared.
    assert!(bstr::ByteSlice::contains_str(&ours[..], b"/d\xe9/y"));
}

/// A parse error in a file whose path is not UTF-8 names it by its
/// bytes, in the message and in the position.
#[test]
fn parse_errors_show_a_file_path_as_its_bytes() {
    let (_scratch, dir) = byte_named_dir();
    let src = b"{ a = 1; a = 2; }";
    write_bytes(&dir, b"caf\xe9.nix", src);
    let file = join_bytes(&dir, b"caf\xe9.nix");
    let out = brix_testutil::Oracle::nix_instantiate()
        .arg("--parse")
        .arg(&file)
        .dummy_store()
        .output();
    assert!(!out.success(), "{}", out.stdout_lossy());
    let mut warnings = Vec::new();
    let err = parse(
        src,
        dir.as_os_str().as_bytes(),
        &ParseOptions::default(),
        &mut warnings,
    )
    .expect_err("a duplicate attribute");
    let origin = Origin::Path(file.as_os_str().as_bytes().into());
    assert_eq!(
        bstr::BStr::new(&err.render(&origin, src)),
        bstr::BStr::new(&out.stderr)
    );
}
