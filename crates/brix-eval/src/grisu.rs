// This file is a Rust port of the Grisu2 implementation in nlohmann::json
// (`include/nlohmann/detail/conversions/to_chars.hpp`), which is distributed
// under the MIT license. The port is distributed under brix's license
// (LGPL-2.1-or-later); the original notice is reproduced below as the MIT
// license requires.
//
// SPDX-FileCopyrightText: 2009 Florian Loitsch <https://florian.loitsch.com/>
// SPDX-FileCopyrightText: 2013 - 2025 Niels Lohmann <https://nlohmann.me>
// SPDX-License-Identifier: MIT
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! The Grisu2 shortest-digits algorithm as nlohmann::json implements it
//! (`detail/conversions/to_chars.hpp`, MIT licensed), which is what Nix's
//! JSON output uses for floats.
//!
//! Grisu2 produces the shortest digit string in about 99.9% of cases; in
//! the rest it emits one digit more than the true shortest representation
//! (`-6553.9000000000005` where a shortest-digits printer says
//! `-6553.900000000001`). Both read back to the same double, but brix
//! must print exactly what Nix prints, so this is a port of the same
//! algorithm rather than a call to Rust's (shortest) formatter.

/// `f * 2^e` with a 64-bit significand.
#[derive(Clone, Copy)]
struct DiyFp {
    f: u64,
    e: i32,
}

impl DiyFp {
    /// `x - y`; requires `x.e == y.e` and `x.f >= y.f`.
    fn sub(x: DiyFp, y: DiyFp) -> DiyFp {
        debug_assert!(x.e == y.e && x.f >= y.f);
        DiyFp {
            f: x.f - y.f,
            e: x.e,
        }
    }

    /// `x * y`, keeping the upper 64 bits of the 128-bit product, rounded
    /// (ties up), as nlohmann does.
    fn mul(x: DiyFp, y: DiyFp) -> DiyFp {
        let u_lo = x.f & 0xFFFF_FFFF;
        let u_hi = x.f >> 32;
        let v_lo = y.f & 0xFFFF_FFFF;
        let v_hi = y.f >> 32;
        let p0 = u_lo * v_lo;
        let p1 = u_lo * v_hi;
        let p2 = u_hi * v_lo;
        let p3 = u_hi * v_hi;
        let p0_hi = p0 >> 32;
        let p1_lo = p1 & 0xFFFF_FFFF;
        let p1_hi = p1 >> 32;
        let p2_lo = p2 & 0xFFFF_FFFF;
        let p2_hi = p2 >> 32;
        let mut q = p0_hi + p1_lo + p2_lo;
        q += 1u64 << (64 - 32 - 1); // round, ties up
        let h = p3 + p2_hi + p1_hi + (q >> 32);
        DiyFp {
            f: h,
            e: x.e + y.e + 64,
        }
    }

    /// Shifts until the top bit of the significand is set.
    fn normalize(mut x: DiyFp) -> DiyFp {
        debug_assert!(x.f != 0);
        while x.f >> 63 == 0 {
            x.f <<= 1;
            x.e -= 1;
        }
        x
    }

    /// Rescales `x` to the given exponent (which must not lose bits).
    fn normalize_to(x: DiyFp, target_exponent: i32) -> DiyFp {
        let delta = x.e - target_exponent;
        debug_assert!(delta >= 0);
        DiyFp {
            f: x.f << delta,
            e: target_exponent,
        }
    }
}

struct Boundaries {
    w: DiyFp,
    minus: DiyFp,
    plus: DiyFp,
}

/// The value and the midpoints to its neighbouring doubles, normalised to
/// a shared exponent.
fn compute_boundaries(value: f64) -> Boundaries {
    debug_assert!(value.is_finite() && value > 0.0);
    const PRECISION: i32 = 53; // includes the hidden bit
    const BIAS: i32 = 1024 - 1 + (PRECISION - 1);
    const MIN_EXP: i32 = 1 - BIAS;
    const HIDDEN_BIT: u64 = 1 << (PRECISION - 1);
    let bits = value.to_bits();
    let e = bits >> (PRECISION - 1);
    let f = bits & (HIDDEN_BIT - 1);
    let is_denormal = e == 0;
    let v = if is_denormal {
        DiyFp { f, e: MIN_EXP }
    } else {
        DiyFp {
            f: f + HIDDEN_BIT,
            e: e as i32 - BIAS,
        }
    };
    // The lower boundary is closer when the significand is a power of two
    // (the previous double is twice as far away).
    let lower_boundary_is_closer = f == 0 && e > 1;
    let m_plus = DiyFp {
        f: 2 * v.f + 1,
        e: v.e - 1,
    };
    let m_minus = if lower_boundary_is_closer {
        DiyFp {
            f: 4 * v.f - 1,
            e: v.e - 2,
        }
    } else {
        DiyFp {
            f: 2 * v.f - 1,
            e: v.e - 1,
        }
    };
    let w_plus = DiyFp::normalize(m_plus);
    let w_minus = DiyFp::normalize_to(m_minus, w_plus.e);
    Boundaries {
        w: DiyFp::normalize(v),
        minus: w_minus,
        plus: w_plus,
    }
}

