#ifndef _INTTYPES_H
#define _INTTYPES_H
#include <stdint.h>
#define PRId32 "d"
#define PRIu32 "u"
#define PRIx32 "x"
#define PRId64 "lld"
#define PRIu64 "llu"
#define PRIx64 "llx"
#ifdef __cplusplus
extern "C" {
#endif
intmax_t strtoimax(const char *, char **, int);
uintmax_t strtoumax(const char *, char **, int);
#ifdef __cplusplus
}
#endif
#endif
