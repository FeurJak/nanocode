use super::hot::{ld, st, word};
use super::{Exec, fast};
use crate::bytecode::OPS;
use crate::heap::cells::ValueArray;
use crate::interp::conv;
use crate::value::Value;
use alloc::vec::Vec;

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
            | fast::PUSH_CONST
            | fast::PUSH_CONST8
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
            | fast::STRICT_EQ
            | fast::STRICT_NE
            | fast::NOT
            | fast::IS_UNDEFINED
            | fast::IS_NULLISH
            | fast::GET_ELEM
            | fast::PUT_ELEM
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

#[derive(Clone, Copy)]
enum Cmp {
    Ge,
    Gt,
    Le,
    Lt,
}

impl Cmp {
    fn mask(self, when: bool) -> u8 {
        let m = match self {
            Cmp::Ge => 0b0110,
            Cmp::Gt => 0b0100,
            Cmp::Le => 0b0011,
            Cmp::Lt => 0b0001,
        };
        if when { m } else { !m & 0b1111 }
    }
}

#[inline(always)]
fn order_int(x: i32, y: i32) -> u32 {
    u32::from(x > y) * 2 + u32::from(x == y)
}

#[inline(always)]
fn order_float(x: f64, y: f64) -> u32 {
    if x < y {
        0
    } else if x == y {
        1
    } else if x > y {
        2
    } else {
        3
    }
}

#[inline(always)]
fn number(n: f64) -> Value {
    if n.abs() > f64::from(i32::MAX) {
        Value::from_raw(n.to_bits())
    } else {
        conv::number(n)
    }
}

#[inline(always)]
fn order(a: Value, c: Value) -> Option<u32> {
    if let (Some(x), Some(y)) = (a.as_int(), c.as_int()) {
        return Some(order_int(x, y));
    }
    Some(order_float(a.as_number()?, c.as_number()?))
}

#[derive(Clone, Copy)]
#[repr(u8)]
enum I {
    Add {
        a: u8,
        c: u8,
        d: u8,
    },
    BAnd {
        a: u8,
        c: u8,
        d: u8,
    },
    BOr {
        a: u8,
        c: u8,
        d: u8,
    },
    BXor {
        a: u8,
        c: u8,
        d: u8,
    },
    Branch {
        a: u8,
        c: u8,
        mask: u8,
        to: u16,
    },
    Compare {
        a: u8,
        c: u8,
        d: u8,
        mask: u8,
    },
    Div {
        a: u8,
        c: u8,
        d: u8,
    },
    Equal {
        a: u8,
        c: u8,
        d: u8,
        ne: bool,
    },
    GetElem {
        a: u8,
        c: u8,
        d: u8,
    },
    IsNullish {
        a: u8,
        d: u8,
    },
    IsUndefined {
        a: u8,
        d: u8,
    },
    Jump {
        to: u16,
    },
    Latch {
        delta: i32,
        mask: u8,
        n: i32,
        to: u16,
        x: u8,
    },
    JumpIf {
        a: u8,
        to: u16,
        when: bool,
    },
    Leave {
        exit: u16,
    },
    Mod {
        a: u8,
        c: u8,
        d: u8,
    },
    Mov {
        a: u8,
        d: u8,
    },
    Mul {
        a: u8,
        c: u8,
        d: u8,
    },
    Nop,
    Not {
        a: u8,
        d: u8,
    },
    PutElem {
        a: u8,
        after: u16,
        c: u8,
        v: u8,
    },
    Sar {
        a: u8,
        c: u8,
        d: u8,
    },
    Shl {
        a: u8,
        c: u8,
        d: u8,
    },
    Step {
        a: u8,
        d: u8,
        delta: i32,
    },
    StepBranch {
        c: u8,
        delta: i32,
        exit: u16,
        fuel: u32,
        mask: u8,
        to: u16,
        x: u8,
    },
    Sub {
        a: u8,
        c: u8,
        d: u8,
    },
}

impl I {
    fn dst(&mut self) -> Option<&mut u8> {
        match self {
            I::Add { d, .. }
            | I::BAnd { d, .. }
            | I::BOr { d, .. }
            | I::BXor { d, .. }
            | I::Compare { d, .. }
            | I::Div { d, .. }
            | I::Equal { d, .. }
            | I::GetElem { d, .. }
            | I::IsNullish { d, .. }
            | I::IsUndefined { d, .. }
            | I::Mod { d, .. }
            | I::Mov { d, .. }
            | I::Mul { d, .. }
            | I::Not { d, .. }
            | I::Sar { d, .. }
            | I::Shl { d, .. }
            | I::Step { d, .. }
            | I::Sub { d, .. } => Some(d),
            _ => None,
        }
    }