const ALPHA: i32 = -60;
const GAMMA: i32 = -32;

/// `c = f * 2^e ~= 10^k`
struct CachedPower {
    f: u64,
    e: i32,
    k: i32,
}

const CACHED_POWERS_MIN_DEC_EXP: i32 = -300;
const CACHED_POWERS_DEC_STEP: i32 = 8;

#[rustfmt::skip]
const CACHED_POWERS: [CachedPower; 79] = [
    CachedPower { f: 0xAB70FE17C79AC6CA, e: -1060, k: -300 },
    CachedPower { f: 0xFF77B1FCBEBCDC4F, e: -1034, k: -292 },
    CachedPower { f: 0xBE5691EF416BD60C, e: -1007, k: -284 },
    CachedPower { f: 0x8DD01FAD907FFC3C, e:  -980, k: -276 },
    CachedPower { f: 0xD3515C2831559A83, e:  -954, k: -268 },
    CachedPower { f: 0x9D71AC8FADA6C9B5, e:  -927, k: -260 },
    CachedPower { f: 0xEA9C227723EE8BCB, e:  -901, k: -252 },
    CachedPower { f: 0xAECC49914078536D, e:  -874, k: -244 },
    CachedPower { f: 0x823C12795DB6CE57, e:  -847, k: -236 },
    CachedPower { f: 0xC21094364DFB5637, e:  -821, k: -228 },
    CachedPower { f: 0x9096EA6F3848984F, e:  -794, k: -220 },
    CachedPower { f: 0xD77485CB25823AC7, e:  -768, k: -212 },
    CachedPower { f: 0xA086CFCD97BF97F4, e:  -741, k: -204 },
    CachedPower { f: 0xEF340A98172AACE5, e:  -715, k: -196 },
    CachedPower { f: 0xB23867FB2A35B28E, e:  -688, k: -188 },
    CachedPower { f: 0x84C8D4DFD2C63F3B, e:  -661, k: -180 },
    CachedPower { f: 0xC5DD44271AD3CDBA, e:  -635, k: -172 },
    CachedPower { f: 0x936B9FCEBB25C996, e:  -608, k: -164 },
    CachedPower { f: 0xDBAC6C247D62A584, e:  -582, k: -156 },
    CachedPower { f: 0xA3AB66580D5FDAF6, e:  -555, k: -148 },
    CachedPower { f: 0xF3E2F893DEC3F126, e:  -529, k: -140 },
    CachedPower { f: 0xB5B5ADA8AAFF80B8, e:  -502, k: -132 },
    CachedPower { f: 0x87625F056C7C4A8B, e:  -475, k: -124 },
    CachedPower { f: 0xC9BCFF6034C13053, e:  -449, k: -116 },
    CachedPower { f: 0x964E858C91BA2655, e:  -422, k: -108 },
    CachedPower { f: 0xDFF9772470297EBD, e:  -396, k: -100 },
    CachedPower { f: 0xA6DFBD9FB8E5B88F, e:  -369, k:  -92 },
    CachedPower { f: 0xF8A95FCF88747D94, e:  -343, k:  -84 },
    CachedPower { f: 0xB94470938FA89BCF, e:  -316, k:  -76 },
    CachedPower { f: 0x8A08F0F8BF0F156B, e:  -289, k:  -68 },
    CachedPower { f: 0xCDB02555653131B6, e:  -263, k:  -60 },
    CachedPower { f: 0x993FE2C6D07B7FAC, e:  -236, k:  -52 },
    CachedPower { f: 0xE45C10C42A2B3B06, e:  -210, k:  -44 },
    CachedPower { f: 0xAA242499697392D3, e:  -183, k:  -36 },
    CachedPower { f: 0xFD87B5F28300CA0E, e:  -157, k:  -28 },
    CachedPower { f: 0xBCE5086492111AEB, e:  -130, k:  -20 },
    CachedPower { f: 0x8CBCCC096F5088CC, e:  -103, k:  -12 },
    CachedPower { f: 0xD1B71758E219652C, e:   -77, k:   -4 },
    CachedPower { f: 0x9C40000000000000, e:   -50, k:    4 },
    CachedPower { f: 0xE8D4A51000000000, e:   -24, k:   12 },
    CachedPower { f: 0xAD78EBC5AC620000, e:     3, k:   20 },
    CachedPower { f: 0x813F3978F8940984, e:    30, k:   28 },
    CachedPower { f: 0xC097CE7BC90715B3, e:    56, k:   36 },
    CachedPower { f: 0x8F7E32CE7BEA5C70, e:    83, k:   44 },
    CachedPower { f: 0xD5D238A4ABE98068, e:   109, k:   52 },
    CachedPower { f: 0x9F4F2726179A2245, e:   136, k:   60 },
    CachedPower { f: 0xED63A231D4C4FB27, e:   162, k:   68 },
    CachedPower { f: 0xB0DE65388CC8ADA8, e:   189, k:   76 },
    CachedPower { f: 0x83C7088E1AAB65DB, e:   216, k:   84 },
    CachedPower { f: 0xC45D1DF942711D9A, e:   242, k:   92 },
    CachedPower { f: 0x924D692CA61BE758, e:   269, k:  100 },
    CachedPower { f: 0xDA01EE641A708DEA, e:   295, k:  108 },
    CachedPower { f: 0xA26DA3999AEF774A, e:   322, k:  116 },
    CachedPower { f: 0xF209787BB47D6B85, e:   348, k:  124 },
    CachedPower { f: 0xB454E4A179DD1877, e:   375, k:  132 },
    CachedPower { f: 0x865B86925B9BC5C2, e:   402, k:  140 },
    CachedPower { f: 0xC83553C5C8965D3D, e:   428, k:  148 },
    CachedPower { f: 0x952AB45CFA97A0B3, e:   455, k:  156 },
    CachedPower { f: 0xDE469FBD99A05FE3, e:   481, k:  164 },
    CachedPower { f: 0xA59BC234DB398C25, e:   508, k:  172 },
    CachedPower { f: 0xF6C69A72A3989F5C, e:   534, k:  180 },
    CachedPower { f: 0xB7DCBF5354E9BECE, e:   561, k:  188 },
    CachedPower { f: 0x88FCF317F22241E2, e:   588, k:  196 },
    CachedPower { f: 0xCC20CE9BD35C78A5, e:   614, k:  204 },
    CachedPower { f: 0x98165AF37B2153DF, e:   641, k:  212 },
    CachedPower { f: 0xE2A0B5DC971F303A, e:   667, k:  220 },
    CachedPower { f: 0xA8D9D1535CE3B396, e:   694, k:  228 },
    CachedPower { f: 0xFB9B7CD9A4A7443C, e:   720, k:  236 },
    CachedPower { f: 0xBB764C4CA7A44410, e:   747, k:  244 },
    CachedPower { f: 0x8BAB8EEFB6409C1A, e:   774, k:  252 },
    CachedPower { f: 0xD01FEF10A657842C, e:   800, k:  260 },
    CachedPower { f: 0x9B10A4E5E9913129, e:   827, k:  268 },
    CachedPower { f: 0xE7109BFBA19C0C9D, e:   853, k:  276 },
    CachedPower { f: 0xAC2820D9623BF429, e:   880, k:  284 },
    CachedPower { f: 0x80444B5E7AA7CF85, e:   907, k:  292 },
    CachedPower { f: 0xBF21E44003ACDD2D, e:   933, k:  300 },
    CachedPower { f: 0x8E679C2F5E44FF8F, e:   960, k:  308 },
    CachedPower { f: 0xD433179D9C8CB841, e:   986, k:  316 },
    CachedPower { f: 0x9E19DB92B4E31BA9, e:  1013, k:  324 },
];

