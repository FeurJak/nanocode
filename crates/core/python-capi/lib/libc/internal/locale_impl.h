#ifndef _LOCALE_IMPL_H
#define _LOCALE_IMPL_H
#include <stdlib.h>
#include "libc.h"
#define C_LOCALE ((struct __locale_struct *)0)
#define UTF8_LOCALE ((struct __locale_struct *)0)
#define CURRENT_LOCALE ((struct __locale_struct *)0)
#define CURRENT_UTF8 1
#undef MB_CUR_MAX
#define MB_CUR_MAX 4
#define LCTRANS(msg, lc, loc) (msg)
#define LCTRANS_CUR(msg) (msg)
#endif
