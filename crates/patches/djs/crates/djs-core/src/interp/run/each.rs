//! `%EachDense` (0137): the dense part of the loops of forEach, map, filter, some, every,
//! find, findIndex and reduce. The callback gets one frame (continuation `Each`) that is
//! entered again for every element, instead of a return into the builtin and a new call
//! per element.
//!
//! The loop's state is the intrinsic's own arguments, which stay on the stack right below
//! the callback's frame: `[mode, o, k, len, f, thisArg, a, kValue]` (kValue added here:
//! it is what filter and find keep, and the callback may assign its parameter). A throw
//! unwinds the frame as usual and leaves them behind on the caller's operand stack, which
//! its handler resets. Collections, fuel and heap images see an ordinary frame and
//! ordinary stack values.

use super::hot::{fill, ld, ld32, object_slot, st, window, write_values};
use super::*;
use crate::builtins::dense::index;
use crate::heap::cells::{Object, ValueArray};

const EACH_DENSE: u16 = super::intrinsics::id("EachDense");
const REGEXP_CREATE: u16 = super::intrinsics::id("RegExpCreate");
pub(super) const EACH_REST: u16 = super::intrinsics::id("EachRest");

/// The loop state below the callback's frame.
const STATE: u32 = 8;
const MODE: u32 = 0;
const O: u32 = 1;
const K: u32 = 2;
const LEN: u32 = 3;
const F: u32 = 4;
const THIS: u32 = 5;
const A: u32 = 6;
const KVALUE: u32 = 7;

// The modes (builtins/intrinsics.d.ts).
const FOR_EACH: i32 = 0;
const MAP: i32 = 1;
const FILTER: i32 = 2;
const SOME: i32 = 3;
const EVERY: i32 = 4;
const FIND: i32 = 5;
const FIND_INDEX: i32 = 6;
const REDUCE: i32 = 7;
/// Array.from's mapping loop over an array-like (D2306).
const FROM: i32 = 8;
/// A replacer function over the matches of a RegExp without groups (regexp.ts): `o`
/// holds the matched strings, `a` their positions and then the results, THIS the string.
const REPLACE: i32 = 9;

