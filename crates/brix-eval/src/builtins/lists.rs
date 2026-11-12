//! List built-ins.

use crate::builtins::core::compare_values;
use crate::builtins::reg;
use crate::error::ErrorKind;
use crate::evaluator::Evaluator;
use crate::symbol::{sym, Symbol};
use crate::value::{Attr, SrcPos, Value, ValueRef};
use crate::Result;

pub fn register(ev: &Evaluator<'_>) {
    reg(ev, "__isList", 1, prim_is_list);
    reg(ev, "__elemAt", 2, prim_elem_at);
    reg(ev, "__head", 1, prim_head);
    reg(ev, "__tail", 1, prim_tail);
    reg(ev, "map", 2, prim_map);
    reg(ev, "__filter", 2, prim_filter);
    reg(ev, "__elem", 2, prim_elem);
    reg(ev, "__concatLists", 1, prim_concat_lists);
    reg(ev, "__length", 1, prim_length);
    reg(ev, "__foldl'", 3, prim_foldl_strict);
    reg(ev, "__any", 2, prim_any);
    reg(ev, "__all", 2, prim_all);
    reg(ev, "__genList", 2, prim_gen_list);
    reg(ev, "__sort", 2, prim_sort);
    reg(ev, "__partition", 2, prim_partition);
    reg(ev, "__groupBy", 2, prim_group_by);
    reg(ev, "__concatMap", 2, prim_concat_map);
}

fn prim_is_list<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let v = ev.force(args[0], pos)?;
    Ok(Value::Bool(matches!(v, Value::List(_))))
}

fn prim_elem_at<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let n = ev.force_int(
        args[1],
        pos,
        "while evaluating the second argument passed to 'builtins.elemAt'",
    )?;
    let list = ev.force_list_elems(
        args[0],
        pos,
        "while evaluating the first argument passed to 'builtins.elemAt'",
    )?;
    if n < 0 || n as usize >= list.len() {
        return Err(ev.error_at(
            ErrorKind::Eval,
            format!(
                "'builtins.elemAt' called with index {n} on a list of size {}",
                list.len()
            ),
            pos,
        ));
    }
    ev.force(list[n as usize], pos)
}

fn prim_head<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let list = ev.force_list_elems(
        args[0],
        pos,
        "while evaluating the first argument passed to 'builtins.head'",
    )?;
    if list.is_empty() {
        return Err(ev.error_at(
            ErrorKind::Eval,
            "'builtins.head' called on an empty list",
            pos,
        ));
    }
    ev.force(list[0], pos)
}

fn prim_tail<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let list = ev.force_list_elems(
        args[0],
        pos,
        "while evaluating the first argument passed to 'builtins.tail'",
    )?;
    if list.is_empty() {
        return Err(ev.error_at(
            ErrorKind::Eval,
            "'builtins.tail' called on an empty list",
            pos,
        ));
    }
    Ok(ev.mk_list(&list[1..]))
}

fn prim_map<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let list = ev.force_list_elems(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.map",
    )?;
    if list.is_empty() {
        return Ok(args[1].get());
    }
    ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.map",
    )?;
    let out: Vec<ValueRef<'a>> = list.iter().map(|&e| ev.mk_app(args[0], e)).collect();
    Ok(ev.mk_list(&out))
}

fn prim_filter<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let list = ev.force_list_elems(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.filter",
    )?;
    if list.is_empty() {
        return Ok(args[1].get());
    }
    let f = ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.filter",
    )?;
    let mut out: Vec<ValueRef<'a>> = Vec::with_capacity(list.len());
    let mut same = true;
    for &e in list {
        let r = ev.call(f, &[e], SrcPos::NONE)?;
        let rc = ev.alloc_cell(r);
        if ev.force_bool(
            rc,
            pos,
            "while evaluating the return value of the filtering function passed to builtins.filter",
        )? {
            out.push(e);
        } else {
            same = false;
        }
    }
    if same {
        Ok(args[1].get())
    } else {
        Ok(ev.mk_list(&out))
    }
}

fn prim_elem<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let list = ev.force_list_elems(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.elem",
    )?;
    for &e in list {
        if ev.eq_values(
            args[0],
            e,
            pos,
            "while searching for the presence of the given element in the list",
        )? {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

fn prim_concat_lists<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let lists = ev.force_list_elems(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.concatLists",
    )?;
    ev.concat_lists(
        lists,
        pos,
        "while evaluating a value of the list passed to builtins.concatLists",
    )
}

fn prim_length<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let list = ev.force_list_elems(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.length",
    )?;
    Ok(Value::Int(list.len() as i64))
}

