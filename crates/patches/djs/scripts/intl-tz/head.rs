use alloc::format;
use alloc::string::String;

const START: i64 = -5_364_662_400;

#[derive(Clone, Copy)]
pub(super) struct Kind {
    pub off: i64,
    pub dst: bool,
    pub tuple: u16,
}

pub(super) enum At {
    Period(Kind, Option<(i64, Option<Kind>)>),
    Rule,
}

fn varint(b: &[u8], i: &mut usize) -> u64 {
    let (mut v, mut s) = (0u64, 0);
    loop {
        let x = b[*i];
        *i += 1;
        v |= u64::from(x & 0x7f) << s;
        if x < 0x80 {
            return v;
        }
        s += 7;
    }
}

fn unzz(v: u64) -> i64 {
    (v >> 1) as i64 ^ -((v & 1) as i64)
}

pub(super) fn at(zone: usize, t: i64) -> At {
    let b = &TZ_HISTORY[..];
    let mut i = TZ_INDEX[zone] as usize;
    let n = varint(b, &mut i) as usize;
    let mut types = [Kind {
        off: 0,
        dst: false,
        tuple: 0,
    }; TZ_MAX_TYPES];
    for k in types.iter_mut().take(n) {
        let o = varint(b, &mut i);
        let off = if o & 1 == 0 {
            unzz(o >> 1) * 60
        } else {
            unzz(o >> 1)
        };
        let x = varint(b, &mut i);
        *k = Kind {
            off,
            dst: x & 1 == 1,
            tuple: (x >> 1) as u16,
        };
    }
    let c = varint(b, &mut i) as usize;
    let np = varint(b, &mut i) as usize;
    let (mut ds, mut tys) = ([0i64; 8], [0usize; 8]);
    let (mut prev, mut cur) = (START, 0usize);
    for p in 0..np {
        let v = varint(b, &mut i);
        let r = p % c;
        let d = unzz(v >> 3) * [3600, 60, 1, 1][((v >> 1) & 3) as usize]
            + if p >= c { ds[r] } else { 0 };
        let ty = if v & 1 == 1 {
            tys[r]
        } else {
            varint(b, &mut i) as usize
        };
        ds[r] = d;
        tys[r] = ty;
        let tp = prev + d;
        if tp > t {
            return At::Period(
                types[cur],
                Some((tp, if ty < n { Some(types[ty]) } else { None })),
            );
        }
        if ty == n {
            return At::Rule;
        }
        prev = tp;
        cur = ty;
    }
    At::Period(types[cur], None)
}

pub(super) fn tuple(n: u16) -> [u16; 6] {
    let b = &TZ_TUPLES[..];
    let mut i = usize::from(TZ_TUPLE_AT[usize::from(n) - 1]);
    let h = b[i];
    i += 1;
    let mut next = || varint(b, &mut i) as u16;
    let mut r = [0u16; 6];
    r[0] = if h & 1 != 0 { 0 } else { next() };
    r[1] = if h & 2 != 0 { 1 } else { next() };
    r[2] = next();
    r[3] = if h & 4 != 0 { r[2] } else { next() };
    if h & 8 != 0 {
        r[4] = r[2];
        r[5] = r[3];
    } else {
        r[4] = next();
        r[5] = next();
    }
    r
}

pub(super) fn gmt(off: i64, long: bool) -> String {
    let sign = if off < 0 { '-' } else { '+' };
    let (h, m, s) = (off.abs() / 3600, off.abs() / 60 % 60, off.abs() % 60);
    let secs = if s != 0 {
        format!(":{s:02}")
    } else {
        String::new()
    };
    if long {
        format!("GMT{sign}{h:02}:{m:02}{secs}")
    } else if m != 0 || s != 0 {
        format!("GMT{sign}{h}:{m:02}{secs}")
    } else {
        format!("GMT{sign}{h}")
    }
}

pub(super) fn name(id: u16, off: i64) -> String {
    match id {
        0 | 1 => gmt(off, id == 1),
        _ => {
            let k = usize::from(id) - 2;
            let start = if k == 0 {
                0
            } else {
                usize::from(TZ_STR_ENDS[k - 1])
            };
            let mut out = String::new();
            for ch in TZ_STR_TEXT[start..usize::from(TZ_STR_ENDS[k])].chars() {
                match TZ_WORDS.get((ch as usize).wrapping_sub(1)) {
                    Some(w) if (ch as u32) < 32 => out.push_str(w),
                    _ => out.push(ch),
                }
            }
            out
        }
    }
}