impl Exec<'_> {
    /// `%EachDense(mode, o, k, len, f, thisArg, a)` from `call_intrinsic`: the arguments
    /// are at `base`, the caller resumes at `next`.
    pub(super) fn each_dense(&mut self, base: u32, next: u32) -> Step {
        let k = self.get(base + K);
        let start = self.each_ok(base).then(|| k.as_number().and_then(index));
        if let Some(Some(k)) = start
            && let Some(x) = self.each_element(base, k)
        {
            self.ensure(base + STATE)?;
            self.sp = base + STATE;
            return self.each_enter(base, k, x, next, Continuation::Each);
        }
        self.sp = base;
        self.push(k);
        self.pc = next;
        Ok(None)
    }

    /// The tier-0 loop's exit at a `call_intrinsic` (D2307): the intrinsic runs here, as
    /// `step` would run it (its fuel, its `next`, a fault thrown at its pc), without the
    /// way through `step` (and `%EachDense` without `call_intrinsic`). `None` for one
    /// that `step` retries after a collection (D1700, nothing changed), else what
    /// `step_once` returns.
    #[inline(never)]
    pub(super) fn intrinsic_decline(&mut self) -> Option<Option<Status>> {
        if self.pc >= self.code_len {
            return None;
        }
        // `call_intrinsic id, argc` or its short form (D2443).
        let (op, id, argc) = if self.byte(self.pc) == super::fast::CALL_INTRINSIC8 {
            let id = self.byte(self.pc + 1) as u16;
            (
                Op::CallIntrinsic8,
                id,
                crate::bytecode::decode::arity(id as u32),
            )
        } else {
            (Op::CallIntrinsic, self.imm_u16(1) as u16, self.imm_u16(3))
        };
        if id != EACH_DENSE && super::retry::retries(id) {
            return None;
        }
        let info = &crate::bytecode::OPS[op as usize];
        if id == REGEXP_CREATE
            && let Some((v, after)) = self.literal_test(self.pc + info.size as u32, argc)
        {
            self.fuel -= info.cost.fuel() as i64;
            self.sp -= argc;
            self.push(v);
            self.pc = after;
            return Some(None);
        }
        self.fuel -= info.cost.fuel() as i64;
        let (pc0, next) = (self.pc, self.pc + info.size as u32);
        let r = if id == EACH_DENSE {
            self.each_dense(self.sp - argc, next)
        } else {
            self.call_intrinsic(id, argc, next)
        };
        if self.retry_len == 0 {
            self.retry_len = super::NO_RETRY;
        }
        Some(match r {
            Ok(st) => st,
            Err(f) => self.throw_fault(f, pc0),
        })
    }

    /// `/re/.test(x)` without the RegExp (`regexp::literal_test`): after a regex literal's
    /// `call_intrinsic %RegExpCreate` (its `argc` arguments on the stack), the code at `at` is
    /// `get_method; get_loc | get_arg; call1`. The result and the pc after `call1`, with
    /// those instructions charged; `None`, with nothing changed, for any other code or case.
    fn literal_test(&mut self, at: u32, argc: u32) -> Option<(Value, u32)> {
        let cell = self.get(self.sp - argc).as_internal()?;
        if self.byte(at) != Op::GetMethod as u8 {
            return None;
        }
        let key = Atom::from_id(self.heap.arena().read::<u32>(self.code + at + 1));
        let p = at + OPS[Op::GetMethod as usize].size as u32;
        let op = self.byte(p);
        let (x, size) = match op {
            _ if (Op::GetLoc0 as u8..=Op::GetLoc3 as u8).contains(&op) => {
                (self.lp + (op - Op::GetLoc0 as u8) as u32, 1)
            }
            _ if (Op::GetArg0 as u8..=Op::GetArg3 as u8).contains(&op) => {
                (self.bp + 2 + (op - Op::GetArg0 as u8) as u32, 1)
            }
            _ if op == Op::GetLoc8 as u8 => (self.lp + self.byte(p + 1) as u32, 2),
            _ if op == Op::GetArg8 as u8 => (self.bp + 2 + self.byte(p + 1) as u32, 2),
            _ => return None,
        };
        let call = p + size;
        if call >= self.code_len || self.byte(call) != Op::Call1 as u8 {
            return None;
        }
        let s = self.get(x).as_string()?;
        let found = crate::builtins::regexp::literal_test(self.heap, cell, key, s, &mut self.fuel)
            .ok()
            .flatten()?;
        for o in [Op::GetMethod, Op::GetLoc0, Op::Call1] {
            self.fuel -= OPS[o as usize].cost.fuel() as i64;
        }
        Some((Value::bool(found), call + 1))
    }

    /// Whether the loop can run at all: a known mode over an Array and a plain closure
    /// (as `n_call_closure` takes). `a`, where a mode stores, must be an Array, whose
    /// CreateDataProperty runs no user code: for filter a fresh one (a plain species),
    /// so that its length is the count of the elements kept; for reduce the box that gets
    /// the accumulator as element 0 (which starts in THIS).
    fn each_ok(&self, base: u32) -> bool {
        let Some(f) = self.get(base + F).as_object() else {
            return false;
        };
        if !self.is_class(f, ObjectClass::Function)
            || u32::from(
                self.heap
                    .arena()
                    .get::<u16>(self.template_of(f), FuncTemplate::FLAGS),
            ) & self.slow_call_flags
                != 0
        {
            return false;
        }
        let array = |i: u32| {
            self.get(base + i)
                .as_object()
                .filter(|&o| self.is_class(o, ObjectClass::Array))
        };
        let mode = self.get(base + MODE).as_int().unwrap_or(-1);
        if mode == FROM {
            return array(A).is_some() && self.get(base + O).as_object().is_some();
        }
        let (Some(o), a) = (array(O), array(A)) else {
            return false;
        };
        match mode {
            FOR_EACH | SOME | EVERY | FIND_INDEX => true,
            MAP | FIND | REPLACE => a.is_some(),
            FILTER => a.is_some() && crate::builtins::fast::plain_species(self.heap, o),
            REDUCE => a.is_some(),
            _ => false,
        }
    }

    /// Element `k` of the loop's array if it is below `len` and present in the dense store
    /// (for Array.from, Get of it if that observes nothing).
    fn each_element(&self, base: u32, k: u32) -> Option<Value> {
        let len = self.get(base + LEN).as_number()?;
        if f64::from(k) >= len {
            return None;
        }
        let o = self.get(base + O).as_object()?;
        // The array modes' element first: the per-element path.
        if let Some(v) = self.heap.get_element_fast(o, k) {
            return Some(v);
        }
        if self.get(base + MODE).as_int() == Some(FROM) {
            return self.plain_get_index(o, k);
        }
        None
    }

    /// `o[k]`, not in `o`'s dense store, when that Get runs no user code and reads no exotic element: every object on
    /// the chain up to the holder is ordinary, an Array or a function, and the holder's
    /// property is a data property (undefined when no object has one).
    #[inline(never)]
    fn plain_get_index(&self, o: Obj, k: u32) -> Option<Value> {
        use crate::heap::cells::flags::OBJ_INDEXED_PROPS;
        let key = Atom::int(k)?;
        let mut c = Some(o);
        while let Some(q) = c {
            if !matches!(
                self.heap.object_class(q),
                ObjectClass::Ordinary | ObjectClass::Array | ObjectClass::Function
            ) {
                return None;
            }
            if let Some(v) = self.heap.get_element_fast(q, k) {
                return Some(v);
            }
            // Without the flag the index is in no property table: the usual case, an
            // array-like `{ length }` and Object.prototype (no lookups per element).
            if self.heap.cell_flags(q.offset()) & OBJ_INDEXED_PROPS == 0 {
                c = self.heap.get_prototype_of(q);
                continue;
            }
            match self.heap.get_own_property(q, key) {
                Some(Property::Data { value, .. }) => return Some(value),
                Some(Property::Accessor { .. }) => return None,
                None => c = self.heap.get_prototype_of(q),
            }
        }
        Some(Value::UNDEFINED)
    }

    /// The callback's arguments for element `k` (`x`, already in KVALUE): `x, k, o`, for
    /// reduce `accumulator, x, k, o`. The count is the second value.
    fn each_args(&self, base: u32, kv: Value) -> ([Value; 4], u32) {
        let (x, o) = (self.get(base + KVALUE), self.get(base + O));
        let mode = self.get(base + MODE).as_int();
        if mode == Some(REDUCE) {
            // The accumulator is THIS (the callback's `this` is undefined).
            ([self.get(base + THIS), x, kv, o], 4)
        } else if mode == Some(FROM) {
            ([x, kv, Value::UNDEFINED, Value::UNDEFINED], 2)
        } else if mode == Some(REPLACE) {
            // `matched, position, string`: the position is in `a` until the result is.
            let at = self
                .get(base + A)
                .as_object()
                .and_then(|a| self.heap.get_element_fast(a, self.each_k(base)));
            (
                [
                    x,
                    at.unwrap_or(Value::UNDEFINED),
                    self.get(base + THIS),
                    Value::UNDEFINED,
                ],
                3,
            )
        } else {
            ([x, kv, o, Value::UNDEFINED], 3)
        }
    }

    /// Enters the callback's frame for element `k` (`x`), with the current frame (the
    /// builtin's, or for `EachNative` the builtin's caller) as its caller resuming at
    /// `ret_pc`. `sp` is `base + STATE`.
    fn each_enter(&mut self, base: u32, k: u32, x: Value, ret_pc: u32, cont: Continuation) -> Step {
        let kv = conv::number(f64::from(k));
        self.set(base + K, kv);
        self.set(base + KVALUE, x);
        self.fuel -= super::native::CALL_FUEL;
        let cb = base + STATE;
        let (args, argc) = self.each_args(base, kv);
        self.ensure(cb + 2 + argc)?;
        self.set(cb, self.get(base + F));
        let this = if matches!(self.get(base + MODE).as_int(), Some(REDUCE | REPLACE)) {
            Value::UNDEFINED
        } else {
            self.get(base + THIS)
        };
        self.set(cb + 1, this);
        for i in 0..argc {
            self.set(cb + 2 + i, args[i as usize]);
        }
        self.sp = cb + 2 + argc;
        self.call(cb, argc, cont, ret_pc)
    }

    /// With the callback's frame popped (the builtin's current at its return pc): enters
    /// it for element `next`, or gives the builtin `next`, where its own loop goes on
    /// (for `EachNative`, the call its result at the end, else `each_rest`).
    fn each_next(&mut self, base: u32, next: u32, cont: Continuation) -> Option<Status> {
        if let Some(x) = self.each_element(base, next) {
            return match self.each_enter(base, next, x, self.pc, cont) {
                Ok(st) => st,
                Err(f) => self.each_throw(f),
            };
        }
        if cont == Continuation::EachNative {
            if f64::from(next) >= self.get(base + LEN).as_number().unwrap_or(0.0) {
                return self.each_done(base, None);
            }
            return self.each_rest(base, next);
        }
        self.each_finish(base, f64::from(next));
        None
    }

    /// The builtin (current frame) gets `r`: the k its loop goes on from, or -1 - k when
    /// the callback's result at k decided the search.
    fn each_finish(&mut self, base: u32, r: f64) {
        self.sp = base;
        self.push(conv::number(r));
        // A return is a safepoint (as in `do_return`).
        self.safepoint();
    }

    /// The callback (the current frame, continuation `Each`) returns `v`, from the hot
    /// loop or `do_return`: completes the step, then reuses the frame for the next
    /// element (its arguments and locals written again, `pc` 0, as `call` would write a
    /// new one), or pops it and gives the builtin the k its loop goes on from.
    #[inline(never)]
    pub(super) fn each_return(&mut self, v: Value) -> Option<Status> {
        if let Some(top) = self.each_again(v) {
            self.fuel -= super::native::CALL_FUEL;
            self.sp = top;
            self.pc = 0;
            self.safepoint();
            return None;
        }
        self.each_leave(v)
    }

    /// `each_return` when `each_again` declined.
    #[inline(never)]
    fn each_leave(&mut self, v: Value) -> Option<Status> {
        let base = self.bp - STATE;
        let cont = Continuation::from_flags(self.header_int(frame::FLAGS) as i32);
        let mode = self.get(base + MODE).as_int().unwrap_or(FOR_EACH);
        let k = self.each_k(base);
        let stop = if mode == FOR_EACH {
            Ok(false)
        } else {
            self.each_complete(base, mode, v)
        };
        let next = match stop {
            Ok(false) => k + 1,
            r => {
                self.pop_frame();
                return match r {
                    Err(f) => self.each_throw(f),
                    _ if cont == Continuation::EachNative => self.each_done(base, Some(k)),
                    _ => {
                        self.each_finish(base, -1.0 - f64::from(k));
                        None
                    }
                };
            }
        };
        // Only the callback's own frame is reused: an async callback's return replaces
        // it by a builtin's (`tail_call_builtin`).
        let x = self.each_element(base, next);
        let (Some(x), true) = (x, self.get(self.bp) == self.get(base + F)) else {
            self.pop_frame();
            return self.each_next(base, next, cont);
        };
        let kv = i32::try_from(next).map_or_else(|_| conv::number(f64::from(next)), Value::int);
        self.set(base + K, kv);
        self.set(base + KVALUE, x);
        self.fuel -= super::native::CALL_FUEL;
        // The frame as `call` writes it: arguments padded with undefined, the header's
        // NEW_TARGET undefined, undefined locals (the callee, `this` and the rest of the
        // header are never written).
        let (bp, hp, lp) = (self.bp, self.hp, self.lp);
        let (args, argc) = if matches!(mode, REDUCE | FROM | REPLACE) {
            self.each_args(base, kv)
        } else {
            ([x, kv, self.get(base + O), Value::UNDEFINED], 3)
        };
        for j in 0..hp - bp - 2 {
            let v = if j < argc {
                args[j as usize]
            } else {
                Value::UNDEFINED
            };
            self.set(bp + 2 + j, v);
        }
        self.set(hp + frame::NEW_TARGET, Value::UNDEFINED);
        let top = lp + self.locals();
        for i in lp..top {
            self.set(i, Value::UNDEFINED);
        }
        self.sp = top;
        self.pc = 0;
        // Frame entry is a safepoint.
        self.safepoint();
        None
    }

    /// `hot_step` and `hot_enter` for `each_return`, with the store through the heap.
    #[inline(never)]
    pub(super) fn each_again(&mut self, v: Value) -> Option<u32> {
        let (bp, hp, lp) = (self.bp, self.hp, self.lp);
        let vals = ValueArray::VALUES.at(self.stack, 0);
        let n = hot_step(self.heap.arena().bytes(), vals, bp, v)?;
        if let Some((a, i, sv)) = n.store
            && !self.each_store(a, i, sv)
        {
            return None;
        }
        let locals = self.locals();
        Some(hot_enter(
            self.heap.arena_mut().hot_bytes(),
            vals,
            (bp, hp, lp, locals),
            &n,
        ))
    }

    /// The store of a step `hot_step` found (map at k, filter at the length of `a`,
    /// `u32::MAX`): false, with nothing changed, when it needs the general path.
    #[inline(never)]
    pub(super) fn each_store(&mut self, a: Obj, i: u32, v: Value) -> bool {
        let i = if i == u32::MAX {
            self.heap.array_length(a)
        } else {
            i
        };
        matches!(self.heap.create_element_fast(a, i, v), Ok(true))
    }

    /// A call at `base` (`F this a1 .. an`, `argc` arguments) of forEach, map, filter,
    /// some, every, find, findIndex or reduce (`mode`, from F's `@fast` entry) that the
    /// interpreter runs itself: over an Array, with a callback `each_ok` takes and the
    /// first element in the dense store, the loop state goes above F in place of the
    /// arguments (`[F, mode, o, k, len, f, thisArg, a, kValue]`), without the builtin's
    /// frames or a box, and the callback's frame (continuation `EachNative`) above it, as
    /// `each_dense` makes it. True when entered; false, with nothing changed, for anything
    /// the builtin does (the call then runs it).
    pub(super) fn each_call(&mut self, base: u32, argc: u32, mode: i32, next: u32) -> bool {
        let Some(o) = self
            .get(base + 1)
            .as_object()
            .filter(|&o| self.is_class(o, ObjectClass::Array))
        else {
            return false;
        };
        let f = if argc >= 1 {
            self.get(base + 2)
        } else {
            Value::UNDEFINED
        };
        let Some(fo) = f
            .as_object()
            .filter(|&fo| self.is_class(fo, ObjectClass::Function))
        else {
            return false;
        };
        let t = self.template_of(fo);
        let th = super::hot::template_head(self.heap.arena().bytes(), t);
        if th.flags & self.slow_call_flags != 0 {
            return false;
        }
        let arg1 = if argc >= 2 {
            self.get(base + 3)
        } else {
            Value::UNDEFINED
        };
        let len = self.heap.array_length(o);
        // Reduce without an initial value starts from element 0 (present: HasProperty
        // and Get of it observe nothing).
        let (k, this) = match (mode, argc) {
            (REDUCE, 0 | 1) => match self.heap.get_element_fast(o, 0) {
                Some(x) => (1, x),
                None => return false,
            },
            _ => (0, arg1),
        };
        if k >= len
            || ((mode == MAP || mode == FILTER)
                && !crate::builtins::fast::plain_species(self.heap, o))
        {
            return false;
        }
        let Some(x) = self.heap.get_element_fast(o, k) else {
            return false;
        };
        let state = base + 1;
        let top = state + STATE + 2 + th.params.max(4) + HEADER + th.locals + th.stack;
        if self.ensure(top).is_err() {
            return false;
        }
        let a = match mode {
            MAP | FILTER => {
                let proto = realm::intrinsic_obj(self.heap, Intrinsic::ArrayProto);
                let reserve = self.heap.element_tail(o).map_or(0, |t| t.1.min(len));
                let a = if mode == MAP {
                    self.heap.new_array_with_length_reserved(Some(proto), len, reserve)
                } else {
                    self.heap.new_array(Some(proto), reserve)
                };
                match a {
                    Ok(a) => Value::object(a),
                    Err(e) => {
                        if matches!(e, HeapError::HostOutOfMemory) {
                            self.host_out_of_memory();
                        }
                        return false;
                    }
                }
            }
            _ => Value::UNDEFINED,
        };
        // The state and the callback's call (`each_enter`, with the frame pushed as
        // `call` pushes a plain closure's) in one block.
        let kv = Value::int(k as i32);
        let (o, len) = (Value::object(o), conv::number(f64::from(len)));
        let mut values = [Value::UNDEFINED; STATE as usize + 6];
        values[..STATE as usize].copy_from_slice(&[Value::int(mode), o, kv, len, f, this, a, x]);
        let argc = if mode == REDUCE {
            values[STATE as usize..].copy_from_slice(&[f, Value::UNDEFINED, this, x, kv, o]);
            4
        } else {
            values[STATE as usize..STATE as usize + 5].copy_from_slice(&[f, this, x, kv, o]);
            3
        };
        let n = STATE + 2 + argc;
        let vals = ValueArray::VALUES.at(self.stack, 0);
        write_values(
            self.heap.arena_mut().hot_bytes(),
            vals + 8 * state,
            &values[..n as usize],
        );
        self.fuel -= super::native::CALL_FUEL;
        self.sp = state + n;
        self.push_frame_head(
            t,
            &th,
            state + STATE,
            argc,
            Continuation::EachNative,
            next,
            None,
        );
        true
    }

    /// The end of a loop entered at the call (`EachNative`, the callback's frame
    /// popped): the call's result in place of F, from the state at `base` and the k
    /// whose callback's result decided the search (`None`: all elements went by).
    fn each_done(&mut self, base: u32, decided: Option<u32>) -> Option<Status> {
        let r = match self.get(base + MODE).as_int() {
            Some(MAP | FILTER) => self.get(base + A),
            Some(SOME) => Value::bool(decided.is_some()),
            Some(EVERY) => Value::bool(decided.is_none()),
            Some(FIND) if decided.is_some() => self.get(base + KVALUE),
            Some(FIND_INDEX) => decided.map_or(Value::int(-1), |k| conv::number(f64::from(k))),
            Some(REDUCE) => self.get(base + THIS),
            _ => Value::UNDEFINED,
        };
        self.sp = base - 1;
        self.push(r);
        // A return is a safepoint (as in `do_return`).
        self.safepoint();
        None
    }

    /// A loop entered at the call (`EachNative`) that the dense store no longer serves
    /// at k (a hole, a shorter or non-dense array): the builtin's own loop from k, as
    /// %EachRest(mode, o, k, len, f, thisArg, a) with `this` F (the stack trace's frame
    /// of the builtin), called from the caller in place of the call.
    fn each_rest(&mut self, base: u32, k: u32) -> Option<Status> {
        let f = self.get(base - 1);
        let rest = realm::installed_intrinsic(self.heap, EACH_REST);
        let state: [Value; 7] = core::array::from_fn(|i| self.get(base + i as u32));
        self.set(base - 1, rest);
        self.set(base, f);
        for (i, v) in state.into_iter().enumerate() {
            let v = if i as u32 == K {
                conv::number(f64::from(k))
            } else {
                v
            };
            self.set(base + 1 + i as u32, v);
        }
        self.sp = base + 8;
        match self.call(base - 1, 7, Continuation::PushResult, self.pc) {
            Ok(st) => st,
            Err(f) => self.each_throw(f),
        }
    }

    /// The builtin whose loop the frame at `bp` (callee `f`, continuation `cont`) runs
    /// without a frame of the builtin's: an `EachNative` callback's, or %EachRest's (in
    /// the prelude) `this`.
    pub(super) fn each_builtin(
        &self,
        f: Obj,
        cont: Continuation,
        (stack, bp): (u32, u32),
        in_prelude: bool,
    ) -> Option<Obj> {
        let get = |i: u32| -> Value { self.heap.arena().read(ValueArray::VALUES.at(stack, i)) };
        if cont == Continuation::EachNative {
            return get(bp - STATE - 1).as_object();
        }
        if in_prelude && Value::object(f) == realm::installed_intrinsic(self.heap, EACH_REST) {
            return get(bp + 1).as_object();
        }
        None
    }

    /// The loop's current k.
    fn each_k(&self, base: u32) -> u32 {
        let k = self.get(base + K);
        match k.as_int() {
            Some(i) => i as u32,
            None => k.as_number().and_then(index).unwrap_or(0),
        }
    }

    /// Completes the step of element k with the callback's result `v`; true if that
    /// decided the search (some, every, find, findIndex). A store to `a` (map at k,
    /// filter at its length, find's and reduce's box at 0) of KVALUE: map and reduce put
    /// `v` there first, which the step no longer needs. At the heap cap the store
    /// collects and is tried once more (D1900).
    fn each_complete(&mut self, base: u32, mode: i32, v: Value) -> Result<bool, Fault> {
        let truthy = !matches!(mode, MAP | REDUCE | FROM | REPLACE)
            && v.as_bool()
                .unwrap_or_else(|| conv::to_boolean(self.heap, v));
        let (stop, store) = match mode {
            MAP | REDUCE | FROM => {
                self.set(base + KVALUE, v);
                if mode == REDUCE {
                    self.set(base + THIS, v);
                }
                (false, true)
            }
            // A result that is not a string is the builtin's to convert, before the next
            // call.
            REPLACE => {
                self.set(base + KVALUE, v);
                (v.as_string().is_none(), true)
            }
            FILTER => (false, truthy),
            SOME | FIND_INDEX => (truthy, false),
            EVERY => (!truthy, false),
            FIND => (truthy, truthy),
            _ => (false, false),
        };
        // A loop entered at the call (`each_call`) keeps find's element and reduce's
        // accumulator in its state, without a box.
        if !store || (matches!(mode, FIND | REDUCE) && self.get(base + A).as_object().is_none()) {
            return Ok(stop);
        }
        let put = |ex: &mut Self| {
            let a = ex.get(base + A).as_object().expect("each: a");
            let i = match mode {
                MAP | FROM | REPLACE => ex.each_k(base),
                FILTER => ex.heap.array_length(a),
                _ => 0,
            };
            let v = ex.get(base + KVALUE);
            if ex.heap.create_element_fast(a, i, v)? {
                return Ok(());
            }
            let key = ex.heap.atom_from_index(i)?;
            if !ex.heap.create_data_property(a, key, v)? {
                return type_error("could not define property");
            }
            Ok(())
        };
        match put(self) {
            Err(e) if e.is_out_of_memory() && !self.host_oom => {
                self.safepoint_collect();
                if self.host_oom {
                    return Err(e);
                }
                put(self)?;
            }
            r => r?,
        }
        Ok(stop)
    }

    pub(super) fn each_leaf(&mut self) {
        let (bp, lp) = (self.bp, self.lp);
        let base = bp - STATE;
        let Some(leaf) = self.leaf() else {
            return;
        };
        if self.pc != 0 || self.get(bp) != self.get(base + F) {
            return;
        }
        let Some(mode) = self.get(base + MODE).as_int() else {
            return;
        };
        if !matches!(
            mode,
            FOR_EACH | MAP | FILTER | SOME | EVERY | FIND | FIND_INDEX | REDUCE
        ) {
            return;
        }
        let (Some(k0), Some(len), Some(o)) = (
            self.get(base + K).as_int(),
            self.get(base + LEN).as_int(),
            self.get(base + O).as_object(),
        ) else {
            return;
        };
        let cap = match leaf.cap {
            Some(c) => {
                let b = self.heap.arena().bytes();
                let Some(f) = ld(b, ValueArray::VALUES.at(self.stack, bp)).as_object() else {
                    return;
                };
                let Some(bx) = ld(b, object_slot(b, f.offset(), c)).as_internal() else {
                    return;
                };
                Some(bx + crate::heap::cells::BoxCell::VALUE.offset)
            }
            None => None,
        };
        let tracked = cap.is_some_and(|at| at < self.heap.arena().tracked_end());
        let stress = self.opts.gc_stress;
        let reduce = mode == REDUCE;
        let (mut k, mut x, mut acc) = (k0, self.get(base + KVALUE), self.get(base + THIS));
        let (mut a0, mut a1) = (self.get(bp + 2), self.get(bp + 3));
        let y = leaf.y.map(Value::int);
        let cost = leaf.fuel + super::native::CALL_FUEL;
        let mut fuel = self.fuel;
        let mut safepoint = self.heap.gc_pending();
        let bounds = |b: &[u8]| {
            let el = ld32(b, o.offset() + Object::ELEMENTS.offset);
            let n = if el == 0 {
                0
            } else {
                ld32(b, el + ValueArray::LEN.offset).min(len as u32)
            };
            (el, n)
        };
        let (mut el, mut lim) = bounds(self.heap.arena().bytes());
        let a = self.get(base + A).as_object();
        let native = Continuation::from_flags(self.header_int(frame::FLAGS) as i32)
            == Continuation::EachNative;
        let mut proven = false;
        let mut entry = false;
        let kind = match (mode, cap.is_some(), leaf.op) {
            (FOR_EACH, true, _) => Some(FOLD),
            (REDUCE, false, ADD | SUB | MUL) if leaf.y.is_none() => Some(FOLD),
            (MAP, false, ADD | SUB | MUL) => Some(MAP_TO),
            (FILTER, false, LT | LE | GT | GE | STRICT_EQ | STRICT_NE) => Some(KEEP),
            (
                SOME | EVERY | FIND | FIND_INDEX,
                false,
                LT | LE | GT | GE | STRICT_EQ | STRICT_NE,
            ) => Some(UNTIL),
            _ => None,
        };
        let mut scan = kind.filter(|_| !stress);
        loop {
            if let Some(kind) = scan.filter(|_| !safepoint && fuel > 0 && k as u32 + 1 < lim) {
                let b = self.heap.arena().bytes();
                let held = match cap {
                    Some(at) => ld(b, at).as_int(),
                    None if reduce => acc.as_int(),
                    None => Some(0),
                };
                let out = match (kind, a) {
                    (KEEP | MAP_TO, Some(a)) if proven => self
                        .heap
                        .element_tail(a)
                        .filter(|&(_, n, _)| {
                            n == if kind == MAP_TO {
                                k as u32
                            } else {
                                self.heap.array_length(a)
                            }
                        })
                        .map(|t| (a, t)),
                    _ => None,
                };
                let ready = matches!(kind, UNTIL | FOLD | KEEP) || out.is_some();
                let at = ValueArray::VALUES.at(el, k as u32);
                if let (Some(held), true, true) = (held, ready, ld(b, at) == x) {
                    let steps = ((fuel - 1) / cost) as u32;
                    let (oel, n, room) = out.map_or((0, 0, 0), |(_, t)| t);
                    let mut r = Run {
                        acc: held,
                        el,
                        end: (lim - 1).min((k as u32).saturating_add(steps)),
                        k: k as u32,
                        n,
                        out: ValueArray::VALUES.at(oel, 0),
                        q: leaf.y,
                        room,
                        stop: mode != EVERY,
                    };
                    scan_any(kind, leaf.op, self.heap.arena_mut().hot_bytes(), &mut r);
                    let done = r.k - k as u32;
                    if done > 0 {
                        fuel -= i64::from(done) * cost;
                        k = r.k as i32;
                        x = ld(self.heap.arena().bytes(), ValueArray::VALUES.at(el, r.k));
                        if reduce {
                            acc = Value::int(r.acc);
                            (a0, a1) = (acc, x);
                        } else {
                            (a0, a1) = (x, Value::int(k));
                        }
                        if let Some(at) = cap {
                            if tracked {
                                self.heap.arena_mut().wrote(at, 8);
                            }
                            st(self.heap.arena_mut().hot_bytes(), at, Value::int(r.acc));
                        }
                        if let Some((a, _)) = out
                            && r.n > n
                        {
                            self.heap.arena_mut().wrote(r.out + 8 * n, 8 * (r.n - n));
                            self.heap.set_element_count(a, oel, r.n);
                        }
                    } else if x.as_int().is_none() {
                        scan = None;
                    }
                } else if held.is_none() || ld(b, at) != x {
                    scan = None;
                }
            }
            let b = self.heap.arena().bytes();
            let v = match cap {
                Some(at) => {
                    let old = ld(b, at);
                    if old.is_uninitialized() {
                        break;
                    }
                    leaf_eval(leaf.op, old, a0)
                }
                None => leaf_eval(leaf.op, a0, y.unwrap_or(a1)),
            };
            let Some(v) = v else {
                break;
            };
            let store = match mode {
                FOR_EACH | REDUCE => None,
                MAP => Some((k as u32, v)),
                _ => match (mode, v.as_bool()) {
                    (SOME | FIND_INDEX | FIND, Some(false))
                    | (EVERY, Some(true))
                    | (FILTER, Some(false)) => None,
                    (FILTER, Some(true)) => Some((u32::MAX, x)),
                    _ => break,
                },
            };
            let next = k as u32 + 1;
            if next >= lim {
                break;
            }
            let nx = ld(b, ValueArray::VALUES.at(el, next));
            if nx.is_hole() {
                break;
            }
            if let Some((i, sv)) = store {
                let Some(a) = a else {
                    break;
                };
                let i = if i == u32::MAX {
                    self.heap.array_length(a)
                } else {
                    i
                };
                if !(proven && self.heap.append_element_reserved(a, i, sv)) {
                    if !self.each_store(a, i, sv) {
                        break;
                    }
                    proven = true;
                    (el, lim) = bounds(self.heap.arena().bytes());
                    safepoint |= self.heap.gc_pending();
                }
            }
            if let Some(at) = cap {
                if tracked {
                    self.heap.arena_mut().wrote(at, 8);
                }
                st(self.heap.arena_mut().hot_bytes(), at, v);
            }
            fuel -= cost;
            let next = next as i32;
            if reduce {
                acc = v;
                (a0, a1) = (v, nx);
            } else {
                (a0, a1) = (nx, Value::int(next));
            }
            (k, x) = (next, nx);
            if stress || safepoint {
                safepoint = true;
                entry = true;
                break;
            }
            if fuel <= 0 {
                entry = true;
                break;
            }
        }
        self.fuel = fuel;
        if native && !entry {
            let b = self.heap.arena().bytes();
            let v = match cap {
                Some(at) => Some(ld(b, at))
                    .filter(|old| !old.is_uninitialized())
                    .and_then(|old| leaf_eval(leaf.op, old, a0)),
                None => leaf_eval(leaf.op, a0, y.unwrap_or(a1)),
            };
            let last = k + 1 >= len;
            let end = v.and_then(|v| match (mode, v.as_bool()) {
                (FOR_EACH | REDUCE, _) if last => Some((v, false, None)),
                (MAP, _) if last => Some((v, false, Some((k as u32, v)))),
                (FILTER, Some(t)) if last => Some((v, false, t.then_some((u32::MAX, x)))),
                (SOME | FIND_INDEX | FIND, Some(true)) | (EVERY, Some(false)) => {
                    Some((v, true, None))
                }
                (SOME | FIND_INDEX | FIND, Some(false)) | (EVERY, Some(true)) if last => {
                    Some((v, false, None))
                }
                _ => None,
            });
            if let Some((v, decided, store)) = end
                && store.is_none_or(|(i, sv)| a.is_some_and(|a| self.each_store(a, i, sv)))
            {
                if let Some(at) = cap {
                    if tracked {
                        self.heap.arena_mut().wrote(at, 8);
                    }
                    st(self.heap.arena_mut().hot_bytes(), at, v);
                }
                self.fuel -= leaf.fuel;
                self.set(base + KVALUE, x);
                if reduce {
                    self.set(base + THIS, v);
                }
                self.pop_frame();
                let _ = self.each_done(base, decided.then_some(k as u32));
                return;
            }
        }
        if k == k0 {
            return;
        }
        let n = HotStep {
            k: k as u32,
            x,
            o: Value::object(o),
            acc: reduce.then_some(acc),
            store: None,
        };
        let vals = ValueArray::VALUES.at(self.stack, 0);
        let locals = self.locals();
        self.sp = hot_enter(
            self.heap.arena_mut().hot_bytes(),
            vals,
            (bp, self.hp, lp, locals),
            &n,
        );
        if safepoint {
            self.safepoint();
        }
    }

    fn leaf(&self) -> Option<Leaf> {
        const ARG0: u8 = Op::GetArg0 as u8;
        const ARG1: u8 = Op::GetArg1 as u8;
        const CAP: u8 = Op::GetCapChk as u8;
        const PUT_CAP: u8 = Op::PutCapChk as u8;
        const PUSH0: u8 = Op::Push0 as u8;
        const PUSH3: u8 = Op::Push3 as u8;
        const PUSH_I8: u8 = Op::PushI8 as u8;
        const PUSH_I32: u8 = Op::PushI32 as u8;
        const RETURN: u8 = Op::Return as u8;
        const RETURN_UNDEF: u8 = Op::ReturnUndef as u8;
        let b = self.heap.arena().bytes();
        let c = b.get(self.code as usize..(self.code + self.code_len) as usize)?;
        let (op, y, cap) = match *c {
            [ARG0, ARG1, op, RETURN] => (op, None, None),
            [ARG0, p @ PUSH0..=PUSH3, op, RETURN] => (op, Some(i32::from(p - PUSH0)), None),
            [ARG0, PUSH_I8, i, op, RETURN] => (op, Some(i32::from(i as i8)), None),
            [ARG0, PUSH_I32, b0, b1, b2, b3, op, RETURN] => {
                (op, Some(i32::from_le_bytes([b0, b1, b2, b3])), None)
            }
            [CAP, c0, c1, ARG0, op, PUT_CAP, d0, d1, RETURN_UNDEF] if [c0, c1] == [d0, d1] => {
                (op, None, Some(u32::from(u16::from_le_bytes([c0, c1]))))
            }
            _ => return None,
        };
        if !LEAF_OPS.contains(&op) || (cap.is_some() && !matches!(op, ADD | SUB | MUL)) {
            return None;
        }
        if cap.is_some() && self.get(self.bp - STATE + MODE).as_int() != Some(FOR_EACH) {
            return None;
        }
        let mut fuel = 0;
        let mut p = 0;
        while p < c.len() {
            let info = &OPS[c[p] as usize];
            fuel += info.cost.fuel() as i64;
            p += info.size as usize;
        }
        Some(Leaf { cap, fuel, op, y })
    }

    /// A failed step, thrown as the builtin's CreateDataPropertyOrThrow at its resume
    /// point (the builtin's frame is current).
    fn each_throw(&mut self, f: Fault) -> Option<Status> {
        let pc = self.pc;
        let exc = self.fault_value(f, Some(pc));
        self.throw_value(exc, self.call_site_key(pc))
    }
}

