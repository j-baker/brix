//! The command-line model: a tree of commands with flags and positional
//! arguments, parsed the way Nix's `Args`/`MultiCommand` classes parse
//! `argv` (`src/libutil/args.cc`), and rendered as `__dump-cli` JSON and
//! `--help` text.
//!
//! The model is data, not code: `tree.rs` builds it from the inventory of
//! the real `nix` CLI, so the structure (names, aliases, arities, labels,
//! categories) is `nix`'s by construction and a test compares the two.
//!
//! Arguments are bytes, as Nix's (`std::string`) are: the parser works on
//! them as given, and the [`Invocation`] keeps each value both as given
//! and as text. Most of what brix reads is text; a value that is not
//! UTF-8 is refused before a command reads it as text (`main`), unless
//! the command reads it as bytes (`commands::BYTE_FLAGS`).

use std::collections::BTreeMap;

use bstr::{BStr, BString, ByteSlice};
use serde_json::{json, Map, Value};

/// How many values a flag or positional argument takes. `Any` collects
/// everything that is left (Nix's `ArityAny`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arity {
    Fixed(usize),
    Any,
}

/// A `--long`/`-s` option.
#[derive(Clone, Debug)]
pub struct Flag {
    pub long: String,
    pub short: Option<char>,
    /// Other long names that select this flag (`--no-net` for `--offline`).
    pub aliases: Vec<String>,
    /// Names of the values, one per value for fixed arities.
    pub labels: Vec<String>,
    pub arity: Arity,
    /// The option category the help groups it under; `""` is the
    /// command's own options.
    pub category: String,
    /// Categories `--help` does not list (the `nix.conf` setting
    /// overrides).
    pub hidden_category: bool,
    pub experimental: Option<String>,
    pub description: String,
}

/// A positional argument.
#[derive(Clone, Debug)]
pub struct Arg {
    pub label: String,
    pub optional: bool,
    pub arity: Arity,
}

/// Where a command appears in `brix --help` (`Command::Category`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Category {
    pub id: i64,
    pub description: String,
}

#[derive(Clone, Debug)]
pub struct Command {
    pub name: String,
    pub description: String,
    /// Not shown in help or `__dump-cli` (the `__…` commands).
    pub hidden: bool,
    pub experimental: Option<String>,
    pub category: Category,
    pub flags: Vec<Flag>,
    pub args: Vec<Arg>,
    /// Subcommands; non-empty makes this a multi-command whose first
    /// positional argument selects one of them.
    pub commands: Vec<Command>,
}

impl Command {
    pub fn subcommand(&self, name: &str) -> Option<&Command> {
        self.commands.iter().find(|c| c.name == name)
    }

    fn find_long(&self, name: &str) -> Option<&Flag> {
        self.flags
            .iter()
            .find(|f| f.long == name || f.aliases.iter().any(|a| a == name))
    }

    fn find_short(&self, c: char) -> Option<&Flag> {
        self.flags.iter().find(|f| f.short == Some(c))
    }

    /// The command at `path` (subcommand names), if it exists.
    pub fn descend(&self, path: &[String]) -> Option<&Command> {
        let mut cur = self;
        for name in path {
            cur = cur.subcommand(name)?;
        }
        Some(cur)
    }
}

/// A top-level alias (`nix shell` → `nix env shell`).
#[derive(Clone, Debug)]
pub struct Alias {
    pub name: String,
    pub replacement: Vec<String>,
    /// Deprecated aliases print a warning when used.
    pub deprecated: bool,
}

/// One use of a flag on the command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlagUse {
    /// The canonical long name (aliases are resolved).
    pub name: String,
    /// The values as text, for what brix reads as text. A value that is
    /// not UTF-8 has its ill-formed bytes replaced here, and brix refuses
    /// it before a command runs ([`Invocation::non_utf8_text`]) unless
    /// the flag is one it reads as bytes, from [`FlagUse::raw`].
    pub values: Vec<String>,
    /// The values as given: bytes, as Nix takes them.
    pub raw: Vec<BString>,
}

