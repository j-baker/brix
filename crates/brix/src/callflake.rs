//! `callFlake` and the flake built-ins (`getFlake`, `parseFlakeRef`,
//! `flakeRefToString`), which Nix's `libflake` adds to `builtins`.

use std::collections::{BTreeSet, HashMap};

use brix_eval::managed_api::{GcCallContext, GcCallValue, GcEvaluator, GcView};
use brix_eval::value::{SrcPos, ValueRef};
use brix_eval::{ErrorKind, Evaluator, Origin};
use brix_flake::{
    call_flake_args, parse_flake_ref, source_info, Attr, Attrs, FlakeRef, LockFile, LockFlags,
    LockedFlake, NodeId, ParseOptions,
};
use brix_store::CanonPath;

use crate::flake::{lock_and_call_raw, LockAndCallError, TreeHandle, TreeRoot};
use crate::host::{NativeHost, Override};

/// brix's rendering of `call-flake.nix`. `__brixFlakeOverride callId key`
/// stands in for Nix's `overrides` attribute set: it yields
/// `{ sourceInfo; dir; }` for a node whose tree is already known and
/// `null` otherwise. Everything else follows Nix's file; the semantics
/// are spelled out in `brix_flake::call_flake`.
const CALL_FLAKE: &str = r#"callId: lockFileStr:

let
  inherit (builtins) mapAttrs;

  lockFile = builtins.fromJSON lockFileStr;

  override = key: __brixFlakeOverride callId key;

  resolveInput =
    inputSpec: if builtins.isList inputSpec then getInputByPath lockFile.root inputSpec else inputSpec;

  getInputByPath =
    nodeName: path:
    if path == [ ] then
      nodeName
    else
      getInputByPath (resolveInput lockFile.nodes.${nodeName}.inputs.${builtins.head path}) (
        builtins.tail path
      );

  allNodes = mapAttrs (
    key: node:
    let
      ov = override key;
      hasOverride = ov != null;
      isRelative = node.locked.type or null == "path" && builtins.substring 0 1 node.locked.path != "/";

      parentNode = allNodes.${getInputByPath lockFile.root node.parent};

      sourceInfo =
        if node.buildTime or false then
          derivation {
            name = "source";
            builder = "builtin:fetch-tree";
            system = "builtin";
            __structuredAttrs = true;
            input = node.locked;
            outputHashMode = "recursive";
            outputHash = node.locked.narHash;
          }
        else if hasOverride then
          ov.sourceInfo
        else if isRelative then
          parentNode.sourceInfo
        else
          builtins.fetchTree (node.info or { } // removeAttrs node.locked [ "dir" ]);

      subdir = if hasOverride then ov.dir else node.locked.dir or "";

      outPath =
        if !hasOverride && isRelative then
          parentNode.outPath + (if node.locked.path == "" then "" else "/" + node.locked.path)
        else
          sourceInfo.outPath + (if subdir == "" then "" else "/" + subdir);

      # Where `flake.nix` is read from. A lazily mounted tree is read
      # through a path into it, so that its store path (`outPath`) is only
      # computed if something asks for it.
      srcPath =
        if !hasOverride && isRelative then
          parentNode.srcPath + (if node.locked.path == "" then "" else "/" + node.locked.path)
        else if hasOverride && ov ? root then
          ov.root + (if subdir == "" then "" else "/" + subdir)
        else
          outPath;

      flake = import (srcPath + "/flake.nix");

      inputs = mapAttrs (inputName: inputSpec: allNodes.${resolveInput inputSpec}.result) (
        node.inputs or { }
      );

      outputs = flake.outputs (inputs // { self = result; });

      result =
        outputs
        // sourceInfo
        // {
          inherit outPath;
          inherit inputs;
          inherit outputs;
          inherit sourceInfo;
          _type = "flake";
        };
    in
    {
      result =
        if node.flake or true then
          assert builtins.isFunction flake.outputs;
          assert !(node.buildTime or false);
          result
        else
          sourceInfo // { inherit sourceInfo outPath; };

      inherit outPath sourceInfo srcPath;
    }
  ) lockFile.nodes;

in
allNodes.${lockFile.root}.result
"#;

/// [`brix_flake::prefetch_plan`] by node key, each node as the input
/// `call-flake.nix` passes to `fetchTree` for it (`removeAttrs
/// node.locked [ "dir" ]` read back from the lock file's JSON) and as
/// `fetchTree` fetches it (final when it has a `narHash`), so that the
/// host matches a background fetch to the request for it.
fn prefetch_plan(
    host: &NativeHost,
    lock: &LockFile,
    known: &BTreeSet<NodeId>,
) -> HashMap<String, Vec<brix_flake::Input>> {
    let (_, keys) = lock.to_json();
    let fetch_tree_input = |node: &NodeId| {
        let locked = lock.node(*node).locked.as_ref()?;
        let mut attrs = locked.locked_ref.input.attrs.clone();
        // Not serialised in the lock file, but `fetchTree` makes an input
        // with a `narHash` final again (`NativeHost::fetch_tree`).
        attrs.remove("__final");
        let mut input = brix_flake::Input::from_attrs(&host.settings, attrs).ok()?;
        if matches!(input.nar_hash(), Ok(Some(_))) {
            input.set_final();
        }
        Some(input)
    };
    brix_flake::prefetch_plan(lock, known)
        .into_iter()
        .map(|(node, others)| {
            (
                keys[&node].clone(),
                others.iter().filter_map(fetch_tree_input).collect(),
            )
        })
        .collect()
}

/// `callFlake`: the value of a locked flake, retained in the moving heap.
pub(crate) fn call_flake<'a, 'h>(
    cx: &mut GcCallContext<'_, 'a, 'h>,
    host: &NativeHost,
    locked: &LockedFlake<TreeHandle>,
) -> brix_eval::Result<GcCallValue<'h>> {
    let args = call_flake_args(locked);
    let known: BTreeSet<NodeId> = args.overrides.iter().map(|ov| ov.node).collect();
    let mut overrides = HashMap::new();
    for ov in args.overrides {
        if ov.dir.subdir == ".." || ov.dir.subdir.starts_with("../") {
            let abs = crate::flake::flake_dir_path(host, &ov.dir);
            let canon = CanonPath::new(&abs).map_err(|e| {
                cx.services()
                    .error_at(ErrorKind::Eval, e.message_bytes(), SrcPos::NONE)
            })?;
            let message = match host.store_dir().to_store_path(canon.abs()) {
                Err(e) => e.message_bytes(),
                Ok(_) => [format!("brix does not support the relative flake input '{}', which leaves its tree for '", ov.input).as_bytes(), canon.as_bytes(), b"'"].concat(),
            };
            return Err(cx
                .services()
                .error_at(ErrorKind::Eval, message, SrcPos::NONE));
        }
        overrides.insert(
            ov.node_key,
            Override {
                root: ov.dir.tree.root,
                subdir: ov.dir.subdir,
                input: ov.input,
                force_dirty: ov.force_dirty,
            },
        );
    }
    let call_id = host.register_overrides(overrides);
    host.register_prefetch_plan(call_id, prefetch_plan(host, &locked.lock_file, &known));
    let fun = cx.eval_source(
        CALL_FLAKE.as_bytes(),
        "/",
        Origin::Internal {
            name: "call-flake.nix".into(),
        },
    )?;
    let id = cx.int(call_id as i64)?;
    let lock = cx.string(args.lock_file_json.as_bytes())?;
    let fun = cx.apply(&fun, &id)?;
    cx.apply(&fun, &lock)
}

