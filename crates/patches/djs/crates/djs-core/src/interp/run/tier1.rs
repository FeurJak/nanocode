use super::hot::{G_ADD_LOC, G_INC_LT, G_INC_LT_LOC, ld, st, word};
use super::{Exec, fast};
use crate::bytecode::{OPS, Op};
use crate::heap::cells::ValueArray;
use crate::interp::conv;
use crate::value::Value;

fn body(b: &[u8], head: u32, end: u32) -> Option<(u32, i64)> {
    let (mut at, mut fuel) = (head, 0);
    while at < end {
        let w = word(b, at);
        let op = w as u8;
        let offset = match op {
            fast::JUMP | fast::JUMP_IF_FALSE | fast::JUMP_IF_TRUE => (w >> 8) as u32 as i32,
            fast::JUMP8 | fast::JUMP_IF_FALSE8 | fast::JUMP_IF_TRUE8 => (w >> 8) as u8 as i8 as i32,
            fast::GET_LOC
            | fast::PUT_LOC
            | fast::SET_LOC
            | fast::GET_ARG
            | fast::GET_LOC0..=fast::GET_LOC3
            | fast::PUT_LOC0..=fast::PUT_LOC3
            | fast::SET_LOC0..=fast::SET_LOC3
            | fast::GET_ARG0..=fast::GET_ARG3
            | fast::GET_LOC8
            | fast::PUT_LOC8
            | fast::SET_LOC8
            | fast::GET_ARG8
            | fast::PUSH_I8
            | fast::PUSH_I32
            | fast::PUSH0..=fast::PUSH3
            | fast::UNDEFINED
            | fast::NULL
            | fast::PUSH_TRUE
            | fast::PUSH_FALSE
            | fast::DROP
            | fast::DUP
            | fast::INC
            | fast::DEC
            | fast::INC_LOC
            | fast::DEC_LOC
            | fast::ADD
            | fast::SUB
            | fast::MUL
            | fast::DIV
            | fast::MOD
            | fast::BAND
            | fast::BOR
            | fast::BXOR
            | fast::SHL
            | fast::SAR
            | fast::LT
            | fast::LE
            | fast::GT
            | fast::GE
            | fast::ADD_LOC8
            | fast::INC_LT
            | fast::INC_LT_LOC => 0,
            _ => return None,
        };
        let info = &OPS[op as usize];
        fuel += info.cost.fuel() as i64;
        at += info.size as u32;
        let to = at as i64 + offset as i64;
        if to < at as i64 {
            return (to == head as i64).then_some((at, fuel));
        }
    }
    None
}

