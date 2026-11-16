#ifndef _TIME_H
#define _TIME_H
#include <sys/types.h>
typedef long long clock_t;
#define CLOCKS_PER_SEC 1000000
#define TIME_UTC 1
struct tm {
    int tm_sec;
    int tm_min;
    int tm_hour;
    int tm_mday;
    int tm_mon;
    int tm_year;
    int tm_wday;
    int tm_yday;
    int tm_isdst;
    long tm_gmtoff;
    const char *tm_zone;
};
struct timespec {
    time_t tv_sec;
    long tv_nsec;
};
#ifdef __cplusplus
extern "C" {
#endif
clock_t clock(void);
time_t time(time_t *);
double difftime(time_t, time_t);
time_t mktime(struct tm *);
struct tm *gmtime(const time_t *);
struct tm *localtime(const time_t *);
struct tm *gmtime_r(const time_t *, struct tm *);
struct tm *localtime_r(const time_t *, struct tm *);
char *asctime(const struct tm *);
char *ctime(const time_t *);
size_t strftime(char *, size_t, const char *, const struct tm *);
int timespec_get(struct timespec *, int);
#ifdef __cplusplus
}
#endif
#endif
