//! String built-ins: `toString`, `substring`, `stringLength`,
//! `hashString`, `convertHash`, `match`, `split`, `concatStringsSep`,
//! `replaceStrings`, versions, JSON/XML/TOML conversions.

use brix_store::{Hash, HashAlgo, HashFormat, XpSettings};
use smallvec::SmallVec;

use crate::builtins::{reg, require_attr};
use crate::coerce::Coercion;
use crate::error::{msg, ErrorKind};
use crate::evaluator::Evaluator;
use crate::string_context::StringContext;
use crate::symbol::sym;
use crate::toml_datetime::normalize_toml_datetime;
use crate::value::{Attr, SrcPos, Value, ValueRef};
use crate::Result;

pub fn register(ev: &Evaluator<'_>) {
    reg(ev, "__toXML", 1, prim_to_xml);
    reg(ev, "__toJSON", 1, prim_to_json);
    reg(ev, "__fromJSON", 1, prim_from_json);
    reg(ev, "toString", 1, prim_to_string);
    reg(ev, "__substring", 3, prim_substring);
    reg(ev, "__stringLength", 1, prim_string_length);
    reg(ev, "__hashString", 2, prim_hash_string);
    reg(ev, "__convertHash", 1, prim_convert_hash);
    reg(ev, "__match", 2, prim_match);
    reg(ev, "__split", 2, prim_split);
    reg(ev, "__concatStringsSep", 2, prim_concat_strings_sep);
    reg(ev, "__replaceStrings", 3, prim_replace_strings);
    reg(ev, "__parseDrvName", 1, prim_parse_drv_name);
    reg(ev, "__compareVersions", 2, prim_compare_versions);
    reg(ev, "__splitVersion", 1, prim_split_version);
    reg(ev, "fromTOML", 1, prim_from_toml);
}

fn prim_to_xml<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let mut ctx = StringContext::new();
    let s = ev.to_xml(args[0], pos, &mut ctx, true, false)?;
    Ok(ev.mk_string_ctx(&s, &ctx))
}

fn prim_to_json<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let mut ctx = StringContext::new();
    let s = ev
        .to_json(args[0], pos, &mut ctx, true, true)
        .map_err(|e| {
            // The primop catches nlohmann's exceptions and rethrows them as a
            // fresh `JSONSerializationError`, a Nix error.
            if e.kind == ErrorKind::Foreign {
                crate::error::EvalError::new(
                    ErrorKind::Eval,
                    msg!("JSON serialization error: ", e.msg),
                )
            } else {
                e
            }
        })?;
    Ok(ev.mk_string_ctx(s.as_bytes(), &ctx))
}

fn prim_from_json<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let s = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.fromJSON",
    )?;
    ev.from_json(s).inspect_err(|e| {
        // Only syntax errors get this trace; the values Nix rejects while
        // building the result are plain errors.
        if e.kind == ErrorKind::Parse {
            ev.add_trace(e, pos, "while decoding a JSON string");
        }
    })
}

fn prim_to_string<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    const CTX: &str = "while evaluating the first argument passed to builtins.toString";
    let v = ev.force_for_coercion(args[0], pos)?;
    match v {
        // `toString` of a string is that string: Nix builds an equal value,
        // which is indistinguishable from reusing this one.
        Value::String(_) => Ok(v),
        // A path's text, a lazy tree's hash left lazy.
        Value::Path(_) => {
            let mut out = crate::lazy_tree::LazyStrBuilder::new();
            ev.coerce_into_lazy(pos, v, &mut out, CTX, Coercion::MORE)?;
            Ok(out.finish(ev))
        }
        _ => {
            let mut ctx = StringContext::new();
            let s = ev.coerce_to_string(pos, args[0], &mut ctx, CTX, Coercion::MORE)?;
            Ok(ev.mk_string_arena(s, &ctx))
        }
    }
}

