#ifndef _FENV_H
#define _FENV_H
#define FE_INEXACT 0
#define FE_DIVBYZERO 0
#define FE_UNDERFLOW 0
#define FE_OVERFLOW 0
#define FE_INVALID 0
#define FE_ALL_EXCEPT 0
#define FE_TONEAREST 0
typedef unsigned long fexcept_t;
typedef struct { unsigned long __cw; } fenv_t;
static inline int feclearexcept(int excepts) { (void)excepts; return 0; }
static inline int fetestexcept(int excepts) { (void)excepts; return 0; }
static inline int feraiseexcept(int excepts) { (void)excepts; return 0; }
static inline int fegetround(void) { return FE_TONEAREST; }
#endif
