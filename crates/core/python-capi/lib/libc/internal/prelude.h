#ifndef _NANOCODE_PRELUDE_H
#define _NANOCODE_PRELUDE_H
#include "features.h"
#include <stddef.h>
#include <sys/types.h>
#define NL_ARGMAX 9
#define __tm_gmtoff tm_gmtoff
#define __tm_zone tm_zone
#include <locale.h>
#include <nl_types.h>
struct tm;
hidden char *__nl_langinfo_l(nl_item, locale_t);
hidden struct tm *__gmtime_r(const long long *restrict, struct tm *restrict);
hidden char *__asctime_r(const struct tm *restrict, char *restrict);
hidden size_t __strftime_l(char *restrict, size_t, const char *restrict, const struct tm *restrict, locale_t);
hidden void *__memrchr(const void *, int, size_t);
hidden char *__stpcpy(char *, const char *);
hidden char *__stpncpy(char *, const char *, size_t);
hidden char *__strchrnul(const char *, int);
hidden void __qsort_r(void *, size_t, size_t, int (*)(const void *, const void *, void *), void *);
#endif
