//! `--profile` on `build`, `develop` and `print-dev-env`
//! (`MixProfile::updateProfile`, `src/libstore/profiles.cc`): the store
//! path becomes the profile's next generation, a numbered GC root beside
//! the profile, and the profile a symlink to that generation. The
//! `nix profile` commands, which edit and delete generations, are Nix's.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use brix::{Session, StorePath};

use crate::commands::build::{add_perm_root, quoting};
use crate::commands::Ctx;
use crate::{CliError, Result};

/// `MixProfile::updateProfile` for one store path: nothing without
/// `--profile`.
pub fn update_profile(ctx: &Ctx<'_>, session: &Session, path: &StorePath) -> Result<()> {
    let Some(profile) = ctx.inv.flag_value("profile") else {
        return Ok(());
    };
    // A `LocalFSStore` only.
    if !session.store_is_local() {
        return Err(CliError::msg(
            "'--profile' is not supported for this Nix store",
        ));
    }
    let profile = PathBuf::from(OsString::from_vec(ctx.abs_path(profile).into()));
    let generation = create_generation(session, &profile, path)?;
    switch_link(&profile, &generation)
}

/// `MixProfile::updateProfile` for what a command built, which must be
/// one store path.
pub fn update_profile_built(ctx: &Ctx<'_>, session: &Session, paths: &[&StorePath]) -> Result<()> {
    if !ctx.inv.has("profile") {
        return Ok(());
    }
    match paths {
        [path] => update_profile(ctx, session, path),
        _ => Err(CliError::usage(format!(
            "'--profile' requires that the arguments produce a single store path, but there are {}",
            paths.len()
        ))),
    }
}

/// `parseName`: the number of the generation link `name`
/// (`<profile>-<number>-link`) of the profile named `profile`. As in Nix,
/// whatever follows `-link` is ignored.
fn parse_name(profile: &[u8], name: &[u8]) -> Option<u64> {
    let rest = name.strip_prefix(profile)?.strip_prefix(b"-")?;
    let end = rest.windows(5).position(|w| w == b"-link")?;
    std::str::from_utf8(&rest[..end]).ok()?.parse().ok()
}

fn read_link(path: &Path) -> Result<PathBuf> {
    std::fs::read_link(path).map_err(|e| {
        brix::Error::io(
            quoting("reading symbolic link ", path.as_os_str().as_bytes(), ""),
            e,
        )
        .into()
    })
}

/// `findGenerations`, as far as `createGeneration` uses it: the newest
/// generation link of `profile`, with its number. The profile's
/// directory must exist, and the profile, if it exists, be a symlink.
fn last_generation(profile: &Path) -> Result<Option<(u64, PathBuf)>> {
    let dir = profile.parent().unwrap_or(Path::new("/"));
    let name = profile.file_name().unwrap_or_default().as_bytes();
    let cannot_read = |e| {
        brix::Error::io(
            quoting("cannot read directory ", dir.as_os_str().as_bytes(), ""),
            e,
        )
    };
    let mut last: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).map_err(cannot_read)? {
        let entry = entry.map_err(cannot_read)?;
        if let Some(n) = parse_name(name, entry.file_name().as_bytes()) {
            if last.as_ref().is_none_or(|(m, _)| n >= *m) {
                last = Some((n, entry.path()));
            }
        }
    }
    // Nix reads the profile's own link here, for the current generation.
    if std::fs::symlink_metadata(profile).is_ok() {
        read_link(profile)?;
    }
    Ok(last)
}

/// `createGeneration`: the generation link of `profile` for `path`, the
/// newest one if it is `path` already, a new GC root numbered one above
/// it if not.
fn create_generation(session: &Session, profile: &Path, path: &StorePath) -> Result<PathBuf> {
    let last = last_generation(profile)?;
    let number = match last {
        Some((number, link)) => {
            let target = session.config().store_dir().display(path);
            if read_link(&link)?.as_os_str().as_bytes() == target.as_bytes() {
                return Ok(link);
            }
            number
        }
        None => 0,
    };
    let mut generation = profile.as_os_str().to_owned();
    generation.push(format!("-{}-link", number + 1));
    add_perm_root(session, path, generation.as_bytes())?;
    Ok(generation.into())
}

/// `switchLink`: `link` becomes a symlink to `target`, by name when they
/// share a directory (they do: a generation is beside its profile),
/// replaced atomically (`replaceSymlink`).
fn switch_link(link: &Path, target: &Path) -> Result<()> {
    let target: &OsStr = match target.file_name() {
        Some(name) if target.parent() == link.parent() => name,
        _ => target.as_os_str(),
    };
    let dir = link.parent().unwrap_or(Path::new("/"));
    let name = link.file_name().unwrap_or_default();
    for n in 0u32.. {
        let mut temp_name = OsString::from(format!(".{n}_"));
        temp_name.push(name);
        let temp = dir.join(temp_name);
        match std::os::unix::fs::symlink(target, &temp) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                let mut what = quoting("creating symlink ", temp.as_os_str().as_bytes(), " -> ");
                what.extend(quoting("", target.as_bytes(), ""));
                return Err(brix::Error::io(what, e).into());
            }
        }
        return std::fs::rename(&temp, link).map_err(|e| {
            // Nix leaves its temporary link behind; brix does not.
            let _ = std::fs::remove_file(&temp);
            let mut what = quoting("renaming ", temp.as_os_str().as_bytes(), " to ");
            what.extend(quoting("", link.as_os_str().as_bytes(), ""));
            brix::Error::io(what, e).into()
        });
    }
    unreachable!("a temporary name is free before u32 runs out")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_names() {
        assert_eq!(parse_name(b"p", b"p-12-link"), Some(12));
        assert_eq!(parse_name(b"p", b"p-12-link.tmp-1-2"), Some(12));
        assert_eq!(parse_name(b"p", b"p"), None);
        assert_eq!(parse_name(b"p", b"p-link"), None);
        assert_eq!(parse_name(b"p", b"p-x-link"), None);
        assert_eq!(parse_name(b"p", b"p-1"), None);
        assert_eq!(parse_name(b"p", b"pq-1-link"), None);
        assert_eq!(parse_name(b"p-1", b"p-1-2-link"), Some(2));
    }

    #[test]
    fn switch_link_replaces_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("p");
        switch_link(&profile, &dir.path().join("p-1-link")).unwrap();
        assert_eq!(std::fs::read_link(&profile).unwrap(), Path::new("p-1-link"));
        switch_link(&profile, &dir.path().join("p-2-link")).unwrap();
        assert_eq!(std::fs::read_link(&profile).unwrap(), Path::new("p-2-link"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