/// A parsed command line.
#[derive(Clone, Debug, Default)]
pub struct Invocation {
    /// Subcommand names from the root down (`["flake", "show"]`).
    pub path: Vec<String>,
    /// Every flag in command-line order.
    pub flags: Vec<FlagUse>,
    /// Positional arguments of the selected command, by label, as text
    /// (see [`FlagUse::values`]).
    pub args: BTreeMap<String, Vec<String>>,
    /// The positional arguments as given: bytes.
    pub raw_args: BTreeMap<String, Vec<BString>>,
    /// Warnings the parser wants printed (deprecated aliases).
    pub warnings: Vec<String>,
}

impl Invocation {
    /// The values of the last use of `name`, if it was given.
    pub fn flag(&self, name: &str) -> Option<&[String]> {
        self.flags
            .iter()
            .rev()
            .find(|f| f.name == name)
            .map(|f| f.values.as_slice())
    }

    pub fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f.name == name)
    }

    /// The last use of any of `names`: flags that set the same thing
    /// (`--pretty` and `--no-pretty`, `--out-link` and `--no-link`), of
    /// which the last one given wins, as each overwrites what the one
    /// before it set.
    pub fn last_of(&self, names: &[&str]) -> Option<&FlagUse> {
        self.flags
            .iter()
            .rev()
            .find(|f| names.contains(&f.name.as_str()))
    }

    /// The first value of the last use of `name`.
    pub fn flag_value(&self, name: &str) -> Option<&str> {
        self.flag(name).and_then(|v| v.first()).map(String::as_str)
    }

    /// Every use of `name`, in order.
    pub fn flag_uses<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a [String]> + 'a {
        self.flags
            .iter()
            .filter(move |f| f.name == name)
            .map(|f| f.values.as_slice())
    }

    pub fn arg(&self, label: &str) -> &[String] {
        self.args.get(label).map_or(&[], Vec::as_slice)
    }

    pub fn arg_one(&self, label: &str) -> Option<&str> {
        self.arg(label).first().map(String::as_str)
    }

    /// [`Invocation::flag_value`] as given: bytes.
    pub fn flag_value_bytes(&self, name: &str) -> Option<&BStr> {
        self.flags
            .iter()
            .rev()
            .find(|f| f.name == name)
            .and_then(|f| f.raw.first())
            .map(|v| v.as_bstr())
    }

    /// [`Invocation::flag_uses`] as given: bytes.
    pub fn flag_uses_bytes<'a>(
        &'a self,
        name: &'a str,
    ) -> impl Iterator<Item = &'a [BString]> + 'a {
        self.flags
            .iter()
            .filter(move |f| f.name == name)
            .map(|f| f.raw.as_slice())
    }

    /// [`Invocation::arg`] as given: bytes.
    pub fn arg_bytes(&self, label: &str) -> &[BString] {
        self.raw_args.get(label).map_or(&[], Vec::as_slice)
    }

    /// [`Invocation::arg_one`] as given: bytes.
    pub fn arg_one_bytes(&self, label: &str) -> Option<&BStr> {
        self.arg_bytes(label).first().map(|v| v.as_bstr())
    }

    /// The first value that is not UTF-8 among those brix reads as text:
    /// the values of every flag but `byte_flags` (then the positional
    /// arguments but `byte_args`), with where it was given (`--flag`, or
    /// the argument's label).
    pub fn non_utf8_text(
        &self,
        byte_flags: &[&str],
        byte_args: &[&str],
    ) -> Option<(String, &BStr)> {
        for f in &self.flags {
            if byte_flags.contains(&f.name.as_str()) {
                continue;
            }
            if let Some(value) = f.raw.iter().find(|v| std::str::from_utf8(v).is_err()) {
                return Some((format!("--{}", f.name), value.as_bstr()));
            }
        }
        for (label, values) in &self.raw_args {
            if byte_args.contains(&label.as_str()) {
                continue;
            }
            if let Some(value) = values.iter().find(|v| std::str::from_utf8(v).is_err()) {
                return Some((label.clone(), value.as_bstr()));
            }
        }
        None
    }
}

