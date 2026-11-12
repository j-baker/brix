//! Parsing files and expressions, `import`, and file-system reads.
//!
//! `EvalState::evalFile`, `resolveExprPath`, `parseExprFromFile` and the
//! accessor-level reads (`readFile`, `readDir`, `pathExists`) that
//! builtins use, with pure-mode access control. Error messages name the
//! path as the user sees it (the logical path on the root file system),
//! not where the host keeps the bytes.

use std::sync::Arc;

use brix_store::{AccessorError, CanonPath, FileType, SourceAccessor, SourcePath, Stat};
use brix_syntax::ast::Expr;

use crate::compile::StaticEnv;
use crate::error::{msg, Error, ErrorKind, EvalError, Position, SourceInfo};
use crate::evaluator::Evaluator;
use crate::ir::{Node, Origin, PathAnchor};
use crate::value::{PathVal, SrcPos, Value, ValueRef};
use crate::Result;

impl<'a> Evaluator<'a> {
    fn syntax_origin(origin: &Origin) -> brix_syntax::Origin {
        match origin {
            Origin::File { display, .. } => brix_syntax::Origin::Path(display.clone()),
            Origin::String => brix_syntax::Origin::String,
            Origin::Stdin => brix_syntax::Origin::Stdin,
            Origin::Internal { name } => {
                brix_syntax::Origin::Path(format!("«nix-internal»/{name}").into())
            }
        }
    }

    /// What the parser accepts: the `pipe-operators` feature and the
    /// `lint-*` settings.
    pub fn parse_options(&self) -> brix_syntax::ParseOptions {
        brix_syntax::ParseOptions {
            pipe_operators: self.experimental("pipe-operators"),
            lints: self.config.lints,
        }
    }

    /// Parses source text, reporting errors as Nix does (`ParseError`).
    /// The warnings the parser finds go to the host as it logs them,
    /// before any error. Relative path literals resolve against
    /// `base_dir`, whose bytes they keep.
    pub fn parse_source(&self, src: &[u8], base_dir: &[u8], origin: &Origin) -> Result<Expr> {
        let opts = self.parse_options();
        let started = crate::platform::Instant::now();
        crate::stats::Stats::bump(&self.stats.files_parsed);
        crate::stats::Stats::add(&self.stats.source_bytes, src.len() as u64);
        let mut warnings = Vec::new();
        let parsed = brix_syntax::parse(src, base_dir, &opts, &mut warnings);
        crate::stats::Stats::add_time(&self.stats.parse_time, started.elapsed());
        if !warnings.is_empty() {
            let syntax_origin = Self::syntax_origin(origin);
            for w in &warnings {
                self.host.log(&w.render(&syntax_origin, src));
            }
        }
        parsed.map_err(|e| {
            let info = crate::runtime::rc::Rc::new(SourceInfo::new(origin.clone(), src.to_vec()));
            parse_error(info, &e.msg, e.pos.map(|p| p.0), e.defined_at.map(|p| p.0))
        })
    }

