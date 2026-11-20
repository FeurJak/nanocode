use super::{
    EXPONENT_BIAS, HIGH_SIGNIFICAND_BITS, HIGH_SIGNIFICAND_MASK, NON_FINITE_HIGH_WORD,
    ONE_HIGH_WORD, SIGNIFICAND_BITS, SMALLEST_NORMAL_HIGH_WORD, from_words, high_word, low_word,
    power_of_two, power_of_two_high_word, with_high_word, with_low_word,
};
use djs_libm::scalbn;

const BASE: [f64; 2] = [1.0, 1.5];
const LOG2_BASE_HIGH: [f64; 2] = [0.0, 0.5849624872207642];
const LOG2_BASE_LOW: [f64; 2] = [0.0, 1.350039202129749e-08];
const HUGE: f64 = 1e300;
const TINY: f64 = 1e-300;
const LOG_SERIES: [f64; 6] = [
    0.5999999999999946,
    0.4285714285785502,
    0.33333332981837743,
    0.272728123808534,
    0.23066074577556175,
    0.20697501780033842,
];
const EXP_SERIES: [f64; 5] = [
    0.16666666666666602,
    -0.0027777777777015593,
    6.613756321437934e-05,
    -1.6533902205465252e-06,
    4.1381367970572385e-08,
];
const LN2_HIGH: f64 = 0.6931471824645996;
const LN2_LOW: f64 = -1.904654299957768e-09;
const OVERFLOW_THRESHOLD: f64 = 8.008566259537294e-17;
const TWO_OVER_THREE_LN2: f64 = 0.9617966939259756;
const TWO_OVER_THREE_LN2_HIGH: f64 = 0.9617967009544373;
const TWO_OVER_THREE_LN2_LOW: f64 = -7.028461650952758e-09;
const INVERSE_LN2_HIGH: f64 = 1.4426950216293335;
const INVERSE_LN2_LOW: f64 = 1.9259629911266175e-08;
const ONE_THIRD: f64 = 0.3333333333333333;
const SUBNORMAL_SCALE_EXPONENT: i32 = 53;
const MAGNITUDE_HIGH_MASK: i32 = 0x7fff_ffff;
const INFINITY_HIGH: i32 = NON_FINITE_HIGH_WORD as i32;
const ONE_HIGH: i32 = ONE_HIGH_WORD as i32;
const BELOW_ONE_HIGH: i32 = ONE_HIGH - 1;
const TWO_HIGH: i32 = power_of_two_high_word(1) as i32;
const HALF_HIGH: i32 = power_of_two_high_word(-1) as i32;
const EVEN_INTEGER_HIGH: i32 = power_of_two_high_word(53) as i32;
const HUGE_EXPONENT_HIGH: i32 = power_of_two_high_word(31) as i32;
const ALWAYS_OVERFLOW_HIGH: i32 = power_of_two_high_word(64) as i32;
const SQRT_THREE_HALVES_SIGNIFICAND: i32 = 0x3_988e;
const SQRT_THREE_SIGNIFICAND: i32 = 0xb_b67a;
const RESULT_OVERFLOW_HIGH: i32 = power_of_two_high_word(10) as i32;
const RESULT_UNDERFLOW_MAGNITUDE_HIGH: i32 = 0x4090_cc00;
const RESULT_UNDERFLOW_HIGH: u32 = 0xc090_cc00;
const SIGNIFICAND_HIGH_BITS: i32 = HIGH_SIGNIFICAND_BITS as i32;
const SIGNIFICAND_HIGH_MASK: i32 = HIGH_SIGNIFICAND_MASK as i32;
const IMPLICIT_HIGH_BIT: i32 = SMALLEST_NORMAL_HIGH_WORD as i32;
const MIDDLE_HIGH_BIAS: u32 = 0x2000_0000;
const MIDDLE_HIGH_ROUND: u32 = 0x0008_0000;
const MIDDLE_HIGH_BASE_SHIFT: u32 = 18;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Parity {
    NotInteger,
    Odd,
    Even,
}

fn integer_parity(y_magnitude: i32, y_low: u32) -> Parity {
    let parity = |j: i32| {
        if j & 1 == 1 {
            Parity::Odd
        } else {
            Parity::Even
        }
    };
    if y_magnitude >= EVEN_INTEGER_HIGH {
        return Parity::Even;
    }
    if y_magnitude < ONE_HIGH {
        return Parity::NotInteger;
    }
    let k = (y_magnitude >> SIGNIFICAND_HIGH_BITS) - EXPONENT_BIAS;
    if k > SIGNIFICAND_HIGH_BITS {
        let j = (y_low >> (SIGNIFICAND_BITS as i32 - k)) as i32;
        if j << (SIGNIFICAND_BITS as i32 - k) == y_low as i32 {
            return parity(j);
        }
    } else if y_low == 0 {
        let j = y_magnitude >> (SIGNIFICAND_HIGH_BITS - k);
        if j << (SIGNIFICAND_HIGH_BITS - k) == y_magnitude {
            return parity(j);
        }
    }
    Parity::NotInteger
}