struct Leaf {
    cap: Option<u32>,
    fuel: i64,
    op: u8,
    y: Option<i32>,
}

const ADD: u8 = Op::Add as u8;
const GE: u8 = Op::Ge as u8;
const GT: u8 = Op::Gt as u8;
const LE: u8 = Op::Le as u8;
const LT: u8 = Op::Lt as u8;
const MUL: u8 = Op::Mul as u8;
const STRICT_EQ: u8 = Op::StrictEq as u8;
const STRICT_NE: u8 = Op::StrictNe as u8;
const SUB: u8 = Op::Sub as u8;
const LEAF_OPS: [u8; 9] = [ADD, GE, GT, LE, LT, MUL, STRICT_EQ, STRICT_NE, SUB];

#[inline(always)]
fn leaf_eval(op: u8, x: Value, y: Value) -> Option<Value> {
    if op == STRICT_EQ || op == STRICT_NE {
        let eq = match (x.as_int(), y.as_int()) {
            (Some(p), Some(q)) => p == q,
            _ => match (x.as_number(), y.as_number()) {
                (Some(p), Some(q)) => p == q,
                _ if x == y => true,
                _ if (x.as_string().is_some() && y.as_string().is_some())
                    || (x.as_bigint().is_some() && y.as_bigint().is_some()) =>
                {
                    return None;
                }
                _ => false,
            },
        };
        return Some(Value::bool(eq == (op == STRICT_EQ)));
    }
    if let (Some(p), Some(q)) = (x.as_int(), y.as_int()) {
        let v = match op {
            ADD => p.checked_add(q).map(Value::int),
            SUB => p.checked_sub(q).map(Value::int),
            MUL => p.checked_mul(q).filter(|&r| r != 0).map(Value::int),
            LT => Some(Value::bool(p < q)),
            LE => Some(Value::bool(p <= q)),
            GT => Some(Value::bool(p > q)),
            _ => Some(Value::bool(p >= q)),
        };
        if v.is_some() {
            return v;
        }
    }
    let (p, q) = (x.as_number()?, y.as_number()?);
    Some(match op {
        ADD => conv::number(p + q),
        SUB => conv::number(p - q),
        MUL => conv::number(p * q),
        LT => Value::bool(p < q),
        LE => Value::bool(p <= q),
        GT => Value::bool(p > q),
        _ => Value::bool(p >= q),
    })
}

