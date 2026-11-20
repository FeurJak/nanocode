#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include "stdio_impl.h"
#include "pyio.h"

int __lockfile(FILE *f)
{
	(void)f;
	return 0;
}

void __unlockfile(FILE *f)
{
	(void)f;
}

void __unlist_locked_file(FILE *f)
{
	(void)f;
}

static size_t send(FILE *f, const unsigned char *data, size_t length)
{
	size_t done = 0;
	while (done < length) {
		ptrdiff_t count = __nanocode_write(f->cookie, f->fd, data + done, length - done);
		if (count <= 0) break;
		done += count;
	}
	return done;
}

static size_t write_file(FILE *f, const unsigned char *data, size_t length)
{
	size_t buffered = f->wpos - f->wbase;
	if (buffered && send(f, f->wbase, buffered) < buffered) {
		f->wpos = f->wbase = f->wend = 0;
		f->flags |= F_ERR;
		return 0;
	}
	size_t done = send(f, data, length);
	if (done < length) {
		f->wpos = f->wbase = f->wend = 0;
		f->flags |= F_ERR;
		return done;
	}
	f->wend = f->buf + f->buf_size;
	f->wpos = f->wbase = f->buf;
	return length;
}

static size_t read_file(FILE *f, unsigned char *data, size_t length)
{
	size_t direct = length - !!f->buf_size;
	ptrdiff_t count;
	if (direct || !f->buf_size) {
		count = __nanocode_read(f->cookie, f->fd, data, direct);
		if (count <= 0) {
			f->flags |= count ? F_ERR : F_EOF;
			return 0;
		}
		return count;
	}
	count = __nanocode_read(f->cookie, f->fd, f->buf, f->buf_size);
	if (count <= 0) {
		f->flags |= count ? F_ERR : F_EOF;
		return 0;
	}
	f->rpos = f->buf;
	f->rend = f->buf + count;
	data[length - 1] = *f->rpos++;
	return length;
}

static off_t seek_file(FILE *f, off_t offset, int whence)
{
	return __nanocode_seek(f->cookie, offset, whence);
}

static int close_file(FILE *f)
{
	if (!f->cookie) return 0;
	int result = __nanocode_close(f->cookie);
	f->cookie = 0;
	return result;
}

#undef stdin
#undef stdout
#undef stderr

static unsigned char stdin_buffer[BUFSIZ + UNGET];
static unsigned char stdout_buffer[UNGET];
static unsigned char stderr_buffer[UNGET];

hidden FILE __stdin_FILE = {
	.buf = stdin_buffer + UNGET,
	.buf_size = sizeof stdin_buffer - UNGET,
	.fd = 0,
	.flags = F_PERM | F_NOWR,
	.read = read_file,
	.seek = seek_file,
	.close = close_file,
	.lock = -1,
};

hidden FILE __stdout_FILE = {
	.buf = stdout_buffer + UNGET,
	.buf_size = 0,
	.fd = 1,
	.flags = F_PERM | F_NORD,
	.lbf = EOF,
	.write = write_file,
	.seek = seek_file,
	.close = close_file,
	.lock = -1,
};

hidden FILE __stderr_FILE = {
	.buf = stderr_buffer + UNGET,
	.buf_size = 0,
	.fd = 2,
	.flags = F_PERM | F_NORD,
	.lbf = EOF,
	.write = write_file,
	.seek = seek_file,
	.close = close_file,
	.lock = -1,
};

FILE *const stdin = &__stdin_FILE;
FILE *const stdout = &__stdout_FILE;
FILE *const stderr = &__stderr_FILE;
FILE *volatile __stdin_used = &__stdin_FILE;
FILE *volatile __stdout_used = &__stdout_FILE;
FILE *volatile __stderr_used = &__stderr_FILE;

static int mode_flags(const char *mode)
{
	if (strchr(mode, '+')) return 0;
	return *mode == 'r' ? F_NOWR : F_NORD;
}

static void *open_python(const char *path, int fd, const char *mode)
{
	char python[8];
	size_t length = 0;
	if (!strchr("rwa", *mode)) {
		errno = EINVAL;
		return 0;
	}
	python[length++] = *mode;
	if (strchr(mode, 'x')) python[length++] = 'x';
	if (strchr(mode, '+')) python[length++] = '+';
	python[length++] = 'b';
	python[length] = 0;
	return __nanocode_open(path, fd, python);
}

static FILE *wrap(void *file, const char *mode)
{
	FILE *f = malloc(sizeof *f + UNGET + BUFSIZ);
	if (!f) {
		__nanocode_close(file);
		errno = ENOMEM;
		return 0;
	}
	memset(f, 0, sizeof *f);
	f->flags = mode_flags(mode);
	if (*mode == 'a') f->flags |= F_APP;
	f->cookie = file;
	f->fd = __nanocode_fileno(file);
	f->buf = (unsigned char *)f + sizeof *f + UNGET;
	f->buf_size = BUFSIZ;
	f->lbf = EOF;
	f->read = read_file;
	f->write = write_file;
	f->seek = seek_file;
	f->close = close_file;
	f->lock = -1;
	return __ofl_add(f);
}

FILE *fopen(const char *restrict path, const char *restrict mode)
{
	void *file = open_python(path, -1, mode);
	return file ? wrap(file, mode) : 0;
}

FILE *fdopen(int fd, const char *mode)
{
	void *file = open_python(0, fd, mode);
	return file ? wrap(file, mode) : 0;
}

FILE *freopen(const char *restrict path, const char *restrict mode, FILE *restrict f)
{
	if (!path) {
		fclose(f);
		errno = ENOTSUP;
		return 0;
	}
	fflush(f);
	void *file = open_python(path, -1, mode);
	if (!file) {
		fclose(f);
		return 0;
	}
	close_file(f);
	f->cookie = file;
	f->fd = __nanocode_fileno(file);
	f->flags = (f->flags & F_PERM) | mode_flags(mode) | (*mode == 'a' ? F_APP : 0);
	f->read = read_file;
	f->write = write_file;
	f->seek = seek_file;
	f->close = close_file;
	f->rpos = f->rend = 0;
	f->wpos = f->wbase = f->wend = 0;
	f->lbf = EOF;
	f->mode = 0;
	return f;
}