fn prim_substring<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let start = ev.force_int(
        args[0],
        pos,
        "while evaluating the first argument (the start offset) passed to builtins.substring",
    )?;
    if start < 0 {
        return Err(ev.error_at(
            ErrorKind::Eval,
            "negative start position in 'substring'",
            pos,
        ));
    }
    let len = ev.force_int(
        args[1],
        pos,
        "while evaluating the second argument (the substring length) passed to builtins.substring",
    )?;
    if len == 0 {
        let v = ev.force(args[2], pos)?;
        if let Value::String(s) = v {
            return Ok(ev.mk_string_view(b"", crate::lazy_tree::context_without_hashes(s)));
        }
    }
    let start = start as usize;
    let clamp = |n: usize| -> (usize, usize) {
        if start >= n {
            (n, n)
        } else if len < 0 {
            (start, n)
        } else {
            (start, n.min(start + len as usize))
        }
    };
    // A string keeps the lazy hashes the range does not cut.
    if let Value::String(s) = ev.force_for_coercion(args[2], pos)? {
        let (a, b) = clamp(s.len());
        return ev.str_slice(s, a, b);
    }
    let mut ctx = StringContext::new();
    let s = ev.coerce_to_string(
        pos,
        args[2],
        &mut ctx,
        "while evaluating the third argument (the string) passed to builtins.substring",
        Coercion::COPY,
    )?;
    let (a, b) = clamp(s.len());
    Ok(ev.mk_string_arena(&s[a..b], &ctx))
}

fn prim_string_length<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    // A lazy hash is as long as the hash it stands for.
    if let Value::String(s) = ev.force_for_coercion(args[0], pos)? {
        return Ok(Value::Int(s.len() as i64));
    }
    let mut ctx = StringContext::new();
    let s = ev.coerce_to_string(
        pos,
        args[0],
        &mut ctx,
        "while evaluating the argument passed to builtins.stringLength",
        Coercion::COPY,
    )?;
    Ok(Value::Int(s.len() as i64))
}

/// The experimental features the store primitives check, from the
/// evaluator's settings.
pub(crate) fn xp_settings(ev: &Evaluator<'_>) -> XpSettings {
    XpSettings::from_names(ev.config.experimental_features.iter().map(String::as_str))
}

/// `parseHashAlgo`: unknown names are a `UsageError`, which the command
/// line front end follows with a "Try --help" hint. `blake3` needs the
/// `blake3-hashes` experimental feature. The name is bytes, quoted as
/// they are.
pub fn parse_hash_algo<'a>(ev: &Evaluator<'a>, s: &[u8], _pos: SrcPos) -> Result<HashAlgo> {
    HashAlgo::parse(s, xp_settings(ev)).map_err(|e| hash_error(ev, &e))
}

fn prim_hash_string<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let algo_s = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.hashString",
    )?;
    let algo = parse_hash_algo(ev, algo_s, pos)?;
    let mut ctx = StringContext::new();
    let s = ev.force_string_ctx(
        args[1],
        &mut ctx,
        pos,
        "while evaluating the second argument passed to builtins.hashString",
    )?;
    Ok(ev.mk_string(&Hash::of(algo, s).to_string(HashFormat::Base16, false)))
}

fn prim_convert_hash<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let attrs = ev.force_attrs(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.convertHash",
    )?;
    let hash_attr = require_attr(
        ev,
        attrs,
        ev.sym("hash"),
        "while locating the attribute 'hash'",
    )?;
    let hash = ev.force_string_no_ctx(
        hash_attr.value,
        pos,
        "while evaluating the attribute 'hash'",
    )?;
    let algo = match attrs.get(ev.sym("hashAlgo")) {
        Some(a) => {
            let s =
                ev.force_string_no_ctx(a.value, pos, "while evaluating the attribute 'hashAlgo'")?;
            Some(parse_hash_algo(ev, s, pos)?)
        }
        None => None,
    };
    let fmt_attr = require_attr(
        ev,
        attrs,
        ev.sym("toHashFormat"),
        "while locating the attribute 'toHashFormat'",
    )?;
    let fmt_s = ev.force_string_no_ctx(
        fmt_attr.value,
        pos,
        "while evaluating the attribute 'toHashFormat'",
    )?;
    // `parseHashFormat` throws a `UsageError`, so the front end adds its
    // "Try --help" hint even though this came from an expression.
    let (format, deprecation) = HashFormat::parse(fmt_s).map_err(|e| hash_error(ev, &e))?;
    if let Some(warning) = deprecation {
        ev.host.warn(warning.as_bytes());
    }
    let parsed = Hash::parse_any(hash, algo, xp_settings(ev)).map_err(|e| hash_error(ev, &e))?;
    Ok(ev.mk_string(&parsed.to_string(format, format == HashFormat::Sri)))
}