const UNTIL: u8 = 0;
const KEEP: u8 = 1;
const MAP_TO: u8 = 2;
const FOLD: u8 = 3;

struct Run {
    acc: i32,
    el: u32,
    end: u32,
    k: u32,
    n: u32,
    out: u32,
    q: Option<i32>,
    room: u32,
    stop: bool,
}

#[inline(always)]
fn compare<const OP: u8>(p: i32, q: i32) -> bool {
    match OP {
        LT => p < q,
        LE => p <= q,
        GT => p > q,
        GE => p >= q,
        STRICT_EQ => p == q,
        _ => p != q,
    }
}

#[inline(always)]
fn arith<const OP: u8>(p: i32, q: i32) -> Option<i32> {
    match OP {
        ADD => p.checked_add(q),
        SUB => p.checked_sub(q),
        _ => p.checked_mul(q).filter(|&r| r != 0),
    }
}

fn scan<const KIND: u8, const OP: u8>(b: &mut [u8], r: &mut Run) {
    let el = r.el;
    let mut x = ld(b, ValueArray::VALUES.at(el, r.k));
    while r.k < r.end {
        let Some(p) = x.as_int() else {
            break;
        };
        let q = r.q.unwrap_or(r.k as i32);
        let mut acc = r.acc;
        let put = match KIND {
            UNTIL => {
                if compare::<OP>(p, q) == r.stop {
                    break;
                }
                None
            }
            KEEP => compare::<OP>(p, q).then_some(x),
            MAP_TO => match arith::<OP>(p, q) {
                Some(v) => Some(Value::int(v)),
                None => break,
            },
            _ => match arith::<OP>(acc, p) {
                Some(v) => {
                    acc = v;
                    None
                }
                None => break,
            },
        };
        let nx = ld(b, ValueArray::VALUES.at(el, r.k + 1));
        if nx.is_hole() {
            break;
        }
        if let Some(v) = put {
            if r.n == r.room {
                break;
            }
            st(b, r.out + 8 * r.n, v);
            r.n += 1;
        }
        r.acc = acc;
        r.k += 1;
        x = nx;
    }
}

