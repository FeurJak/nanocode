#ifndef _PTHREAD_IMPL_H
#define _PTHREAD_IMPL_H
#include "atomic.h"
struct __pthread {
	int tid;
};
static inline struct __pthread *__pthread_self(void)
{
	static struct __pthread self = { 1 };
	return &self;
}
static inline void __wake(volatile void *address, int count, int private)
{
	(void)address;
	(void)count;
	(void)private;
}
#endif