/// A command-line error: Nix's `UsageError`, which the front end follows
/// with a "Try '--help'" hint. Bytes: it can quote an argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageError(pub BString);

/// Lossy, for Rust-side text only: the front end prints the bytes.
impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// `prefix`, the argument `arg` as given, then `suffix`: a message that
/// quotes an argument, which need not be UTF-8.
fn quoting(prefix: &str, arg: &[u8], suffix: &str) -> BString {
    [prefix.as_bytes(), arg, suffix.as_bytes()].concat().into()
}

/// The text view of an argument (see [`FlagUse::values`]).
fn text(arg: &[u8]) -> String {
    String::from_utf8_lossy(arg).into_owned()
}

/// Why [`parse`] stopped, and what it had read by then: Nix's flag
/// handlers run as each flag is read, so what they do (set a setting,
/// fail on its value, ask for help) happens before the error.
#[derive(Clone, Debug)]
pub struct ParseFailure {
    pub error: UsageError,
    /// The command path and flags read before the error.
    pub parsed: Invocation,
}

/// Parses `argv` (without the program name) against `root`. The
/// arguments are bytes, as Nix's are; text (`&[String]`) will do.
pub fn parse(
    root: &Command,
    aliases: &[Alias],
    argv: &[impl AsRef<[u8]>],
) -> Result<Invocation, Box<ParseFailure>> {
    let argv: Vec<BString> = argv.iter().map(|a| BString::from(a.as_ref())).collect();
    let mut inv = Invocation::default();
    match parse_into(root, aliases, argv, &mut inv) {
        Ok(()) => Ok(inv),
        Err(error) => Err(Box::new(ParseFailure { error, parsed: inv })),
    }
}

/// `Args::parseCmdline` on bytes: flag names and subcommands are ASCII,
/// values whatever was given.
fn parse_into(
    root: &Command,
    aliases: &[Alias],
    mut argv: Vec<BString>,
    inv: &mut Invocation,
) -> Result<(), UsageError> {
    // The chain of selected commands, root first.
    let mut chain: Vec<&Command> = vec![root];
    let mut pending: Vec<BString> = Vec::new();
    let mut dash_dash = false;
    let mut alias_used = false;
    // Positional arguments of the innermost command consumed so far.
    let mut next_arg = 0usize;

    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].clone();
        // `-qlf` → `-q -l -f`; `-j3` → `-j 3`. Byte by byte, with
        // `isalpha` in the C locale (Nix never sets another): ASCII
        // letters only.
        if !dash_dash && arg.len() > 2 && arg[0] == b'-' && arg[1].is_ascii_alphabetic() {
            let mut expanded = vec![BString::from(vec![b'-', arg[1]])];
            let mut j = 2;
            while j < arg.len() {
                if arg[j].is_ascii_alphabetic() {
                    expanded.push(BString::from(vec![b'-', arg[j]]));
                    j += 1;
                } else {
                    expanded.push(BString::from(&arg[j..]));
                    break;
                }
            }
            argv.splice(i..=i, expanded);
            continue;
        }
        if !dash_dash && arg == "--" {
            dash_dash = true;
            i += 1;
            continue;
        }
        if !dash_dash && arg.first() == Some(&b'-') {
            let (name, flag) = lookup_flag(&chain, &arg)
                .ok_or_else(|| UsageError(quoting("unrecognised flag '", &arg, "'")))?;
            i += 1;
            let mut raw = Vec::new();
            match flag.arity {
                Arity::Fixed(n) => {
                    for k in 0..n {
                        match argv.get(i) {
                            Some(v) => {
                                raw.push(v.clone());
                                i += 1;
                            }
                            None => {
                                return Err(UsageError(
                                    format!(
                                "flag '{name}' requires {n} argument(s), but only {k} were given"
                            )
                                    .into(),
                                ))
                            }
                        }
                    }
                }
                Arity::Any => {
                    while let Some(v) = argv.get(i) {
                        raw.push(v.clone());
                        i += 1;
                    }
                }
            }
            inv.flags.push(FlagUse {
                name: flag.long.clone(),
                values: raw.iter().map(|v| text(v)).collect(),
                raw,
            });
            continue;
        }
        // A positional argument. Top-level aliases are rewritten once,
        // before a subcommand is chosen.
        if chain.len() == 1 && !alias_used {
            if let Some(alias) = aliases.iter().find(|a| arg == a.name.as_bytes()) {
                if alias.deprecated {
                    inv.warnings.push(format!(
                        "'{}' is a deprecated alias for '{}'",
                        alias.name,
                        alias.replacement.join(" ")
                    ));
                }
                argv.splice(
                    i..=i,
                    alias.replacement.iter().map(|r| BString::from(r.as_str())),
                );
                alias_used = true;
                continue;
            }
        }
        pending.push(arg);
        i += 1;
        process_args(&mut chain, inv, &mut pending, &mut next_arg, false)?;
    }
    process_args(&mut chain, inv, &mut pending, &mut next_arg, true)
}

