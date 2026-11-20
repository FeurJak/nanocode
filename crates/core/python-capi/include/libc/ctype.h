#ifndef _CTYPE_H
#define _CTYPE_H
struct __locale_struct;
#ifdef __cplusplus
extern "C" {
#endif
int isalnum(int);
int isalpha(int);
int isblank(int);
int isdigit(int);
int isspace(int);
int isupper(int);
int islower(int);
int isxdigit(int);
int iscntrl(int);
int isgraph(int);
int isprint(int);
int ispunct(int);
int tolower_l(int, struct __locale_struct *);
int toupper_l(int, struct __locale_struct *);
int tolower(int);
int toupper(int);
#ifdef __cplusplus
}
#endif
#endif
