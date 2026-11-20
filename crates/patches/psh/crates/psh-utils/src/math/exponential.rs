use std::f64::consts::LOG2_10;

use super::exponential_table::{EXP2_POINTS, EXP2_VALUE_AND_ERROR_BITS};
use super::power::pow;
use super::rounding::modf;
use super::{
    EXPONENT_BIAS, HIGH_MAGNITUDE_MASK, NON_FINITE_HIGH_WORD, WORD_BITS, biased_exponent,
    power_of_two, power_of_two_high_word,
};
use djs_libm::scalbn;

const OVERFLOW_SCALE: f64 = power_of_two(1023);
const EXP2_ROUNDING_BITS: u64 = 0x4338_0000_0000_0000;
const EXP2: [u64; 5] = [
    0x3fe6_2e42_fefa_39ef,
    0x3fce_bfbd_ff82_c575,
    0x3fac_6b08_d704_a0a6,
    0x3f83_b2ab_88f7_0400,
    0x3f55_d880_0387_5c74,
];
const EXP2_LARGE_HIGH_WORD: u32 = 0x408f_f000;
const EXP2_OVERFLOW_HIGH_WORD: u32 = power_of_two_high_word(10);
const EXP2_UNDERFLOW_LIMIT: f64 = -1075.0;
const EXP2_TINY_HIGH_WORD: u32 = power_of_two_high_word(-54);
const EXP10_TABLE_OFFSET: isize = 15;
const EXP10_TABLE_EXPONENT_LIMIT: u32 = 4;
const POWERS_OF_TEN: [f64; 31] = [
    1e-15,
    1e-14,
    1e-13,
    1e-12,
    1e-11,
    1e-10,
    1e-09,
    1e-08,
    1e-07,
    1e-06,
    1e-05,
    0.0001,
    0.001,
    0.01,
    0.1,
    1.0,
    10.0,
    100.0,
    1000.0,
    10000.0,
    100000.0,
    1000000.0,
    10000000.0,
    100000000.0,
    1000000000.0,
    10000000000.0,
    100000000000.0,
    1000000000000.0,
    10000000000000.0,
    100000000000000.0,
    1000000000000000.0,
];

pub fn exp2(x: f64) -> f64 {
    let points = EXP2_POINTS as u32;
    let redux = f64::from_bits(EXP2_ROUNDING_BITS) / f64::from(points);
    let bits = x.to_bits();
    let negative = bits >> 63 != 0;
    let magnitude = (bits >> WORD_BITS) as u32 & HIGH_MAGNITUDE_MASK;
    if magnitude >= EXP2_LARGE_HIGH_WORD {
        if magnitude >= EXP2_OVERFLOW_HIGH_WORD && !negative {
            return x * OVERFLOW_SCALE;
        }
        if magnitude >= NON_FINITE_HIGH_WORD {
            return -1.0 / x;
        }
        if negative && x <= EXP2_UNDERFLOW_LIMIT {
            return 0.0;
        }
    } else if magnitude < EXP2_TINY_HIGH_WORD {
        return 1.0 + x;
    }
    let rounded = (x + redux).to_bits();
    let index = (rounded as u32).wrapping_add(points / 2);
    let scale = (index / points * points) as i32 / points as i32;
    let index = (index % points) as usize;
    let mut z = x - (f64::from_bits(rounded) - redux);
    let (value_bits, error_bits) = EXP2_VALUE_AND_ERROR_BITS[index];
    let t = f64::from_bits(value_bits);
    z -= f64::from_bits(error_bits);
    let p = EXP2.map(f64::from_bits);
    let r = t + t * z * (p[0] + z * (p[1] + z * (p[2] + z * (p[3] + z * p[4]))));
    scalbn(r, scale)
}

pub fn exp10(x: f64) -> f64 {
    let (fraction, integral) = modf(x);
    if biased_exponent(integral) < EXPONENT_BIAS as u32 + EXP10_TABLE_EXPONENT_LIMIT {
        let power = POWERS_OF_TEN[(integral as isize + EXP10_TABLE_OFFSET) as usize];
        if fraction == 0.0 {
            return power;
        }
        return exp2(LOG2_10 * fraction) * power;
    }
    pow(10.0, x)
}