fn prim_foldl_strict<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let f = ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.foldlStrict",
    )?;
    let list = ev.force_list_elems(
        args[2],
        pos,
        "while evaluating the third argument passed to builtins.foldlStrict",
    )?;
    if list.is_empty() {
        return ev.force(args[1], pos);
    }
    let mut cur = args[1];
    for &elem in list {
        let r = ev.call(f, &[cur, elem], pos)?;
        cur = ev.alloc_cell(r);
    }
    ev.force(cur, pos)
}

fn any_or_all<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
    any: bool,
) -> Result<Value<'a>> {
    let name = if any { "any" } else { "all" };
    let f = ev.force_function(
        args[0],
        pos,
        &format!("while evaluating the first argument passed to builtins.{name}"),
    )?;
    let list = ev.force_list_elems(
        args[1],
        pos,
        &format!("while evaluating the second argument passed to builtins.{name}"),
    )?;
    let ctx =
        format!("while evaluating the return value of the function passed to builtins.{name}");
    for &e in list {
        let r = ev.call(f, &[e], pos)?;
        let rc = ev.alloc_cell(r);
        if ev.force_bool(rc, pos, &ctx)? == any {
            return Ok(Value::Bool(any));
        }
    }
    Ok(Value::Bool(!any))
}

fn prim_any<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    any_or_all(ev, pos, args, true)
}

fn prim_all<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    any_or_all(ev, pos, args, false)
}

fn prim_gen_list<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let len = ev.force_int(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.genList",
    )?;
    if len < 0 {
        return Err(ev.error_at(
            ErrorKind::Eval,
            format!("cannot create list of size {len}"),
            pos,
        ));
    }
    ev.force_function(
        args[0],
        SrcPos::NONE,
        "while evaluating the first argument passed to builtins.genList",
    )?;
    // Nix allocates the list up front and dies with `std::bad_alloc`
    // ("out of memory") when it cannot; `with_capacity` would abort the
    // process instead.
    let mut out: Vec<ValueRef<'a>> = Vec::new();
    if usize::try_from(len)
        .ok()
        .and_then(|n| out.try_reserve_exact(n).ok())
        .is_none()
    {
        return Err(ev.error(ErrorKind::OutOfMemory, "out of memory"));
    }
    for n in 0..len {
        let arg = ev.alloc_cell(Value::Int(n));
        out.push(ev.mk_app(args[0], arg));
    }
    Ok(ev.mk_list(&out))
}

/// `peeksort` from `sort.hh`: Nix's own merge sort, reproduced so that
/// the sequence of comparator calls (and therefore any traces or errors
/// a comparator produces) is the same.
pub fn peeksort<T: Copy, E>(
    items: &mut [T],
    comp: &mut dyn FnMut(T, T) -> std::result::Result<bool, E>,
) -> std::result::Result<(), E> {
    let len = items.len();
    match len {
        0 | 1 => return Ok(()),
        2 => {
            if comp(items[1], items[0])? {
                items.swap(0, 1);
            }
            return Ok(());
        }
        _ => {}
    }
    let mut buffer: Vec<T> = items.to_vec();
    peeksort_impl(items, &mut buffer, 0, len, 0, len, comp)
}

#[allow(clippy::too_many_arguments)]
fn peeksort_impl<T: Copy, E>(
    items: &mut [T],
    buffer: &mut [T],
    begin: usize,
    end: usize,
    left_run_end: usize,
    right_run_begin: usize,
    comp: &mut dyn FnMut(T, T) -> std::result::Result<bool, E>,
) -> std::result::Result<(), E> {
    if left_run_end == end || right_run_begin == begin {
        return Ok(());
    }
    const INSERTION_THRESHOLD: usize = 16;
    let length = end - begin;
    if length <= INSERTION_THRESHOLD {
        return insertion_sort(items, begin, end, comp);
    }
    let middle = begin + length / 2;
    if middle <= left_run_end {
        peeksort_impl(
            items,
            buffer,
            left_run_end,
            end,
            left_run_end + 1,
            right_run_begin,
            comp,
        )?;
        merge(items, buffer, begin, left_run_end, end, comp)?;
        return Ok(());
    } else if middle >= right_run_begin {
        peeksort_impl(
            items,
            buffer,
            begin,
            right_run_begin,
            left_run_end,
            right_run_begin - 1,
            comp,
        )?;
        merge(items, buffer, begin, right_run_begin, end, comp)?;
        return Ok(());
    }
    let (i, j);
    if !comp(items[middle], items[middle - 1])? {
        i = weakly_increasing_suffix(items, left_run_end, middle, comp)?;
        j = weakly_increasing_prefix(items, middle - 1, right_run_begin, comp)?;
    } else {
        i = strictly_decreasing_suffix(items, left_run_end, middle, comp)?;
        j = strictly_decreasing_prefix(items, middle - 1, right_run_begin, comp)?;
        items[i..j].reverse();
    }
    if i == begin && j == end {
        return Ok(());
    }
    if middle - i < j - middle {
        peeksort_impl(items, buffer, begin, i, left_run_end, i - 1, comp)?;
        peeksort_impl(items, buffer, i, end, j, right_run_begin, comp)?;
        merge(items, buffer, begin, i, end, comp)?;
    } else {
        peeksort_impl(items, buffer, begin, j, left_run_end, i, comp)?;
        peeksort_impl(items, buffer, j, end, j + 1, right_run_begin, comp)?;
        merge(items, buffer, begin, j, end, comp)?;
    }
    Ok(())
}

