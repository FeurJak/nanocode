#include <errno.h>
#include <string.h>
#include <time.h>
#include "../src/time/time_impl.h"

const char __utc[] = "UTC";

const char *__tm_to_tzname(const struct tm *tm)
{
	return tm->tm_zone ? tm->tm_zone : __utc;
}

time_t time(time_t *out)
{
	struct timespec now;
	clock_gettime(CLOCK_REALTIME, &now);
	if (out) *out = now.tv_sec;
	return now.tv_sec;
}

int timespec_get(struct timespec *now, int base)
{
	if (base != TIME_UTC) return 0;
	return clock_gettime(CLOCK_REALTIME, now) ? 0 : base;
}

int nanosleep(const struct timespec *request, struct timespec *remaining)
{
	(void)request;
	(void)remaining;
	errno = ENOSYS;
	return -1;
}

struct tm *localtime_r(const time_t *restrict t, struct tm *restrict tm)
{
	return gmtime_r(t, tm);
}

struct tm *localtime(const time_t *t)
{
	return gmtime(t);
}

time_t mktime(struct tm *tm)
{
	return timegm(tm);
}
