#ifndef _ATOMIC_H
#define _ATOMIC_H
#include <stdint.h>
static inline int a_cas(volatile int *p, int t, int s)
{
	int old = *p;
	if (old == t) *p = s;
	return old;
}
static inline int a_swap(volatile int *p, int v)
{
	int old = *p;
	*p = v;
	return old;
}
static inline int a_ctz_32(uint32_t x)
{
	return __builtin_ctz(x);
}
static inline int a_ctz_64(uint64_t x)
{
	return __builtin_ctzll(x);
}
static inline int a_ctz_l(unsigned long x)
{
	return __builtin_ctzl(x);
}
static inline int a_clz_32(uint32_t x)
{
	return __builtin_clz(x);
}
static inline int a_clz_64(uint64_t x)
{
	return __builtin_clzll(x);
}
#define a_crash() __builtin_trap()
#endif