fn compile_regex<'a>(
    ev: &Evaluator<'a>,
    re: &[u8],
    pos: SrcPos,
) -> Result<crate::runtime::rc::Rc<crate::regex::Regex>> {
    if let Some(r) = ev.regex_cache.borrow().get(re) {
        return Ok(r.clone());
    }
    let r = crate::runtime::rc::Rc::new(crate::regex::Regex::new(re).inspect_err(|e| {
        e.set_pos(ev.position(pos));
    })?);
    ev.regex_cache.borrow_mut().insert(re.to_vec(), r.clone());
    Ok(r)
}

fn prim_match<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let re = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.match",
    )?;
    let regex = compile_regex(ev, re, pos)?;
    // `regex_match`: the whole string must match.
    let whole = regex.whole().inspect_err(|e| e.set_pos(ev.position(pos)))?;
    let mut ctx = StringContext::new();
    let s = ev.force_string_ctx(
        args[1],
        &mut ctx,
        pos,
        "while evaluating the second argument passed to builtins.match",
    )?;
    match whole.captures(s) {
        Some(caps) => Ok(captures_to_list(ev, &caps)),
        None => Ok(Value::Null),
    }
}

/// The capture groups as a list of strings. The groups are sub-slices of
/// the haystack, which is an arena string, so nothing is copied.
fn captures_to_list<'a>(ev: &Evaluator<'a>, caps: &regex::bytes::Captures<'a>) -> Value<'a> {
    let items: Vec<ValueRef<'a>> = (1..caps.len())
        .map(|i| match caps.get(i) {
            Some(m) => ev.alloc_cell(ev.mk_string_view(m.as_bytes(), &[])),
            None => ev.consts.null,
        })
        .collect();
    ev.mk_list(&items)
}

fn prim_split<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let re = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.split",
    )?;
    let regex = compile_regex(ev, re, pos)?;
    let re = regex
        .search()
        .inspect_err(|e| e.set_pos(ev.position(pos)))?;
    let mut ctx = StringContext::new();
    let s = ev.force_string_ctx(
        args[1],
        &mut ctx,
        pos,
        "while evaluating the second argument passed to builtins.split",
    )?;
    // Iterates like `std::regex_iterator`, which Nix uses: after a
    // non-empty match an empty match at its end *is* reported (the `regex`
    // crate's iterator skips it), and after an empty match at `p` the
    // search continues at `p` for a non-empty match, else from `p + 1`.
    let mut out: Vec<ValueRef<'a>> = Vec::new();
    let mut last = 0usize;
    let mut any = false;
    let mut pos = 0usize;
    let mut prev_empty_at: Option<usize> = None;
    while pos <= s.len() {
        let Some(caps) = re.captures_at(s, pos) else {
            break;
        };
        let m = caps.get(0).unwrap();
        if m.is_empty() && prev_empty_at == Some(m.start()) {
            // `match_not_null` failed at this position: step past it.
            if m.start() >= s.len() {
                break;
            }
            pos = m.start() + 1;
            prev_empty_at = None;
            continue;
        }
        any = true;
        out.push(ev.alloc_cell(ev.mk_string_view(&s[last..m.start()], &[])));
        out.push(ev.alloc_cell(captures_to_list(ev, &caps)));
        last = m.end();
        prev_empty_at = m.is_empty().then_some(m.end());
        pos = m.end();
    }
    if !any {
        return Ok(ev.mk_list(&[args[1]]));
    }
    out.push(ev.alloc_cell(ev.mk_string_view(&s[last..], &[])));
    Ok(ev.mk_list(&out))
}