/// Finds `arg` (`--long` or `-s`) in the selected commands, outermost
/// first, as `MultiCommand::processFlag` does. Flag names are ASCII, so
/// an argument that is not UTF-8 names none.
fn lookup_flag<'c>(chain: &[&'c Command], arg: &[u8]) -> Option<(String, &'c Flag)> {
    if let Some(long) = arg.strip_prefix(b"--") {
        let Ok(long) = std::str::from_utf8(long) else {
            return None;
        };
        for cmd in chain {
            if let Some(f) = cmd.find_long(long) {
                return Some((format!("--{long}"), f));
            }
        }
        return None;
    }
    // `-c`: a dash and one byte.
    let [b'-', c] = arg else {
        return None;
    };
    let c = char::from(*c);
    for cmd in chain {
        if let Some(f) = cmd.find_short(c) {
            return Some((format!("-{c}"), f));
        }
    }
    None
}

/// `Args::processArgs`: hands `pending` to the innermost command's next
/// expected argument when enough values have arrived (or at the end for
/// `Any`). A multi-command's expected argument is its subcommand.
fn process_args(
    chain: &mut Vec<&Command>,
    inv: &mut Invocation,
    pending: &mut Vec<BString>,
    next_arg: &mut usize,
    finish: bool,
) -> Result<(), UsageError> {
    loop {
        let cur = *chain.last().expect("the root is always selected");
        if !cur.commands.is_empty() {
            // Selecting a subcommand (arity 1).
            if pending.is_empty() {
                return Ok(());
            }
            let name = pending.remove(0);
            match cur.commands.iter().find(|c| name == c.name.as_bytes()) {
                Some(sub) => {
                    inv.path.push(sub.name.clone());
                    chain.push(sub);
                    *next_arg = 0;
                    if pending.is_empty() {
                        return Ok(());
                    }
                    continue;
                }
                None => return Err(UsageError(unknown_command(cur, &name))),
            }
        }
        let Some(expected) = cur.args.get(*next_arg) else {
            if let Some(extra) = pending.first() {
                return Err(UsageError(quoting("unexpected argument '", extra, "'")));
            }
            return Ok(());
        };
        match expected.arity {
            Arity::Fixed(n) if pending.len() >= n => {
                let values: Vec<BString> = pending.drain(..n).collect();
                record_arg(inv, &expected.label, values);
                *next_arg += 1;
                if pending.is_empty() {
                    return Ok(());
                }
            }
            Arity::Any if finish => {
                record_arg(inv, &expected.label, std::mem::take(pending));
                *next_arg += 1;
                return Ok(());
            }
            _ => {
                // `--help` wins over missing arguments, as `nix flake new --help`
                // shows the help rather than complaining.
                let help = inv.flags.iter().any(|f| f.name == "help");
                if finish && !expected.optional && !help {
                    return Err(UsageError("more arguments are required".into()));
                }
                return Ok(());
            }
        }
    }
}

/// Records the values of the positional argument `label`, as given and
/// as text.
fn record_arg(inv: &mut Invocation, label: &str, values: Vec<BString>) {
    inv.args
        .insert(label.to_owned(), values.iter().map(|v| text(v)).collect());
    inv.raw_args.insert(label.to_owned(), values);
}

