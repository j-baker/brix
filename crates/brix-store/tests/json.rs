//! nlohmann::json's parser (`brix_store::json`) against the reference
//! `nix`: `builtins.fromJSON` of a document fails with the exception
//! nlohmann throws for it (line, column, what it read last), which brix
//! must give byte for byte, and parses what nlohmann accepts.
//!
//! Needs `BRIX_NIX_BIN` (the devshell sets it).

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

use brix_store::json::{sax_parse, Sax, SaxError};
use brix_testutil::Oracle;

/// A Nix string literal holding `bytes`.
fn nix_string(bytes: &[u8]) -> Vec<u8> {
    let mut out = b"\"".to_vec();
    for (i, &c) in bytes.iter().enumerate() {
        match c {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'$' if bytes.get(i + 1) == Some(&b'{') => out.extend_from_slice(b"\\$"),
            _ => out.push(c),
        }
    }
    out.push(b'"');
    out
}

/// How `builtins.fromJSON` ends on a document.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// It is a value.
    Parses,
    /// nlohmann's exception (its message, shown as text).
    Exception(String),
    /// Nix refuses a value in a document that parses so far (an unsigned
    /// number above `i64::MAX`, a string with a NUL byte).
    Refused,
}

/// What the reference `nix` says of `builtins.fromJSON` on `document`.
fn theirs(document: &[u8]) -> Outcome {
    let mut expr = b"builtins.fromJSON ".to_vec();
    expr.extend(nix_string(document));
    let out = Oracle::new()
        .dummy_store()
        .args(["eval", "--expr"])
        .arg(OsString::from_vec(expr))
        .output();
    if out.success() {
        return Outcome::Parses;
    }
    let stderr = out.stderr;
    let marker = b"error: ";
    let at = stderr
        .windows(marker.len())
        .rposition(|w| w == marker)
        .unwrap_or_else(|| panic!("no error in {:?}", String::from_utf8_lossy(&stderr)));
    let mut message = stderr[at + marker.len()..].to_vec();
    while message.last() == Some(&b'\n') {
        message.pop();
    }
    if message.starts_with(b"[json.exception.") {
        Outcome::Exception(String::from_utf8_lossy(&message).into_owned())
    } else {
        Outcome::Refused
    }
}

/// What `brix_store::json` says of `document`, with a handler that
/// refuses what Nix's `JSONSax` refuses.
fn ours(document: &[u8]) -> Outcome {
    struct NixValues;
    impl Sax for NixValues {
        type Error = ();
        fn number_unsigned(&mut self, value: u64) -> Result<(), ()> {
            i64::try_from(value).map(|_| ()).map_err(|_| ())
        }
        fn string(&mut self, value: &[u8]) -> Result<(), ()> {
            if value.contains(&0) {
                return Err(());
            }
            Ok(())
        }
        fn key(&mut self, key: &[u8]) -> Result<(), ()> {
            self.string(key)
        }
    }
    match sax_parse(document, &mut NixValues) {
        Ok(()) => Outcome::Parses,
        Err(SaxError::Parse(e)) => Outcome::Exception(e.message_lossy()),
        Err(SaxError::Handler(())) => Outcome::Refused,
    }
}

/// Every document of `documents` (none holding a NUL byte, which no Nix
/// string can), compared with the reference `nix`, several at a time.
fn same_as_nix(documents: &[Vec<u8>]) {
    let chunk = documents.len().div_ceil(8).max(1);
    std::thread::scope(|scope| {
        for part in documents.chunks(chunk) {
            scope.spawn(move || {
                for document in part {
                    assert_eq!(
                        ours(document),
                        theirs(document),
                        "{:?}",
                        String::from_utf8_lossy(document)
                    );
                }
            });
        }
    });
}

#[test]
fn hand_picked_documents_match_nix() {
    let documents: Vec<&[u8]> = vec![
        b"",
        b" \t\r\n ",
        b"notjson",
        b"[1, tru]",
        b"1 x",
        b"[1,]",
        b"{\"a\" 1}",
        b"{\"a\": 1,}",
        b"{1: 2}",
        b"[1 2]",
        b"{\"a\": 1 \"b\"}",
        b"\"abc",
        b"\"a\\qb\"",
        b"\"\\u12\"",
        b"\"\\u12G4\"",
        b"\"\\uD800\"",
        b"\"\\uD800\\u0041\"",
        b"\"\\uD800x\"",
        b"\"\\uDC00\"",
        b"\"\\uD83D\\uDE00\" x",
        b"\"\\/\\b\\f\\n\\r\\t\" x",
        b"\"a\tb\"",
        b"\"a\nb\"",
        b"\"a\x1fb\"",
        b"\"a\x7fb\" x",
        b"[\n1,\n  x]",
        b"[1,\r\n2,\r\nx]",
        b"-",
        b"-a",
        b"--1",
        b"+1",
        b".5",
        b"01",
        b"1.",
        b"1.e5",
        b"1e",
        b"1e+",
        b"1E+2 x",
        b"0x10",
        b"Infinity",
        b"NaN",
        b"[1e400]",
        b"[-1e400, x]",
        b"[1, 1e400",
        b"1e-400 x",
        b"123456789012345678901234567890 x",
        b"-123456789012345678901234567890 x",
        b"[1]]",
        b"{}}",
        b"tr",
        b"nul",
        b"nulll",
        b"falsey",
        b"[,1]",
        b"[1,2,3",
        b"{\"a\"",
        b"{\"a\":",
        b"{\"a\": {\"b\": [1, {\"c\": nul}]}}",
        b"/* c */ 1",
        b"// c\n1",
        b"\xef\xbb\xbf[1]",
        b"\xef\xbb\xbfx",
        b"\xef\xa3\xbf",
        b"\xef\xbb",
        b"\"\xc3\xa9\" x",
        b"\"\xff\"",
        b"\"\xc3\x28\"",
        b"\"\xe0\x80\x80\"",
        b"\"\xed\xa0\x80\"",
        b"\"\xf0\x8f\xbf\xbf\"",
        b"\"\xf4\x90\x80\x80\"",
        b"\"\xc3",
        b"x\xc3\xa9",
        b"[\"\\u0000\", x]",
        b"[18446744073709551615, x]",
    ];
    same_as_nix(
        &documents
            .into_iter()
            .map(<[u8]>::to_vec)
            .collect::<Vec<_>>(),
    );
}

/// A document cut short at every byte, and with each byte replaced by a
/// few that break it in different places.
#[test]
fn damaged_documents_match_nix() {
    let base: &[u8] = br#"{"a": [1, -2.5e3, "x\u00e9\"", true, null], "b": {"c": 0}}"#;
    let mut documents: Vec<Vec<u8>> = (0..=base.len()).map(|n| base[..n].to_vec()).collect();
    for i in 0..base.len() {
        for c in [b'x', b',', b']', b'"', b'\n'] {
            if base[i] != c {
                let mut damaged = base.to_vec();
                damaged[i] = c;
                documents.push(damaged);
            }
        }
    }
    same_as_nix(&documents);
}
