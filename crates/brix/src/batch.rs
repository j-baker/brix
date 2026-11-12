//! Worker-local cursors over the shared evaluator graph. Only owned handles
//! cross threads; borrowed views remain confined to a synchronous batch job.

use crate::host::NativeHost;
use crate::{attr_path_to_string, Config, Cursor, Error, Result, Session};
use brix_eval::managed_api::{GcCallContext, GcValue, GcValueView};
use brix_eval::value::SrcPos;
use brix_store::StorePath;
use std::cell::RefCell;

/// Access to worker-local cursors during discovery or evaluation.
pub struct BatchContext<'e, 'call, 'a> {
    ev: RefCell<&'e mut GcCallContext<'call, 'a, 'static>>,
    host: &'e NativeHost,
    config: &'e Config,
}

struct Context<'e, 'call, 'a> {
    ev: &'e mut GcCallContext<'call, 'a, 'static>,
    host: &'e NativeHost,
}

/// An owning cursor handle that can be queued between batch callbacks. Access
/// requires a BatchContext; the evaluator checks the handle's session identity.
#[derive(Clone)]
pub struct BatchValue {
    slot: GcValue,
    attr_path: Vec<String>,
    pos: SrcPos,
}

impl Cursor<'_> {
    pub fn batch_value(&self) -> BatchValue {
        BatchValue {
            slot: self.slot.clone(),
            attr_path: self.attr_path.clone(),
            pos: self.pos,
        }
    }
}

impl<'e, 'call, 'a> BatchContext<'e, 'call, 'a> {
    pub fn cursor(&self, value: &BatchValue) -> BatchCursor<'_, 'e, 'call, 'a> {
        BatchCursor::new(self, value.slot.clone(), value.attr_path.clone()).with_pos(value.pos)
    }
}

/// A cursor confined to one batch callback. Clone it to retain a value within
/// that callback; it cannot escape to another thread or outlive the batch.
#[derive(Clone)]
pub struct BatchCursor<'s, 'e, 'call, 'a> {
    session: &'s BatchContext<'e, 'call, 'a>,
    slot: GcValue,
    attr_path: Vec<String>,
    pos: SrcPos,
}

impl<'s, 'e, 'call, 'a> BatchCursor<'s, 'e, 'call, 'a> {
    pub fn batch_value(&self) -> BatchValue {
        BatchValue {
            slot: self.slot.clone(),
            attr_path: self.attr_path.clone(),
            pos: self.pos,
        }
    }

    pub fn renamed(mut self, attr_path: Vec<String>) -> Self {
        self.attr_path = attr_path;
        self
    }

