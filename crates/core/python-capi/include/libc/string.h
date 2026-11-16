#ifndef _STRING_H
#define _STRING_H
#include <stddef.h>
struct __locale_struct;
#ifdef __cplusplus
extern "C" {
#endif
void *memcpy(void *, const void *, size_t);
void *memmove(void *, const void *, size_t);
void *memset(void *, int, size_t);
int memcmp(const void *, const void *, size_t);
void *memchr(const void *, int, size_t);
size_t strlen(const char *);
size_t strnlen(const char *, size_t);
int strcmp(const char *, const char *);
int strncmp(const char *, const char *, size_t);
int strcoll(const char *, const char *);
size_t strxfrm(char *, const char *, size_t);
char *strcpy(char *, const char *);
char *strncpy(char *, const char *, size_t);
char *strcat(char *, const char *);
char *strncat(char *, const char *, size_t);
char *strchr(const char *, int);
char *strrchr(const char *, int);
char *strstr(const char *, const char *);
char *strpbrk(const char *, const char *);
size_t strspn(const char *, const char *);
size_t strcspn(const char *, const char *);
int strcoll_l(const char *, const char *, struct __locale_struct *);
size_t strxfrm_l(char *, const char *, size_t, struct __locale_struct *);
char *strtok(char *, const char *);
char *strerror(int);
int strerror_r(int, char *, size_t);
char *strdup(const char *);
char *strndup(const char *, size_t);
#ifdef __cplusplus
}
#else
#define memcpy __builtin_memcpy
#define memmove __builtin_memmove
#define memset __builtin_memset
#define memcmp __builtin_memcmp
#define strlen __builtin_strlen
#endif
#endif
