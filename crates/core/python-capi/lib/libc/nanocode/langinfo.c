#include <langinfo.h>
#include "locale_impl.h"

static const char c_time[] =
	"Sun\0" "Mon\0" "Tue\0" "Wed\0" "Thu\0" "Fri\0" "Sat\0"
	"Sunday\0" "Monday\0" "Tuesday\0" "Wednesday\0"
	"Thursday\0" "Friday\0" "Saturday\0"
	"Jan\0" "Feb\0" "Mar\0" "Apr\0" "May\0" "Jun\0"
	"Jul\0" "Aug\0" "Sep\0" "Oct\0" "Nov\0" "Dec\0"
	"January\0" "February\0" "March\0" "April\0"
	"May\0" "June\0" "July\0" "August\0"
	"September\0" "October\0" "November\0" "December\0"
	"AM\0" "PM\0"
	"%a %b %e %T %Y\0"
	"%m/%d/%y\0"
	"%H:%M:%S\0"
	"%I:%M:%S %p\0"
	"\0"
	"\0"
	"%m/%d/%y\0"
	"0123456789\0"
	"%a %b %e %T %Y\0"
	"%H:%M:%S";

static const char c_messages[] = "^[yY]\0" "^[nN]\0" "yes\0" "no";
static const char c_numeric[] = ".\0" "";

char *__nl_langinfo_l(nl_item item, locale_t locale)
{
	(void)locale;
	int category = item >> 16;
	int index = item & 65535;
	const char *text;
	if (item == CODESET) return "UTF-8";
	switch (category) {
	case 1:
		if (index > 1) return "";
		text = c_numeric;
		break;
	case 2:
		if (index > 0x31) return "";
		text = c_time;
		break;
	case 4:
		if (index > 0) return "";
		return "";
	case 5:
		if (index > 3) return "";
		text = c_messages;
		break;
	default:
		return "";
	}
	for (; index; index--, text++)
		for (; *text; text++);
	return (char *)text;
}

char *nl_langinfo_l(nl_item item, locale_t locale)
{
	return __nl_langinfo_l(item, locale);
}

char *nl_langinfo(nl_item item)
{
	return __nl_langinfo_l(item, 0);
}