fn scan_test<const KIND: u8>(op: u8, b: &mut [u8], r: &mut Run) {
    match op {
        LT => scan::<KIND, LT>(b, r),
        LE => scan::<KIND, LE>(b, r),
        GT => scan::<KIND, GT>(b, r),
        GE => scan::<KIND, GE>(b, r),
        STRICT_EQ => scan::<KIND, STRICT_EQ>(b, r),
        _ => scan::<KIND, STRICT_NE>(b, r),
    }
}

fn scan_arith<const KIND: u8>(op: u8, b: &mut [u8], r: &mut Run) {
    match op {
        ADD => scan::<KIND, ADD>(b, r),
        SUB => scan::<KIND, SUB>(b, r),
        _ => scan::<KIND, MUL>(b, r),
    }
}

#[inline(never)]
fn scan_any(kind: u8, op: u8, b: &mut [u8], r: &mut Run) {
    match kind {
        UNTIL => scan_test::<UNTIL>(op, b, r),
        KEEP => scan_test::<KEEP>(op, b, r),
        MAP_TO => scan_arith::<MAP_TO>(op, b, r),
        _ => scan_arith::<FOLD>(op, b, r),
    }
}

/// A step of the loop that its own frame takes (`each_again`, and run_hot's `return`
/// arm): the callback of the frame at `bp` returned a result that did not decide the
/// search, and the next element is present in the dense store.
pub(super) struct HotStep {
    k: u32,
    x: Value,
    o: Value,
    /// Reduce's new accumulator (the callback's result).
    acc: Option<Value>,
    /// What the step stores in `a` (`each_store`).
    pub(super) store: Option<(Obj, u32, Value)>,
}

