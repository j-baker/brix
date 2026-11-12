//! Compiled images of fetched trees, through a session: a module that
//! fails to link is reported naming the image file, and only that image's
//! cache file is deleted; an image the user named never is.

use std::path::{Path, PathBuf};

use brix::{Config, Format, Session};
use brix_eval::image::Image;
use brix_store::{CanonPath, FsAccessor, HashAlgo, HashFormat};

/// A tree with one module worth importing, and its NAR hash (SRI).
fn tree(dir: &Path, value: u32) -> String {
    std::fs::write(dir.join("a.nix"), format!("{{ n = {value}; }}\n")).unwrap();
    let (hash, _) =
        brix_store::nar::hash(&FsAccessor::new(dir), &CanonPath::root(), HashAlgo::Sha256).unwrap();
    hash.to_string(HashFormat::Sri, true)
}

/// `(import <tree>/a.nix).n` for the locked trees `trees`, summed.
fn expr(trees: &[(&Path, &str)]) -> String {
    let terms: Vec<String> = trees
        .iter()
        .map(|(dir, hash)| {
            format!(
                "(import \"${{builtins.fetchTree {{ type = \"path\"; path = \"{}\"; narHash = \"{hash}\"; }}}}/a.nix\").n",
                dir.display()
            )
        })
        .collect();
    terms.join(" + ")
}

/// A session compiling every tree into the cache under `cache`, or using
/// the images in `explicit`.
fn session(cache: &Path, explicit: Vec<(String, PathBuf)>) -> Session {
    let mut config = Config::default();
    config.fetch.cache_dir = Some(cache.join("fetch"));
    config.compiled.cache_dir = Some(cache.to_path_buf());
    config.compiled.min_nix_files = 1;
    config.compiled.explicit = explicit;
    Session::new(config).expect("a session")
}

fn cache_files(cache: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(cache.join("compiled"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "brix"))
        .collect();
    files.sort();
    files
}

/// Damages the code of module `a.nix` in the image file at `path`, in a
/// way opening the image does not see (its node count).
fn damage_module(path: &Path) {
    let mut bytes = std::fs::read(path).unwrap();
    let words = {
        let image = Image::from_vec(bytes.clone(), None).expect("the image opens");
        let index = image
            .find_module("a.nix")
            .expect("the path table is intact")
            .expect("the module");
        image.module_words_in_file(index).expect("its words")
    };
    let at = words.start + 4 * 2;
    bytes[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn a_damaged_module_deletes_only_its_own_cache_file() {
    let one = tempfile::tempdir().unwrap();
    let two = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let (hash_one, hash_two) = (tree(one.path(), 40), tree(two.path(), 2));
    let expr = expr(&[(one.path(), &hash_one), (two.path(), &hash_two)]);

    // The first evaluation compiles both trees into the cache.
    let s = session(cache.path(), Vec::new());
    assert_eq!(
        s.eval_expr_to_string(&expr, "/", Format::Nix).unwrap(),
        "42"
    );
    drop(s);
    let files = cache_files(cache.path());
    assert_eq!(files.len(), 2, "{files:?}");
    let nix32_one = brix_store::Hash::parse_any_prefixed(&hash_one, brix_store::XpSettings::NONE)
        .unwrap()
        .to_string(HashFormat::Nix32, false);
    let (damaged, sound): (Vec<PathBuf>, Vec<PathBuf>) = files.into_iter().partition(|p| {
        p.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(&nix32_one)
    });
    let (damaged, sound) = (&damaged[0], &sound[0]);
    damage_module(damaged);

    // The next evaluation fails naming the damaged image, and deletes it
    // alone.
    let s = session(cache.path(), Vec::new());
    let err = s
        .eval_expr_to_string(&expr, "/", Format::Nix)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(&format!(
            "compiled image '{}': corrupt brix image: module 'a.nix': ",
            damaged.display()
        )),
        "{err}"
    );
    drop(s);
    assert!(!damaged.exists(), "the damaged cache file is deleted");
    assert!(sound.exists(), "the other image's cache file is kept");

    // The one after compiles it again.
    let s = session(cache.path(), Vec::new());
    assert_eq!(
        s.eval_expr_to_string(&expr, "/", Format::Nix).unwrap(),
        "42"
    );
    drop(s);
    assert!(damaged.exists());

    // An image the user named fails the same way, named by its path, and
    // is left where it is.
    let named = cache.path().join("named.brix");
    std::fs::copy(damaged, &named).unwrap();
    damage_module(&named);
    let s = session(cache.path(), vec![(hash_one.clone(), named.clone())]);
    let err = s
        .eval_expr_to_string(&expr, "/", Format::Nix)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(&format!("compiled image '{}': ", named.display())),
        "{err}"
    );
    drop(s);
    assert!(named.exists(), "an image the user named is never deleted");
    assert!(damaged.exists() && sound.exists());
}