fn attr_to_value<'h>(
    cx: &mut GcCallContext<'_, '_, 'h>,
    attr: &Attr,
) -> brix_eval::Result<GcCallValue<'h>> {
    match attr {
        Attr::String(s) => cx.string(s.as_bytes()),
        Attr::Int(n) => cx.int(*n as i64),
        Attr::Bool(b) => cx.bool(*b),
    }
}
fn attrs_value<'h>(
    cx: &mut GcCallContext<'_, '_, 'h>,
    attrs: Vec<(Vec<u8>, GcCallValue<'h>)>,
) -> brix_eval::Result<GcCallValue<'h>> {
    let attrs: Vec<_> = attrs
        .iter()
        .map(|(name, value)| (name.as_slice(), value.clone()))
        .collect();
    cx.attrs(&attrs)
}
fn attrs_to_value<'h>(
    cx: &mut GcCallContext<'_, '_, 'h>,
    attrs: &Attrs,
) -> brix_eval::Result<GcCallValue<'h>> {
    let mut out = Vec::with_capacity(attrs.len());
    for (name, value) in attrs {
        out.push((name.as_bytes().to_vec(), attr_to_value(cx, value)?));
    }
    attrs_value(cx, out)
}

fn prim_flake_override<'h>(
    cx: &mut GcCallContext<'_, '_, 'h>,
    host: &NativeHost,
    pos: SrcPos,
    args: &[GcCallValue<'h>],
) -> brix_eval::Result<GcCallValue<'h>> {
    let call = cx.force_int(&args[0], pos, "while evaluating a flake override id")? as u64;
    let key = cx.force_string_no_ctx(&args[1], pos, "while evaluating a lock file node key")?;
    let key = flake_ref_text(cx.services(), &key, "__brixFlakeOverride", pos)?;
    let built = host.with_override(call, &key, |ov| -> brix_eval::Result<GcCallValue<'h>> {
        let display = ov.root.mount_path(host.store_dir());
        let info = source_info(&display, &ov.input, false, ov.force_dirty)
            .map_err(|e| cx.services().error_at(ErrorKind::Eval, e.to_string(), pos))?;
        let mut attrs = Vec::with_capacity(info.len() + 1);
        for (name, value) in &info {
            let value = match (name.as_str(), &ov.root) {
                ("outPath", TreeRoot::Store(path)) => cx.store_path_string(path)?,
                ("outPath", TreeRoot::Lazy { id, .. }) => cx.lazy_tree_out_path_thunk(*id)?,
                _ => attr_to_value(cx, value)?,
            };
            attrs.push((name.as_bytes().to_vec(), value));
        }
        if let TreeRoot::Store(path) = &ov.root {
            if !info.contains_key("narHash") {
                let known = match host.tree(path) {
                    Some(tree) => tree
                        .input
                        .nar_hash()
                        .map_err(|e| cx.services().error_at(ErrorKind::Eval, e.to_string(), pos))?,
                    None => None,
                };
                if let Some(hash) = known {
                    attrs.push((b"narHash".to_vec(), cx.string(hash.to_sri().as_bytes())?));
                }
            }
        }
        let mut node = vec![(b"dir".to_vec(), cx.string(ov.subdir.as_bytes())?)];
        if let TreeRoot::Lazy { id, placeholder } = &ov.root {
            if !info.contains_key("narHash") {
                attrs.push((b"narHash".to_vec(), cx.lazy_tree_nar_hash_thunk(*id)?));
            }
            let root = CanonPath::new(placeholder).map_err(|e| {
                cx.services()
                    .error_at(ErrorKind::Eval, e.message_bytes(), pos)
            })?;
            node.push((
                b"root".to_vec(),
                cx.path(&brix_store::SourcePath::new(cx.root_accessor(), root))?,
            ));
        }
        node.push((b"sourceInfo".to_vec(), attrs_value(cx, attrs)?));
        attrs_value(cx, node)
    });
    match built {
        Some(result) => result,
        None => {
            host.node_needed(call, &key);
            cx.null()
        }
    }
}