/// A cached power of ten `c` such that `alpha <= e + c.e + 64 <= gamma`.
fn cached_power_for_binary_exponent(e: i32) -> &'static CachedPower {
    debug_assert!((-1500..=1500).contains(&e));
    let f = ALPHA - e - 1;
    let k = ((f * 78913) / (1 << 18)) + i32::from(f > 0);
    let index =
        (-CACHED_POWERS_MIN_DEC_EXP + k + (CACHED_POWERS_DEC_STEP - 1)) / CACHED_POWERS_DEC_STEP;
    let cached = &CACHED_POWERS[index as usize];
    debug_assert!(ALPHA <= cached.e + e + 64 && GAMMA >= cached.e + e + 64);
    cached
}

/// For `n != 0`: `k` such that `10^(k-1) <= n < 10^k`, and `10^(k-1)`.
fn find_largest_pow10(n: u32) -> (i32, u32) {
    const POW10: [u32; 10] = [
        1,
        10,
        100,
        1_000,
        10_000,
        100_000,
        1_000_000,
        10_000_000,
        100_000_000,
        1_000_000_000,
    ];
    for (i, p) in POW10.iter().enumerate().rev() {
        if n >= *p {
            return (i as i32 + 1, *p);
        }
    }
    (1, 1)
}

/// Rounds the last digit down while that gets closer to `w`.
fn grisu2_round(buf: &mut [u8], dist: u64, delta: u64, mut rest: u64, ten_k: u64) {
    debug_assert!(dist <= delta && rest <= delta && ten_k > 0);
    let last = buf.len() - 1;
    while rest < dist
        && delta - rest >= ten_k
        && (rest + ten_k < dist || dist - rest > rest + ten_k - dist)
    {
        debug_assert!(buf[last] != b'0');
        buf[last] -= 1;
        rest += ten_k;
    }
}

