#ifndef _UNISTD_H
#define _UNISTD_H
#include <sys/types.h>
#define SEEK_SET 0
#define SEEK_CUR 1
#define SEEK_END 2
#ifdef __cplusplus
extern "C" {
#endif
off_t lseek(int, off_t, int);
ssize_t read(int, void *, size_t);
ssize_t write(int, const void *, size_t);
int close(int);
int isatty(int);
int unlink(const char *);
int access(const char *, int);
pid_t getpid(void);
#ifdef __cplusplus
}
#endif
#endif