/// The step of the callback of the frame at `bp` returning `v`, from the loop state
/// below the frame, or `None` for anything `each_leave` does. Reads only.
#[inline(always)]
pub(super) fn hot_step(b: &[u8], vals: u32, bp: u32, v: Value) -> Option<HotStep> {
    let w: [u8; 8 * (STATE as usize + 1)] = window(b, vals + 8 * (bp - STATE));
    let get = |i: u32| {
        let i = 8 * i as usize;
        Value::from_raw(u64::from_le_bytes(w[i..i + 8].try_into().unwrap()))
    };
    // Only the callback's own frame is reused: an async callback's return replaces it
    // by a builtin's (`tail_call_builtin`).
    if get(STATE) != get(F) {
        return None;
    }
    let mode = get(MODE).as_int()?;
    let k = get(K).as_int()?;
    let (store, acc) = match mode {
        FOR_EACH => (None, None),
        REDUCE => (None, Some(v)),
        MAP => (Some((k as u32, v)), None),
        _ => match (mode, v.as_bool()?) {
            (SOME | FIND_INDEX | FIND, false) | (EVERY, true) | (FILTER, false) => (None, None),
            (FILTER, true) => (Some((u32::MAX, get(KVALUE))), None),
            _ => return None,
        },
    };
    let next = k.checked_add(1)?;
    let len = get(LEN);
    if !len.as_int().map_or_else(
        || len.as_number().is_some_and(|n| f64::from(next) < n),
        |n| next < n,
    ) {
        return None;
    }
    let o = get(O);
    let el = ld32(b, o.as_object()?.offset() + Object::ELEMENTS.offset);
    let next = next as u32;
    if el == 0 || next >= ld32(b, el + ValueArray::LEN.offset) {
        return None;
    }
    let x = ld(b, ValueArray::VALUES.at(el, next));
    if x.is_hole() {
        return None;
    }
    let store = match store {
        Some((i, sv)) => Some((get(A).as_object()?, i, sv)),
        None => None,
    };
    Some(HotStep {
        k: next,
        x,
        o,
        acc,
        store,
    })
}

/// Writes the step into the frame at `bp` as `call` writes a new one (the arguments
/// padded with undefined, undefined locals; the callee, `this` and the header stay)
/// and into the loop state (k, KVALUE, reduce's accumulator). The frame's new `sp`.
#[inline(always)]
pub(super) fn hot_enter(
    b: &mut [u8],
    vals: u32,
    (bp, hp, lp, locals): (u32, u32, u32, u32),
    n: &HotStep,
) -> u32 {
    let slot = |i: u32| vals + 8 * i;
    let base = bp - STATE;
    let kv = Value::int(n.k as i32);
    st(b, slot(base + K), kv);
    st(b, slot(base + KVALUE), n.x);
    let argc = if let Some(acc) = n.acc {
        st(b, slot(base + THIS), acc);
        write_values(b, slot(bp + 2), &[acc, n.x, kv, n.o]);
        4
    } else {
        write_values(b, slot(bp + 2), &[n.x, kv, n.o]);
        3
    };
    fill(b, slot(bp + 2 + argc), hp - bp - 2 - argc, Value::UNDEFINED);
    fill(b, slot(lp), locals, Value::UNDEFINED);
    lp + locals
}
