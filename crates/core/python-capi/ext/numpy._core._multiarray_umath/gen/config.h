#ifndef NUMPY_CORE_SRC_COMMON_NPY_CONFIG_H_
#error config.h should never be included directly, include npy_config.h instead
#endif
#define HAVE__THREAD_LOCAL 1
#define HAVE___BUILTIN_ISNAN 1
#define HAVE___BUILTIN_ISINF 1
#define HAVE___BUILTIN_ISFINITE 1
#define HAVE___BUILTIN_BSWAP32 1
#define HAVE___BUILTIN_BSWAP64 1
#define HAVE___BUILTIN_EXPECT 1
#define HAVE___BUILTIN_MUL_OVERFLOW 1
#define HAVE___BUILTIN_ADD_OVERFLOW 1
#define HAVE___BUILTIN_SUB_OVERFLOW 1
#define HAVE___BUILTIN_PREFETCH 1
#if __SIZEOF_LONG_DOUBLE__ == 8
#define HAVE_LDOUBLE_IEEE_DOUBLE_LE 1
#elif __SIZEOF_LONG_DOUBLE__ == 16 && (defined(__x86_64__) || defined(__i386__))
#define HAVE_LDOUBLE_INTEL_EXTENDED_16_BYTES_LE 1
#elif __SIZEOF_LONG_DOUBLE__ == 16 && __LDBL_MANT_DIG__ == 113
#define HAVE_LDOUBLE_IEEE_QUAD_LE 1
#endif
