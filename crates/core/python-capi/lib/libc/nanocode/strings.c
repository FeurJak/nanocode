#include <ctype.h>
#include <string.h>
#include <strings.h>

int isblank(int c)
{
	return c == ' ' || c == '\t';
}

int strcasecmp(const char *left, const char *right)
{
	const unsigned char *l = (const unsigned char *)left, *r = (const unsigned char *)right;
	for (; *l && *r && (*l == *r || tolower(*l) == tolower(*r)); l++, r++);
	return tolower(*l) - tolower(*r);
}

int strncasecmp(const char *left, const char *right, size_t length)
{
	const unsigned char *l = (const unsigned char *)left, *r = (const unsigned char *)right;
	if (!length--) return 0;
	for (; *l && *r && length && (*l == *r || tolower(*l) == tolower(*r)); l++, r++, length--);
	return tolower(*l) - tolower(*r);
}

int strcoll(const char *left, const char *right)
{
	return strcmp(left, right);
}

size_t strxfrm(char *restrict destination, const char *restrict source, size_t length)
{
	size_t needed = strlen(source);
	if (needed < length) memcpy(destination, source, needed + 1);
	return needed;
}