    fn target(&mut self) -> Option<&mut u16> {
        match self {
            I::Branch { to, .. }
            | I::Jump { to }
            | I::JumpIf { to, .. }
            | I::Latch { to, .. }
            | I::StepBranch { to, .. } => Some(to),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
struct Ins {
    exit: u16,
    fuel: u32,
    i: I,
}

#[derive(Clone, Copy)]
#[repr(u8)]
enum K {
    Add,
    BAnd,
    BOr,
    BXor,
    Branch,
    Compare,
    Div,
    Equal,
    GetElem,
    IsNullish,
    IsUndefined,
    Jump,
    JumpIf,
    Latch,
    Leave,
    Mod,
    Mov,
    Mul,
    Nop,
    Not,
    PutElem,
    Sar,
    Shl,
    Step,
    StepBranch,
    Sub,
}

#[derive(Clone, Copy)]
struct Op {
    a: u8,
    aux: u16,
    c: u8,
    d: u8,
    exit: u16,
    fuel: u32,
    k: K,
    mask: u8,
    n: i32,
    to: u16,
}

fn lower(ins: &Ins) -> Op {
    let mut o = Op {
        a: 0,
        aux: 0,
        c: 0,
        d: 0,
        exit: ins.exit,
        fuel: ins.fuel,
        k: K::Nop,
        mask: 0,
        n: 0,
        to: 0,
    };
    let mut abc = |k: K, a: u8, c: u8, d: u8| {
        o.k = k;
        (o.a, o.c, o.d) = (a, c, d);
    };
    match ins.i {
        I::Add { a, c, d } => abc(K::Add, a, c, d),
        I::BAnd { a, c, d } => abc(K::BAnd, a, c, d),
        I::BOr { a, c, d } => abc(K::BOr, a, c, d),
        I::BXor { a, c, d } => abc(K::BXor, a, c, d),
        I::Div { a, c, d } => abc(K::Div, a, c, d),
        I::GetElem { a, c, d } => abc(K::GetElem, a, c, d),
        I::Mod { a, c, d } => abc(K::Mod, a, c, d),
        I::Mul { a, c, d } => abc(K::Mul, a, c, d),
        I::Sar { a, c, d } => abc(K::Sar, a, c, d),
        I::Shl { a, c, d } => abc(K::Shl, a, c, d),
        I::Sub { a, c, d } => abc(K::Sub, a, c, d),
        I::IsNullish { a, d } => abc(K::IsNullish, a, 0, d),
        I::IsUndefined { a, d } => abc(K::IsUndefined, a, 0, d),
        I::Mov { a, d } => abc(K::Mov, a, 0, d),
        I::Not { a, d } => abc(K::Not, a, 0, d),
        I::Nop => abc(K::Nop, 0, 0, 0),
        I::PutElem { a, after, c, v } => {
            abc(K::PutElem, a, c, v);
            o.aux = after;
        }
        I::Step { a, d, delta } => abc(K::Step, a, delta as i8 as u8, d),
        I::Branch { a, c, mask, to } => {
            abc(K::Branch, a, c, 0);
            (o.mask, o.to) = (mask, to);
        }
        I::Compare { a, c, d, mask } => {
            abc(K::Compare, a, c, d);
            o.mask = mask;
        }
        I::Equal { a, c, d, ne } => {
            abc(K::Equal, a, c, d);
            o.mask = u8::from(ne);
        }
        I::Jump { to } => {
            abc(K::Jump, 0, 0, 0);
            o.to = to;
        }
        I::JumpIf { a, to, when } => {
            abc(K::JumpIf, a, 0, 0);
            (o.mask, o.to) = (u8::from(when), to);
        }
        I::Latch {
            delta,
            mask,
            n,
            to,
            x,
        } => {
            abc(K::Latch, x, delta as i8 as u8, 0);
            (o.mask, o.n, o.to) = (mask, n, to);
        }
        I::Leave { exit } => {
            abc(K::Leave, 0, 0, 0);
            o.to = exit;
        }
        I::StepBranch {
            c,
            delta,
            exit,
            fuel,
            mask,
            to,
            x,
        } => {
            abc(K::StepBranch, x, c, delta as i8 as u8);
            (o.aux, o.mask, o.n, o.to) = (exit, mask, fuel as i32, to);
        }
    }
    o
}

#[derive(Clone, Copy)]
struct Exit {
    at: u32,
    ip: u32,
    len: u32,
}

#[derive(Clone, Copy)]
enum Src {
    Slot(u32),
    Value(Value),
}

enum Dest {
    Head,
    In,
    Out,
}

struct Frame {
    bp: u32,
    consts: u32,
    head: u32,
    lp: u32,
    sp: u32,
    stop: u32,
}

#[derive(Default)]
struct Plan {
    barrier: usize,
    exits: Vec<Exit>,
    fixes: Vec<(usize, u32)>,
    fuel: u32,
    fuse: Option<usize>,
    init: Vec<Src>,
    labels: Vec<(u32, u32, Option<u16>)>,
    mark: Option<u16>,
    outs: Vec<(usize, u16)>,
    prog: Vec<Ins>,
    stacks: Vec<u8>,
    stk: Vec<u8>,
    writes: Vec<(u8, u32)>,
}

impl Plan {
    fn reg(&mut self, src: Src) -> Option<u8> {
        let found = self.init.iter().position(|s| match (*s, src) {
            (Src::Slot(x), Src::Slot(y)) => x == y,
            (Src::Value(x), Src::Value(y)) => x.to_bits() == y.to_bits(),
            _ => false,
        });
        if let Some(g) = found {
            return u8::try_from(g).ok();
        }
        let g = u8::try_from(self.init.len()).ok()?;
        self.init.push(src);
        Some(g)
    }

    fn temp(&mut self, f: &Frame, j: usize) -> Option<u8> {
        self.reg(Src::Slot(f.sp + j as u32))
    }

    fn exit_at(&mut self, ip: u32) -> Option<u16> {
        let e = u16::try_from(self.exits.len()).ok()?;
        self.exits.push(Exit {
            at: self.stacks.len() as u32,
            ip,
            len: self.stk.len() as u32,
        });
        self.stacks.extend_from_slice(&self.stk);
        Some(e)
    }

    fn mark(&mut self, ip: u32) -> Option<()> {
        if self.mark.is_none() {
            self.mark = Some(self.exit_at(ip)?);
        }
        Some(())
    }

    fn emit(&mut self, i: I, cost: u32) -> Option<usize> {
        let exit = self.mark.take()?;
        self.prog.push(Ins {
            exit,
            fuel: core::mem::take(&mut self.fuel) + cost,
            i,
        });
        self.fuse = None;
        Some(self.prog.len() - 1)
    }

    fn settle(&mut self) {
        if let Some(exit) = self.mark.take() {
            self.prog.push(Ins {
                exit,
                fuel: core::mem::take(&mut self.fuel),
                i: I::Nop,
            });
        }
        self.fuse = None;
    }

    fn materialize(&mut self, f: &Frame, n: usize, only: Option<u8>) -> Option<()> {
        let mut settled = false;
        for j in 0..n {
            let t = self.temp(f, j)?;
            let g = self.stk[j];
            if g == t || only.is_some_and(|x| x != g) {
                continue;
            }
            if !settled {
                self.settle();
                settled = true;
            }
            self.prog.push(Ins {
                exit: 0,
                fuel: 0,
                i: I::Mov { a: g, d: t },
            });
            self.stk[j] = t;
        }
        Some(())
    }

    fn push(&mut self, ip: u32, cost: u32, src: Src) -> Option<()> {
        self.mark(ip)?;
        self.fuel += cost;
        let g = self.reg(src)?;
        self.stk.push(g);
        Some(())
    }

    fn binary(
        &mut self,
        f: &Frame,
        ip: u32,
        cost: u32,
        mk: impl Fn(u8, u8, u8) -> I,
    ) -> Option<()> {
        self.mark(ip)?;
        let c = self.stk.pop()?;
        let a = self.stk.pop()?;
        let d = self.temp(f, self.stk.len())?;
        let k = self.emit(mk(a, c, d), cost)?;
        self.stk.push(d);
        self.fuse = Some(k);
        Some(())
    }

    fn unary(&mut self, f: &Frame, ip: u32, cost: u32, mk: impl Fn(u8, u8) -> I) -> Option<()> {
        self.mark(ip)?;
        let a = self.stk.pop()?;
        let d = self.temp(f, self.stk.len())?;
        let k = self.emit(mk(a, d), cost)?;
        self.stk.push(d);
        self.fuse = Some(k);
        Some(())
    }

    fn written(&mut self, x: u8, slot: u32) {
        if !self.writes.iter().any(|w| w.0 == x) {
            self.writes.push((x, slot));
        }
    }

    fn step(&mut self, f: &Frame, ip: u32, cost: u32, slot: u32, delta: i32) -> Option<()> {
        let x = self.reg(Src::Slot(slot))?;
        self.materialize(f, self.stk.len(), Some(x))?;
        self.mark(ip)?;
        self.emit(I::Step { a: x, d: x, delta }, cost)?;
        self.written(x, slot);
        Some(())
    }

    fn put(&mut self, f: &Frame, ip: u32, cost: u32, slot: u32, keep: bool) -> Option<()> {
        let x = self.reg(Src::Slot(slot))?;
        let src = *self.stk.last()?;
        let n = self.stk.len() - 1;
        self.materialize(f, n, Some(x))?;
        let last = self.prog.len().checked_sub(1);
        let fused = !keep
            && self.mark.is_none()
            && self.fuse.is_some()
            && self.fuse == last
            && self.temp(f, n)? == src;
        if fused {
            let ins = self.prog.last_mut()?;
            let d = ins.i.dst()?;
            if *d != src {
                return None;
            }
            *d = x;
            ins.fuel += cost;
            self.stk.pop();
            self.fuse = None;
        } else {
            self.mark(ip)?;
            if !keep {
                self.stk.pop();
            }
            self.emit(I::Mov { a: src, d: x }, cost)?;
        }
        self.written(x, slot);
        Some(())
    }

    fn jump(
        &mut self,
        f: &Frame,
        ip: u32,
        cost: u32,
        target: u32,
        cond: Option<bool>,
    ) -> Option<bool> {
        let n = self.stk.len().checked_sub(usize::from(cond.is_some()))?;
        let dest = if target == f.head {
            Dest::Head
        } else if target >= f.stop {
            Dest::Out
        } else if target > ip {
            Dest::In
        } else {
            return None;
        };
        match dest {
            Dest::Head if n != 0 => return None,
            Dest::In => {
                self.materialize(f, n, None)?;
                match self.labels.iter().find(|l| l.0 == target) {
                    Some(l) if l.1 != n as u32 => return None,
                    Some(_) => {}
                    None => self.labels.push((target, n as u32, None)),
                }
            }
            _ => {}
        }
        let k = match cond {
            Some(when) => {
                let top = *self.stk.last()?;
                let last = self.prog.len().checked_sub(1);
                let fusable = self.mark.is_none() && self.fuse.is_some() && self.fuse == last;
                match self.prog.last().map(|i| i.i) {
                    Some(I::Compare { a, c, d, mask }) if fusable && d == top => {
                        self.stk.pop();
                        let k = self.prog.len() - 1;
                        let mask = if when { mask } else { !mask & 0b1111 };
                        self.prog[k].i = I::Branch { a, c, mask, to: 0 };
                        self.prog[k].fuel += cost;
                        self.fuse = None;
                        match k.checked_sub(1).map(|s| (s, self.prog[s].i)) {
                            Some((
                                s,
                                I::Step {
                                    a: sa,
                                    d: sd,
                                    delta,
                                },
                            )) if s >= self.barrier && sa == sd && sd == a => {
                                let br = self.prog.pop()?;
                                let bound = match self.init[usize::from(c)] {
                                    Src::Value(v) => v.as_int(),
                                    Src::Slot(_) => None,
                                };
                                let step = &mut self.prog[s];
                                step.i = match bound {
                                    Some(n) => I::Latch {
                                        delta,
                                        mask,
                                        n,
                                        to: 0,
                                        x: a,
                                    },
                                    None => I::StepBranch {
                                        c,
                                        delta,
                                        exit: br.exit,
                                        fuel: step.fuel,
                                        mask,
                                        to: 0,
                                        x: a,
                                    },
                                };
                                step.fuel += br.fuel;
                                s
                            }
                            _ => k,
                        }
                    }
                    _ => {
                        self.mark(ip)?;
                        self.stk.pop();
                        self.emit(
                            I::JumpIf {
                                a: top,
                                to: 0,
                                when,
                            },
                            cost,
                        )?
                    }
                }
            }
            None => {
                self.mark(ip)?;
                match dest {
                    Dest::Out => {
                        let exit = self.exit_at(target)?;
                        self.emit(I::Leave { exit }, cost)?;
                        return Some(false);
                    }
                    _ => self.emit(I::Jump { to: 0 }, cost)?,
                }
            }
        };
        match dest {
            Dest::Head => {}
            Dest::In => self.fixes.push((k, target)),
            Dest::Out => {
                let exit = self.exit_at(target)?;
                self.outs.push((k, exit));
            }
        }
        Some(cond.is_some())
    }
}

fn translate(b: &[u8], f: &Frame) -> Option<Plan> {
    let mut p = Plan::default();
    p.exit_at(f.head)?;
    let mut reach = true;
    let mut ip = f.head;
    while ip < f.stop {
        if let Some(l) = p.labels.iter().position(|l| l.0 == ip) {
            let depth = p.labels[l].1 as usize;
            if reach {
                p.materialize(f, p.stk.len(), None)?;
                if p.stk.len() != depth {
                    return None;
                }
            } else {
                p.stk.clear();
                for j in 0..depth {
                    let t = p.temp(f, j)?;
                    p.stk.push(t);
                }
            }
            p.settle();
            p.labels[l].2 = Some(u16::try_from(p.prog.len()).ok()?);
            p.barrier = p.prog.len();
            reach = true;
        } else if !reach {
            return None;
        }
        let w = word(b, ip);
        let op = w as u8;
        let info = &OPS[op as usize];
        let cost = info.cost.fuel() as u32;
        let next = ip + info.size as u32;
        let imm16 = (w >> 8) as u16 as u32;
        let imm32 = (w >> 8) as u32;
        let loc = |i: u32| Src::Slot(f.lp + i);
        let arg = |i: u32| Src::Slot(f.bp + 2 + i);
        let int = |i: i32| Src::Value(Value::int(i));
        match op {
            fast::GET_LOC => p.push(ip, cost, loc(imm16))?,
            fast::GET_LOC0..=fast::GET_LOC3 => {
                p.push(ip, cost, loc((op - fast::GET_LOC0) as u32))?
            }
            fast::GET_LOC8 | fast::ADD_LOC8 => p.push(ip, cost, loc(imm16 & 0xFF))?,
            fast::GET_ARG => p.push(ip, cost, arg(imm16))?,
            fast::GET_ARG0..=fast::GET_ARG3 => {
                p.push(ip, cost, arg((op - fast::GET_ARG0) as u32))?
            }
            fast::GET_ARG8 => p.push(ip, cost, arg(imm16 & 0xFF))?,
            fast::PUT_LOC => p.put(f, ip, cost, f.lp + imm16, false)?,
            fast::PUT_LOC0..=fast::PUT_LOC3 => {
                p.put(f, ip, cost, f.lp + (op - fast::PUT_LOC0) as u32, false)?
            }
            fast::PUT_LOC8 => p.put(f, ip, cost, f.lp + (imm16 & 0xFF), false)?,
            fast::SET_LOC => p.put(f, ip, cost, f.lp + imm16, true)?,
            fast::SET_LOC0..=fast::SET_LOC3 => {
                p.put(f, ip, cost, f.lp + (op - fast::SET_LOC0) as u32, true)?
            }
            fast::SET_LOC8 => p.put(f, ip, cost, f.lp + (imm16 & 0xFF), true)?,
            fast::PUSH_I8 => p.push(ip, cost, int(imm32 as u8 as i8 as i32))?,
            fast::PUSH_I32 => p.push(ip, cost, int(imm32 as i32))?,
            fast::PUSH0..=fast::PUSH3 => p.push(ip, cost, int((op - fast::PUSH0) as i32))?,
            fast::PUSH_CONST | fast::PUSH_CONST8 => {
                if f.consts == 0 {
                    return None;
                }
                let i = if op == fast::PUSH_CONST {
                    imm32
                } else {
                    imm16 & 0xFF
                };
                p.push(
                    ip,
                    cost,
                    Src::Value(ld(b, ValueArray::VALUES.at(f.consts, i))),
                )?
            }
            fast::UNDEFINED => p.push(ip, cost, Src::Value(Value::UNDEFINED))?,
            fast::NULL => p.push(ip, cost, Src::Value(Value::NULL))?,
            fast::PUSH_TRUE => p.push(ip, cost, Src::Value(Value::TRUE))?,
            fast::PUSH_FALSE => p.push(ip, cost, Src::Value(Value::FALSE))?,
            fast::DROP => {
                p.mark(ip)?;
                p.fuel += cost;
                p.stk.pop()?;
            }
            fast::DUP => {
                p.mark(ip)?;
                p.fuel += cost;
                let g = *p.stk.last()?;
                p.stk.push(g);
            }
            fast::INC | fast::DEC => {
                let delta = if op == fast::INC { 1 } else { -1 };
                p.unary(f, ip, cost, |a, d| I::Step { a, d, delta })?
            }
            fast::INC_LOC | fast::INC_LT | fast::INC_LT_LOC => {
                p.step(f, ip, cost, f.lp + imm16, 1)?
            }
            fast::DEC_LOC => p.step(f, ip, cost, f.lp + imm16, -1)?,
            fast::ADD => p.binary(f, ip, cost, |a, c, d| I::Add { a, c, d })?,
            fast::SUB => p.binary(f, ip, cost, |a, c, d| I::Sub { a, c, d })?,
            fast::MUL => p.binary(f, ip, cost, |a, c, d| I::Mul { a, c, d })?,
            fast::DIV => p.binary(f, ip, cost, |a, c, d| I::Div { a, c, d })?,
            fast::MOD => p.binary(f, ip, cost, |a, c, d| I::Mod { a, c, d })?,
            fast::BAND => p.binary(f, ip, cost, |a, c, d| I::BAnd { a, c, d })?,
            fast::BOR => p.binary(f, ip, cost, |a, c, d| I::BOr { a, c, d })?,
            fast::BXOR => p.binary(f, ip, cost, |a, c, d| I::BXor { a, c, d })?,
            fast::SHL => p.binary(f, ip, cost, |a, c, d| I::Shl { a, c, d })?,
            fast::SAR => p.binary(f, ip, cost, |a, c, d| I::Sar { a, c, d })?,
            fast::LT | fast::LE | fast::GT | fast::GE => {
                let cmp = match op {
                    fast::LT => Cmp::Lt,
                    fast::LE => Cmp::Le,
                    fast::GT => Cmp::Gt,
                    _ => Cmp::Ge,
                };
                p.binary(f, ip, cost, |a, c, d| I::Compare {
                    a,
                    c,
                    d,
                    mask: cmp.mask(true),
                })?
            }
            fast::STRICT_EQ | fast::STRICT_NE => {
                let ne = op == fast::STRICT_NE;
                p.binary(f, ip, cost, |a, c, d| I::Equal { a, c, d, ne })?
            }
            fast::GET_ELEM => p.binary(f, ip, cost, |a, c, d| I::GetElem { a, c, d })?,
            fast::PUT_ELEM => {
                p.mark(ip)?;
                let v = p.stk.pop()?;
                let c = p.stk.pop()?;
                let a = p.stk.pop()?;
                let k = p.emit(I::PutElem { a, after: 0, c, v }, cost)?;
                p.mark(next)?;
                let after = p.mark?;
                p.prog[k].i = I::PutElem { a, after, c, v };
            }
            fast::NOT => p.unary(f, ip, cost, |a, d| I::Not { a, d })?,
            fast::IS_UNDEFINED => p.unary(f, ip, cost, |a, d| I::IsUndefined { a, d })?,
            fast::IS_NULLISH => p.unary(f, ip, cost, |a, d| I::IsNullish { a, d })?,
            fast::JUMP => {
                reach = p.jump(
                    f,
                    ip,
                    cost,
                    (next as i64 + imm32 as i32 as i64) as u32,
                    None,
                )?
            }
            fast::JUMP8 => {
                reach = p.jump(
                    f,
                    ip,
                    cost,
                    (next as i64 + imm16 as u8 as i8 as i64) as u32,
                    None,
                )?
            }
            fast::JUMP_IF_FALSE | fast::JUMP_IF_TRUE => {
                let target = (next as i64 + imm32 as i32 as i64) as u32;
                reach = p.jump(f, ip, cost, target, Some(op == fast::JUMP_IF_TRUE))?
            }
            fast::JUMP_IF_FALSE8 | fast::JUMP_IF_TRUE8 => {
                let target = (next as i64 + imm16 as u8 as i8 as i64) as u32;
                reach = p.jump(f, ip, cost, target, Some(op == fast::JUMP_IF_TRUE8))?
            }
            _ => return None,
        }
        ip = next;
    }
    if reach {
        let exit = p.exit_at(f.stop)?;
        if p.mark.is_none() {
            p.mark = Some(exit);
        }
        p.emit(I::Leave { exit }, 0)?;
    }
    for k in 0..p.outs.len() {
        let (at, exit) = p.outs[k];
        let to = u16::try_from(p.prog.len()).ok()?;
        p.prog.push(Ins {
            exit,
            fuel: 0,
            i: I::Leave { exit },
        });
        *p.prog[at].i.target()? = to;
    }
    for k in 0..p.fixes.len() {
        let (at, ip) = p.fixes[k];
        let to = p.labels.iter().find(|l| l.0 == ip)?.2?;
        *p.prog[at].i.target()? = to;
    }
    u16::try_from(p.prog.len()).ok()?;
    Some(p)
}

impl Exec<'_> {
    #[inline(never)]
    pub(super) fn run_tier1(&mut self) -> bool {
        if self.opts.gc_stress || self.heap.gc_pending() || self.pc >= self.code_len {
            return false;
        }
        let head = self.code + self.pc;
        let vals = ValueArray::VALUES.at(self.stack, 0);
        let b: &mut [u8] = self.heap.arena_mut().hot_bytes();
        let Some((stop, pass)) = body(b, head, self.code + self.code_len) else {
            return false;
        };
        if self.fuel <= pass {
            return false;
        }
        let f = Frame {
            bp: self.bp,
            consts: self.consts,
            head,
            lp: self.lp,
            sp: self.sp,
            stop,
        };
        let Some(plan) = translate(b, &f) else {
            return false;
        };
        let mut r = [Value::UNDEFINED; 256];
        for (k, s) in plan.init.iter().enumerate() {
            r[k] = match *s {
                Src::Slot(x) => ld(b, vals + 8 * x),
                Src::Value(v) => v,
            };
        }
        let prog: Vec<Op> = plan.prog.iter().map(lower).collect();
        let mut budget = self.opts.hot_backedges;
        let mut fuel = self.fuel;
        let mut k = 0usize;
        let (exit, again) = loop {
            let o = prog[k];
            let (a, c, d) = (o.a as usize, o.c as usize, o.d as usize);
            macro_rules! next {
                () => {{
                    fuel -= o.fuel as i64;
                    k += 1;
                }};
            }
            macro_rules! goto {
                () => {{
                    fuel -= o.fuel as i64;
                    if o.to == 0 {
                        if fuel <= pass {
                            break (0, false);
                        }
                        budget -= 1;
                        if budget == 0 {
                            break (0, true);
                        }
                    }
                    k = o.to as usize;
                }};
            }
            macro_rules! arith {
                ($int:expr, $float:expr) => {{
                    let (p, q) = (r[a], r[c]);
                    let int: Option<Value> = match (p.as_int(), q.as_int()) {
                        (Some(x), Some(y)) => ($int)(x, y),
                        _ => None,
                    };
                    let v = match int {
                        Some(v) => v,
                        None => match (p.as_number(), q.as_number()) {
                            (Some(x), Some(y)) => match ($float)(x, y) {
                                Some(v) => v,
                                None => break (o.exit, false),
                            },
                            _ => break (o.exit, false),
                        },
                    };
                    r[d] = v;
                    next!();
                }};
            }
            macro_rules! no_float {
                () => {
                    |_: f64, _: f64| -> Option<Value> { None }
                };
            }
            match o.k {
                K::Add => {
                    let (p, q) = (r[a], r[c]);
                    let v = match (p.as_int(), q.as_int()) {
                        (Some(x), Some(y)) => match x.checked_add(y) {
                            Some(z) => Value::int(z),
                            None => Value::from_raw((f64::from(x) + f64::from(y)).to_bits()),
                        },
                        _ => match (p.as_number(), q.as_number()) {
                            (Some(x), Some(y)) => number(x + y),
                            _ => break (o.exit, false),
                        },
                    };
                    r[d] = v;
                    next!();
                }
                K::Latch => {
                    let Some(i) = r[a]
                        .as_int()
                        .and_then(|i| i.checked_add(i32::from(o.c as i8)))
                    else {
                        break (o.exit, false);
                    };
                    r[a] = Value::int(i);
                    if o.mask >> order_int(i, o.n) & 1 != 0 {
                        goto!()
                    } else {
                        next!()
                    }
                }
                K::StepBranch => {
                    let Some(i) = r[a]
                        .as_int()
                        .and_then(|i| i.checked_add(i32::from(o.d as i8)))
                    else {
                        break (o.exit, false);
                    };
                    r[a] = Value::int(i);
                    let n = r[c];
                    let t = match n.as_int() {
                        Some(n) => order_int(i, n),
                        None => match n.as_number() {
                            Some(n) => order_float(f64::from(i), n),
                            None => {
                                fuel -= o.n as i64;
                                break (o.aux, false);
                            }
                        },
                    };
                    if o.mask >> t & 1 != 0 {
                        goto!()
                    } else {
                        next!()
                    }
                }
                K::Branch => {
                    let Some(t) = order(r[a], r[c]) else {
                        break (o.exit, false);
                    };
                    if o.mask >> t & 1 != 0 {
                        goto!()
                    } else {
                        next!()
                    }
                }
                K::Compare => {
                    let Some(t) = order(r[a], r[c]) else {
                        break (o.exit, false);
                    };
                    r[d] = Value::bool(o.mask >> t & 1 != 0);
                    next!();
                }
                K::GetElem => {
                    let (Some(obj), Some(i)) = (
                        r[a].as_object(),
                        r[c].as_int().and_then(|i| u32::try_from(i).ok()),
                    ) else {
                        break (o.exit, false);
                    };
                    let Some(v) = self.heap.get_element_fast(obj, i) else {
                        break (o.exit, false);
                    };
                    r[d] = v;
                    next!();
                }
                K::PutElem => {
                    let (Some(obj), Some(i)) = (
                        r[a].as_object(),
                        r[c].as_int().and_then(|i| u32::try_from(i).ok()),
                    ) else {
                        break (o.exit, false);
                    };
                    if !matches!(self.heap.set_element_fast(obj, i, r[d]), Ok(true)) {
                        break (o.exit, false);
                    }
                    if self.heap.gc_pending() {
                        fuel -= o.fuel as i64;
                        break (o.aux, false);
                    }
                    next!();
                }
                K::Mov => {
                    r[d] = r[a];
                    next!();
                }
                K::Step => {
                    let Some(i) = r[a]
                        .as_int()
                        .and_then(|i| i.checked_add(i32::from(o.c as i8)))
                    else {
                        break (o.exit, false);
                    };
                    r[d] = Value::int(i);
                    next!();
                }
                K::Sub => arith!(
                    |x: i32, y: i32| x.checked_sub(y).map(Value::int),
                    |x: f64, y: f64| Some(number(x - y))
                ),
                K::Mul => arith!(
                    |x: i32, y: i32| x.checked_mul(y).filter(|&v| v != 0).map(Value::int),
                    |x: f64, y: f64| Some(number(x * y))
                ),
                K::Div => arith!(|_: i32, _: i32| None, |x: f64, y: f64| Some(number(x / y))),
                K::Mod => arith!(
                    |x: i32, y: i32| (x >= 0 && y != 0).then(|| Value::int(x % y)),
                    no_float!()
                ),
                K::BAnd => arith!(|x: i32, y: i32| Some(Value::int(x & y)), no_float!()),
                K::BOr => arith!(|x: i32, y: i32| Some(Value::int(x | y)), no_float!()),
                K::BXor => arith!(|x: i32, y: i32| Some(Value::int(x ^ y)), no_float!()),
                K::Shl => arith!(
                    |x: i32, y: i32| Some(Value::int(x.wrapping_shl(y as u32 & 31))),
                    no_float!()
                ),
                K::Sar => arith!(
                    |x: i32, y: i32| Some(Value::int(x >> (y as u32 & 31))),
                    no_float!()
                ),
                K::Equal => {
                    let (p, q) = (r[a], r[c]);
                    let eq = match (p.as_number(), q.as_number()) {
                        (Some(x), Some(y)) => x == y,
                        _ if p == q => true,
                        _ if (p.as_string().is_some() && q.as_string().is_some())
                            || (p.as_bigint().is_some() && q.as_bigint().is_some()) =>
                        {
                            break (o.exit, false);
                        }
                        _ => false,
                    };
                    r[d] = Value::bool(eq != (o.mask != 0));
                    next!();
                }
                K::Not => {
                    let v = r[a];
                    let truthy = if let Some(x) = v.as_bool() {
                        x
                    } else if let Some(n) = v.as_number() {
                        !(n == 0.0 || n.is_nan())
                    } else if v.is_undefined() || v.is_null() {
                        false
                    } else if v.as_object().is_some() {
                        true
                    } else {
                        break (o.exit, false);
                    };
                    r[d] = Value::bool(!truthy);
                    next!();
                }
                K::IsUndefined => {
                    r[d] = Value::bool(r[a].is_undefined());
                    next!();
                }
                K::IsNullish => {
                    r[d] = Value::bool(r[a].is_nullish());
                    next!();
                }
                K::JumpIf => {
                    let Some(t) = r[a].as_bool() else {
                        break (o.exit, false);
                    };
                    if t == (o.mask != 0) { goto!() } else { next!() }
                }
                K::Jump => goto!(),
                K::Nop => next!(),
                K::Leave => {
                    fuel -= o.fuel as i64;
                    break (o.to, false);
                }
            }
        };
        let b: &mut [u8] = self.heap.arena_mut().hot_bytes();
        let e = plan.exits[exit as usize];
        let list = &plan.stacks[e.at as usize..(e.at + e.len) as usize];
        for (j, &g) in list.iter().enumerate() {
            st(b, vals + 8 * (f.sp + j as u32), r[g as usize]);
        }
        for &(g, x) in &plan.writes {
            st(b, vals + 8 * x, r[g as usize]);
        }
        self.pc = e.ip - self.code;
        self.sp = f.sp + e.len;
        self.fuel = fuel;
        again
    }
}