fn prim_concat_strings_sep<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let mut ctx = StringContext::new();
    let sep = ev.force_string_ctx(
        args[0],
        &mut ctx,
        pos,
        "while evaluating the first argument (the separator string) passed to builtins.concatStringsSep",
    )?;
    let list = ev.force_list_elems(
        args[1],
        pos,
        "while evaluating the second argument (the list of strings to concat) passed to builtins.concatStringsSep",
    )?;
    // Every part borrows the arena, so the result is written once, after
    // the total length is known.
    const CTX: &str = "while evaluating one element of the list of strings to concat passed to builtins.concatStringsSep";
    let mut parts: SmallVec<[&'a [u8]; 16]> = SmallVec::with_capacity(list.len());
    // From the first element holding a lazy hash on, the result is built
    // with the hashes kept lazy (see `crate::lazy_tree`).
    let mut lazy: Option<crate::lazy_tree::LazyStrBuilder<'a>> = None;
    for (i, &elem) in list.iter().enumerate() {
        if lazy.is_none()
            && matches!(ev.force_for_coercion(elem, pos)?, Value::String(s) if s.is_lazy())
        {
            let mut b = crate::lazy_tree::LazyStrBuilder::new();
            b.context = std::mem::take(&mut ctx);
            for (j, part) in parts.iter().enumerate() {
                if j > 0 {
                    b.push_bytes(sep);
                }
                b.push_bytes(part);
            }
            lazy = Some(b);
        }
        match &mut lazy {
            Some(b) => {
                if i > 0 {
                    b.push_bytes(sep);
                }
                let v = ev.force_for_coercion(elem, pos)?;
                ev.coerce_into_lazy(pos, v, b, CTX, Coercion::COPY)?;
            }
            None => parts.push(ev.coerce_to_string(pos, elem, &mut ctx, CTX, Coercion::COPY)?),
        }
    }
    if let Some(b) = lazy {
        return Ok(b.finish(ev));
    }
    let ctx = ctx.finish(ev.bump);
    if let [single] = parts[..] {
        // One element: its bytes, with the separator's context merged in.
        return Ok(ev.mk_string_view(single, ctx));
    }
    let total =
        parts.iter().map(|p| p.len()).sum::<usize>() + sep.len() * parts.len().saturating_sub(1);
    if total == 0 && ctx.is_empty() {
        return Ok(ev.consts.empty_string.get());
    }
    let bytes = ev.bump.alloc_slice_fill_copy(total, 0u8);
    let mut offset = 0;
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            bytes[offset..offset + sep.len()].copy_from_slice(sep);
            offset += sep.len();
        }
        bytes[offset..offset + part.len()].copy_from_slice(part);
        offset += part.len();
    }
    Ok(ev.mk_string_view(bytes, ctx))
}

fn prim_replace_strings<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let from_list = ev.force_list_elems(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.replaceStrings",
    )?;
    let to_list = ev.force_list_elems(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.replaceStrings",
    )?;
    if from_list.len() != to_list.len() {
        return Err(ev.error_at(
            ErrorKind::Eval,
            "'from' and 'to' arguments passed to builtins.replaceStrings have different lengths",
            pos,
        ));
    }
    let mut from: Vec<&[u8]> = Vec::with_capacity(from_list.len());
    for &f in from_list {
        from.push(
            ev.force_string(
                f,
                pos,
                "while evaluating one of the strings to replace passed to builtins.replaceStrings",
            )
            .and_then(|s| ev.str_bytes(s))?,
        );
    }
    let mut cache: Vec<Option<&'a [u8]>> = vec![None; to_list.len()];
    let mut ctx = StringContext::new();
    let s = ev.force_string_ctx(
        args[2],
        &mut ctx,
        pos,
        "while evaluating the third argument passed to builtins.replaceStrings",
    )?;
    let mut res: Vec<u8> = Vec::with_capacity(s.len());
    let mut p = 0usize;
    while p <= s.len() {
        let mut found = false;
        for (j, f) in from.iter().enumerate() {
            if s[p..].starts_with(f) {
                found = true;
                let replacement = match cache[j] {
                    Some(r) => r,
                    None => {
                        let mut tctx = StringContext::new();
                        let ts = ev.force_string_ctx(
                            to_list[j],
                            &mut tctx,
                            pos,
                            "while evaluating one of the replacement strings passed to builtins.replaceStrings",
                        )?;
                        ctx.extend_from(&tctx);
                        cache[j] = Some(ts);
                        ts
                    }
                };
                res.extend_from_slice(replacement);
                if f.is_empty() {
                    if p < s.len() {
                        res.push(s[p]);
                    }
                    p += 1;
                } else {
                    p += f.len();
                }
                break;
            }
        }
        if !found {
            if p < s.len() {
                res.push(s[p]);
            }
            p += 1;
        }
    }
    Ok(ev.mk_string_ctx(&res, &ctx))
}

/// `DrvName`: `apache-httpd-2.0.48` is (`apache-httpd`, `2.0.48`), the name
/// ending at the first dash not followed by a letter (`parseDrvName`, `nix
/// search`, the names of build logs).
pub fn parse_drv_name(s: &[u8]) -> (&[u8], &[u8]) {
    match drv_name_dash(s) {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, b""),
    }
}

/// [`parse_drv_name`] on text: the dash is ASCII, so both parts are text.
pub fn parse_drv_name_str(s: &str) -> (&str, &str) {
    match drv_name_dash(s.as_bytes()) {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    }
}

