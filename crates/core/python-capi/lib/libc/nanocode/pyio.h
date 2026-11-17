#ifndef _NANOCODE_PYIO_H
#define _NANOCODE_PYIO_H
#include <stddef.h>
#include <sys/types.h>
#include "features.h"
hidden ptrdiff_t __nanocode_write(void *, int, const unsigned char *, size_t);
hidden ptrdiff_t __nanocode_read(void *, int, unsigned char *, size_t);
hidden off_t __nanocode_seek(void *, off_t, int);
hidden int __nanocode_close(void *);
hidden void *__nanocode_open(const char *, int, const char *);
hidden int __nanocode_fileno(void *);
hidden void __nanocode_flush(void);
#endif
