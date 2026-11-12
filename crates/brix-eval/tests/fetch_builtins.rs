//! `fetchurl`, `fetchTarball`, `fetchGit`, `fetchTree` and `fetchClosure`:
//! the argument checks Nix makes before fetching anything, compared with
//! the reference `nix-instantiate`, and the handling of what a host's
//! fetcher returns, with a host that serves canned trees and files.

mod common;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use brix_eval::{
    Arena, Config, Evaluator, FetchAttr, FetchError, FetchRequest, FetchedFile, FetchedTree, Host,
};
use brix_store::{
    CanonPath, FileIngestionMethod, FixedOutputInfo, Hash, HashAlgo, HashFormat, MemoryAccessor,
    SourceAccessor, StoreDir, StorePath,
};
use bstr::{BString, ByteSlice};
use common::{run_brix_expr, run_nix_expr, Format, TestHost};

/// `expr` gives the same output and errors in brix and in Nix (pure
/// mode, no store).
fn same_as_nix(expr: &str, experimental: &[&str]) {
    let pwd = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let host = TestHost::with_allowed(&[("/pwd", pwd.path()), ("/nix/store", store.path())], &[]);
    let config = Config {
        eval_cores: 0,
        experimental_features: experimental.iter().map(|f| (*f).to_owned()).collect(),
        ..Config::default()
    };
    let ours = run_brix_expr(&host, config, expr, "/pwd", Format::Plain);
    let features = experimental.join(" ");
    let extra: Vec<&str> = if experimental.is_empty() {
        Vec::new()
    } else {
        vec!["--extra-experimental-features", &features]
    };
    let theirs = run_nix_expr(expr, pwd.path(), Format::Plain, &extra, false);
    assert_eq!(ours, theirs, "{expr}");
}

#[test]
fn fetch_tree_arguments_are_checked_as_in_nix() {
    for expr in [
        r#"builtins.fetchGit { url = "https://example.invalid/x"; type = "git"; }"#,
        r#"builtins.fetchTree { type = "git"; url = "https://example.invalid/x"; name = "foo"; }"#,
        r#"builtins.fetchTree { type = "github"; owner = "a"; repo = "b"; revCount = -1; }"#,
        r#"builtins.fetchTree { type = "git"; url = "https://example.invalid/x"; x = [ ]; }"#,
        r#"builtins.fetchTree { type = "git"; url = "https://example.invalid/x"; narHash = 1.5; }"#,
        r#"builtins.fetchTree { url = "https://example.invalid/x"; }"#,
    ] {
        same_as_nix(expr, &[]);
    }
}

