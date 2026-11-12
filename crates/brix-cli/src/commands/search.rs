//! `brix search` (`src/nix/search.cc`).

use crate::installable::parse_drv_name;
use brix::Cursor;
use regex::{Regex, RegexBuilder};
use serde_json::{json, Map, Value};

use crate::commands::{print_stdout, Ctx};
use crate::{CliError, Result};

const ANSI_NORMAL: &str = "\u{1b}[0m";
const ANSI_GREEN: &str = "\u{1b}[32;1m";
const ANSI_BOLD_PLAIN: &str = "\u{1b}[0;1m";

fn compile(re: &str) -> Result<Regex> {
    RegexBuilder::new(re)
        .case_insensitive(true)
        .build()
        .map_err(|e| CliError::msg(format!("invalid regular expression '{re}': {e}")))
}

/// `hiliteMatches`: wraps every match (merging overlapping ones) in
/// `prefix … postfix`.
fn hilite(s: &str, matches: &[(usize, usize)], prefix: &str, postfix: &str) -> String {
    if matches.is_empty() {
        return s.to_owned();
    }
    let mut sorted = matches.to_vec();
    sorted.sort();
    let mut out = String::new();
    let mut last_end = 0;
    let mut i = 0;
    while i < sorted.len() {
        let (start, mut end) = sorted[i];
        out.push_str(&s[last_end..start]);
        i += 1;
        while i < sorted.len() && sorted[i].0 <= end {
            end = end.max(sorted[i].1);
            i += 1;
        }
        out.push_str(prefix);
        out.push_str(&s[start..end]);
        out.push_str(postfix);
        last_end = end;
    }
    out.push_str(&s[last_end..]);
    out
}

pub fn run(ctx: &Ctx<'_>) -> Result<()> {
    let patterns: Vec<String> = ctx.inv.arg("regex").to_vec();
    if patterns.is_empty() {
        return Err(CliError::usage("Must provide at least one regex! To match all packages, use 'nix search <installable> ^'."));
    }
    let regexes = patterns
        .iter()
        .map(|p| compile(p))
        .collect::<Result<Vec<_>>>()?;
    let excludes = ctx
        .inv
        .flag_uses("exclude")
        .map(|v| compile(&v[0]))
        .collect::<Result<Vec<_>>>()?;
    let json = ctx.inv.has("json");
    let mut config_session = ctx.config()?;
    config_session.eval.allow_import_from_derivation = false;
    let session = brix::Session::new(config_session)?;
    let mut resolver = ctx.resolver(&session, &["nix-search"])?;
    let installable = ctx.installable(&resolver)?;
    let cursors = resolver.cursors(&installable, false)?;
    let mut state = State {
        regexes,
        excludes,
        json,
        results: 0,
        total: 0,
        json_out: Map::new(),
        text: String::new(),
    };
    for cursor in cursors {
        visit(&mut state, &cursor, true)?;
    }
    if json {
        ctx.print_json(&Value::Object(state.json_out))?;
    } else {
        print_stdout(&state.text)?;
    }
    if !json && state.results == 0 {
        return Err(CliError::msg("no results for the given search term(s)!"));
    }
    eprintln!(
        "Found {} matching packages out of {}.",
        state.results, state.total
    );
    Ok(())
}

struct State {
    regexes: Vec<Regex>,
    excludes: Vec<Regex>,
    json: bool,
    results: u64,
    total: u64,
    json_out: Map<String, Value>,
    text: String,
}

fn spans(re: &Regex, s: &str) -> Vec<(usize, usize)> {
    re.find_iter(s).map(|m| (m.start(), m.end())).collect()
}

fn visit(state: &mut State, cursor: &Cursor<'_>, initial_recurse: bool) -> Result<()> {
    let attr_path = cursor.attr_path().to_vec();
    let attr_path_str = cursor.attr_path_str();
    let outcome: Result<()> = (|| {
        if cursor.is_derivation()? {
            state.total += 1;
            let full_name = cursor.get_attr("name")?.get_string()?;
            let (name, version) = parse_drv_name(&full_name);
            let (name, version) = (name.to_owned(), version.to_owned());
            let description = match cursor.maybe_get_attr("meta")? {
                Some(meta) => match meta.maybe_get_attr("description")? {
                    Some(d) => d.get_string()?,
                    None => String::new(),
                },
                None => String::new(),
            };
            let description = description.replace('\n', " ");
            for re in &state.excludes {
                if re.is_match(&attr_path_str) || re.is_match(&name) || re.is_match(&description) {
                    return Ok(());
                }
            }
            let mut attr_matches = Vec::new();
            let mut desc_matches = Vec::new();
            let mut found = false;
            for re in &state.regexes {
                let a = spans(re, &attr_path_str);
                let n = spans(re, &name);
                let d = spans(re, &description);
                found = !a.is_empty() || !n.is_empty() || !d.is_empty();
                attr_matches.extend(a);
                desc_matches.extend(d);
                if !found {
                    break;
                }
            }
            if found {
                state.results += 1;
                if state.json {
                    state.json_out.insert(
                        attr_path_str.clone(),
                        json!({ "pname": name, "version": version, "description": description }),
                    );
                } else {
                    let mut out = String::new();
                    if state.results > 1 {
                        out.push('\n');
                    }
                    out.push_str(&format!(
                        "* {}{}{}",
                        ANSI_BOLD_PLAIN,
                        hilite(&attr_path_str, &attr_matches, ANSI_GREEN, ANSI_BOLD_PLAIN),
                        ANSI_NORMAL
                    ));
                    if !version.is_empty() {
                        out.push_str(&format!(" ({version})"));
                    }
                    if !description.is_empty() {
                        out.push_str(&format!(
                            "\n  {}",
                            hilite(&description, &desc_matches, ANSI_GREEN, ANSI_NORMAL)
                        ));
                    }
                    out.push('\n');
                    state.text.push_str(&out);
                }
            }
            return Ok(());
        }
        let first = attr_path.first().map(String::as_str);
        let recurse = attr_path.is_empty()
            || (first == Some("legacyPackages") && attr_path.len() <= 2)
            || (first == Some("packages") && attr_path.len() <= 2)
            || initial_recurse
            || (first == Some("legacyPackages") && attr_path.len() > 2 && {
                match cursor.maybe_get_attr("recurseForDerivations")? {
                    Some(r) => r.get_bool()?,
                    None => false,
                }
            });
        if recurse && cursor.is_attrs()? {
            for name in cursor.get_attrs()? {
                let child = cursor.get_attr(&name)?;
                visit(state, &child, false)?;
            }
        }
        Ok(())
    })();
    match outcome {
        Ok(()) => Ok(()),
        // Evaluation errors inside `legacyPackages` are skipped, as Nix does.
        Err(CliError::Brix(brix::Error::Eval { .. }))
            if attr_path.first().is_some_and(|a| a == "legacyPackages") =>
        {
            Ok(())
        }
        Err(e) => Err(e),
    }
}
