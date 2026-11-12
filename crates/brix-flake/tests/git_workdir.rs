//! Local Git working trees: read in place on every fetch, clean or dirty,
//! like Nix's `getAccessorFromWorkdir`. Needs the `git` command; the
//! comparisons with Nix itself are in `brix-cli/tests/cli.rs`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use brix_flake::native::git_workdir::Workdir;
use brix_flake::{fetch_input, FetchSettings, Fetcher, Input, NativeFetcher};
use brix_store::CanonPath;
use brix_testutil::{git, GitRepo};

/// A [`GitRepo`] with what these tests ask of one.
struct Repo(GitRepo);

impl std::ops::Deref for Repo {
    type Target = GitRepo;
    fn deref(&self) -> &GitRepo {
        &self.0
    }
}

impl Repo {
    /// An empty repository on branch `main`.
    fn new() -> Repo {
        Repo(GitRepo::new())
    }

    /// A repository with `a`, `sub/b` and `run.sh` committed.
    fn committed() -> Repo {
        let repo = Repo::new();
        repo.write("a", "a\n");
        repo.write("sub/b", "b\n");
        repo.write("run.sh", "#!/bin/sh\n");
        repo.commit();
        repo
    }

    fn input(&self, settings: &FetchSettings, query: &str) -> Input {
        let url = format!("git+file://{}{query}", self.path().display());
        Input::from_url_str(settings, &url, true).unwrap()
    }

    fn status(&self) -> (bool, Vec<brix_store::BString>) {
        let info = Workdir::open(self.path()).unwrap().info;
        let mut files: Vec<_> = info.files.iter().map(|f| f.rel().to_owned()).collect();
        files.sort();
        (info.is_dirty, files)
    }
}

/// A fetcher with its own cache that records its warnings.
fn fetcher(cache: &Path) -> (NativeFetcher, Arc<Mutex<Vec<String>>>) {
    let warnings = Arc::new(Mutex::new(Vec::new()));
    let sink = warnings.clone();
    let mut fetcher = NativeFetcher::new()
        .unwrap()
        .with_cache_dir(cache.to_path_buf());
    fetcher.warn = Box::new(move |msg| sink.lock().unwrap().push(msg.to_owned()));
    (fetcher, warnings)
}

fn strs(v: &[&str]) -> Vec<brix_store::BString> {
    v.iter().map(|s| (*s).into()).collect()
}

#[test]
fn status_counts_tracked_changes_only() {
    let repo = Repo::committed();
    let all = strs(&["a", "run.sh", "sub/b"]);
    assert_eq!(repo.status(), (false, all.clone()));

    repo.write("untracked", "u\n");
    assert_eq!(
        repo.status(),
        (false, all.clone()),
        "untracked files are invisible"
    );

    repo.write("a", "changed\n");
    assert_eq!(repo.status(), (true, all.clone()));
    repo.git(&["checkout", "-q", "a"]);

    repo.git(&["add", "untracked"]);
    assert_eq!(
        repo.status(),
        (true, strs(&["a", "run.sh", "sub/b", "untracked"]))
    );
    repo.git(&["rm", "-q", "--cached", "untracked"]);

    std::fs::remove_file(repo.path().join("a")).unwrap();
    assert_eq!(repo.status(), (true, strs(&["run.sh", "sub/b"])));
    repo.git(&["checkout", "-q", "a"]);

    repo.git(&["rm", "-q", "sub/b"]);
    assert_eq!(repo.status(), (true, strs(&["a", "run.sh"])));
    repo.git(&["reset", "-q"]);
    repo.git(&["checkout", "-q", "sub/b"]);

    repo.git(&["update-index", "--chmod=+x", "run.sh"]);
    assert!(repo.status().0, "a mode change is a change");
}

#[test]
fn status_of_a_repository_without_commits() {
    let repo = Repo::new();
    let info = Workdir::open(repo.path()).unwrap().info;
    assert!(info.head_rev.is_none());
    assert!(!info.is_dirty);
    assert!(info.files.is_empty());

    repo.write("a", "a\n");
    repo.git(&["add", "a"]);
    let info = Workdir::open(repo.path()).unwrap().info;
    assert!(info.head_rev.is_none());
    assert!(info.is_dirty);
    assert_eq!(
        info.files.into_iter().collect::<Vec<_>>(),
        [CanonPath::new("/a").unwrap()]
    );
}

/// The regression behind this module: a fetch of a local checkout was
/// answered from the cache by reference, so commits and edits made after
/// the first fetch were invisible for `tarball-ttl`.
#[test]
fn every_fetch_sees_the_working_tree_as_it_is_now() {
    let repo = Repo::committed();
    let cache = tempfile::tempdir().unwrap();
    let settings = FetchSettings::default();
    let (fetcher, warnings) = fetcher(cache.path());
    let input = repo.input(&settings, "");

    let clean = fetch_input(&fetcher, &settings, &input).unwrap();
    let rev = clean.input.rev().unwrap().expect("a clean tree has a rev");
    assert_eq!(clean.input.ref_().unwrap(), Some("refs/heads/main"));
    assert_eq!(clean.input.rev_count().unwrap(), Some(1));

    repo.write("a", "edited\n");
    let dirty = fetch_input(&fetcher, &settings, &input).unwrap();
    assert_eq!(dirty.input.rev().unwrap(), None);
    assert_eq!(dirty.input.rev_count().unwrap(), None);
    assert_eq!(dirty.input.ref_().unwrap(), None);
    assert_eq!(
        dirty.input.attrs.get("dirtyRev"),
        Some(&format!("{}-dirty", rev.to_hex()).into())
    );
    assert_eq!(
        dirty.input.attrs.get("dirtyShortRev"),
        Some(&format!("{}-dirty", rev.short()).into())
    );
    assert_eq!(dirty.input.last_modified().unwrap(), Some(1704164645));
    assert_ne!(dirty.nar_hash().unwrap(), clean.nar_hash().unwrap());
    assert_eq!(
        dirty
            .accessor
            .read_file(&CanonPath::new("/a").unwrap())
            .unwrap(),
        b"edited\n"
    );
    assert_eq!(
        *warnings.lock().unwrap(),
        [format!(
            "Git tree '{}' has uncommitted changes",
            repo.path().display()
        )]
    );

    repo.commit();
    let next = fetch_input(&fetcher, &settings, &input).unwrap();
    let next_rev = next.input.rev().unwrap().expect("committed again");
    assert_ne!(next_rev, rev);
    assert_eq!(next.input.rev_count().unwrap(), Some(2));
    assert_eq!(next.nar_hash().unwrap(), dirty.nar_hash().unwrap());
}

