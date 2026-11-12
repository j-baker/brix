//! `brix help`, `brix help-stores`, `brix --version`, `brix __dump-cli`
//! and the development aid `brix __parse`.

use serde_json::json;

use crate::args::{self, Command};
use crate::commands::{print_stdout, print_stdout_bytes, Ctx};
use crate::{CliError, Result};

/// `nix --version`: `nix (Nix) 2.35.3`; brix names itself and the Nix it
/// is compatible with.
pub fn version() -> String {
    format!(
        "brix (brix {}, compatible with Nix 2.35.3) 2.35.3",
        env!("CARGO_PKG_VERSION")
    )
}

/// `__dump-cli`: the whole command tree as JSON, in the shape `nix` uses
/// (`args`, plus the `stores` and `fetchers` catalogues, which brix leaves
/// empty: it has no store implementations of its own to document).
pub fn dump_cli(root: &Command) -> String {
    json!({
        "args": args::dump_command(root),
        "stores": {},
        "fetchers": {},
    })
    .to_string()
}

pub fn help(ctx: &Ctx<'_>) -> Result<()> {
    let mut path: Vec<String> = ctx.inv.arg("subcommand").to_vec();
    if path.len() == 1 {
        if let Some(alias) = crate::tree::aliases().iter().find(|a| a.name == path[0]) {
            path = alias.replacement.clone();
        }
    }
    // Nix renders the page from its command tree as parsed so far: the
    // root has taken `help` as its subcommand and expects no more
    // arguments, so its own page shows none.
    let parsed_root;
    let root = if path.is_empty() {
        let mut root = ctx.root.clone();
        root.args.clear();
        parsed_root = root;
        &parsed_root
    } else {
        ctx.root
    };
    match args::render_help(root, &path, "brix") {
        Some(text) => print_stdout(&text),
        None => Err(CliError::usage(format!(
            "brix has no subcommand '{}'",
            path.join("")
        ))),
    }
}

pub fn help_stores(_ctx: &Ctx<'_>) -> Result<()> {
    print_stdout(
        "Name\n\nbrix help-stores - show help about store types and their settings\n\n\
         Description\n\n\
         brix does not implement stores itself; it talks to a Nix daemon. The\n\
         store URIs it accepts are:\n\n\
           · daemon — the local daemon at /nix/var/nix/daemon-socket/socket\n\
           · unix:///path/to/socket — a daemon at another socket\n\
           · ssh-ng://[user@]host — a daemon on another machine, via ssh\n\
           · dummy:// — no store: evaluation only\n",
    )
}

/// `__parse FILE`: the syntax tree as `nix-instantiate --parse` prints it.
pub fn parse(ctx: &Ctx<'_>) -> Result<()> {
    let file = ctx
        .inv
        .arg_one("file")
        .expect("the parser requires the argument");
    // Bytes, as the current directory's name is: the file is parsed in its
    // directory and named as it is.
    let abs = ctx.abs_path(file);
    let abs: &[u8] = abs.as_ref();
    let src = std::fs::read(crate::commands::realise::fs_path(abs))
        .map_err(|e| brix::Error::io(format!("reading '{file}'"), e))?;
    // `dirOf`: everything before the last `/` (`abs` is absolute).
    let dir = match abs.iter().rposition(|&b| b == b'/') {
        Some(0) | None => &b"/"[..],
        Some(slash) => &abs[..slash],
    };
    let origin = brix_syntax::Origin::Path(abs.into());
    let mut warnings = Vec::new();
    let parsed = brix_syntax::parse(
        &src,
        dir,
        &brix_syntax::ParseOptions::default(),
        &mut warnings,
    );
    for w in &warnings {
        brix::log::stderr_bytes(&w.render(&origin, &src));
    }
    let expr = parsed.map_err(|e| {
        // The rendering is complete, `error: ` prefix and excerpt, and ends
        // in the newline the logger adds.
        let rendered = e.render(&origin, &src);
        brix::log::stderr_bytes(rendered.strip_suffix(b"\n").unwrap_or(&rendered));
        CliError::Exit(1)
    })?;
    let mut shown = brix_syntax::show_bytes(&expr);
    shown.push(b'\n');
    print_stdout_bytes(&shown)
}