/// Where `DrvName` splits `s`: the first dash not followed by a letter.
fn drv_name_dash(s: &[u8]) -> Option<usize> {
    s.windows(2)
        .position(|w| w[0] == b'-' && !w[1].is_ascii_alphabetic())
}

fn prim_parse_drv_name<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let s = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.parseDrvName",
    )?;
    let (name, version) = parse_drv_name(s);
    let attrs = vec![
        ev.attr(sym::NAME, ev.mk_string_view(name, &[])),
        ev.attr(ev.sym("version"), ev.mk_string_view(version, &[])),
    ];
    Ok(ev.mk_attrs(attrs, SrcPos::NONE))
}

/// `nextComponent`: the next version component (digits, or non-digits
/// up to a separator).
fn next_component<'s>(s: &'s [u8], p: &mut usize) -> &'s [u8] {
    while *p < s.len() && (s[*p] == b'.' || s[*p] == b'-') {
        *p += 1;
    }
    if *p >= s.len() {
        return b"";
    }
    let start = *p;
    if s[*p].is_ascii_digit() {
        while *p < s.len() && s[*p].is_ascii_digit() {
            *p += 1;
        }
    } else {
        while *p < s.len() && !s[*p].is_ascii_digit() && s[*p] != b'.' && s[*p] != b'-' {
            *p += 1;
        }
    }
    &s[start..*p]
}

/// `string2Int<int>`: a decimal `int` (32-bit), or none.
fn parse_c_int(c: &[u8]) -> Option<i32> {
    let s = std::str::from_utf8(c).ok()?;
    if s.is_empty() {
        return None;
    }
    s.parse::<i32>().ok()
}

fn components_lt(c1: &[u8], c2: &[u8]) -> bool {
    let n1 = parse_c_int(c1);
    let n2 = parse_c_int(c2);
    match (n1, n2) {
        (Some(a), Some(b)) => a < b,
        _ if c1.is_empty() && n2.is_some() => true,
        _ if c1 == b"pre" && c2 != b"pre" => true,
        _ if c2 == b"pre" => false,
        _ if n2.is_some() => true,
        _ if n1.is_some() => false,
        _ => c1 < c2,
    }
}

/// `compareVersions`.
pub fn compare_versions(v1: &[u8], v2: &[u8]) -> std::cmp::Ordering {
    let (mut p1, mut p2) = (0usize, 0usize);
    while p1 < v1.len() || p2 < v2.len() {
        let c1 = next_component(v1, &mut p1);
        let c2 = next_component(v2, &mut p2);
        if components_lt(c1, c2) {
            return std::cmp::Ordering::Less;
        } else if components_lt(c2, c1) {
            return std::cmp::Ordering::Greater;
        }
    }
    std::cmp::Ordering::Equal
}

fn prim_compare_versions<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let v1 = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.compareVersions",
    )?;
    let v2 = ev.force_string_no_ctx(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.compareVersions",
    )?;
    Ok(Value::Int(match compare_versions(v1, v2) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }))
}

fn prim_split_version<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let v = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.splitVersion",
    )?;
    let mut p = 0usize;
    let mut parts: Vec<ValueRef<'a>> = Vec::new();
    while p < v.len() {
        let c = next_component(v, &mut p);
        if c.is_empty() {
            break;
        }
        parts.push(ev.alloc_cell(ev.mk_string_view(c, &[])));
    }
    Ok(ev.mk_list(&parts))
}

fn prim_from_toml<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let s = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.fromTOML",
    )?;
    let text = String::from_utf8_lossy(s);
    let doc: toml::Value = toml::from_str(&text).map_err(|e| {
        ev.error_at(
            ErrorKind::Eval,
            format!("while parsing TOML: {}", e.message()),
            pos,
        )
    })?;
    toml_to_value(ev, &doc, pos)
}

