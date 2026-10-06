//! Deterministic `f32` math shared by the compute service and the host tool that
//! pins the inference baseline.
//!
//! There is no libm in a freestanding Space OS program, and a baseline is only
//! meaningful if both sides compute it the same way: these functions are the single
//! implementation both use, so a mismatch between guest and host means a real
//! difference in the pipeline, not a difference in `exp`.

/// Newton-Raphson square root.
pub fn sqrt_f32(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    let mut g = x;
    let mut i = 0;
    while i < 24 {
        g = 0.5 * (g + x / g);
        i += 1;
    }
    g
}

/// `2^k` for integer `k`, by repeated multiplication (exact for the range used here).
pub fn pow2i(k: i32) -> f32 {
    let mut out = 1.0f32;
    let mut i = 0;
    while i < k.abs() {
        out *= if k > 0 { 2.0 } else { 0.5 };
        i += 1;
    }
    out
}

/// `exp(x)` via range reduction to `2^k * exp(r)` and a Taylor series for `exp(r)`.
pub fn exp_f32(x: f32) -> f32 {
    if x < -60.0 {
        return 0.0;
    }
    if x > 60.0 {
        return f32::MAX;
    }
    let k = (x / core::f32::consts::LN_2 + if x >= 0.0 { 0.5 } else { -0.5 }) as i32;
    let r = x - k as f32 * core::f32::consts::LN_2;
    let mut term = 1.0f32;
    let mut sum = 1.0f32;
    let mut i = 1;
    while i < 14 {
        term *= r / i as f32;
        sum += term;
        i += 1;
    }
    sum * pow2i(k)
}

/// `ln(x)` from the exponent bits plus the `atanh` series on the mantissa.
pub fn ln_f32(x: f32) -> f32 {
    if x <= 0.0 {
        return f32::MIN;
    }
    let bits = x.to_bits();
    let exponent = ((bits >> 23) & 0xFF) as i32 - 127;
    let mantissa = f32::from_bits((bits & 0x807F_FFFF) | 0x3F80_0000);
    let z = (mantissa - 1.0) / (mantissa + 1.0);
    let z2 = z * z;
    let mut term = z;
    let mut sum = 0.0f32;
    let mut i = 0;
    while i < 12 {
        sum += term / (2 * i + 1) as f32;
        term *= z2;
        i += 1;
    }
    2.0 * sum + exponent as f32 * core::f32::consts::LN_2
}

pub fn powf_f32(base: f32, exp: f32) -> f32 {
    exp_f32(exp * ln_f32(base))
}

fn wrap_pi(x: f32) -> f32 {
    let mut v = x;
    while v > core::f32::consts::PI {
        v -= core::f32::consts::TAU;
    }
    while v < -core::f32::consts::PI {
        v += core::f32::consts::TAU;
    }
    v
}

pub fn sin_f32(x: f32) -> f32 {
    let x = wrap_pi(x);
    let x2 = x * x;
    let mut term = x;
    let mut sum = x;
    let mut i = 1;
    while i < 10 {
        term *= -x2 / ((2 * i) as f32 * (2 * i + 1) as f32);
        sum += term;
        i += 1;
    }
    sum
}

pub fn cos_f32(x: f32) -> f32 {
    let x = wrap_pi(x);
    let x2 = x * x;
    let mut term = 1.0f32;
    let mut sum = 1.0f32;
    let mut i = 1;
    while i < 10 {
        term *= -x2 / ((2 * i - 1) as f32 * (2 * i) as f32);
        sum += term;
        i += 1;
    }
    sum
}

/// `x * sigmoid(x)`, the SiLU activation.
pub fn silu_f32(x: f32) -> f32 {
    x / (1.0 + exp_f32(-x))
}
