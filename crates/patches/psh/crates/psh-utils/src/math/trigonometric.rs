const SINE: [f64; 6] = [
    -0.16666666666666632,
    0.00833333333332249,
    -0.0001984126982985795,
    2.7557313707070068e-06,
    -2.5050760253406863e-08,
    1.58969099521155e-10,
];
const COSINE: [f64; 6] = [
    0.0416666666666666,
    -0.001388888888887411,
    2.480158728947673e-05,
    -2.7557314351390663e-07,
    2.087572321298175e-09,
    -1.1359647557788195e-11,
];

fn sine_kernel(x: f64, tail: f64, has_tail: bool) -> f64 {
    let z = x * x;
    let w = z * z;
    let r = SINE[1] + z * (SINE[2] + z * SINE[3]) + z * w * (SINE[4] + z * SINE[5]);
    let v = z * x;
    if has_tail {
        x - ((z * (0.5 * tail - v * r) - tail) - v * SINE[0])
    } else {
        x + v * (SINE[0] + z * r)
    }
}

fn cosine_kernel(x: f64, tail: f64) -> f64 {
    let z = x * x;
    let w = z * z;
    let r = z * (COSINE[0] + z * (COSINE[1] + z * COSINE[2]))
        + w * w * (COSINE[3] + z * (COSINE[4] + z * COSINE[5]));
    let half_z = 0.5 * z;
    let w = 1.0 - half_z;
    w + (((1.0 - w) - half_z) + (z * r - x * tail))
}

pub(super) fn sin_pi(x: f64) -> f64 {
    let half = x * 0.5;
    let x = 2.0 * (half - half.floor());
    let n = (x * 4.0) as i32;
    let n = (n + 1) / 2;
    let x = (x - f64::from(n) * 0.5) * std::f64::consts::PI;
    match n {
        1 => cosine_kernel(x, 0.0),
        2 => sine_kernel(-x, 0.0, false),
        3 => -cosine_kernel(x, 0.0),
        _ => sine_kernel(x, 0.0, false),
    }
}