fn prim_get_flake<'h>(
    cx: &mut GcCallContext<'_, '_, 'h>,
    host: &NativeHost,
    pos: SrcPos,
    args: &[GcCallValue<'h>],
) -> brix_eval::Result<GcCallValue<'h>> {
    cx.force_at(&args[0], pos)?;
    let path = match cx.view(&args[0])? {
        GcView::Path { accessor, bytes } => Some((accessor, bytes.to_vec())),
        _ => None,
    };
    let reference = if let Some((accessor, bytes)) = &path {
        let text = cx.services().path_text(&brix_eval::value::PathVal {
            accessor: *accessor,
            path: bytes,
        })?;
        flake_ref_text(
            cx.services(),
            &[&b"path:"[..], text.as_ref()].concat(),
            "builtins.getFlake",
            pos,
        )?
    } else {
        let (bytes, context) = cx.force_string_parts(
            &args[0],
            pos,
            "while evaluating the argument passed to builtins.getFlake",
        )?;
        if !context.is_empty() {
            cx.realise_context(&context, false)?;
        }
        flake_ref_text(cx.services(), &bytes, "builtins.getFlake", pos)?
    };
    let flake_ref = parse_flake_ref(&host.settings, &reference, &ParseOptions::flake())
        .map_err(|e| cx.services().error_at(ErrorKind::Eval, e.to_string(), pos))?;
    let locked = flake_ref
        .input
        .is_locked(&host.settings)
        .map_err(|e| cx.services().error_at(ErrorKind::Eval, e.to_string(), pos))?;
    if path.is_none() && !locked {
        let at = cx
            .services()
            .position(pos)
            .map(|p| p.display())
            .unwrap_or_else(|| "«none»:0".into());
        return Err(cx.services().error_at(
            ErrorKind::Eval,
            [
                format!("cannot call 'getFlake' on unlocked flake reference '{reference}', at ")
                    .as_bytes(),
                at.as_slice(),
                b" (use --impure to override)",
            ]
            .concat(),
            pos,
        ));
    }
    match lock_and_call_raw(cx, host, &flake_ref, &LockFlags::pure(), true) {
        Ok((_, value)) => Ok(value),
        Err(LockAndCallError::Eval(error)) => Err(error),
        Err(LockAndCallError::Other(error)) => {
            Err(error.into_eval(cx.services(), ErrorKind::Eval, pos))
        }
    }
}