fn insertion_sort<T: Copy, E>(
    items: &mut [T],
    begin: usize,
    end: usize,
    comp: &mut dyn FnMut(T, T) -> std::result::Result<bool, E>,
) -> std::result::Result<(), E> {
    if begin == end {
        return Ok(());
    }
    for current in begin + 1..end {
        let mut p = current;
        while p != begin && comp(items[p], items[p - 1])? {
            items.swap(p, p - 1);
            p -= 1;
        }
    }
    Ok(())
}

/// `strictlyDecreasingPrefix`: one past the end of the strictly
/// decreasing run starting at `begin`.
fn strictly_decreasing_prefix<T: Copy, E>(
    items: &[T],
    mut begin: usize,
    end: usize,
    comp: &mut dyn FnMut(T, T) -> std::result::Result<bool, E>,
) -> std::result::Result<usize, E> {
    if begin == end {
        return Ok(begin);
    }
    while begin + 1 != end && comp(items[begin + 1], items[begin])? {
        begin += 1;
    }
    Ok(begin + 1)
}

fn strictly_decreasing_suffix<T: Copy, E>(
    items: &[T],
    begin: usize,
    mut end: usize,
    comp: &mut dyn FnMut(T, T) -> std::result::Result<bool, E>,
) -> std::result::Result<usize, E> {
    if begin == end {
        return Ok(end);
    }
    while end - 1 > begin && comp(items[end - 1], items[end - 2])? {
        end -= 1;
    }
    Ok(end - 1)
}

fn weakly_increasing_prefix<T: Copy, E>(
    items: &[T],
    mut begin: usize,
    end: usize,
    comp: &mut dyn FnMut(T, T) -> std::result::Result<bool, E>,
) -> std::result::Result<usize, E> {
    if begin == end {
        return Ok(begin);
    }
    while begin + 1 != end && !comp(items[begin + 1], items[begin])? {
        begin += 1;
    }
    Ok(begin + 1)
}

fn weakly_increasing_suffix<T: Copy, E>(
    items: &[T],
    begin: usize,
    mut end: usize,
    comp: &mut dyn FnMut(T, T) -> std::result::Result<bool, E>,
) -> std::result::Result<usize, E> {
    if begin == end {
        return Ok(end);
    }
    while end - 1 > begin && !comp(items[end - 1], items[end - 2])? {
        end -= 1;
    }
    Ok(end - 1)
}

/// `mergeSortedRunsInPlace`: stable merge of `[begin, middle)` and
/// `[middle, end)` through the buffer.
fn merge<T: Copy, E>(
    items: &mut [T],
    buffer: &mut [T],
    begin: usize,
    middle: usize,
    end: usize,
    comp: &mut dyn FnMut(T, T) -> std::result::Result<bool, E>,
) -> std::result::Result<(), E> {
    buffer[begin..end].copy_from_slice(&items[begin..end]);
    let (mut l, mut r, mut out) = (begin, middle, begin);
    while l < middle && r < end {
        if !comp(buffer[r], buffer[l])? {
            items[out] = buffer[l];
            l += 1;
        } else {
            items[out] = buffer[r];
            r += 1;
        }
        out += 1;
    }
    while l < middle {
        items[out] = buffer[l];
        l += 1;
        out += 1;
    }
    while r < end {
        items[out] = buffer[r];
        r += 1;
        out += 1;
    }
    Ok(())
}

