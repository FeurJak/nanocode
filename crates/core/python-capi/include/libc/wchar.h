#ifndef _WCHAR_H
#define _WCHAR_H
#include <stddef.h>
typedef unsigned int wint_t;
typedef struct { unsigned __opaque1, __opaque2; } mbstate_t;
#define WEOF 0xffffffffU
#ifdef __cplusplus
extern "C" {
#endif
size_t wcslen(const wchar_t *);
#ifdef __cplusplus
}
#endif
#endif
