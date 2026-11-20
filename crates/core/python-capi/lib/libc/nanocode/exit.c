#include <stdlib.h>
#include "stdio_impl.h"
#include "pyio.h"

_Noreturn void exit(int code)
{
	(void)code;
	__stdio_exit();
	__nanocode_flush();
	__builtin_trap();
}

_Noreturn void _Exit(int code)
{
	(void)code;
	__builtin_trap();
}

int atexit(void (*function)(void))
{
	(void)function;
	return -1;
}

char *getenv(const char *name)
{
	(void)name;
	return 0;
}