#[test]
fn a_clean_tree_is_remembered_by_revision() {
    let repo = Repo::committed();
    let cache = tempfile::tempdir().unwrap();
    let settings = FetchSettings::default();
    let input = repo.input(&settings, "");

    let (first, _) = fetcher(cache.path());
    let hashed = fetch_input(&first, &settings, &input).unwrap();
    // A new fetcher on the same cache knows the hash without hashing: as
    // Nix's `fetchToStore` does, by the tree's fingerprint (`git:<HEAD>`).
    // The fetch itself reports no hash, as Nix's Git fetcher reports none.
    let (second, _) = fetcher(cache.path());
    let known = second.fetch(&settings, &input).unwrap();
    assert!(!known.verified);
    let rev = known
        .input
        .rev()
        .unwrap()
        .expect("a clean tree has its HEAD");
    let fingerprint = format!("git:{}", rev.to_hex());
    assert_eq!(known.accessor.fingerprint(), Some(fingerprint.as_str()));
    assert_eq!(
        second.remembered_nar_hash(&settings, &fingerprint).unwrap(),
        Some(hashed.nar_hash().unwrap())
    );
    assert_eq!(
        fetch_input(&second, &settings, &input)
            .unwrap()
            .nar_hash()
            .unwrap(),
        hashed.nar_hash().unwrap()
    );
    assert_eq!(known.input.rev_count().unwrap(), Some(1));
    // Still read from the working tree, so untracked files stay hidden.
    repo.write("untracked", "u\n");
    let untracked = CanonPath::new("/untracked").unwrap();
    assert!(known.accessor.maybe_lstat(&untracked).unwrap().is_none());
    // A dirty tree is neither looked up nor remembered.
    repo.write("a", "edited\n");
    assert!(second.resolve_cached(&settings, &input).unwrap().is_none());
    assert!(!second.fetch(&settings, &input).unwrap().verified);
}

#[test]
fn dirty_settings_are_honoured() {
    let repo = Repo::committed();
    repo.write("a", "edited\n");
    let cache = tempfile::tempdir().unwrap();
    let (fetcher, warnings) = fetcher(cache.path());

    let quiet = FetchSettings {
        warn_dirty: false,
        ..FetchSettings::default()
    };
    fetch_input(&fetcher, &quiet, &repo.input(&quiet, "")).unwrap();
    assert!(warnings.lock().unwrap().is_empty());

    let strict = FetchSettings {
        allow_dirty: false,
        ..FetchSettings::default()
    };
    let err = fetch_input(&fetcher, &strict, &repo.input(&strict, "")).unwrap_err();
    assert_eq!(
        err.message(),
        format!(
            "Git tree '{}' has uncommitted changes",
            repo.path().display()
        )
    );
}

/// A branch of a local checkout is resolved in the repository each time,
/// not remembered for `tarball-ttl` like a remote one.
#[test]
fn a_local_branch_is_resolved_in_the_repository() {
    let repo = Repo::committed();
    let cache = tempfile::tempdir().unwrap();
    let settings = FetchSettings::default();
    let (fetcher, _) = fetcher(cache.path());
    let input = repo.input(&settings, "?ref=main");

    let first = fetch_input(&fetcher, &settings, &input).unwrap();
    repo.write("a", "next\n");
    repo.commit();
    let second = fetch_input(&fetcher, &settings, &input).unwrap();
    assert_ne!(second.input.rev().unwrap(), first.input.rev().unwrap());
    assert_eq!(second.input.rev_count().unwrap(), Some(2));
}

/// A shallow clone has no `revCount`: without `shallow = true` that is an
/// error, as in Nix.
#[test]
fn a_shallow_clone_needs_shallow_set() {
    let source = Repo::committed();
    source.write("a", "two\n");
    source.commit();
    let dir = tempfile::tempdir().unwrap();
    let clone = dir.path().join("clone");
    git(
        dir.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &format!("file://{}", source.path().display()),
            clone.to_str().unwrap(),
        ],
    );

    let cache = tempfile::tempdir().unwrap();
    let settings = FetchSettings::default();
    let (fetcher, _) = fetcher(cache.path());
    let url = format!("git+file://{}", clone.display());
    let err = fetch_input(
        &fetcher,
        &settings,
        &Input::from_url_str(&settings, &url, true).unwrap(),
    )
    .unwrap_err();
    assert_eq!(
        err.message(),
        format!(
            "'{}' is a shallow Git repository, so 'revCount' is not available",
            clone.display()
        )
    );
    let shallow = Input::from_url_str(&settings, &format!("{url}?shallow=1"), true).unwrap();
    let fetched = fetch_input(&fetcher, &settings, &shallow).unwrap();
    assert!(fetched.input.rev().unwrap().is_some());
    assert_eq!(fetched.input.rev_count().unwrap(), None);
}
