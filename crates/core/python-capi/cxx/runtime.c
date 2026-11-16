#include <errno.h>
#include <locale.h>
#include <stdlib.h>
#include <string.h>
#include <ctype.h>
#include <time.h>

__asm__(".globl __cpp_exception\n.tagtype __cpp_exception i32\n__cpp_exception:\n");

void *__dso_handle = &__dso_handle;

int __cxa_atexit(void (*destructor)(void *), void *argument, void *handle) {
    (void)destructor;
    (void)argument;
    (void)handle;
    return 0;
}

struct __locale_struct {
    int mask;
};

static struct __locale_struct c_locale = {LC_ALL_MASK};
static locale_t current = LC_GLOBAL_LOCALE;
static char c_name[] = "C";
static char empty[] = "";
static char point[] = ".";
static struct lconv c_conventions = {
    point, empty, empty, empty, empty, empty, empty, empty, empty, empty,
    127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127,
};

static int is_c(const char *name) {
    return name[0] == 0 || strcmp(name, "C") == 0 || strcmp(name, "POSIX") == 0 || strcmp(name, "C.UTF-8") == 0;
}

char *setlocale(int category, const char *name) {
    if (category < LC_CTYPE || category > LC_ALL) {
        return NULL;
    }
    if (name == NULL || is_c(name)) {
        return c_name;
    }
    return NULL;
}

struct lconv *localeconv(void) {
    return &c_conventions;
}

locale_t newlocale(int mask, const char *name, locale_t base) {
    (void)mask;
    (void)base;
    if (name == NULL) {
        errno = EINVAL;
        return NULL;
    }
    if (!is_c(name)) {
        errno = ENOENT;
        return NULL;
    }
    return &c_locale;
}

locale_t duplocale(locale_t locale) {
    (void)locale;
    return &c_locale;
}

void freelocale(locale_t locale) {
    (void)locale;
}

locale_t uselocale(locale_t locale) {
    locale_t previous = current;
    if (locale != NULL) {
        current = locale;
    }
    return previous;
}

float strtof_l(const char *text, char **end, locale_t locale) {
    (void)locale;
    return strtof(text, end);
}

double strtod_l(const char *text, char **end, locale_t locale) {
    (void)locale;
    return strtod(text, end);
}

long double strtold_l(const char *text, char **end, locale_t locale) {
    (void)locale;
    return strtold(text, end);
}

long long strtoll_l(const char *text, char **end, int base, locale_t locale) {
    (void)locale;
    return strtoll(text, end, base);
}

unsigned long long strtoull_l(const char *text, char **end, int base, locale_t locale) {
    (void)locale;
    return strtoull(text, end, base);
}

int strcoll_l(const char *left, const char *right, locale_t locale) {
    (void)locale;
    return strcmp(left, right);
}

size_t strxfrm_l(char *destination, const char *source, size_t size, locale_t locale) {
    (void)locale;
    size_t length = strlen(source);
    if (length < size) {
        memcpy(destination, source, length + 1);
    }
    return length;
}

int tolower_l(int c, locale_t locale) {
    (void)locale;
    return tolower(c);
}

int toupper_l(int c, locale_t locale) {
    (void)locale;
    return toupper(c);
}

size_t strftime_l(char *destination, size_t size, const char *format, const struct tm *time, locale_t locale) {
    (void)locale;
    return strftime(destination, size, format, time);
}