#[test]
fn fetchurl_and_fetch_tarball_arguments_are_checked_as_in_nix() {
    for who in ["fetchurl", "fetchTarball"] {
        for args in [
            r#"{ url = "https://example.invalid/x"; foo = 1; }"#,
            r#"{ sha256 = ""; }"#,
            r#"{ url = "https://example.invalid/x"; }"#,
            r#""https://example.invalid/x""#,
            r#"{ url = "https://example.invalid/x"; sha256 = "sha512-abc"; }"#,
            r#"{ url = "https://example.invalid/x"; name = "a b"; sha256 = ""; }"#,
        ] {
            same_as_nix(&format!("builtins.{who} {args}"), &[]);
        }
    }
    // The other two hints for an invalid name, which only `fetchurl`'s
    // default (the URL's last component) can be: in an attribute set, in
    // a string.
    same_as_nix(
        r#"builtins.fetchurl { url = "https://example.invalid/a b"; sha256 = ""; }"#,
        &[],
    );
    same_as_nix(r#"builtins.fetchurl "https://example.invalid/a b""#, &[]);
    // An unsupported argument is named as it is, UTF-8 or not.
    for who in ["fetchurl", "fetchTarball"] {
        same_as_nix(
            &format!(
                r#"builtins.{who} {{ url = "https://example.invalid/x"; ${{builtins.substring 0 1 "é"}} = 1; }}"#
            ),
            &[],
        );
    }
}

/// Fetcher arguments that are not UTF-8 are refused, never altered
/// (`KNOWN_DEVIATIONS.md`): the fetchers take text in brix.
#[test]
fn fetcher_arguments_that_are_not_utf8_are_refused() {
    let host = FetchHost::default();
    let l = r#"(builtins.substring 0 1 "é")"#;
    for (expr, what) in [
        (
            format!(r#"builtins.fetchTree {{ type = "git"; url = "https://x/" + {l}; }}"#),
            "argument 'url' to 'fetchTree'",
        ),
        (
            format!(r#"builtins.fetchTree {{ type = "git" + {l}; }}"#),
            "argument 'type' to 'fetchTree'",
        ),
        (
            format!(r#"builtins.fetchGit ("https://x/" + {l})"#),
            "the URL passed to 'fetchGit'",
        ),
        (
            format!(r#"builtins.fetchurl {{ url = "https://x/" + {l}; }}"#),
            "argument 'url' to 'fetchurl'",
        ),
        (
            format!(r#"builtins.fetchTarball {{ url = "https://x/f"; name = "a" + {l}; }}"#),
            "argument 'name' to 'fetchTarball'",
        ),
    ] {
        let err = eval(&host, &expr, &[]).unwrap_err();
        assert_eq!(
            err,
            format!("{what} is not valid UTF-8; brix does not support non-UTF-8 fetcher arguments"),
            "{expr}"
        );
    }
    let err = eval(
        &host,
        &format!(r#"builtins.fetchTree {{ type = "git"; ${{{l}}} = "x"; }}"#),
        &[],
    )
    .unwrap_err();
    assert_eq!(
        err,
        b"the name of argument '\xc3' to 'fetchTree' is not valid UTF-8; brix does not support non-UTF-8 fetcher arguments"
            .as_bstr()
    );
}

#[test]
fn to_json_rejects_invalid_utf8_as_in_nix() {
    for expr in [
        r#"builtins.toJSON (builtins.substring 0 1 "é")"#,
        r#"builtins.toJSON [ "a" ((builtins.substring 0 1 "é") + "A") ]"#,
        r#"builtins.toJSON { x = builtins.substring 1 1 "é"; }"#,
    ] {
        same_as_nix(expr, &[]);
    }
}

/// `fetchClosure` is `__fetchClosure` in the global scope, as in Nix
/// (`primops/fetchClosure.cc`), and only with its experimental feature.
#[test]
fn fetch_closure_is_registered_as_in_nix() {
    for expr in [
        "[ (builtins ? fetchClosure) (builtins.typeOf __fetchClosure) ]",
        "fetchClosure",
    ] {
        same_as_nix(expr, &["fetch-closure"]);
    }
    same_as_nix("builtins ? fetchClosure", &[]);
    let r = eval(
        &FetchHost::default(),
        r#"builtins.fetchClosure { fromStore = "https://cache.nixos.org"; fromPath = /nix/store/x; }"#,
        &["fetch-closure"],
    );
    assert!(r
        .unwrap_err()
        .contains_str("brix does not implement 'builtins.fetchClosure'"),);
}

/// A host whose fetchers serve canned trees and files by URL, and record
/// what they were asked for.
#[derive(Default)]
struct FetchHost {
    root: MemoryAccessor,
    trees: BTreeMap<String, (MemoryAccessor, BTreeMap<String, FetchAttr>)>,
    /// URLs whose trees fail to fetch, and how.
    failures: BTreeMap<String, FetchError>,
    files: BTreeMap<String, Vec<u8>>,
    valid: BTreeSet<StorePath>,
    tree_requests: RefCell<Vec<FetchRequest>>,
    file_requests: RefCell<Vec<String>>,
    warnings: RefCell<Vec<String>>,
}

fn sha256(bytes: &[u8]) -> Hash {
    Hash::of(HashAlgo::Sha256, bytes)
}

fn fixed_output(name: &str, method: FileIngestionMethod, hash: Hash) -> StorePath {
    StoreDir::default()
        .make_fixed_output_path(
            name,
            &FixedOutputInfo {
                method,
                hash,
                references: Default::default(),
            },
        )
        .unwrap()
}

impl Host for FetchHost {
    fn root_accessor(&self) -> Arc<dyn SourceAccessor> {
        Arc::new(self.root.clone())
    }

    fn allowed_paths(&self) -> Vec<CanonPath> {
        vec![CanonPath::new("/src").unwrap()]
    }

    fn warn(&self, msg: &[u8]) {
        self.warnings
            .borrow_mut()
            .push(String::from_utf8(msg.to_vec()).expect("the warnings in these tests are text"));
    }

    fn is_valid_path(&self, path: &StorePath) -> Result<bool, String> {
        Ok(self.valid.contains(path))
    }

    fn fetch_tree(&self, request: &FetchRequest) -> Result<FetchedTree, FetchError> {
        self.tree_requests.borrow_mut().push(request.clone());
        let Some(FetchAttr::String(url)) = request.attrs.get("url") else {
            return Err("no url".into());
        };
        if let Some(failure) = self.failures.get(url) {
            return Err(failure.clone());
        }
        let Some((tree, extra)) = self.trees.get(url) else {
            return Err(format!("unable to download '{url}': canned failure").into());
        };
        let (hash, _) = brix_store::nar::hash(tree, &CanonPath::root(), HashAlgo::Sha256).unwrap();
        let name = match request.attrs.get("name") {
            Some(FetchAttr::String(n)) => n.as_str(),
            _ => "source",
        };
        let mut attrs = extra.clone();
        attrs.insert(
            "narHash".into(),
            FetchAttr::String(hash.to_string(HashFormat::Sri, true)),
        );
        Ok(FetchedTree {
            accessor: Arc::new(tree.clone()),
            store_path: fixed_output(name, FileIngestionMethod::NixArchive, hash),
            attrs,
            locked_attrs: BTreeMap::new(),
            locked_url: url.clone(),
            display_prefix: None,
        })
    }

    fn fetch_file(&self, url: &str, name: &str) -> Result<FetchedFile, String> {
        self.file_requests.borrow_mut().push(url.to_owned());
        let contents = self
            .files
            .get(url)
            .ok_or_else(|| format!("unable to download '{url}': canned failure"))?;
        Ok(FetchedFile {
            contents: contents.clone(),
            store_path: fixed_output(name, FileIngestionMethod::Flat, sha256(contents)),
        })
    }
}

/// `expr` deeply forced and printed, or its error without traces.
fn eval(host: &FetchHost, expr: &str, experimental: &[&str]) -> Result<BString, BString> {
    let arena = Arena::new();
    let config = Config {
        experimental_features: experimental.iter().map(|f| (*f).to_owned()).collect(),
        ..Config::default()
    };
    let ev = Evaluator::new(&arena, config, host);
    ev.eval_str(expr.as_bytes(), "/src")
        .and_then(|v| {
            let cell = ev.alloc_cell(v);
            ev.force_deep(cell)?;
            ev.print_ambiguous(cell)
        })
        .map_err(|e| e.msg.clone())
}

fn store_text(sp: &StorePath) -> String {
    StoreDir::default().display(sp)
}

#[test]
fn fetchurl_mounts_the_file_it_downloaded() {
    let mut host = FetchHost::default();
    host.files.insert("https://x/f".into(), b"hello".to_vec());
    let h = sha256(b"hello").to_string(HashFormat::Sri, true);
    let out = eval(
        &host,
        &format!(r#"let p = builtins.fetchurl {{ url = "https://x/f"; sha256 = "{h}"; }}; in [ p (builtins.readFile p) ]"#),
        &[],
    )
    .unwrap();
    let sp = fixed_output("f", FileIngestionMethod::Flat, sha256(b"hello"));
    assert_eq!(out, format!(r#"[ "{}" "hello" ]"#, store_text(&sp)));
}

#[test]
fn fetchurl_rejects_a_hash_mismatch_with_exit_status_102() {
    let mut host = FetchHost::default();
    host.files.insert("https://x/f".into(), b"hello".to_vec());
    let h = sha256(b"other");
    let arena = Arena::new();
    let ev = Evaluator::new(&arena, Config::default(), &host);
    let err = ev
        .eval_str(
            format!(
                r#"builtins.fetchurl {{ url = "https://x/f"; sha256 = "{}"; }}"#,
                h.to_string(HashFormat::Sri, true)
            )
            .as_bytes(),
            "/src",
        )
        .unwrap_err();
    assert_eq!(
        err.msg,
        format!(
            "hash mismatch in file downloaded from 'https://x/f':\n  specified: {}\n  got:       {}",
            h.to_string(HashFormat::Nix32, true),
            sha256(b"hello").to_string(HashFormat::Nix32, true)
        )
    );
    assert_eq!(err.exit_status.get(), 102);
}

/// Nix tries the expected path first (`ensurePath`) and downloads only
/// when it is not there; a failed download is an error.
#[test]
fn a_pinned_fetch_uses_a_valid_path_and_otherwise_fails_with_the_fetcher() {
    let hash = sha256(b"hello");
    let h = hash.to_string(HashFormat::Sri, true);
    let file_path = fixed_output("f", FileIngestionMethod::Flat, hash);
    let tree_path = fixed_output("source", FileIngestionMethod::NixArchive, hash);
    let fetchurl = format!(r#"builtins.fetchurl {{ url = "https://x/f"; sha256 = "{h}"; }}"#);
    let fetch_tarball =
        format!(r#"builtins.fetchTarball {{ url = "https://x/t.tar"; sha256 = "{h}"; }}"#);

    let mut host = FetchHost::default();
    host.valid.insert(file_path.clone());
    host.valid.insert(tree_path.clone());
    assert_eq!(
        eval(&host, &fetchurl, &[]).unwrap(),
        format!("\"{}\"", store_text(&file_path))
    );
    assert_eq!(
        eval(&host, &fetch_tarball, &[]).unwrap(),
        format!("\"{}\"", store_text(&tree_path))
    );
    assert!(host.file_requests.borrow().is_empty());
    assert!(host.tree_requests.borrow().is_empty());

    let host = FetchHost::default();
    assert_eq!(
        eval(&host, &fetchurl, &[]).unwrap_err(),
        "unable to download 'https://x/f': canned failure"
    );
    assert_eq!(
        eval(&host, &fetch_tarball, &[]).unwrap_err(),
        "unable to download 'https://x/t.tar': canned failure"
    );
    assert!(host.warnings.borrow().is_empty());
}

/// A path the evaluation already added to its store counts as there.
#[test]
fn a_pinned_fetch_uses_a_path_already_in_the_virtual_store() {
    let mut host = FetchHost::default();
    host.root
        .add_file(&CanonPath::new("/src/f").unwrap(), "hello", false)
        .unwrap();
    let h = sha256(b"hello").to_string(HashFormat::Sri, true);
    let out = eval(
        &host,
        &format!(
            r#"let added = builtins.path {{ path = /src/f; name = "f"; recursive = false; }};
                   fetched = builtins.fetchurl {{ url = "https://x/f"; sha256 = "{h}"; }};
               in builtins.seq added [ (added == fetched) (builtins.readFile fetched) ]"#
        ),
        &[],
    )
    .unwrap();
    assert_eq!(out, r#"[ true "hello" ]"#);
    assert!(host.file_requests.borrow().is_empty());
}

/// `fetchGit` asks for `exportIgnore` unless told otherwise or fetching
/// submodules; `fetchTree` never does.
#[test]
fn fetch_git_defaults_export_ignore() {
    let host = FetchHost::default();
    let cases = [
        (r#"builtins.fetchGit "https://x/r""#, Some(true)),
        (r#"builtins.fetchGit { url = "https://x/r"; }"#, Some(true)),
        (
            r#"builtins.fetchGit { url = "https://x/r"; submodules = true; }"#,
            None,
        ),
        (
            r#"builtins.fetchGit { url = "https://x/r"; submodules = false; }"#,
            Some(true),
        ),
        (
            r#"builtins.fetchGit { url = "https://x/r"; exportIgnore = false; }"#,
            Some(false),
        ),
        (
            r#"builtins.fetchTree { type = "git"; url = "https://x/r"; }"#,
            None,
        ),
    ];
    for (expr, _) in cases {
        assert!(eval(&host, expr, &[]).is_err(), "{expr}");
    }
    let got: Vec<Option<bool>> = host
        .tree_requests
        .borrow()
        .iter()
        .map(|r| match r.attrs.get("exportIgnore") {
            Some(FetchAttr::Bool(b)) => Some(*b),
            None => None,
            other => panic!("exportIgnore = {other:?}"),
        })
        .collect();
    let want: Vec<Option<bool>> = cases.iter().map(|(_, e)| *e).collect();
    assert_eq!(got, want);
}

/// `emitTreeAttrs`: the host computes the attributes (`fetchGit` asks it
/// for the empty-revision fallback, `fetchTree` does not) and the
/// evaluator returns them as they are, beside `outPath`.
#[test]
fn fetch_results_are_the_hosts_tree_attributes() {
    let mut tree = MemoryAccessor::new();
    tree.add_file(&CanonPath::new("/f").unwrap(), "x", false)
        .unwrap();
    let rev = "0123456789abcdef0123456789abcdef01234567";
    let attrs: BTreeMap<String, FetchAttr> = [
        ("rev".to_owned(), FetchAttr::String(rev.into())),
        ("shortRev".to_owned(), FetchAttr::String(rev[..7].into())),
        ("revCount".to_owned(), FetchAttr::Int(5)),
        ("submodules".to_owned(), FetchAttr::Bool(false)),
    ]
    .into();
    let mut host = FetchHost::default();
    host.trees.insert("https://x/r".into(), (tree, attrs));
    let show = r#"r: [ r.rev r.shortRev r.revCount r.submodules (builtins.attrNames r) ]"#;
    let names = r#"[ "narHash" "outPath" "rev" "revCount" "shortRev" "submodules" ]"#;
    for expr in [
        r#"builtins.fetchGit "https://x/r""#,
        r#"builtins.fetchTree { type = "git"; url = "https://x/r"; }"#,
    ] {
        assert_eq!(
            eval(&host, &format!("({show}) ({expr})"), &[]).unwrap(),
            format!(r#"[ "{rev}" "0123456" 5 false {names} ]"#),
            "{expr}"
        );
    }
    let fallback: Vec<bool> = host
        .tree_requests
        .borrow()
        .iter()
        .map(|r| r.empty_rev_fallback)
        .collect();
    assert_eq!(fallback, [true, false]);
}

/// A failed fetch is the host's error with the traces the fetchers added
/// to it, below "while calling the 'fetchTree' builtin", ending the
/// process with the fetchers' exit status; nlohmann's exception is shown
/// alone.
#[test]
fn a_failed_fetch_carries_the_fetchers_traces() {
    let mut host = FetchHost::default();
    host.failures.insert(
        "https://x/r".into(),
        FetchError {
            message: "NAR hash mismatch".into(),
            traces: vec!["while fetching the input 'git+https://x/r'".into()],
            exit_status: 102,
            foreign: false,
        },
    );
    host.failures.insert(
        "https://x/j".into(),
        FetchError {
            message: "[json.exception.parse_error.101] parse error".into(),
            traces: Vec::new(),
            exit_status: 1,
            foreign: true,
        },
    );
    let arena = Arena::new();
    let ev = Evaluator::new(&arena, Config::default(), &host);
    let fetch = |url: &str| {
        let expr = format!(r#"builtins.fetchTree {{ type = "git"; url = "{url}"; }}"#);
        let err = ev.eval_str(expr.as_bytes(), "/src").map(drop).unwrap_err();
        let shown = String::from_utf8(err.render(false).to_vec()).unwrap();
        (err.exit_status.get(), shown)
    };
    let (status, shown) = fetch("https://x/r");
    assert_eq!(status, 102);
    let builtin = shown
        .find("… while calling the 'fetchTree' builtin")
        .expect(&shown);
    let fetching = shown
        .find("… while fetching the input 'git+https://x/r'\n\n       error: NAR hash mismatch")
        .expect(&shown);
    assert!(builtin < fetching, "{shown}");
    let (status, shown) = fetch("https://x/j");
    assert_eq!(status, 1);
    assert_eq!(shown, "error: [json.exception.parse_error.101] parse error");
}

/// A tree whose reported `narHash` does not parse cannot be mounted.
#[test]
fn mounting_a_tree_with_a_malformed_nar_hash_fails() {
    let host = FetchHost::default();
    let arena = Arena::new();
    let ev = Evaluator::new(&arena, Config::default(), &host);
    let tree = FetchedTree {
        accessor: Arc::new(MemoryAccessor::new()),
        store_path: fixed_output("source", FileIngestionMethod::NixArchive, sha256(b"")),
        attrs: [(
            "narHash".to_owned(),
            FetchAttr::String("sha256-nonsense".into()),
        )]
        .into(),
        locked_attrs: BTreeMap::new(),
        locked_url: "https://x/t".into(),
        display_prefix: None,
    };
    let err = ev.mount_fetched_tree(&tree).unwrap_err();
    assert!(err.msg.starts_with_str("invalid SRI hash"), "{}", err.msg);
    // Evaluation goes on.
    let v = ev.eval_str(b"1", "/").unwrap();
    assert_eq!(ev.print_ambiguous(ev.alloc_cell(v)).unwrap(), "1");
}

/// A missing path in a fetched tree is named under the tree's `«url»`,
/// as Nix names it, whatever looks for it: `builtins.path` and
/// `filterSource` find it missing through the tree's own `lstat`, whose
/// message names the path as the tree's storage shows it.
#[test]
fn missing_paths_in_a_fetched_tree_are_shown_under_its_url() {
    let mut host = FetchHost::default();
    let mut tree = MemoryAccessor::new();
    tree.add_file(&CanonPath::new("/a").unwrap(), "x", false)
        .unwrap();
    let (hash, _) = brix_store::nar::hash(&tree, &CanonPath::root(), HashAlgo::Sha256).unwrap();
    host.trees
        .insert("https://x/t".into(), (tree, BTreeMap::new()));
    let fetch = format!(
        r#"builtins.fetchTarball {{ url = "https://x/t"; sha256 = "{}"; }}"#,
        hash.to_string(HashFormat::Sri, true)
    );
    for read in [
        r#"builtins.path { path = t + "/missing"; }"#,
        r#"builtins.filterSource (p: t: true) (t + "/missing")"#,
        r#"builtins.readFile (t + "/missing")"#,
    ] {
        let expr = format!("let t = {fetch}; in {read}");
        assert_eq!(
            eval(&host, &expr, &[]).unwrap_err(),
            "path '«https://x/t»/missing' does not exist",
            "{read}"
        );
    }
}
