#ifndef _TIME_H
#define _TIME_H
#include <sys/types.h>
typedef long long clock_t;
#define CLOCKS_PER_SEC 1000000
#define TIME_UTC 1
typedef int clockid_t;
#define CLOCK_REALTIME 0
#define CLOCK_MONOTONIC 1
#define TIME_MONOTONIC 2
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
struct __locale_struct;
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
size_t strftime_l(char *, size_t, const char *, const struct tm *, struct __locale_struct *);
int timespec_get(struct timespec *, int);
int clock_gettime(clockid_t, struct timespec *);
int clock_getres(clockid_t, struct timespec *);
int nanosleep(const struct timespec *, struct timespec *);
time_t timegm(struct tm *);
char *asctime_r(const struct tm *, char *);
char *ctime_r(const time_t *, char *);
#ifdef __cplusplus
}
#endif
#endif
