//! Builtins and printers compared with the reference `nix-instantiate
//! --eval --strict` byte for byte (its output decoded as UTF-8 the way a
//! terminal shows it): `fromJSON`'s numbers and nesting.

mod common;

use brix_eval::Config;
use bstr::ByteSlice;
use common::{
    nix_instantiate, oracle_outcome, run_brix_expr, run_brix_file, run_nix_expr, Format, TestHost,
};

/// `expr` gives the same output and errors in brix and in Nix (pure
/// mode, no store).
fn same_as_nix(expr: &str) {
    let pwd = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let host = TestHost::with_allowed(&[("/pwd", pwd.path()), ("/nix/store", store.path())], &[]);
    let config = Config {
        eval_cores: 0,
        ..Config::default()
    };
    let ours = run_brix_expr(&host, config, expr, "/pwd", Format::Plain);
    let theirs = run_nix_expr(expr, pwd.path(), Format::Plain, &[], false);
    assert_eq!(ours, theirs, "{expr}");
}

/// A JSON text nesting `open` … `close` `depth` times around `inner`, as
/// a Nix expression.
fn nested(open: &str, inner: &str, close: &str, depth: usize) -> String {
    format!(
        r#"(let rep = s: builtins.concatStringsSep "" (builtins.genList (_: s) {depth}); in rep "{open}" + "{inner}" + rep "{close}")"#
    )
}