fn toml_to_value<'a>(ev: &Evaluator<'a>, t: &toml::Value, pos: SrcPos) -> Result<Value<'a>> {
    Ok(match t {
        toml::Value::Table(table) => {
            let mut attrs: Vec<Attr<'a>> = Vec::with_capacity(table.len());
            for (k, v) in table {
                toml_no_nul(ev, k, pos)?;
                let value = toml_to_value(ev, v, pos)?;
                attrs.push(ev.attr(ev.sym(k), value));
            }
            ev.mk_attrs(attrs, SrcPos::NONE)
        }
        toml::Value::Array(items) => {
            let cells: Vec<ValueRef<'a>> = items
                .iter()
                .map(|i| Ok(ev.alloc_cell(toml_to_value(ev, i, pos)?)))
                .collect::<Result<_>>()?;
            ev.mk_list(&cells)
        }
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Integer(i) => Value::Int(*i),
        toml::Value::Float(f) => Value::Float(*f),
        toml::Value::String(s) => {
            toml_no_nul(ev, s, pos)?;
            ev.mk_string(s)
        }
        toml::Value::Datetime(d) => {
            if !ev.experimental("parse-toml-timestamps") {
                return Err(ev.error_at(
                    ErrorKind::Eval,
                    "while parsing TOML: Dates and times are not supported",
                    pos,
                ));
            }
            let attrs = vec![
                ev.attr(ev.sym("_type"), ev.mk_string("timestamp")),
                ev.attr(
                    sym::VALUE,
                    ev.mk_string(&normalize_toml_datetime(&d.to_string())),
                ),
            ];
            ev.mk_attrs(attrs, SrcPos::NONE)
        }
    })
}

/// `forceNoNullByte` inside `fromTOML`: Nix wraps the message in its
/// "while parsing TOML" error.
fn toml_no_nul<'a>(ev: &Evaluator<'a>, s: &str, pos: SrcPos) -> Result<()> {
    if s.contains('\0') {
        return Err(ev.error_at(
            ErrorKind::Eval,
            format!(
                "while parsing TOML: error: input string '{}' cannot be represented as Nix string because it contains null bytes",
                s.replace('\0', "␀")
            ),
            pos,
        ));
    }
    Ok(())
}

/// The error kind for a failed hash parse. Nix's `parseHashAlgo` and
/// `parseHashFormat` throw `UsageError` (the front end then prints its
/// "Try --help" hint), every other `BadHash` is a plain `Error`.
pub(crate) fn hash_error_kind(e: &brix_store::HashError) -> ErrorKind {
    match e {
        brix_store::HashError::UnknownAlgo(_) | brix_store::HashError::UnknownFormat(_) => {
            ErrorKind::Usage
        }
        _ => ErrorKind::Eval,
    }
}

/// A failed hash parse as an evaluation error: its kind
/// ([`hash_error_kind`]) and Nix's message, the hash's or name's bytes
/// as they are.
#[cold]
pub(crate) fn hash_error(ev: &Evaluator<'_>, e: &brix_store::HashError) -> crate::Error {
    ev.error(hash_error_kind(e), e.message_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn drv_names() {
        assert_eq!(
            parse_drv_name(b"apache-httpd-2.0.48"),
            (&b"apache-httpd"[..], &b"2.0.48"[..])
        );
        assert_eq!(parse_drv_name(b"hello"), (&b"hello"[..], &b""[..]));
        assert_eq!(parse_drv_name(b"foo-"), (&b"foo-"[..], &b""[..]));
        assert_eq!(
            parse_drv_name_str("\u{e9}t\u{e9}-1.0"),
            ("\u{e9}t\u{e9}", "1.0")
        );
        assert_eq!(parse_drv_name_str("a-b-c"), ("a-b-c", ""));
    }

    #[test]
    fn versions() {
        assert_eq!(compare_versions(b"1.0", b"2.3"), Ordering::Less);
        assert_eq!(compare_versions(b"2.1", b"2.3"), Ordering::Less);
        assert_eq!(compare_versions(b"2.3", b"2.3"), Ordering::Equal);
        assert_eq!(compare_versions(b"2.5", b"2.3"), Ordering::Greater);
        assert_eq!(compare_versions(b"3.1", b"2.3"), Ordering::Greater);
        assert_eq!(compare_versions(b"2.3.1", b"2.3"), Ordering::Greater);
        assert_eq!(compare_versions(b"2.3.1", b"2.3a"), Ordering::Greater);
        assert_eq!(compare_versions(b"2.3pre1", b"2.3"), Ordering::Less);
        assert_eq!(compare_versions(b"2.3pre3", b"2.3pre12"), Ordering::Less);
        assert_eq!(compare_versions(b"2.3a", b"2.3c"), Ordering::Less);
        assert_eq!(compare_versions(b"2.3pre1", b"2.3c"), Ordering::Less);
        assert_eq!(compare_versions(b"2.3pre1", b"2.3q"), Ordering::Less);
    }
}
