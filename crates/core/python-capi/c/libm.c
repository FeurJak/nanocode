#if defined(__wasm__)
#include <math.h>
#include <stdint.h>

static uint64_t bits(double x) { union { double d; uint64_t u; } v = { x }; return v.u; }
static double from_bits(uint64_t u) { union { uint64_t u; double d; } v = { u }; return v.d; }
static uint32_t bitsf(float x) { union { float f; uint32_t u; } v = { x }; return v.u; }
static float from_bitsf(uint32_t u) { union { uint32_t u; float f; } v = { u }; return v.f; }

double atanh(double x) {
    double s = signbit(x) ? -1.0 : 1.0, y = fabs(x);
    if (y < 0.5) return s * 0.5 * log1p(2 * y + 2 * y * y / (1 - y));
    return s * 0.5 * log1p(2 * y / (1 - y));
}
float atanhf(float x) { return (float)atanh(x); }

double frexp(double x, int *e) {
    uint64_t u = bits(x);
    int ee = (int)(u >> 52 & 0x7ff);
    if (!ee) {
        if (x) {
            x = frexp(x * 0x1p64, e);
            *e -= 64;
        } else {
            *e = 0;
        }
        return x;
    }
    if (ee == 0x7ff) return x;
    *e = ee - 0x3fe;
    return from_bits((u & 0x800fffffffffffffull) | 0x3fe0000000000000ull);
}
float frexpf(float x, int *e) { double m = frexp(x, e); return (float)m; }

double modf(double x, double *ip) {
    double t = trunc(x);
    *ip = t;
    if (isinf(x)) return copysign(0.0, x);
    return copysign(x - t, x);
}
float modff(float x, float *ip) { double i; double f = modf(x, &i); *ip = (float)i; return (float)f; }

double scalbn(double x, int n) { return ldexp(x, n); }
float scalbnf(float x, int n) { return ldexpf(x, n); }

int ilogb(double x) {
    int e;
    if (x == 0) return -2147483647 - 1;
    if (isnan(x)) return -2147483647 - 1;
    if (isinf(x)) return 2147483647;
    frexp(x, &e);
    return e - 1;
}
int ilogbf(float x) { return ilogb(x); }
double logb(double x) {
    if (isinf(x)) return fabs(x);
    if (isnan(x)) return x;
    if (x == 0) return -1.0 / 0.0;
    return ilogb(x);
}
float logbf(float x) { return (float)logb(x); }

double nearbyint(double x) { return rint(x); }
float nearbyintf(float x) { return rintf(x); }

double nextafter(double x, double y) {
    if (isnan(x) || isnan(y)) return x + y;
    if (x == y) return y;
    uint64_t u = bits(x);
    if (x == 0) return from_bits((bits(y) & 0x8000000000000000ull) | 1);
    if ((x < y) == (x > 0)) u++; else u--;
    return from_bits(u);
}
float nextafterf(float x, float y) {
    if (isnan(x) || isnan(y)) return x + y;
    if (x == y) return y;
    uint32_t u = bitsf(x);
    if (x == 0) return from_bitsf((bitsf(y) & 0x80000000u) | 1);
    if ((x < y) == (x > 0)) u++; else u--;
    return from_bitsf(u);
}

double remainder(double x, double p) {
    double x0 = x;
    if (isnan(x) || isnan(p) || isinf(x) || p == 0) return (x * p) / (x * p);
    if (isinf(p)) return x;
    p = fabs(p);
    if (p <= 0x1p1022) x = fmod(x, p + p);
    x = fabs(x);
    if (p < 0x1p-1021) {
        if (x + x > p) {
            x -= p;
            if (x + x >= p) x -= p;
        }
    } else {
        double half = 0.5 * p;
        if (x > half) {
            x -= p;
            if (x >= half) x -= p;
        }
    }
    return signbit(x0) ? -x : x;
}
float remainderf(float x, float y) { return (float)remainder(x, y); }

double remquo(double x, double y, int *q) {
    double r = remainder(x, y);
    double n = (x - r) / y;
    *q = (int)((long long)rint(fmod(n, 2147483648.0)) & 0x7fffffff) * ((n < 0) ? -1 : 1);
    return r;
}
float remquof(float x, float y, int *q) { return (float)remquo(x, y, q); }

double lgamma(double x) { int s; return lgamma_r(x, &s); }
float lgammaf(float x) { int s; return lgammaf_r(x, &s); }

long lround(double x) { return (long)round(x); }
long lroundf(float x) { return (long)roundf(x); }
long long llround(double x) { return (long long)round(x); }
long long llroundf(float x) { return (long long)roundf(x); }
long lrint(double x) { return (long)rint(x); }
long lrintf(float x) { return (long)rintf(x); }
long long llrint(double x) { return (long long)rint(x); }
long long llrintf(float x) { return (long long)rintf(x); }

#define L1(f) long double f##l(long double x) { return f((double)x); }
#define L2(f) long double f##l(long double x, long double y) { return f((double)x, (double)y); }
L1(acos) L1(asin) L1(atan) L1(cos) L1(sin) L1(tan) L1(acosh) L1(asinh) L1(atanh) L1(cosh) L1(sinh) L1(tanh)
L1(exp) L1(exp2) L1(expm1) L1(log) L1(log10) L1(log1p) L1(log2) L1(logb) L1(cbrt) L1(erf) L1(erfc)
L1(tgamma) L1(lgamma) L1(nearbyint)
L2(atan2) L2(pow) L2(remainder) L2(nextafter) L2(hypot)
long double sqrtf128(long double);
long double sqrtl(long double x) { return sqrtf128(x); }
long double fabsf128(long double);
long double fabsl(long double x) { return fabsf128(x); }
long double ceilf128(long double);
long double ceill(long double x) { return ceilf128(x); }
long double floorf128(long double);
long double floorl(long double x) { return floorf128(x); }
long double truncf128(long double);
long double truncl(long double x) { return truncf128(x); }
long double rintf128(long double);
long double rintl(long double x) { return rintf128(x); }
long double roundf128(long double);
long double roundl(long double x) { return roundf128(x); }
long double copysignf128(long double, long double);
long double copysignl(long double x, long double y) { return copysignf128(x, y); }
long double fmaxf128(long double, long double);
long double fmaxl(long double x, long double y) { return fmaxf128(x, y); }
long double fminf128(long double, long double);
long double fminl(long double x, long double y) { return fminf128(x, y); }
long double fmodf128(long double, long double);
long double fmodl(long double x, long double y) { return fmodf128(x, y); }
long double fdimf128(long double, long double);
long double fdiml(long double x, long double y) { return fdimf128(x, y); }
long double fmaf128(long double, long double, long double);
long double fmal(long double x, long double y, long double z) { return fmaf128(x, y, z); }
long double frexpl(long double x, int *e) { return frexp((double)x, e); }
long double ldexpl(long double x, int n) { return ldexp((double)x, n); }
long double scalbnl(long double x, int n) { return ldexp((double)x, n); }
long double modfl(long double x, long double *ip) { double i; double f = modf((double)x, &i); *ip = i; return f; }
int ilogbl(long double x) { return ilogb((double)x); }
long lroundl(long double x) { return lround((double)x); }
long long llroundl(long double x) { return llround((double)x); }
long lrintl(long double x) { return lrint((double)x); }
long long llrintl(long double x) { return llrint((double)x); }
long double remquol(long double x, long double y, int *q) { return remquo((double)x, (double)y, q); }
long double lgammal_r(long double x, int *s) { return lgamma_r((double)x, s); }
#endif