fn flake_ref_text(
    ev: &Evaluator<'_>,
    bytes: &[u8],
    builtin: &str,
    pos: SrcPos,
) -> brix_eval::Result<String> {
    std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| {
        let mut message = b"'".to_vec(); message.extend_from_slice(bytes);
        message.extend_from_slice(format!("' given to '{builtin}' is not valid UTF-8; brix does not support non-UTF-8 flake references").as_bytes());
        ev.error_at(ErrorKind::Eval, message, pos)
    })
}
fn prim_parse_flake_ref<'h>(
    cx: &mut GcCallContext<'_, '_, 'h>,
    host: &NativeHost,
    pos: SrcPos,
    args: &[GcCallValue<'h>],
) -> brix_eval::Result<GcCallValue<'h>> {
    let bytes = cx.force_string_no_ctx(
        &args[0],
        pos,
        "while evaluating the argument passed to builtins.parseFlakeRef",
    )?;
    let text = flake_ref_text(cx.services(), &bytes, "builtins.parseFlakeRef", pos)?;
    let reference = parse_flake_ref(&host.settings, &text, &ParseOptions::flake())
        .map_err(|e| cx.services().error_at(ErrorKind::Eval, e.to_string(), pos))?;
    attrs_to_value(cx, &reference.to_attrs())
}
fn prim_flake_ref_to_string<'h>(
    cx: &mut GcCallContext<'_, '_, 'h>,
    host: &NativeHost,
    pos: SrcPos,
    args: &[GcCallValue<'h>],
) -> brix_eval::Result<GcCallValue<'h>> {
    cx.force_attrs(
        &args[0],
        SrcPos::NONE,
        "while evaluating the argument passed to builtins.flakeRefToString",
    )?;
    let bindings = cx.attrs_with_pos(&args[0])?;
    let mut attrs = Attrs::new();
    let mut context = Vec::new();
    for (name, attr_pos, value) in bindings {
        let name = flake_ref_text(cx.services(), &name, "builtins.flakeRefToString", pos)?;
        cx.force_at(&value, attr_pos)?;
        match cx.view(&value)? {
            GcView::Int(n) => { if n < 0 { return Err(cx.services().error_at(ErrorKind::Eval, format!("negative value given for flake ref attr {name}: {n}"), pos)); } attrs.insert(name, Attr::Int(n as u64)); }
            GcView::Bool(b) => { attrs.insert(name, Attr::Bool(b)); }
            GcView::String(_) => {
                let (bytes, more) = cx.force_string_parts(&value, attr_pos, "while evaluating an attribute in 'builtins.flakeRefToString'")?;
                context.extend(more);
                attrs.insert(name, Attr::String(flake_ref_text(cx.services(), &bytes, "builtins.flakeRefToString", pos)?));
            }
            _ => return Err(cx.services().error(ErrorKind::Eval, format!("flake reference attribute sets may only contain integers, Booleans, and strings, but attribute '{name}' is {}", cx.show_type(&value)?))),
        }
    }
    let reference = FlakeRef::from_attrs(&host.settings, &attrs)
        .map_err(|e| cx.services().error_at(ErrorKind::Eval, e.to_string(), pos))?;
    let url = reference
        .to_url_string(false)
        .map_err(|e| cx.services().error_at(ErrorKind::Eval, e.to_string(), pos))?;
    cx.string_with_context(url.as_bytes(), &context)
}

/// Metadata registration only. Managed callbacks provide every implementation.
pub(crate) fn register<'a>(ev: &Evaluator<'a>, arena: &'a brix_eval::Arena, _host: &'a NativeHost) {
    for (name, arity) in [
        ("__brixFlakeOverride", 2),
        ("__getFlake", 1),
        ("__parseFlakeRef", 1),
        ("__flakeRefToString", 1),
    ] {
        let imp: &'a brix_eval::value::PrimOpFn<'a> =
            arena.alloc(move |ev: &Evaluator<'a>, pos: SrcPos, _: &[ValueRef<'a>]| {
                Err(ev.error_at(
                    ErrorKind::Unsupported,
                    format!("'{name}' requires the managed evaluator"),
                    pos,
                ))
            });
        ev.add_primop(name, arity, imp);
    }
}

pub(crate) fn register_callbacks<'a>(
    runtime: &mut GcEvaluator<'a>,
    host: &'a NativeHost,
) -> brix_eval::Result<()> {
    runtime.register_callback("brixFlakeOverride", move |cx, pos, args| {
        prim_flake_override(cx, host, pos, args)
    })?;
    runtime.register_callback("getFlake", move |cx, pos, args| {
        prim_get_flake(cx, host, pos, args)
    })?;
    runtime.register_callback("parseFlakeRef", move |cx, pos, args| {
        prim_parse_flake_ref(cx, host, pos, args)
    })?;
    runtime.register_callback("flakeRefToString", move |cx, pos, args| {
        prim_flake_ref_to_string(cx, host, pos, args)
    })?;
    Ok(())
}