fn special_y(
    x: f64,
    y: f64,
    x_high: i32,
    x_magnitude: i32,
    x_low: u32,
    y_high: i32,
) -> Option<f64> {
    let y_magnitude = y_high & MAGNITUDE_HIGH_MASK;
    if y_magnitude == INFINITY_HIGH {
        if (x_magnitude - ONE_HIGH) | x_low as i32 == 0 {
            return Some(1.0);
        }
        if x_magnitude >= ONE_HIGH {
            return Some(if y_high >= 0 { y } else { 0.0 });
        }
        return Some(if y_high >= 0 { 0.0 } else { -y });
    }
    if y_magnitude == ONE_HIGH {
        return Some(if y_high >= 0 { x } else { 1.0 / x });
    }
    if y_high == TWO_HIGH {
        return Some(x * x);
    }
    if y_high == HALF_HIGH && x_high >= 0 {
        return Some(x.sqrt());
    }
    None
}

fn special_x(x_high: i32, x_magnitude: i32, y_high: i32, parity: Parity, ax: f64) -> f64 {
    let mut z = ax;
    if y_high < 0 {
        z = 1.0 / z;
    }
    if x_high < 0 {
        if x_magnitude == ONE_HIGH && parity == Parity::NotInteger {
            z = super::quotient(super::difference(z, z), super::difference(z, z));
        } else if parity == Parity::Odd {
            z = -z;
        }
    }
    z
}

fn huge_y(
    s: f64,
    x_magnitude: i32,
    y_magnitude: i32,
    y_high: i32,
    ax: f64,
) -> Result<(f64, f64), f64> {
    if y_magnitude > ALWAYS_OVERFLOW_HIGH {
        if x_magnitude <= BELOW_ONE_HIGH {
            return Err(if y_high < 0 { HUGE * HUGE } else { TINY * TINY });
        }
        if x_magnitude >= ONE_HIGH {
            return Err(if y_high > 0 { HUGE * HUGE } else { TINY * TINY });
        }
    }
    if x_magnitude < BELOW_ONE_HIGH {
        return Err(if y_high < 0 {
            s * HUGE * HUGE
        } else {
            s * TINY * TINY
        });
    }
    if x_magnitude > ONE_HIGH {
        return Err(if y_high > 0 {
            s * HUGE * HUGE
        } else {
            s * TINY * TINY
        });
    }
    let t = ax - 1.0;
    let w = (t * t) * (0.5 - t * (ONE_THIRD - t * 0.25));
    let u = INVERSE_LN2_HIGH * t;
    let v = t * INVERSE_LN2_LOW - w * std::f64::consts::LOG2_E;
    let t1 = with_low_word(u + v, 0);
    Ok((t1, v - (t1 - u)))
}

fn log2_split(ax: f64, x_magnitude: i32) -> (f64, f64) {
    let mut ax = ax;
    let mut magnitude = x_magnitude;
    let mut n = 0;
    if magnitude < IMPLICIT_HIGH_BIT {
        ax *= power_of_two(SUBNORMAL_SCALE_EXPONENT);
        n -= SUBNORMAL_SCALE_EXPONENT;
        magnitude = high_word(ax) as i32;
    }
    n += (magnitude >> SIGNIFICAND_HIGH_BITS) - EXPONENT_BIAS;
    let j = magnitude & SIGNIFICAND_HIGH_MASK;
    let mut normalized = j | ONE_HIGH;
    let k = if j <= SQRT_THREE_HALVES_SIGNIFICAND {
        0
    } else if j < SQRT_THREE_SIGNIFICAND {
        1
    } else {
        n += 1;
        normalized -= IMPLICIT_HIGH_BIT;
        0
    };
    let ax = with_high_word(ax, normalized as u32);
    let u = ax - BASE[k];
    let v = 1.0 / (ax + BASE[k]);
    let ss = u * v;
    let s_high = with_low_word(ss, 0);
    let middle = ((normalized as u32 >> 1) | MIDDLE_HIGH_BIAS)
        + MIDDLE_HIGH_ROUND
        + ((k as u32) << MIDDLE_HIGH_BASE_SHIFT);
    let t_high = from_words(middle, 0);
    let t_low = ax - (t_high - BASE[k]);
    let s_low = v * ((u - s_high * t_high) - s_high * t_low);
    let s2 = ss * ss;
    let l = &LOG_SERIES;
    let mut r =
        s2 * s2 * (l[0] + s2 * (l[1] + s2 * (l[2] + s2 * (l[3] + s2 * (l[4] + s2 * l[5])))));
    r += s_low * (s_high + ss);
    let s2 = s_high * s_high;
    let t_high = with_low_word(3.0 + s2 + r, 0);
    let t_low = r - ((t_high - 3.0) - s2);
    let u = s_high * t_high;
    let v = s_low * t_high + t_low * ss;
    let p_high = with_low_word(u + v, 0);
    let p_low = v - (p_high - u);
    let z_high = TWO_OVER_THREE_LN2_HIGH * p_high;
    let z_low = TWO_OVER_THREE_LN2_LOW * p_high + p_low * TWO_OVER_THREE_LN2 + LOG2_BASE_LOW[k];
    let t = f64::from(n);
    let t1 = with_low_word(((z_high + z_low) + LOG2_BASE_HIGH[k]) + t, 0);
    (t1, z_low - (((t1 - t) - LOG2_BASE_HIGH[k]) - z_high))
}