/// `MultiCommand::processArgs`' error for `name`, which is none of
/// `cur`'s subcommands: the name as given, and the closest subcommands
/// (`Suggestions::bestMatches`, a Levenshtein distance on bytes).
fn unknown_command(cur: &Command, name: &[u8]) -> BString {
    let mut msg = quoting("'", name, "' is not a recognised command");
    // The subcommands' names are ASCII without NUL, so a byte of `name`
    // outside ASCII equals none of theirs, whatever its value: compared
    // as NUL, which none of them has either, it leaves every distance as
    // it was, and the query is text.
    let query: String = name
        .iter()
        .map(|&b| if b.is_ascii() { char::from(b) } else { '\0' })
        .collect();
    let suggestions = brix::best_suggestions(cur.commands.iter().map(|c| c.name.as_str()), &query);
    if !suggestions.is_empty() {
        msg.extend_from_slice(
            format!("\nDid you mean {}?", brix::format_suggestions(&suggestions)).as_bytes(),
        );
    }
    msg
}

// ---------------------------------------------------------------------
// `__dump-cli`
// ---------------------------------------------------------------------

/// The `args` member of `nix __dump-cli`: the command tree with every
/// flag, in the shape `Args::toJSON`/`MultiCommand::toJSON` produce
/// (nlohmann sorts object keys).
pub fn dump_command(cmd: &Command) -> Value {
    let mut flags = Map::new();
    for f in &cmd.flags {
        let mut j = Map::new();
        j.insert("hiddenCategory".into(), json!(f.hidden_category));
        if let Some(c) = f.short {
            j.insert("shortName".into(), json!(c.to_string()));
        }
        if !f.description.is_empty() {
            j.insert("description".into(), json!(f.description.trim()));
        }
        j.insert("category".into(), json!(f.category));
        if let Arity::Fixed(n) = f.arity {
            j.insert("arity".into(), json!(n));
        }
        if !f.labels.is_empty() {
            j.insert("labels".into(), json!(f.labels));
        }
        j.insert("experimental-feature".into(), json!(f.experimental));
        flags.insert(f.long.clone(), Value::Object(j));
    }
    let args: Vec<Value> = cmd
        .args
        .iter()
        .map(|a| {
            let mut j = Map::new();
            j.insert("label".into(), json!(a.label));
            j.insert("optional".into(), json!(a.optional));
            if let Arity::Fixed(n) = a.arity {
                j.insert("arity".into(), json!(n));
            }
            Value::Object(j)
        })
        .collect();
    let mut res = Map::new();
    res.insert("description".into(), json!(cmd.description.trim()));
    res.insert("flags".into(), Value::Object(flags));
    res.insert("args".into(), Value::Array(args));
    if !cmd.commands.is_empty() {
        let mut cmds = Map::new();
        for sub in &cmd.commands {
            if sub.hidden {
                continue;
            }
            let mut j = dump_command(sub);
            let cat = json!({
                "id": sub.category.id,
                "description": sub.category.description.trim(),
                "experimental-feature": sub.experimental,
            });
            j.as_object_mut()
                .expect("dump_command returns an object")
                .insert("category".into(), cat);
            cmds.insert(sub.name.clone(), j);
        }
        res.insert("commands".into(), Value::Object(cmds));
    }
    Value::Object(res)
}

// ---------------------------------------------------------------------
// `--help`
// ---------------------------------------------------------------------