#[test]
fn from_json_numbers_match_nix() {
    for json in [
        "1e400",
        "-1e400",
        "1e+400",
        "1E400",
        "-1.5e400",
        "[1, 1e400]",
        r#"{\"a\": [1, 1e999]}"#,
        "1e-400",
        "-1e-400",
        "99999999999999999999999",
        "-99999999999999999999999",
        "18446744073709551615",
        "-0",
        "[1e2, 1.5e300, -0, 0.1]",
        // Errors in document order: the first one wins.
        r#"[1e400, \"\\u0000\"]"#,
        r#"[\"\\u0000\", 1e400]"#,
    ] {
        same_as_nix(&format!(r#"builtins.fromJSON "{json}""#));
    }
    // An integer too long for 64 bits is a double, which can overflow.
    let digits = "1".to_owned() + &"0".repeat(400);
    same_as_nix(&format!(r#"builtins.fromJSON "{digits}""#));
}

/// nlohmann's parser has no depth limit, and neither has brix's.
#[test]
fn from_json_nests_as_deep_as_nix() {
    for (expr, depth) in [
        ("builtins.fromJSON {json}", 200),
        ("builtins.length (builtins.fromJSON {json})", 50_000),
    ] {
        same_as_nix(&expr.replace("{json}", &nested("[", "1", "]", depth)));
    }
    for (expr, depth) in [
        ("builtins.fromJSON {json}", 200),
        ("builtins.attrNames (builtins.fromJSON {json})", 50_000),
    ] {
        same_as_nix(&expr.replace("{json}", &nested(r#"{\"a\":"#, "1", "}", depth)));
    }
}

/// Invalid documents fail with nlohmann's messages (line, column, what
/// was read last), deep ones included, and the values Nix refuses fail
/// where they are read, before a syntax error further on.
#[test]
fn from_json_rejects_what_nix_rejects() {
    let deep_and_broken = nested("[", "1,", "]", 300);
    let mut exprs = vec![format!("builtins.fromJSON {deep_and_broken}")];
    for json in [
        "",
        " ",
        "   ",
        "1x",
        "[1] x",
        "1 2",
        "[1,",
        r#"{\"a\":1,}"#,
        "notjson",
        "[1, tru]",
        "[1,]",
        r#"{\"a\" 1}"#,
        "{1: 2}",
        "[1 2]",
        r#"{\"a\": 1 \"b\"}"#,
        r#"\"abc"#,
        r#"\"a\\qb\""#,
        r#"\"\\u12\""#,
        r#"\"\\uD800\""#,
        r#"\"\\uD800\\u0041\""#,
        r#"\"\\uDC00\""#,
        r#"\"a\tb\""#,
        r#"\"a\nb\""#,
        r#"[\n1,\n  x]"#,
        r#"[1,\r\n2,\r\nx]"#,
        "-",
        "-a",
        "01",
        "1.",
        "1e",
        "1e+",
        "[1]]",
        "{}}",
        "tr",
        "nulll",
        "[,1]",
        "/* c */ 1",
        r#"{\"a\": [1, {\"b\": nul}]}"#,
        r#"\"\\u00e9\" x"#,
        "\\\"é\\\" x",
        // A byte order mark is skipped (and counts in "last read"); a
        // character whose UTF-8 starts like one is a broken mark.
        "\u{feff}[1]",
        "\u{feff}x",
        "\u{f8ff}",
        // The values Nix refuses, before a later syntax error.
        "[18446744073709551615, x]",
        r#"[\"\\u0000\", x]"#,
        r#"{\"a\\u0000\": 1"#,
        r#"{\"a\": \"x\\u0000\", \"a\": 1}"#,
        "[1e400, x",
        "[x, 1e400]",
    ] {
        exprs.push(format!(r#"builtins.fromJSON "{json}""#));
    }
    for expr in exprs {
        same_as_nix(&expr);
    }
}

/// `std::regex` matches bytes: a multi-byte character is several
/// characters, in a bracket expression too.
#[test]
fn regexes_match_bytes_as_in_nix() {
    let lead = r#"(builtins.substring 0 1 "é")"#;
    let tail = r#"(builtins.substring 1 1 "é")"#;
    for (re, s) in [
        (r#""[é]+""#, r#""é""#),
        (r#""[é]""#, r#""é""#),
        (r#""é""#, r#""é""#),
        (r#""é+""#, r#""éé""#),
        (r#""(é)+""#, r#""éé""#),
        (r#""[^é]+""#, r#""abc""#),
        (r#""[^é]+""#, r#""é""#),
        (r#""[^a]""#, r#""é""#),
        (r#""[^a][^a]""#, r#""é""#),
        (r#""[é-z]""#, r#""a""#),
        (r#""[é-z]+""#, r#""é""#),
        (r#""[é-z]""#, r#""{""#),
        (r#""[a-é]""#, r#""b""#),
        (r#""[é-é]""#, r#""é""#),
        (r#"".""#, r#""é""#),
        (r#""..""#, r#""é""#),
        (r#"".é""#, r#""aé""#),
        (r#""[[:alpha:]]+""#, r#""é""#),
        (r#""[\\é]""#, r#""é""#),
        (r#""\\é""#, r#""é""#),
        // Patterns that are not UTF-8.
        (lead, lead),
        (
            &format!(r#""[${{{tail}}}]x""#),
            &format!(r#""${{{tail}}}x""#),
        ),
        (&format!(r#""(${{{lead}}})(.)""#), r#""é""#),
    ] {
        same_as_nix(&format!("builtins.match {re} {s}"));
    }
}

#[test]
fn regex_splits_bytes_as_in_nix() {
    for (re, s) in [
        (r#""[é]""#, r#""aébé""#),
        (r#""é""#, r#""aébé""#),
        (r#""[^a]""#, r#""aéa""#),
        (r#""[^é]""#, r#""aéb""#),
        (r#""(é)""#, r#""aéb""#),
        (r#""[é-z]""#, r#""aé{""#),
    ] {
        same_as_nix(&format!("builtins.split {re} {s}"));
    }
}

/// Bracket expressions, escapes and intervals as libstdc++ reads an ERE.
#[test]
fn regex_syntax_is_read_as_in_nix() {
    for (re, s) in [
        ("[]a]", "]"),
        ("[^]a]", "b"),
        ("[]-a]", "^"),
        ("[a\\\\]", "\\\\"),
        ("[-a]", "-"),
        ("[^-a]", "b"),
        ("[a-z-]", "-"),
        ("[--/]", "."),
        ("[+--]", ","),
        ("[%--]", "+"),
        ("[a-c-e]", "d"),
        ("[[:alpha:]-z]", "-"),
        ("[z-a]", "a"),
        ("[a-a]", "a"),
        ("[&&a]", "&"),
        ("[~~]", "~"),
        ("[a[]", "["),
        ("[[]", "["),
        ("[[:ALPHA:]]", "a"),
        ("[[:w:]]+", "a_1"),
        ("[[:word:]]", "a"),
        ("[[:foo:]]", "a"),
        ("[[:alpha:]", "a"),
        ("[[.space.]]", " "),
        ("[[.hyphen.]]", "-"),
        ("[[.zero.]]", "0"),
        ("[[.0.]]", "0"),
        ("[[.ab.]]", "a"),
        ("[[.é.]]", "é"),
        ("[[.A.]]", "a"),
        ("[[.a.]-c]", "b"),
        ("[a-[.c.]]", "b"),
        ("[[=a=]]", "A"),
        ("[[=é=]]", "é"),
        ("[[=space=]-z]", "a"),
        ("[", "["),
        ("[a", "a"),
        ("[^]", "a"),
        ("\\\\d", "d"),
        ("a{x}", "a{x}"),
        ("a}", "a}"),
        ("a{,2}", "a"),
        ("a{2,1}", "a"),
        ("a{2,}", "aaa"),
        ("(?:a)", "a"),
        ("(?x)a", "a"),
        ("a)(b", "ab"),
        ("a|ab", "ab"),
        ("(a|ab)(c|bcd)", "abcd"),
    ] {
        same_as_nix(&format!(r#"builtins.match "{re}" "{s}""#));
    }
}

/// A repetition of a repetition repeats it again, greedily (`a*?` is
/// `(a*)?`, not a lazy `a*`), and a repetition needs something before it
/// to repeat, as libstdc++ reads an ERE.
#[test]
fn repeated_repetitions_are_read_as_in_nix() {
    for (re, s) in [
        ("(a*?)(a*)", "aaa"),
        ("(a+?)(a*)", "aaa"),
        ("(a??)(a*)", "aaa"),
        ("(a{1,2}?)(a*)", "aaa"),
        ("(a**)(a*)", "aaa"),
        ("(a*+)(a*)", "aaa"),
        ("(a?*)(a*)", "aaa"),
        ("(a{2}{2})(a*)", "aaaaa"),
        ("a{2}*", "aaaa"),
        ("((a)*?)", "aa"),
        ("((a)+?)(b)", "b"),
        ("((ab)??)(.*)", "abab"),
        ("([ab]+?)(b*)", "abbb"),
        ("(.*?)x", "aaxbx"),
        ("^*a", "a"),
        ("a$*", "a"),
        ("(*a)", "a"),
        ("a|*b", "b"),
        ("*a", "a"),
        ("{2}a", "a"),
    ] {
        same_as_nix(&format!(r#"builtins.match "{re}" "{s}""#));
    }
    for (re, s) in [("a*?", "baaac"), ("(a)*?", "aab"), ("(a+?)", "aab")] {
        same_as_nix(&format!(r#"builtins.split "{re}" "{s}""#));
    }
}

/// Nix prints a string's bytes as they are, escaping only ASCII; output
/// that is not UTF-8 reads as U+FFFD for each ill-formed sequence.
#[test]
fn strings_that_are_not_utf8_print_as_in_nix() {
    let lead = r#"(builtins.substring 0 1 "é")"#;
    let tail = r#"(builtins.substring 1 1 "é")"#;
    for expr in [
        lead.to_owned(),
        format!(r#"[ ({lead} + "\"") ({tail} + "\${{x}}") ({lead} + "\\") ]"#),
        format!(r#"{{ "${{{lead}}}" = 1; "a${{{tail}}}\"" = 2; }}"#),
        format!(r#"builtins.attrNames ({lead} + "\"" + {lead} + "\${{")"#),
    ] {
        same_as_nix(&expr);
    }
}

/// Error messages show at most 1024 bytes of a string
/// (`errorPrintOptions`), wherever the cut falls.
#[test]
fn long_strings_in_errors_are_cut_as_in_nix() {
    for prefix in ["", "a", "ab"] {
        same_as_nix(&format!(
            r#"builtins.attrNames ("{prefix}" + builtins.concatStringsSep "" (builtins.genList (_: "é") 600))"#
        ));
    }
    same_as_nix(
        r#"builtins.attrNames (builtins.concatStringsSep "" (builtins.genList (_: "€") 400))"#,
    );
}

/// `toXML` keeps a string's bytes, UTF-8 or not.
#[test]
fn to_xml_keeps_bytes_as_in_nix() {
    let lead = r#"(builtins.substring 0 1 "é")"#;
    for expr in [
        format!("builtins.toXML {lead}"),
        format!("builtins.stringLength (builtins.toXML {lead})"),
        format!(r#"builtins.hashString "sha256" (builtins.toXML {{ a = {lead}; }})"#),
    ] {
        same_as_nix(&expr);
    }
}

/// The file `src` (raw bytes) gives the same output and errors in brix
/// and in Nix, evaluated as `/pwd/x.nix`.
fn file_same_as_nix(src: &[u8]) {
    let pwd = tempfile::tempdir().unwrap();
    std::fs::write(pwd.path().join("x.nix"), src).unwrap();
    let host = TestHost::with_allowed(&[("/pwd", pwd.path())], &["/pwd"]);
    let config = Config {
        eval_cores: 0,
        ..Config::default()
    };
    let ours = run_brix_file(&host, config, "/pwd/x.nix", Format::Plain);
    let mut nix = nix_instantiate(pwd.path(), Format::Plain, false, false);
    // Reading the file is what pure mode forbids; brix's host allows it.
    nix.args(["--option", "pure-eval", "false", "x.nix"]);
    let out = nix
        .command()
        .output()
        .unwrap_or_else(|e| panic!("running {}: {e}", nix.describe()));
    let theirs = oracle_outcome(&out, pwd.path());
    assert_eq!(ours, theirs, "{:?}", src.as_bstr());
}

/// Names written in a source file that is not UTF-8 keep their bytes
/// through the parser, the compiler and the printers: `"\xff"` and
/// `"\xfe"` (Latin-1 `ÿ` and `þ`) are two attributes, not one U+FFFD.
#[test]
fn latin1_names_in_the_source_are_bytes_as_in_nix() {
    for src in [
        &b"let s = { \"\xff\" = 1; \"\xfe\" = 2; }; in [ (builtins.length (builtins.attrNames s)) s ]"[..],
        b"{ a.\"\xff\".b = 1; a.\"\xfe\" = 2; \"\xc3\" = { }; }",
        b"let s = { \"\xc3\" = 1; }; in [ (s ? \"\xc3\") s.\"\xc3\" (s ? \"\xc3\xa9\") ]",
        // Errors quote the names as they are.
        b"{ \"\xff\" = 1; \"\xff\" = 2; }",
        b"{ a.\"\xff\" = 1; a.\"\xff\" = 2; }",
        b"let x = 1; in { inherit \"\xff\"; }",
        b"{ \"\xff\" = 1; }.\"\xfe\"",
        b"\"\xff\x00\"",
    ] {
        file_same_as_nix(src);
    }
}