    fn new(
        session: &'s BatchContext<'e, 'call, 'a>,
        slot: GcValue,
        attr_path: Vec<String>,
    ) -> Self {
        Self {
            session,
            slot,
            attr_path,
            pos: SrcPos::NONE,
        }
    }
    fn with_pos(mut self, pos: SrcPos) -> Self {
        self.pos = pos;
        self
    }
    fn with<R>(
        &self,
        f: impl FnOnce(&mut Context<'_, 'call, 'a>, GcValue) -> Result<R>,
    ) -> Result<R> {
        f(
            &mut Context {
                ev: &mut self.session.ev.borrow_mut(),
                host: self.session.host,
            },
            self.slot.clone(),
        )
    }
    fn eval_err(&self, ctx: &Context<'_, '_, '_>, error: brix_eval::Error) -> Error {
        ctx.host
            .eval_error(&error, self.session.config.eval.show_trace)
    }
    pub fn get_string(&self) -> Result<String> {
        self.with(|ctx, cell| {
            ctx.ev.force(&cell).map_err(|e| self.eval_err(ctx, e))?;
            if !matches!(
                ctx.ev.view(&cell).map_err(|e| self.eval_err(ctx, e))?,
                GcValueView::String(_)
            ) {
                let kind = ctx.ev.show_type(&cell).map_err(|e| self.eval_err(ctx, e))?;
                return Err(Error::Usage(
                    format!("'{}' is not a string but {}", self.attr_path_str(), kind).into(),
                ));
            }
            let (bytes, _) = ctx
                .ev
                .string_parts(&cell)
                .map_err(|e| self.eval_err(ctx, e))?;
            String::from_utf8(bytes).map_err(|e| self.not_utf8("a string", e.as_bytes()))
        })
    }
    pub fn force_derivation(&self) -> Result<StorePath> {
        self.session
            .config
            .store_dir()
            .parse(&self.get_attr("drvPath")?.get_string()?)
            .map_err(Error::from)
    }
    pub fn attr_path(&self) -> &[String] {
        &self.attr_path
    }

    pub fn attr_path_str(&self) -> String {
        attr_path_to_string(&self.attr_path)
    }

    fn attr_path_str_with(&self, name: &str) -> String {
        let mut p = self.attr_path.clone();
        p.push(name.to_owned());
        attr_path_to_string(&p)
    }

    pub fn maybe_get_attr(&self, name: &str) -> Result<Option<Self>> {
        let slot = self.with(|ctx, cell| {
            ctx.ev.force(&cell).map_err(|e| self.eval_err(ctx, e))?;
            let GcValueView::Attrs(attrs) =
                ctx.ev.view(&cell).map_err(|e| self.eval_err(ctx, e))?
            else {
                return Ok(None);
            };
            Ok(attrs.get_with_pos(name.as_bytes()))
        })?;
        Ok(slot.map(|(slot, pos)| {
            let mut path = self.attr_path.clone();
            path.push(name.to_owned());
            Self::new(self.session, slot, path).with_pos(pos)
        }))
    }

    pub fn get_attr(&self, name: &str) -> Result<Self> {
        match self.maybe_get_attr(name)? {
            Some(c) => Ok(c),
            None => Err(Error::Usage(
                format!(
                    "attribute '{}' does not exist",
                    self.attr_path_str_with(name)
                )
                .into(),
            )),
        }
    }

    pub fn get_attrs(&self) -> Result<Vec<String>> {
        let mut names = self.get_attrs_in_set_order()?;
        names.sort();
        Ok(names)
    }

    pub fn get_attrs_in_set_order(&self) -> Result<Vec<String>> {
        self.with(|ctx, cell| {
            ctx.ev
                .force_attrs(&cell, SrcPos::NONE, "")
                .map_err(|e| self.eval_err(ctx, e))?;
            let GcValueView::Attrs(attrs) =
                ctx.ev.view(&cell).map_err(|e| self.eval_err(ctx, e))?
            else {
                unreachable!("forced attrs")
            };
            attrs
                .names()
                .map(|name| {
                    std::str::from_utf8(name)
                        .map(str::to_owned)
                        .map_err(|_| self.not_utf8("an attribute name", name))
                })
                .collect()
        })
    }

    fn not_utf8(&self, what: &str, bytes: &[u8]) -> Error {
        use bstr::ByteSlice;
        Error::Usage(
            format!(
                "{what} at '{}' is not valid UTF-8 ('{}'), which brix does not support here",
                self.attr_path_str(),
                bytes.to_str_lossy()
            )
            .into(),
        )
    }

    pub fn get_bool(&self) -> Result<bool> {
        self.with(|ctx, cell| {
            ctx.ev.force(&cell).map_err(|e| self.eval_err(ctx, e))?;
            match ctx.ev.view(&cell).map_err(|e| self.eval_err(ctx, e))? {
                GcValueView::Bool(value) => Ok(value),
                _ => Err(Error::Usage(
                    format!("'{}' is not a Boolean", self.attr_path_str()).into(),
                )),
            }
        })
    }

    pub fn get_list_of_strings(&self) -> Result<Vec<String>> {
        self.with(|ctx, cell| {
            ctx.ev.force(&cell).map_err(|e| self.eval_err(ctx, e))?;
            let GcValueView::List(list) = ctx.ev.view(&cell).map_err(|e| self.eval_err(ctx, e))?
            else {
                return Err(Error::Usage(
                    format!("'{}' is not a list", self.attr_path_str()).into(),
                ));
            };
            let len = list.len();
            let mut result = Vec::with_capacity(len);
            for index in 0..len {
                let elem = ctx
                    .ev
                    .list_item(&cell, index)
                    .map_err(|e| self.eval_err(ctx, e))?
                    .expect("list length stays fixed");
                ctx.ev.force(&elem).map_err(|e| self.eval_err(ctx, e))?;
                if !matches!(
                    ctx.ev.view(&elem).map_err(|e| self.eval_err(ctx, e))?,
                    GcValueView::String(_)
                ) {
                    return Err(Error::Usage(
                        format!("'{}' is not a list of strings", self.attr_path_str()).into(),
                    ));
                }
                let bytes = ctx
                    .ev
                    .string_bytes(&elem)
                    .map_err(|e| self.eval_err(ctx, e))?;
                result.push(
                    String::from_utf8(bytes)
                        .map_err(|error| self.not_utf8("a string", error.as_bytes()))?,
                );
            }
            Ok(result)
        })
    }

    pub fn find_along_attr_path(&self, path: &[String]) -> Result<Option<Self>> {
        let mut current = self.clone();
        for component in path {
            let next = current.with(|ctx, cell| {
                ctx.ev.force(&cell).map_err(|e| self.eval_err(ctx, e))?;
                Ok(
                    match ctx.ev.view(&cell).map_err(|e| self.eval_err(ctx, e))? {
                        GcValueView::Attrs(attrs) => attrs.get(component.as_bytes()),
                        GcValueView::List(list) => component
                            .parse::<usize>()
                            .ok()
                            .and_then(|index| list.get(index)),
                        _ => None,
                    },
                )
            })?;
            match next {
                Some(slot) => {
                    let mut path = current.attr_path.clone();
                    path.push(component.clone());
                    current = Self::new(self.session, slot, path);
                }
                None => return Ok(None),
            }
        }
        Ok(Some(current))
    }
}

impl Session {
    /// Continuously discover and evaluate jobs with bounded queueing. Reports
    /// run on completing workers, serialized, and may stop further admission by
    /// returning false. Callbacks must use BatchContext rather than reentering
    /// this Session through ordinary cursors.
    pub fn evaluate_stream<T: Send, R: Send>(
        &self,
        mut next: impl FnMut(&BatchContext<'_, '_, '_>) -> Result<Option<T>>,
        job: impl Fn(&BatchContext<'_, '_, '_>, T) -> Result<R> + Sync,
        mut report: impl FnMut(Result<R>) -> bool + Send,
    ) -> Result<()> {
        let config = &self.config;
        self.with(|ctx| {
            let host = ctx.host;
            let mut discovery_error = None;
            let result = ctx.ev.evaluate_stream(
                |ev| {
                    let context = BatchContext {
                        ev: RefCell::new(ev),
                        host,
                        config,
                    };
                    match next(&context) {
                        Ok(value) => Ok(value),
                        Err(error) => {
                            discovery_error = Some(error);
                            Ok(None)
                        }
                    }
                },
                |ev, input| {
                    let context = BatchContext {
                        ev: RefCell::new(ev),
                        host,
                        config,
                    };
                    Ok(job(&context, input))
                },
                |result| {
                    report(
                        result
                            .map_err(|e| host.eval_error(&e, config.eval.show_trace))
                            .and_then(|r| r),
                    )
                },
            );
            result.map_err(|e| host.eval_error(&e, config.eval.show_trace))?;
            match discovery_error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })
    }

    /// Evaluate a bounded batch of independent cursor jobs. Results retain
    /// input order. The auxiliary cursor commonly holds a flake's outputs.
    pub fn evaluate_many<R: Send>(
        &self,
        values: &[Cursor<'_>],
        auxiliary: &Cursor<'_>,
        job: impl for<'s, 'e, 'call, 'a> Fn(
                BatchCursor<'s, 'e, 'call, 'a>,
                BatchCursor<'s, 'e, 'call, 'a>,
            ) -> Result<R>
            + Sync,
    ) -> Result<Vec<Result<R>>> {
        if !std::ptr::eq(self, auxiliary.session)
            || values.iter().any(|c| !std::ptr::eq(self, c.session))
        {
            return Err(Error::Usage(
                "a cursor belongs to a different session".into(),
            ));
        }
        let values: Vec<_> = values
            .iter()
            .map(|c| (c.slot.clone(), c.attr_path.clone(), c.pos))
            .collect();
        let auxiliary = (
            auxiliary.slot.clone(),
            auxiliary.attr_path.clone(),
            auxiliary.pos,
        );
        let config = &self.config;
        Ok(self.with(|ctx| {
            let host = ctx.host;
            ctx.ev
                .evaluate_many(values.len(), |ev, index| {
                    let worker = BatchContext {
                        ev: RefCell::new(ev),
                        host,
                        config,
                    };
                    let (slot, path, pos) = &values[index];
                    let value =
                        BatchCursor::new(&worker, slot.clone(), path.clone()).with_pos(*pos);
                    let aux = BatchCursor::new(&worker, auxiliary.0.clone(), auxiliary.1.clone())
                        .with_pos(auxiliary.2);
                    Ok(job(value, aux))
                })
                .into_iter()
                .map(|r| {
                    r.map_err(|e| host.eval_error(&e, config.eval.show_trace))
                        .and_then(|r| r)
                })
                .collect()
        }))
    }
}

crate::cursor::impl_read_cursor!(BatchCursor<'_, '_, '_, '_>);