/// Renders the manual-page style help `nix` shows for a command, laid out
/// as Nix's `generate-manpage.nix` lays it out from the same tree: name,
/// synopsis, subcommands (by category when there are several), and the
/// options of the command and of the root, grouped by category (the
/// command's own first, then by name), each with brix's description.
/// Nix's documentation pages are not part of it, and lines are not
/// wrapped.
pub fn render_help(root: &Command, path: &[String], program: &str) -> Option<String> {
    let cmd = root.descend(path)?;
    let full = std::iter::once(program.to_owned())
        .chain(path.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = String::new();
    out.push_str(&format!("Name\n\n{full} - {}\n\n", cmd.description));
    // `showSynopsis`: the arguments, `...` for any number, after a space
    // that is there even when there are none.
    let arguments: Vec<String> = cmd
        .args
        .iter()
        .map(|a| match a.arity {
            Arity::Any => format!("{}...", a.label),
            Arity::Fixed(_) => a.label.clone(),
        })
        .collect();
    let synopsis = format!("{full} [option...] {}", arguments.join(" "));
    out.push_str(&format!("Synopsis\n\n{synopsis}\n\n"));
    let subcommands: Vec<&Command> = cmd.commands.iter().filter(|c| !c.hidden).collect();
    if !subcommands.is_empty() {
        out.push_str("where subcommand is one of the following:\n\n");
        let mut categories: Vec<&Category> = subcommands.iter().map(|c| &c.category).collect();
        categories.sort_by_key(|c| c.id);
        categories.dedup();
        let show = |out: &mut String, cmds: &mut dyn Iterator<Item = &&Command>| {
            for sub in cmds {
                out.push_str(&format!("  · {full} {} - {}\n", sub.name, sub.description));
            }
            out.push('\n');
        };
        if categories.len() > 1 {
            for cat in categories {
                out.push_str(&format!("{}:\n\n", cat.description));
                show(
                    &mut out,
                    &mut subcommands.iter().filter(|c| &c.category == cat),
                );
            }
        } else {
            show(&mut out, &mut subcommands.iter());
        }
    }
    // `details.flags // toplevel.flags`: the command's flags and the
    // root's, the root's winning a name both have; not those of the
    // groups in between.
    let mut flags: BTreeMap<&str, &Flag> = BTreeMap::new();
    for f in cmd.flags.iter().chain(&root.flags) {
        if !f.hidden_category {
            flags.insert(&f.long, f);
        }
    }
    let mut groups: BTreeMap<&str, Vec<&Flag>> = BTreeMap::new();
    for f in flags.into_values() {
        groups.entry(&f.category).or_default().push(f);
    }
    if !groups.is_empty() {
        out.push_str("Options\n\n");
    }
    for (category, flags) in groups {
        if !category.is_empty() {
            let title = category.replace(
                "[installables](@docroot@/command-ref/new-cli/nix.md#installables)",
                "installables",
            );
            out.push_str(&format!("## {title}\n\n"));
        }
        for f in flags {
            let mut item = format!("  · --{}", f.long);
            if let Some(c) = f.short {
                item.push_str(&format!(" / -{c}"));
            }
            for l in &f.labels {
                item.push_str(&format!(" {l}"));
            }
            out.push_str(&item);
            out.push_str("\n\n");
            if !f.description.is_empty() {
                out.push_str(&format!("    {}\n\n", f.description.trim()));
            }
        }
    }
    Some(out.trim_end().to_owned() + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flag(long: &str, short: Option<char>, arity: usize) -> Flag {
        Flag {
            long: long.into(),
            short,
            aliases: Vec::new(),
            labels: (0..arity).map(|i| format!("v{i}")).collect(),
            arity: Arity::Fixed(arity),
            category: String::new(),
            hidden_category: false,
            experimental: None,
            description: String::new(),
        }
    }

    fn cmd(name: &str, args: Vec<Arg>, flags: Vec<Flag>, commands: Vec<Command>) -> Command {
        Command {
            name: name.into(),
            description: String::new(),
            hidden: false,
            experimental: None,
            category: Category {
                id: 0,
                description: String::new(),
            },
            flags,
            args,
            commands,
        }
    }

    fn tree() -> Command {
        cmd(
            "",
            vec![],
            vec![flag("verbose", Some('v'), 0), flag("option", None, 2)],
            vec![
                cmd(
                    "eval",
                    vec![Arg {
                        label: "installable".into(),
                        optional: true,
                        arity: Arity::Fixed(1),
                    }],
                    vec![flag("json", None, 0), flag("expr", None, 1)],
                    vec![],
                ),
                cmd(
                    "flake",
                    vec![],
                    vec![],
                    vec![cmd(
                        "new",
                        vec![
                            Arg {
                                label: "dest-dir".into(),
                                optional: false,
                                arity: Arity::Fixed(1),
                            },
                            Arg {
                                label: "rest".into(),
                                optional: true,
                                arity: Arity::Any,
                            },
                        ],
                        vec![flag("template", Some('t'), 1)],
                        vec![],
                    )],
                ),
            ],
        )
    }

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn flags_anywhere_and_positionals() {
        let inv = parse(&tree(), &[], &argv("-v eval --json x --option a b")).unwrap();
        assert_eq!(inv.path, vec!["eval"]);
        assert_eq!(inv.arg("installable"), ["x"]);
        assert!(inv.has("json"));
        assert_eq!(
            inv.flag("option"),
            Some(&["a".to_owned(), "b".to_owned()][..])
        );
        assert_eq!(inv.flag_uses("verbose").count(), 1);
    }

    #[test]
    fn the_last_of_opposing_flags_wins() {
        let inv = parse(&tree(), &[], &argv("eval --json --expr 1 --json")).unwrap();
        assert_eq!(
            inv.last_of(&["expr", "json"]).map(|f| f.name.as_str()),
            Some("json")
        );
        assert_eq!(
            inv.last_of(&["expr", "option"])
                .map(|f| f.values.as_slice()),
            Some(&["1".to_owned()][..])
        );
        assert_eq!(inv.last_of(&["verbose"]), None);
    }

    #[test]
    fn compound_short_flags_and_dash_dash() {
        // `-tfoo` would expand to `-t -f -o -o`, as in Nix; `-j3` style only
        // attaches a value that is not a letter.
        let inv = parse(&tree(), &[], &argv("flake new -t foo dir -- --json")).unwrap();
        assert_eq!(inv.path, vec!["flake", "new"]);
        assert_eq!(inv.flag_value("template"), Some("foo"));
        assert_eq!(inv.arg("dest-dir"), ["dir"]);
        assert_eq!(inv.arg("rest"), ["--json"]);
    }

    #[test]
    fn errors_match_nix() {
        assert_eq!(
            parse(&tree(), &[], &argv("eval --bogus"))
                .unwrap_err()
                .error
                .0,
            "unrecognised flag '--bogus'"
        );
        assert_eq!(
            parse(&tree(), &[], &argv("eval --expr"))
                .unwrap_err()
                .error
                .0,
            "flag '--expr' requires 1 argument(s), but only 0 were given"
        );
        assert_eq!(
            parse(&tree(), &[], &argv("eval a b")).unwrap_err().error.0,
            "unexpected argument 'b'"
        );
        assert_eq!(
            parse(&tree(), &[], &argv("flake new")).unwrap_err().error.0,
            "more arguments are required"
        );
        // The suggestion is in Nix's colour, which the logger filters out
        // when stderr is not a terminal.
        let unknown = parse(&tree(), &[], &argv("evl")).unwrap_err().error.0;
        assert_eq!(
            brix::log::filter_ansi_escapes(&unknown, true, usize::MAX).as_bstr(),
            "'evl' is not a recognised command\nDid you mean eval?"
        );
        assert!(unknown.contains_str("\u{1b}[35;1meval\u{1b}[0m"));
    }

    /// Arguments as bytes: each value is kept as given and as text.
    #[test]
    fn values_are_bytes() {
        let argv: [&[u8]; 4] = [b"eval", b"--expr", b"\"caf\xe9\"", b"a\xffb"];
        let inv = parse(&tree(), &[], &argv).unwrap();
        assert_eq!(inv.flag_value_bytes("expr"), Some(b"\"caf\xe9\"".as_bstr()));
        assert_eq!(inv.flag_value("expr"), Some("\"caf\u{fffd}\""));
        assert_eq!(inv.arg_one_bytes("installable"), Some(b"a\xffb".as_bstr()));
        assert_eq!(inv.arg_one("installable"), Some("a\u{fffd}b"));
        assert_eq!(
            inv.flag_uses_bytes("expr").collect::<Vec<_>>(),
            [[BString::from(&b"\"caf\xe9\""[..])].as_slice()]
        );
        // `-t\xe9` is `-t` and the value `\xe9`: a byte outside ASCII is
        // no letter (`isalpha` in the C locale).
        let argv: [&[u8]; 4] = [b"flake", b"new", b"-t\xe9", b"dir"];
        let inv = parse(&tree(), &[], &argv).unwrap();
        assert_eq!(inv.flag_value_bytes("template"), Some(b"\xe9".as_bstr()));
        assert_eq!(inv.arg_bytes("dest-dir"), [BString::from("dir")]);
    }

    /// The values brix would read as text that are not UTF-8: the first
    /// flag value, then the first positional argument, outside the slots
    /// read as bytes.
    #[test]
    fn values_that_are_not_text() {
        let raw: [&[u8]; 7] = [
            b"--option",
            b"a",
            b"\xff",
            b"eval",
            b"--expr",
            b"\xfe",
            b"x\xfd",
        ];
        let inv = parse(&tree(), &[], &raw).unwrap();
        assert_eq!(
            inv.non_utf8_text(&[], &[]),
            Some(("--option".to_owned(), b"\xff".as_bstr()))
        );
        assert_eq!(
            inv.non_utf8_text(&["option"], &[]),
            Some(("--expr".to_owned(), b"\xfe".as_bstr()))
        );
        assert_eq!(
            inv.non_utf8_text(&["option", "expr"], &[]),
            Some(("installable".to_owned(), b"x\xfd".as_bstr()))
        );
        assert_eq!(
            inv.non_utf8_text(&["option", "expr"], &["installable"]),
            None
        );
        let inv = parse(&tree(), &[], &argv("eval --expr 1 caf\u{e9}")).unwrap();
        assert_eq!(inv.non_utf8_text(&[], &[]), None);
    }

    /// Usage errors quote the argument as given, and an unknown command's
    /// suggestions compare bytes, as Nix's do (`nix ev$'\xff'al` suggests
    /// `eval`, one byte away).
    #[test]
    fn errors_quote_bytes() {
        let error = |argv: &[&[u8]]| parse(&tree(), &[], argv).unwrap_err().error.0;
        assert_eq!(
            error(&[b"eval", b"--ex\xffpr"]),
            &b"unrecognised flag '--ex\xffpr'"[..]
        );
        assert_eq!(
            error(&[b"eval", b"-\xff"]),
            &b"unrecognised flag '-\xff'"[..]
        );
        assert_eq!(
            error(&[b"eval", b"a", b"b\xff"]),
            &b"unexpected argument 'b\xff'"[..]
        );
        let cases: [(&[u8], &[u8]); 3] = [
            (
                b"ev\xffal",
                b"'ev\xffal' is not a recognised command\nDid you mean eval?",
            ),
            (
                b"\xe9val",
                b"'\xe9val' is not a recognised command\nDid you mean eval?",
            ),
            // `é` is two bytes, as Nix counts: three edits from `eval`.
            (
                "\u{e9}vl".as_bytes(),
                "'\u{e9}vl' is not a recognised command".as_bytes(),
            ),
        ];
        for (name, shown) in cases {
            let message = error(&[name]);
            assert_eq!(
                brix::log::filter_ansi_escapes(&message, true, usize::MAX).as_bstr(),
                shown.as_bstr()
            );
        }
    }

    #[test]
    fn aliases_rewrite_once() {
        let aliases = [Alias {
            name: "new".into(),
            replacement: vec!["flake".into(), "new".into()],
            deprecated: true,
        }];
        let inv = parse(&tree(), &aliases, &argv("new dir")).unwrap();
        assert_eq!(inv.path, vec!["flake", "new"]);
        assert_eq!(inv.warnings.len(), 1);
    }

    #[test]
    fn dump_shape() {
        let j = dump_command(&tree());
        assert_eq!(j["flags"]["option"]["arity"], 2);
        assert_eq!(j["flags"]["verbose"]["shortName"], "v");
        assert_eq!(j["commands"]["eval"]["args"][0]["label"], "installable");
        assert_eq!(
            j["commands"]["flake"]["commands"]["new"]["args"][1]["optional"],
            true
        );
        assert!(j["commands"]["flake"]["commands"]["new"]["args"][1]
            .get("arity")
            .is_none());
    }
}