    /// Parses, loads and evaluates an expression given as text (like
    /// `nix-instantiate --expr`). Relative paths resolve against
    /// `base_dir` (an absolute path, bytes that need not be UTF-8).
    pub fn eval_str(&self, src: &[u8], base_dir: impl AsRef<[u8]>) -> Result<Value<'a>> {
        self.eval_source(src, base_dir, Origin::String)
    }

    /// Like [`Evaluator::eval_str`] but with an explicit origin.
    pub fn eval_source(
        &self,
        src: &[u8],
        base_dir: impl AsRef<[u8]>,
        origin: Origin,
    ) -> Result<Value<'a>> {
        let expr = self.parse_source(src, base_dir.as_ref(), &origin)?;
        let code = self.load(&expr, origin, src.to_vec(), None, false)?;
        self.eval_node(code, code.root, self.base_env)
    }

    /// Evaluates a helper expression of the evaluator's own, shown as
    /// `«nix-internal»/<name>` in positions.
    pub fn eval_internal(&self, name: &str, src: &str) -> Result<ValueRef<'a>> {
        let origin = Origin::Internal {
            name: name.to_owned(),
        };
        let expr = self.parse_source(src.as_bytes(), b"/", &origin)?;
        let code = self.load(&expr, origin, src.as_bytes().to_vec(), None, true)?;
        let v = self.eval_node(code, code.root, self.base_env)?;
        Ok(self.alloc_cell(v))
    }

    /// Evaluates a file given on the command line (`nix-instantiate
    /// file.nix`): parsed and evaluated directly, without the import
    /// cache and without the "while evaluating the file" trace that
    /// `import` adds.
    pub fn eval_main_file(&self, p: &PathVal<'a>) -> Result<Value<'a>> {
        let resolved = self.resolve_expr_path(p)?;
        let display = self.accessor(resolved.accessor).show_path(&resolved.path);
        self.eval_file_contents(&resolved, &display, None, false)
    }

    /// Translates an accessor error into Nix's message for the logical
    /// path, which it quotes as the bytes it is.
    fn fs_error(&self, accessor: &dyn SourceAccessor, path: &CanonPath, e: AccessorError) -> Error {
        let shown = accessor.show_path(path);
        let msg = match e.untraced() {
            AccessorError::NotFound(_) => msg!("path '", shown, "' does not exist"),
            AccessorError::NotADirectory(_) => msg!("'", shown, "' is not a directory"),
            AccessorError::NotARegularFile(_) => msg!("'", shown, "' is not a regular file"),
            AccessorError::NotASymlink(_) => msg!("'", shown, "' is not a symlink"),
            AccessorError::SymlinkLoop(_) => {
                msg!("infinite symlink recursion in path '", shown, "'")
            }
            other => other.message_bytes(),
        };
        let error = self.error(ErrorKind::FileSystem, msg);
        // What the accessor was doing when it failed (fetching a Git LFS
        // file: "while smudging git-lfs file '…'"), as Nix's `addTrace`
        // adds it on the way out.
        for trace in e.traces() {
            error.add_trace(None, trace);
        }
        error
    }

    /// The error for a path `maybe_lstat` did not find: what the
    /// accessor's `lstat` says (a Git checkout explains that a file is not
    /// tracked), as Nix reports the failure of the operation itself.
    fn not_found(&self, accessor: &dyn SourceAccessor, path: &CanonPath) -> Error {
        match accessor.lstat(path) {
            Err(e) => self.fs_error(accessor, path, e),
            // It appeared in the meantime; it still was not there.
            Ok(_) => self.error(
                ErrorKind::FileSystem,
                msg!("path '", accessor.show_path(path), "' does not exist"),
            ),
        }
    }

    /// `lstat`: what is at `path`, which must exist (an accessor error, or
    /// the accessor's own reason for a missing path, otherwise).
    fn existing(&self, accessor: &dyn SourceAccessor, path: &CanonPath) -> Result<Stat> {
        match accessor
            .maybe_lstat(path)
            .map_err(|e| self.fs_error(accessor, path, e))?
        {
            Some(st) => Ok(st),
            None => Err(self.not_found(accessor, path)),
        }
    }

    /// The accessor and canonical path of a path value, after pure-mode
    /// access checks.
    pub fn checked_source(&self, p: &PathVal<'_>) -> Result<(Arc<dyn SourceAccessor>, CanonPath)> {
        self.check_path_access(p)?;
        let path = CanonPath::new(p.path)
            .map_err(|e| self.error(ErrorKind::FileSystem, e.message_bytes()))?;
        Ok((self.accessor(p.accessor), path))
    }

    fn resolve(
        &self,
        accessor: &dyn SourceAccessor,
        path: &CanonPath,
        resolve_final: bool,
    ) -> Result<CanonPath> {
        accessor
            .resolve_symlinks(path, resolve_final)
            .map_err(|e| self.fs_error(accessor, path, e))
    }

    /// `SourcePath::readFile` after resolving symlinks: the file must
    /// exist.
    pub fn read_file(&self, p: &PathVal<'_>) -> Result<Vec<u8>> {
        let (accessor, path) = self.checked_source(p)?;
        let resolved = self.resolve(&*accessor, &path, true)?;
        self.existing(&*accessor, &resolved)?;
        accessor
            .read_file(&resolved)
            .map_err(|e| self.fs_error(&*accessor, &resolved, e))
    }

    /// `maybeLstat` after resolving symlinks in the ancestors (and in the
    /// final component when `resolve_final`).
    pub fn lstat(&self, p: &PathVal<'_>, resolve_final: bool) -> Result<Option<Stat>> {
        let (accessor, path) = self.checked_source(p)?;
        let resolved = self.resolve(&*accessor, &path, resolve_final)?;
        accessor
            .maybe_lstat(&resolved)
            .map_err(|e| self.fs_error(&*accessor, &resolved, e))
    }

    /// `lstat` without resolving the final component; the path must exist.
    pub fn lstat_required(&self, p: &PathVal<'_>) -> Result<Stat> {
        let (accessor, path) = self.checked_source(p)?;
        let resolved = self.resolve(&*accessor, &path, false)?;
        self.existing(&*accessor, &resolved)
    }

    /// `SourcePath::readDirectory` after resolving symlinks.
    pub fn read_dir(&self, p: &PathVal<'_>) -> Result<brix_store::DirEntries> {
        let (accessor, path) = self.checked_source(p)?;
        let resolved = self.resolve(&*accessor, &path, true)?;
        if self.existing(&*accessor, &resolved)?.file_type != FileType::Directory {
            return Err(self.error(
                ErrorKind::FileSystem,
                msg!("'", accessor.show_path(&resolved), "' is not a directory"),
            ));
        }
        accessor
            .read_dir(&resolved)
            .map_err(|e| self.fs_error(&*accessor, &resolved, e))
    }

    /// `resolveExprPath`: follow a symlink at the path itself, then append
    /// `default.nix` for a directory.
    pub fn resolve_expr_path(&self, p: &PathVal<'_>) -> Result<SourcePath> {
        let (accessor, mut path) = self.checked_source(p)?;
        let mut follows = 0;
        // What `path` is once its symlinks are resolved. Nix resolves the
        // whole path again to find out; the loop below has just resolved
        // its parent and looked at its last component, which comes to
        // the same.
        let st = loop {
            if path.is_root() {
                let resolved = self.resolve(&*accessor, &path, true)?;
                break self.existing(&*accessor, &resolved)?;
            }
            follows += 1;
            if follows >= 1024 {
                return Err(self.error(
                    ErrorKind::FileSystem,
                    msg!(
                        "too many symbolic links encountered while traversing the path '",
                        accessor.show_path(&path),
                        "'"
                    ),
                ));
            }
            let parent = path.parent().unwrap_or_else(CanonPath::root);
            let resolved_parent = self.resolve(&*accessor, &parent, true)?;
            let name = path
                .base_name()
                .expect("a path other than the root has a last component");
            let candidate = resolved_parent
                .join_component(name)
                .map_err(|e| self.error(ErrorKind::FileSystem, e.message_bytes()))?;
            let st = self.existing(&*accessor, &candidate)?;
            if st.file_type != FileType::Symlink {
                break st;
            }
            let target = accessor
                .read_link(&candidate)
                .map_err(|e| self.fs_error(&*accessor, &candidate, e))?;
            path = CanonPath::new_relative(&target, &parent)
                .map_err(|e| self.error(ErrorKind::FileSystem, e.message_bytes()))?;
        };
        if st.file_type == FileType::Directory {
            path = path
                .join_component("default.nix")
                .map_err(|e| self.error(ErrorKind::FileSystem, e.message_bytes()))?;
        }
        Ok(SourcePath::new(p.accessor, path))
    }

    /// `EvalState::evalFile`: evaluates a file once and caches the result.
    pub fn eval_file(&self, p: &PathVal<'a>) -> Result<Value<'a>> {
        self.eval_file_checked(p, false)
    }

    /// `evalFile(path, v, mustBeTrivial = true)`, for `flake.nix`: unless
    /// the file was evaluated before, it must be an attribute set as
    /// written (`{ … }` or `rec { … }`), not a computation of one.
    pub fn eval_flake_file(&self, p: &PathVal<'a>) -> Result<Value<'a>> {
        self.eval_file_checked(p, true)
    }

    /// How the path literal whose value `p` is was written, if `p` is the
    /// value of a path literal of the file the position `pos` is in:
    /// [`PathAnchor::Root`] for an absolute one (`/…`),
    /// [`PathAnchor::Source`] for one relative to the file. Nix gives the
    /// two different accessors (`parser.y`: the root file system's, and
    /// the file's own), by which `parseFlakeInput` tells an input's path
    /// in the flake's own tree from one in another; brix gives both the
    /// root accessor, so the literal tells. A literal's value is made once,
    /// when its file is linked, so it is recognised by identity, through
    /// variables too, and a path computed from it is not one.
    pub fn path_literal_anchor(&self, pos: SrcPos, p: &PathVal<'a>) -> Option<PathAnchor> {
        let (code_id, _) = self.locate(pos)?;
        let pages = self.node_pages.get();
        let code = pages[..self.node_pages_len.get()]
            .iter()
            .map(crate::runtime::cell::Cell::get)
            .find(|code| code.id == code_id)?;
        code.nodes
            .iter()
            .zip(code.consts)
            .find_map(|(node, cell)| match (*node, cell) {
                (Node::Path { anchor, .. }, Some(cell)) => match cell.get() {
                    Value::Path(q) if std::ptr::eq(q, p) => Some(anchor),
                    _ => None,
                },
                _ => None,
            })
    }

    fn eval_file_checked(&self, p: &PathVal<'a>, must_be_attrs: bool) -> Result<Value<'a>> {
        // Trees are immutable for the evaluation, so a path value imported
        // before names the same file (Nix's `importResolutionCache`).
        let imported = self.imports.borrow().get(&(p.accessor, p.path)).copied();
        if let Some(cell) = imported {
            return self.force(cell, SrcPos::NONE);
        }
        let resolved = self.resolve_expr_path(p)?;
        let cached = self.file_cache.borrow().get(&resolved).copied();
        if let Some(cell) = cached {
            self.imports.borrow_mut().insert((p.accessor, p.path), cell);
            return self.force(cell, SrcPos::NONE);
        }
        // Register before evaluating so that a recursive import of the
        // same file sees the cell (and reports infinite recursion).
        let cell = self.alloc_cell(Value::Blackhole);
        self.file_cache.borrow_mut().insert(resolved.clone(), cell);
        self.imports.borrow_mut().insert((p.accessor, p.path), cell);
        match self.parse_and_eval_file_checked(&resolved, None, must_be_attrs) {
            Ok(v) => {
                cell.set(v);
                Ok(v)
            }
            Err(e) => {
                cell.set(Value::Failed(self.bump.alloc(e.clone())));
                Err(e)
            }
        }
    }

    /// Parses and evaluates a file in the base scope (plus `scope` for
    /// `scopedImport`), uncached. Nix's `scopedImport` evaluates the parsed
    /// expression itself, so unlike `import` (`evalFile`) no "while
    /// evaluating the file" trace is added.
    pub fn parse_and_eval_file(
        &self,
        path: &SourcePath,
        scope: Option<(&StaticEnv<'_>, crate::value::Env<'a>)>,
    ) -> Result<Value<'a>> {
        let display = self.accessor(path.accessor).show_path(&path.path);
        self.eval_file_contents(path, &display, scope, false)
    }

    fn parse_and_eval_file_checked(
        &self,
        path: &SourcePath,
        scope: Option<(&StaticEnv<'_>, crate::value::Env<'a>)>,
        must_be_attrs: bool,
    ) -> Result<Value<'a>> {
        let display = self.accessor(path.accessor).show_path(&path.path);
        self.eval_file_contents(path, &display, scope, must_be_attrs)
            .inspect_err(|e| {
                if e.kind != ErrorKind::Parse {
                    e.add_trace(None, msg!("while evaluating the file '", display, "':"));
                }
            })
    }

    /// Reads, parses, loads and evaluates the file at `path` (shown as
    /// `display`) in the base scope plus `scope`. A file inside a
    /// compiled-input image is linked instead of parsed, except with a
    /// `scope`: `scopedImport` compiles against its extra scope. With
    /// `must_be_attrs` (`mustBeTrivial`), a file that is not an attribute
    /// set as written is an error.
    fn eval_file_contents(
        &self,
        path: &SourcePath,
        display: &[u8],
        scope: Option<(&StaticEnv<'_>, crate::value::Env<'a>)>,
        must_be_attrs: bool,
    ) -> Result<Value<'a>> {
        let code =
            self.load_file_code(path, scope.as_ref().map(|(scope, _)| *scope), must_be_attrs)?;
        let env = scope.map_or(self.base_env, |(_, env)| env);
        let _ = display;
        self.eval_node(code, code.root, env)
    }

    /// Load immutable code without evaluating or constructing runtime values.
    pub(crate) fn load_file_code(
        &self,
        path: &SourcePath,
        scope: Option<&StaticEnv<'_>>,
        must_be_attrs: bool,
    ) -> Result<&'a crate::code::LinkedCode<'a>> {
        let display = self.accessor(path.accessor).show_path(&path.path);
        if scope.is_none() {
            if let Some(code) = self.image_code_for(path)? {
                self.check_attrs_file(code, &display, must_be_attrs)?;
                return Ok(code);
            }
        }
        let accessor = self.accessor(path.accessor);
        let src = accessor
            .read_file(&path.path)
            .map_err(|e| self.fs_error(&*accessor, &path.path, e))?;
        let base_dir = path.path.parent().unwrap_or_else(CanonPath::root);
        let origin = Origin::File {
            display: display.clone(),
            path: path.path.abs().to_owned(),
        };
        let expr = self.parse_source(&src, base_dir.as_bytes(), &origin)?;
        let code = self.load(&expr, origin, src, scope, false)?;
        self.check_attrs_file(code, &display, must_be_attrs)?;
        Ok(code)
    }

    /// `ExprParseFile`'s check: with `must_be_attrs`, the file's expression
    /// must be an attribute set (`ExprAttrs`).
    fn check_attrs_file(
        &self,
        code: &crate::code::LinkedCode<'a>,
        display: &[u8],
        must_be_attrs: bool,
    ) -> Result<()> {
        if must_be_attrs && !matches!(code.node(code.root), crate::ir::Node::Attrs(_)) {
            return Err(self.error(
                ErrorKind::Eval,
                msg!("file '", display, "' must be an attribute set"),
            ));
        }
        Ok(())
    }
}

/// A parse error in the file `info` describes, positioned in it. For
/// "already defined" errors the message gets the position of the earlier
/// definition, shown with the origin the file is loaded from (a compiled
/// image stores the parser's error before it knows that origin).
pub(crate) fn parse_error(
    info: crate::runtime::rc::Rc<SourceInfo>,
    msg: &[u8],
    pos: Option<u32>,
    defined_at: Option<u32>,
) -> Error {
    let position = |offset: u32| {
        let (line, column) = info.line_col(offset.min(info.len()));
        Position {
            info: info.clone(),
            line,
            column,
        }
    };
    let msg = match defined_at {
        Some(at) => msg!(msg, " at ", position(at).display()),
        None => msg.to_vec(),
    };
    EvalError::new(ErrorKind::Parse, msg).at(pos.map(position))
}
