#ifndef _UNISTD_H
#define _UNISTD_H
#include <sys/types.h>
#define _POSIX_TIMERS 200809L
#define STDIN_FILENO 0
#define STDOUT_FILENO 1
#define STDERR_FILENO 2
#define F_OK 0
#define X_OK 1
#define W_OK 2
#define R_OK 4
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
