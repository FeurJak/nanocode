#ifndef _SETJMP_H
#define _SETJMP_H
#ifdef __cplusplus
extern "C" {
#endif
typedef unsigned long long jmp_buf[8];
int setjmp(jmp_buf);
_Noreturn void longjmp(jmp_buf, int);
#ifdef __cplusplus
}
#endif
#endif