fn prim_sort<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let list = ev.force_list_elems(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.sort",
    )?;
    if list.is_empty() {
        return Ok(args[1].get());
    }
    let f = ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.sort",
    )?;
    let mut items: Vec<ValueRef<'a>> = list.to_vec();
    for &e in &items {
        ev.force(e, pos)?;
    }
    let is_less_than = matches!(f, Value::PrimOp(p) if p.name == "lessThan");
    let mut comp = |a: ValueRef<'a>, b: ValueRef<'a>| -> Result<bool> {
        if is_less_than {
            return compare_values(
                ev,
                a,
                b,
                SrcPos::NONE,
                "while evaluating the ordering function passed to builtins.sort",
            );
        }
        let r = ev.call(f, &[a, b], SrcPos::NONE)?;
        let rc = ev.alloc_cell(r);
        ev.force_bool(
            rc,
            pos,
            "while evaluating the return value of the sorting function passed to builtins.sort",
        )
    };
    peeksort(&mut items, &mut comp)?;
    Ok(ev.mk_list(&items))
}

fn prim_partition<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let f = ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.partition",
    )?;
    let list = ev.force_list_elems(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.partition",
    )?;
    let mut right: Vec<ValueRef<'a>> = Vec::new();
    let mut wrong: Vec<ValueRef<'a>> = Vec::new();
    for &e in list {
        ev.force(e, pos)?;
        let r = ev.call(f, &[e], pos)?;
        let rc = ev.alloc_cell(r);
        if ev.force_bool(rc, pos, "while evaluating the return value of the partition function passed to builtins.partition")? {
            right.push(e);
        } else {
            wrong.push(e);
        }
    }
    let attrs = vec![
        ev.attr(sym::RIGHT, ev.mk_list(&right)),
        ev.attr(sym::WRONG, ev.mk_list(&wrong)),
    ];
    Ok(ev.mk_attrs(attrs, SrcPos::NONE))
}

fn prim_group_by<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let f = ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.groupBy",
    )?;
    let list = ev.force_list_elems(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.groupBy",
    )?;
    let mut items: Vec<(Symbol, ValueRef<'a>)> = Vec::with_capacity(list.len());
    for &e in list {
        let r = ev.call(f, &[e], pos)?;
        let rc = ev.alloc_cell(r);
        let name = ev.force_string_no_ctx(
            rc,
            pos,
            "while evaluating the return value of the grouping function passed to builtins.groupBy",
        )?;
        items.push((ev.intern(name), e));
    }
    // Nix's `sortByName`: stable, so each group keeps list order, and by
    // symbol, the set's order.
    items.sort_by_key(|&(name, _)| name);
    let mut attrs: Vec<Attr<'a>> = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let name = items[i].0;
        let mut j = i;
        while j < items.len() && items[j].0 == name {
            j += 1;
        }
        let values: Vec<ValueRef<'a>> = items[i..j].iter().map(|(_, v)| *v).collect();
        attrs.push(ev.attr(name, ev.mk_list(&values)));
        i = j;
    }
    Ok(ev.mk_attrs_sorted(&attrs, SrcPos::NONE))
}

fn prim_concat_map<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let f = ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.concatMap",
    )?;
    let list = ev.force_list_elems(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.concatMap",
    )?;
    let mut out: Vec<ValueRef<'a>> = Vec::new();
    for &e in list {
        let r = ev.call(f, &[e], pos)?;
        let rc = ev.alloc_cell(r);
        let elems = ev.force_list_elems(
            rc,
            ev.determine_pos(r, ev.determine_pos(f, pos)),
            "while evaluating the return value of the function passed to builtins.concatMap",
        )?;
        out.extend_from_slice(elems);
    }
    Ok(ev.mk_list(&out))
}

#[cfg(test)]
mod tests {
    use super::peeksort;

    #[test]
    fn peeksort_sorts() {
        for n in 0..60usize {
            let mut v: Vec<u32> = (0..n as u32).map(|i| (i * 7919 + 13) % 31).collect();
            let mut expected = v.clone();
            expected.sort();
            peeksort(&mut v, &mut |a, b| Ok::<bool, ()>(a < b)).unwrap();
            assert_eq!(v, expected, "n={n}");
        }
    }

    #[test]
    fn peeksort_is_stable() {
        let mut v: Vec<(u32, u32)> = vec![(1, 0), (0, 1), (1, 2), (0, 3), (1, 4), (0, 5)];
        peeksort(&mut v, &mut |a, b| Ok::<bool, ()>(a.0 < b.0)).unwrap();
        assert_eq!(v, vec![(0, 1), (0, 3), (0, 5), (1, 0), (1, 2), (1, 4)]);
    }
}
