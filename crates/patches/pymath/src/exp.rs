//! exp, correctly rounded for normal results. CPython takes math.exp from
//! the platform libm, and glibc's rounds correctly in practice, while musl's
//! and djs_libm's are only within 1 ulp: math.exp(1) came out one ulp high.
//! The reduction and series below run in double-double arithmetic, about
//! 100 bits.

#[inline]
fn two_sum(a: f64, b: f64) -> (f64, f64) {
    let s = a + b;
    let bb = s - a;
    (s, (a - (s - bb)) + (b - bb))
}

#[inline]
fn split(a: f64) -> (f64, f64) {
    let t = 134_217_729.0 * a; // 2^27 + 1
    let hi = t - (t - a);
    (hi, a - hi)
}

#[inline]
fn two_prod(a: f64, b: f64) -> (f64, f64) {
    let p = a * b;
    let (ah, al) = split(a);
    let (bh, bl) = split(b);
    (p, ((ah * bh - p) + ah * bl + al * bh) + al * bl)
}

#[inline]
fn dd_add(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    let (s, e) = two_sum(a.0, b.0);
    let (t, f) = two_sum(a.1, b.1);
    let (s, e) = two_sum(s, e + t);
    two_sum(s, e + f)
}

#[inline]
fn dd_mul(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    let (p, e) = two_prod(a.0, b.0);
    two_sum(p, e + (a.0 * b.1 + a.1 * b.0))
}

#[inline]
fn dd_div_f64(a: (f64, f64), b: f64) -> (f64, f64) {
    let q1 = a.0 / b;
    let (p, e) = two_prod(q1, b);
    two_sum(q1, (((a.0 - p) - e) + a.1) / b)
}

pub(crate) fn exp(x: f64, edge: fn(f64) -> f64) -> f64 {
    // Outside this range the result overflows, is subnormal or is NaN: the
    // platform routine (edge) handles those.
    if !(-708.0..=709.0).contains(&x) {
        return edge(x);
    }
    // ln 2 split in three: HI has 32 significant bits, so k * HI is exact.
    const LN2_HI: f64 = f64::from_bits(0x3fe6_2e42_fee0_0000);
    const LN2_MID: f64 = f64::from_bits(0x3dea_39ef_3579_3c76);
    const LN2_LO: f64 = f64::from_bits(0x3a8c_c01f_97b5_7a08);
    let k = (x * core::f64::consts::LOG2_E).round();
    let (p, e) = two_prod(-k, LN2_MID);
    let r = dd_add((x - k * LN2_HI, 0.0), (p, e - k * LN2_LO));
    // Taylor series by Horner's rule: |r| <= 0.35, so 27 terms reach 2^-120.
    let mut s = (1.0, 0.0);
    for n in (1..=27).rev() {
        s = dd_add((1.0, 0.0), dd_div_f64(dd_mul(r, s), n as f64));
    }
    // Scaling by 2^k is exact for a normal result.
    (s.0 + s.1) * f64::from_bits(((k as i64 + 1023) as u64) << 52)
}
