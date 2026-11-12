//! Names and strings that are not UTF-8 where the facade hands out text:
//! refused with a message rather than altered, and kept as bytes where it
//! hands out bytes. Paths are bytes throughout, as in Nix.

use brix::{Config, FetchConfig, Format, Session, Value};

/// A one-byte string that is not UTF-8: the first byte of "é".
const BYTE: &str = r#"(builtins.substring 0 1 "é")"#;

fn session() -> Session {
    let config = Config {
        fetch: FetchConfig {
            enabled: false,
            ..FetchConfig::default()
        },
        ..Config::default()
    };
    Session::new(config).unwrap()
}

#[test]
fn cursor_text_refuses_what_is_not_utf8() {
    let session = session();
    let expr = format!(
        r#"{{ s = {BYTE}; l = [ "a" {BYTE} ]; n = builtins.listToAttrs [ {{ name = {BYTE}; value = 1; }} ]; }}"#
    );
    let cursor = session.eval_expr_cursor(&expr, "/").unwrap();
    let s = cursor.get_attr("s").unwrap();
    assert_eq!(
        s.get_string().unwrap_err().to_string(),
        "a string at 's' is not valid UTF-8 ('\u{fffd}'), which brix does not support here"
    );
    assert_eq!(s.get_bytes_with_context().unwrap().0, b"\xc3");
    assert!(cursor.get_attr("l").unwrap().get_list_of_strings().is_err());
    let n = cursor.get_attr("n").unwrap();
    assert_eq!(
        n.get_attrs().unwrap_err().to_string(),
        "an attribute name at 'n' is not valid UTF-8 ('\u{fffd}'), which brix does not support here"
    );
    // An owned copy keeps the name's bytes.
    let Value::Attrs(attrs) = n.to_owned().unwrap() else {
        panic!("a set");
    };
    assert_eq!(attrs.keys().collect::<Vec<_>>(), [&b"\xc3".to_vec()]);
}

/// A path whose name is not UTF-8 is kept as it is, as in Nix (`/a +
/// "\xff"` is the path `/a\xff`, canonicalised byte by byte): owned,
/// rendered, coerced, and as the directory relative paths resolve
/// against.
#[test]
fn path_values_keep_their_bytes() {
    let session = session();
    // The bytes are raw in the source, as in a Latin-1 file.
    let expr: &[u8] =
        b"{ p = /a + \"\xff\"; up = /a + \"/\xff/..\"; dir = dirOf (/a + \"\xe9/b\"); }";
    let Value::Attrs(attrs) = session.eval_expr(expr, "/").unwrap() else {
        panic!("a set");
    };
    assert_eq!(attrs[b"p".as_slice()], Value::Path(b"/a\xff".into()));
    assert_eq!(attrs[b"up".as_slice()], Value::Path("/a".into()));
    assert_eq!(attrs[b"dir".as_slice()], Value::Path(b"/a\xe9".into()));

    let p: &[u8] = b"/a + \"\xff\"";
    let cursor = session.eval_expr_cursor(p, "/").unwrap();
    assert_eq!(cursor.render_bytes(Format::Nix).unwrap(), b"/a\xff");
    let (_, path, shown) = cursor.coerce_to_source_path().unwrap();
    assert_eq!(path.as_bytes(), b"/a\xff");
    assert_eq!(shown, b"/a\xff");

    // A relative path literal resolves against a base directory whose
    // name is bytes.
    let rel = session.eval_expr("./x", b"/d\xe9".as_slice()).unwrap();
    assert_eq!(rel, Value::Path(b"/d\xe9/x".into()));
}

/// A file read in place whose name and directory are not UTF-8: imported,
/// its relative paths resolved in its directory, and its sibling read,
/// all as bytes (the file's directory is allowed, as `--file` allows it).
#[test]
fn files_with_byte_names_are_read_in_place() {
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join(std::ffi::OsStr::from_bytes(b"d\xe9"));
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(
        sub.join(std::ffi::OsStr::from_bytes(b"caf\xe9.nix")),
        b"{ here = ./.; sibling = builtins.readFile (./. + \"/\xff\"); names = builtins.attrNames (builtins.readDir ./.); }\n",
    )
    .unwrap();
    std::fs::write(sub.join(std::ffi::OsStr::from_bytes(b"\xff")), b"bytes").unwrap();
    let config = Config {
        fetch: FetchConfig {
            enabled: false,
            ..FetchConfig::default()
        },
        allowed_paths: vec![sub.clone()],
        ..Config::default()
    };
    let session = Session::new(config).unwrap();
    let sub_bytes = sub.as_os_str().as_bytes();
    let file = [sub_bytes, b"/caf\xe9.nix"].concat();
    let Value::Attrs(attrs) = session.eval_file_cursor(&file).unwrap().to_owned().unwrap() else {
        panic!("a set");
    };
    assert_eq!(attrs[b"here".as_slice()], Value::Path(sub_bytes.into()));
    assert_eq!(
        attrs[b"sibling".as_slice()],
        Value::String {
            bytes: b"bytes".to_vec(),
            context: Vec::new()
        }
    );
    let string = |s: &[u8]| Value::String {
        bytes: s.to_vec(),
        context: Vec::new(),
    };
    // Unsigned byte order, as Nix's `readDir` gives them.
    assert_eq!(
        attrs[b"names".as_slice()],
        Value::List(vec![string(b"caf\xe9.nix"), string(b"\xff")])
    );
}

#[test]
fn flake_references_that_are_not_utf8_are_refused() {
    let session = session();
    for (expr, builtin) in [
        (
            format!("builtins.parseFlakeRef {BYTE}"),
            "builtins.parseFlakeRef",
        ),
        (
            format!(r#"builtins.flakeRefToString {{ type = "path"; path = {BYTE}; }}"#),
            "builtins.flakeRefToString",
        ),
    ] {
        let err = session
            .eval_expr_to_string(&expr, "/", Format::Nix)
            .expect_err(&expr)
            .render(false);
        assert!(
            err.contains(&format!(
                "' given to '{builtin}' is not valid UTF-8; brix does not support non-UTF-8 flake references"
            )),
            "{expr}: {err}"
        );
    }
}

/// `flake.nix` is a Latin-1 file here, so the input's name is one byte.
#[test]
fn flake_attribute_names_that_are_not_utf8_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        b"{ inputs.\"\xff\".url = \"path:/x\"; outputs = _: { }; }\n",
    )
    .unwrap();
    // Reading a local flake goes through the fetchers, with a cache of
    // its own.
    let config = Config {
        fetch: FetchConfig {
            cache_dir: Some(dir.path().join("cache")),
            ..FetchConfig::default()
        },
        ..Config::default()
    };
    let session = Session::new(config).unwrap();
    let flake_ref = brix_flake::parse_flake_ref(
        &session.fetch_settings(),
        &format!("path:{}", dir.path().display()),
        &brix_flake::ParseOptions::flake(),
    )
    .unwrap();
    let Err(err) = session.flake(&flake_ref) else {
        panic!("the flake was read");
    };
    let err = err.to_string();
    assert!(
        err.contains("' in a flake is not valid UTF-8, which brix does not support"),
        "{err}"
    );
}