impl Exec<'_> {
    #[inline(never)]
    pub(super) fn run_tier1(&mut self) -> bool {
        if self.opts.gc_stress || self.heap.gc_pending() || self.pc >= self.code_len {
            return false;
        }
        let (code, bp, lp) = (self.code, self.bp, self.lp);
        let head = code + self.pc;
        let vals = ValueArray::VALUES.at(self.stack, 0);
        let b: &mut [u8] = self.heap.arena_mut().hot_bytes();
        let Some((stop, pass)) = body(b, head, code + self.code_len) else {
            return false;
        };
        if self.fuel <= pass {
            return false;
        }
        let mut budget = self.opts.hot_backedges;
        let (mut ip, mut sp, mut fuel) = (head, self.sp, self.fuel);
        macro_rules! slot {
            ($i:expr) => {
                vals + 8 * ($i)
            };
        }
        let again = loop {
            let w = word(b, ip);
            let op = w as u8;
            macro_rules! imm16 {
                () => {
                    (w >> 8) as u16 as u32
                };
            }
            macro_rules! imm32 {
                () => {
                    (w >> 8) as u32
                };
            }
            macro_rules! next {
                ($op:expr) => {{
                    let info = &OPS[$op as usize];
                    fuel -= info.cost.fuel() as i64;
                    ip += info.size as u32;
                }};
            }
            macro_rules! top {
                ($k:expr) => {
                    ld(b, slot!(sp - 1 - $k))
                };
            }
            macro_rules! push {
                ($v:expr) => {{
                    let v: Value = $v;
                    st(b, slot!(sp), v);
                    sp += 1;
                }};
            }
            macro_rules! back {
                ($next:expr) => {
                    if ip < $next {
                        if fuel <= pass {
                            break false;
                        }
                        budget -= 1;
                        if budget == 0 {
                            break true;
                        }
                    } else if ip >= stop {
                        break false;
                    }
                };
            }
            macro_rules! jump {
                ($op:expr, $taken:expr, $offset:expr) => {{
                    let info = &OPS[$op as usize];
                    fuel -= info.cost.fuel() as i64;
                    let next = ip + info.size as u32;
                    ip = next;
                    if $taken {
                        let offset: i32 = $offset;
                        ip = (next as i64 + offset as i64) as u32;
                    }
                    back!(next);
                }};
            }
            macro_rules! binary {
                ($op:expr, $int:expr, $float:expr) => {{
                    let (a, c) = (top!(1), top!(0));
                    let int: Option<Value> = match (a.as_int(), c.as_int()) {
                        (Some(x), Some(y)) => ($int)(x, y),
                        _ => None,
                    };
                    let v = match int {
                        Some(v) => v,
                        None => match (a.as_number(), c.as_number()) {
                            (Some(x), Some(y)) => match ($float)(x, y) {
                                Some(v) => v,
                                None => break false,
                            },
                            _ => break false,
                        },
                    };
                    sp -= 1;
                    st(b, slot!(sp - 1), v);
                    next!($op);
                }};
            }
            macro_rules! no_float {
                () => {
                    |_: f64, _: f64| -> Option<Value> { None }
                };
            }
            macro_rules! get_loc {
                ($i:expr, $op:expr) => {{
                    push!(ld(b, slot!(lp + $i)));
                    next!($op);
                }};
            }
            macro_rules! put_loc {
                ($i:expr, $op:expr) => {{
                    sp -= 1;
                    let v = ld(b, slot!(sp));
                    st(b, slot!(lp + $i), v);
                    next!($op);
                }};
            }
            macro_rules! set_loc {
                ($i:expr, $op:expr) => {{
                    let v = top!(0);
                    st(b, slot!(lp + $i), v);
                    next!($op);
                }};
            }
            macro_rules! get_arg {
                ($i:expr, $op:expr) => {{
                    push!(ld(b, slot!(bp + 2 + $i)));
                    next!($op);
                }};
            }
            match op {
                fast::ADD_LOC8 => {
                    let x = ld(b, slot!(lp + (imm16!() & 0xFF)));
                    let y = ld(b, slot!(lp + ((w >> 24) as u8 as u32)));
                    let (xi, yi) = (x.as_int(), y.as_int());
                    let v = match (xi, yi) {
                        (Some(p), Some(q)) => p.checked_add(q).map(Value::int),
                        _ => None,
                    };
                    let num = |v: Value, i: Option<i32>| i.map(f64::from).or_else(|| v.as_number());
                    let v = match v {
                        Some(v) => Some(v),
                        None => match (num(x, xi), num(y, yi)) {
                            (Some(p), Some(q)) => Some(match p + q {
                                r if r.abs() > f64::from(i32::MAX) => Value::from_raw(r.to_bits()),
                                r => conv::number(r),
                            }),
                            _ => None,
                        },
                    };
                    match v {
                        Some(v) => {
                            st(b, slot!(lp + ((w >> 48) as u8 as u32)), v);
                            fuel -= G_ADD_LOC.0;
                            ip += G_ADD_LOC.1;
                        }
                        None => get_loc!(imm16!() & 0xFF, Op::AddLoc8),
                    }
                }
                fast::INC_LT => {
                    let at = slot!(lp + imm16!());
                    let Some(i) = ld(b, at).as_int().and_then(|i| i.checked_add(1)) else {
                        break false;
                    };
                    st(b, at, Value::int(i));
                    let t = word(b, ip + 5);
                    match t >> 48 {
                        tail if tail as u8 == fast::JUMP_IF_TRUE8 => {
                            fuel -= G_INC_LT.0;
                            let next = ip + G_INC_LT.1;
                            ip = next;
                            if i < (t >> 8) as u32 as i32 {
                                ip = (next as i64 + (tail >> 8) as u8 as i8 as i64) as u32;
                            }
                            back!(next);
                        }
                        _ => next!(Op::IncLt),
                    }
                }
                fast::INC_LT_LOC => {
                    let at = slot!(lp + imm16!());
                    let Some(i) = ld(b, at).as_int().and_then(|i| i.checked_add(1)) else {
                        break false;
                    };
                    st(b, at, Value::int(i));
                    let t = word(b, ip + 5);
                    let base = if t as u8 == fast::GET_ARG8 {
                        bp + 2
                    } else {
                        lp
                    };
                    match ld(b, slot!(base + (t >> 8) as u8 as u32)).as_int() {
                        Some(n) if (t >> 24) as u8 == fast::JUMP_IF_TRUE8 => {
                            fuel -= G_INC_LT_LOC.0;
                            let next = ip + G_INC_LT_LOC.1;
                            ip = next;
                            if i < n {
                                ip = (next as i64 + (t >> 32) as u8 as i8 as i64) as u32;
                            }
                            back!(next);
                        }
                        _ => next!(Op::IncLtLoc),
                    }
                }
                fast::GET_LOC => get_loc!(imm16!(), Op::GetLoc),
                fast::PUT_LOC => put_loc!(imm16!(), Op::PutLoc),
                fast::SET_LOC => set_loc!(imm16!(), Op::SetLoc),
                fast::GET_ARG => get_arg!(imm16!(), Op::GetArg),
                fast::GET_LOC0..=fast::GET_LOC3 => {
                    get_loc!((op - fast::GET_LOC0) as u32, Op::GetLoc0)
                }
                fast::GET_LOC8 => get_loc!(imm16!() & 0xFF, Op::GetLoc8),
                fast::PUT_LOC0..=fast::PUT_LOC3 => {
                    put_loc!((op - fast::PUT_LOC0) as u32, Op::PutLoc0)
                }
                fast::PUT_LOC8 => put_loc!(imm16!() & 0xFF, Op::PutLoc8),
                fast::SET_LOC0..=fast::SET_LOC3 => {
                    set_loc!((op - fast::SET_LOC0) as u32, Op::SetLoc0)
                }
                fast::SET_LOC8 => set_loc!(imm16!() & 0xFF, Op::SetLoc8),
                fast::GET_ARG0..=fast::GET_ARG3 => {
                    get_arg!((op - fast::GET_ARG0) as u32, Op::GetArg0)
                }
                fast::GET_ARG8 => get_arg!(imm16!() & 0xFF, Op::GetArg8),
                fast::PUSH_I8 => {
                    push!(Value::int(imm32!() as u8 as i8 as i32));
                    next!(Op::PushI8);
                }
                fast::PUSH_I32 => {
                    push!(Value::int(imm32!() as i32));
                    next!(Op::PushI32);
                }
                fast::PUSH0..=fast::PUSH3 => {
                    push!(Value::int((op - fast::PUSH0) as i32));
                    next!(Op::Push0);
                }
                fast::UNDEFINED => {
                    push!(Value::UNDEFINED);
                    next!(Op::Undefined);
                }
                fast::NULL => {
                    push!(Value::NULL);
                    next!(Op::Null);
                }
                fast::PUSH_TRUE => {
                    push!(Value::TRUE);
                    next!(Op::PushTrue);
                }
                fast::PUSH_FALSE => {
                    push!(Value::FALSE);
                    next!(Op::PushFalse);
                }
                fast::DROP => {
                    sp -= 1;
                    next!(Op::Drop);
                }
                fast::DUP => {
                    push!(top!(0));
                    next!(Op::Dup);
                }
                fast::INC | fast::DEC => {
                    let d = if op == fast::INC { 1 } else { -1 };
                    let Some(i) = top!(0).as_int().and_then(|i| i.checked_add(d)) else {
                        break false;
                    };
                    st(b, slot!(sp - 1), Value::int(i));
                    next!(if op == fast::INC { Op::Inc } else { Op::Dec });
                }
                fast::INC_LOC | fast::DEC_LOC => {
                    let d = if op == fast::INC_LOC { 1 } else { -1 };
                    let at = slot!(lp + imm16!());
                    let Some(i) = ld(b, at).as_int().and_then(|i| i.checked_add(d)) else {
                        break false;
                    };
                    st(b, at, Value::int(i));
                    next!(if op == fast::INC_LOC {
                        Op::IncLoc
                    } else {
                        Op::DecLoc
                    });
                }
                fast::ADD => binary!(
                    Op::Add,
                    |x: i32, y: i32| x.checked_add(y).map(Value::int),
                    |x: f64, y: f64| Some(conv::number(x + y))
                ),
                fast::SUB => binary!(
                    Op::Sub,
                    |x: i32, y: i32| x.checked_sub(y).map(Value::int),
                    |x: f64, y: f64| Some(conv::number(x - y))
                ),
                fast::MUL => binary!(
                    Op::Mul,
                    |x: i32, y: i32| x.checked_mul(y).filter(|&r| r != 0).map(Value::int),
                    |x: f64, y: f64| Some(conv::number(x * y))
                ),
                fast::DIV => binary!(Op::Div, |_: i32, _: i32| None, |x: f64, y: f64| Some(
                    conv::number(x / y)
                )),
                fast::MOD => binary!(
                    Op::Mod,
                    |x: i32, y: i32| (x >= 0 && y != 0).then(|| Value::int(x % y)),
                    no_float!()
                ),
                fast::BAND => binary!(
                    Op::BAnd,
                    |x: i32, y: i32| Some(Value::int(x & y)),
                    no_float!()
                ),
                fast::BOR => binary!(
                    Op::BOr,
                    |x: i32, y: i32| Some(Value::int(x | y)),
                    no_float!()
                ),
                fast::BXOR => binary!(
                    Op::BXor,
                    |x: i32, y: i32| Some(Value::int(x ^ y)),
                    no_float!()
                ),
                fast::SHL => binary!(
                    Op::Shl,
                    |x: i32, y: i32| Some(Value::int(x.wrapping_shl(y as u32 & 31))),
                    no_float!()
                ),
                fast::SAR => binary!(
                    Op::Sar,
                    |x: i32, y: i32| Some(Value::int(x >> (y as u32 & 31))),
                    no_float!()
                ),
                fast::LT => binary!(
                    Op::Lt,
                    |x: i32, y: i32| Some(Value::bool(x < y)),
                    |x: f64, y: f64| Some(Value::bool(x < y))
                ),
                fast::LE => binary!(
                    Op::Le,
                    |x: i32, y: i32| Some(Value::bool(x <= y)),
                    |x: f64, y: f64| Some(Value::bool(x <= y))
                ),
                fast::GT => binary!(
                    Op::Gt,
                    |x: i32, y: i32| Some(Value::bool(x > y)),
                    |x: f64, y: f64| Some(Value::bool(x > y))
                ),
                fast::GE => binary!(
                    Op::Ge,
                    |x: i32, y: i32| Some(Value::bool(x >= y)),
                    |x: f64, y: f64| Some(Value::bool(x >= y))
                ),
                fast::JUMP => jump!(Op::Jump, true, imm32!() as i32),
                fast::JUMP_IF_FALSE | fast::JUMP_IF_TRUE => {
                    let Some(c) = top!(0).as_bool() else {
                        break false;
                    };
                    sp -= 1;
                    if op == fast::JUMP_IF_TRUE {
                        jump!(Op::JumpIfTrue, c, imm32!() as i32)
                    } else {
                        jump!(Op::JumpIfFalse, !c, imm32!() as i32)
                    }
                }
                fast::JUMP8 => jump!(Op::Jump8, true, imm16!() as u8 as i8 as i32),
                fast::JUMP_IF_FALSE8 | fast::JUMP_IF_TRUE8 => {
                    let Some(c) = top!(0).as_bool() else {
                        break false;
                    };
                    sp -= 1;
                    if op == fast::JUMP_IF_TRUE8 {
                        jump!(Op::JumpIfTrue8, c, imm16!() as u8 as i8 as i32)
                    } else {
                        jump!(Op::JumpIfFalse8, !c, imm16!() as u8 as i8 as i32)
                    }
                }
                _ => break false,
            }
        };
        self.pc = ip - code;
        self.sp = sp;
        self.fuel = fuel;
        again
    }
}