/// Generates `digits * 10^decimal_exponent` with `M- <= V <= M+`; the three
/// inputs share an exponent in `[alpha, gamma]`.
fn grisu2_digit_gen(
    buf: &mut Vec<u8>,
    decimal_exponent: &mut i32,
    m_minus: DiyFp,
    w: DiyFp,
    m_plus: DiyFp,
) {
    debug_assert!(m_plus.e >= ALPHA && m_plus.e <= GAMMA);
    let mut delta = DiyFp::sub(m_plus, m_minus).f;
    let mut dist = DiyFp::sub(m_plus, w).f;
    let one = DiyFp {
        f: 1u64 << -m_plus.e,
        e: m_plus.e,
    };
    let shift = (-one.e) as u32;
    let mut p1 = (m_plus.f >> shift) as u32;
    let mut p2 = m_plus.f & (one.f - 1);
    debug_assert!(p1 > 0);

    // Integral part: the digits of p1.
    let (k, mut pow10) = find_largest_pow10(p1);
    let mut n = k;
    while n > 0 {
        let d = p1 / pow10;
        let r = p1 % pow10;
        buf.push(b'0' + d as u8);
        p1 = r;
        n -= 1;
        let rest = (u64::from(p1) << shift) + p2;
        if rest <= delta {
            *decimal_exponent += n;
            let ten_n = u64::from(pow10) << shift;
            grisu2_round(buf, dist, delta, rest, ten_n);
            return;
        }
        pow10 /= 10;
    }

    // Fractional part: p2 is exact as long as it does not overflow.
    debug_assert!(p2 > delta);
    let mut m = 0;
    loop {
        p2 *= 10;
        let d = p2 >> shift;
        let r = p2 & (one.f - 1);
        buf.push(b'0' + d as u8);
        p2 = r;
        m += 1;
        delta *= 10;
        dist *= 10;
        if p2 <= delta {
            break;
        }
    }
    *decimal_exponent -= m;
    grisu2_round(buf, dist, delta, p2, one.f);
}

/// The Grisu2 digits of a finite positive double and the exponent such that
/// `value = digits * 10^exponent`.
pub fn grisu2(value: f64) -> (Vec<u8>, i32) {
    debug_assert!(value.is_finite() && value > 0.0);
    let b = compute_boundaries(value);
    let cached = cached_power_for_binary_exponent(b.plus.e);
    let c_minus_k = DiyFp {
        f: cached.f,
        e: cached.e,
    };
    let w = DiyFp::mul(b.w, c_minus_k);
    let w_minus = DiyFp::mul(b.minus, c_minus_k);
    let w_plus = DiyFp::mul(b.plus, c_minus_k);
    let m_minus = DiyFp {
        f: w_minus.f + 1,
        e: w_minus.e,
    };
    let m_plus = DiyFp {
        f: w_plus.f - 1,
        e: w_plus.e,
    };
    let mut decimal_exponent = -cached.k;
    let mut buf = Vec::with_capacity(17);
    grisu2_digit_gen(&mut buf, &mut decimal_exponent, m_minus, w, m_plus);
    (buf, decimal_exponent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digits(v: f64) -> (String, i32) {
        let (d, e) = grisu2(v);
        (String::from_utf8(d).unwrap(), e)
    }

    #[test]
    fn shortest_digits_in_the_common_case() {
        assert_eq!(digits(0.1), ("1".into(), -1));
        assert_eq!(digits(1.0), ("1".into(), 0));
        assert_eq!(digits(123.456), ("123456".into(), -3));
        assert_eq!(digits(1e21), ("1".into(), 21));
        assert_eq!(digits(5e-324), ("5".into(), -324));
        assert_eq!(digits(f64::MAX), ("17976931348623157".into(), 292));
    }

    #[test]
    fn grisu2_is_not_always_shortest() {
        // The case the fuzzer found: nlohmann prints -6553.9000000000005.
        let v = 65536.0 * -0.1 - 0.3;
        assert_eq!(digits(-v), ("65539000000000005".into(), -13));
    }
}
