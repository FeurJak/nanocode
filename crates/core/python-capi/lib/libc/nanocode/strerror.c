#include <errno.h>
#include <string.h>

static const char *const messages[] = {
#define E(number, text) [number] = text,
#include "strerror.h"
#undef E
};

char *strerror(int number)
{
	if (number < 0 || (size_t)number >= sizeof messages / sizeof *messages || !messages[number])
		number = 0;
	return (char *)messages[number];
}