fn exp2_split(s: f64, p_high: f64, p_low: f64) -> f64 {
    let mut p_high = p_high;
    let z = p_low + p_high;
    let j = high_word(z) as i32;
    let i = low_word(z) as i32;
    if j >= RESULT_OVERFLOW_HIGH {
        if (j - RESULT_OVERFLOW_HIGH) | i != 0 || p_low + OVERFLOW_THRESHOLD > z - p_high {
            return s * HUGE * HUGE;
        }
    } else if j & MAGNITUDE_HIGH_MASK >= RESULT_UNDERFLOW_MAGNITUDE_HIGH
        && (((j as u32) - RESULT_UNDERFLOW_HIGH) | i as u32 != 0 || p_low <= z - p_high)
    {
        return s * TINY * TINY;
    }
    let i = j & MAGNITUDE_HIGH_MASK;
    let mut k = (i >> SIGNIFICAND_HIGH_BITS) - EXPONENT_BIAS;
    let mut n = 0;
    if i > HALF_HIGH {
        n = j + (IMPLICIT_HIGH_BIT >> (k + 1));
        k = ((n & MAGNITUDE_HIGH_MASK) >> SIGNIFICAND_HIGH_BITS) - EXPONENT_BIAS;
        let t = from_words((n & !(SIGNIFICAND_HIGH_MASK >> k)) as u32, 0);
        n = ((n & SIGNIFICAND_HIGH_MASK) | IMPLICIT_HIGH_BIT) >> (SIGNIFICAND_HIGH_BITS - k);
        if j < 0 {
            n = -n;
        }
        p_high -= t;
    }
    let t = with_low_word(p_low + p_high, 0);
    let u = t * LN2_HIGH;
    let v = (p_low - (t - p_high)) * std::f64::consts::LN_2 + t * LN2_LOW;
    let z = u + v;
    let w = v - (z - u);
    let t = z * z;
    let e = &EXP_SERIES;
    let t1 = z - t * (e[0] + t * (e[1] + t * (e[2] + t * (e[3] + t * e[4]))));
    let r = (z * t1) / (t1 - 2.0) - (w + z * w);
    let z = 1.0 - (r - z);
    let j = (high_word(z) as i32).wrapping_add(n << SIGNIFICAND_HIGH_BITS);
    let z = if j >> SIGNIFICAND_HIGH_BITS <= 0 {
        scalbn(z, n)
    } else {
        with_high_word(z, j as u32)
    };
    s * z
}

pub fn pow(x: f64, y: f64) -> f64 {
    let x_high = high_word(x) as i32;
    let x_low = low_word(x);
    let y_high = high_word(y) as i32;
    let y_low = low_word(y);
    let x_magnitude = x_high & MAGNITUDE_HIGH_MASK;
    let y_magnitude = y_high & MAGNITUDE_HIGH_MASK;
    if y_magnitude as u32 | y_low == 0 {
        return 1.0;
    }
    if x_high == ONE_HIGH && x_low == 0 {
        return 1.0;
    }
    if x_magnitude > INFINITY_HIGH
        || (x_magnitude == INFINITY_HIGH && x_low != 0)
        || y_magnitude > INFINITY_HIGH
        || (y_magnitude == INFINITY_HIGH && y_low != 0)
    {
        return x + y;
    }
    let parity = if x_high < 0 {
        integer_parity(y_magnitude, y_low)
    } else {
        Parity::NotInteger
    };
    if y_low == 0
        && let Some(result) = special_y(x, y, x_high, x_magnitude, x_low, y_high)
    {
        return result;
    }
    let ax = x.abs();
    if x_low == 0 && (x_magnitude == INFINITY_HIGH || x_magnitude == 0 || x_magnitude == ONE_HIGH) {
        return special_x(x_high, x_magnitude, y_high, parity, ax);
    }
    let mut s = 1.0;
    if x_high < 0 {
        match parity {
            Parity::NotInteger => {
                return super::quotient(super::difference(x, x), super::difference(x, x));
            }
            Parity::Odd => s = -1.0,
            Parity::Even => {}
        }
    }
    let (t1, t2) = if y_magnitude > HUGE_EXPONENT_HIGH {
        match huge_y(s, x_magnitude, y_magnitude, y_high, ax) {
            Ok(parts) => parts,
            Err(result) => return result,
        }
    } else {
        log2_split(ax, x_magnitude)
    };
    let y1 = with_low_word(y, 0);
    let p_low = (y - y1) * t1 + y * t2;
    let p_high = y1 * t1;
    exp2_split(s, p_high, p_low)
}
