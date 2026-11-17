#include <Python.h>
#include <errno.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include "pyio.h"

static const struct {
	const char *name;
	int number;
} codes[] = {
#define E(number, text) { #number, number },
#include "strerror.h"
#undef E
};

static int code(PyObject *error)
{
	int number = EIO;
	if (!error || !PyErr_GivenExceptionMatches(error, PyExc_OSError)) return number;
	PyObject *value = PyObject_GetAttrString(error, "errno");
	PyObject *module = value ? PyImport_ImportModule("errno") : 0;
	PyObject *table = module ? PyObject_GetAttrString(module, "errorcode") : 0;
	PyObject *name = table && value != Py_None ? PyDict_GetItemWithError(table, value) : 0;
	const char *text = name ? PyUnicode_AsUTF8(name) : 0;
	for (size_t index = 0; text && index < sizeof codes / sizeof *codes; index++)
		if (!strcmp(codes[index].name, text)) number = codes[index].number;
	Py_XDECREF(table);
	Py_XDECREF(module);
	Py_XDECREF(value);
	PyErr_Clear();
	return number;
}

static PyObject *saved;

static void enter(void)
{
	saved = PyErr_GetRaisedException();
}

static void leave(void)
{
	PyErr_SetRaisedException(saved);
	saved = 0;
}

static void fail(void)
{
	PyObject *error = PyErr_GetRaisedException();
	errno = code(error);
	Py_XDECREF(error);
}

static PyObject *standard(int fd)
{
	PyObject *stream = PySys_GetObject(fd == 0 ? "stdin" : fd == 1 ? "stdout" : "stderr");
	if (!stream || stream == Py_None) {
		errno = EBADF;
		return 0;
	}
	Py_INCREF(stream);
	return stream;
}

static unsigned char pending[3][4];
static size_t waiting[3];

static size_t incomplete(const unsigned char *data, size_t length)
{
	for (size_t back = 1; back <= 3 && back <= length; back++) {
		unsigned char byte = data[length - back];
		if ((byte & 0xc0) != 0x80) {
			size_t need = byte >= 0xf0 && byte < 0xf8 ? 4 : byte >= 0xe0 ? 3 : byte >= 0xc0 ? 2 : 1;
			return need > back ? back : 0;
		}
	}
	return 0;
}

static ptrdiff_t write_text(int fd, const unsigned char *data, size_t length)
{
	PyObject *stream = standard(fd);
	if (!stream) return -1;
	size_t held = waiting[fd];
	size_t total = held + length;
	unsigned char *joined = held ? PyMem_Malloc(total) : (unsigned char *)data;
	if (!joined) {
		Py_DECREF(stream);
		errno = ENOMEM;
		return -1;
	}
	if (held) {
		memcpy(joined, pending[fd], held);
		memcpy(joined + held, data, length);
	}
	size_t keep = incomplete(joined, total);
	PyObject *text = PyUnicode_DecodeUTF8((const char *)joined, total - keep, "surrogateescape");
	PyObject *result = text ? PyObject_CallMethod(stream, "write", "O", text) : 0;
	ptrdiff_t done = (ptrdiff_t)length;
	if (result) {
		memcpy(pending[fd], joined + total - keep, keep);
		waiting[fd] = keep;
	} else {
		fail();
		done = -1;
	}
	if (held) PyMem_Free(joined);
	Py_XDECREF(result);
	Py_XDECREF(text);
	Py_DECREF(stream);
	return done;
}

static ptrdiff_t count(PyObject *result)
{
	if (!result) {
		fail();
		return -1;
	}
	if (result == Py_None) {
		Py_DECREF(result);
		errno = EAGAIN;
		return -1;
	}
	Py_ssize_t value = PyLong_AsSsize_t(result);
	Py_DECREF(result);
	if (value < 0 && PyErr_Occurred()) {
		fail();
		return -1;
	}
	return value;
}

ptrdiff_t __nanocode_write(void *file, int fd, const unsigned char *data, size_t length)
{
	enter();
	ptrdiff_t done;
	if (!file) {
		done = write_text(fd, data, length);
	} else {
		PyObject *bytes = PyBytes_FromStringAndSize((const char *)data, (Py_ssize_t)length);
		done = bytes ? count(PyObject_CallMethod(file, "write", "O", bytes)) : -1;
		if (!bytes) fail();
		Py_XDECREF(bytes);
	}
	leave();
	return done;
}

static ptrdiff_t copy(PyObject *bytes, unsigned char *data, size_t length)
{
	if (!bytes) {
		fail();
		return -1;
	}
	char *source;
	Py_ssize_t size;
	if (PyBytes_AsStringAndSize(bytes, &source, &size) < 0) {
		Py_DECREF(bytes);
		fail();
		return -1;
	}
	if ((size_t)size > length) size = (Py_ssize_t)length;
	memcpy(data, source, (size_t)size);
	Py_DECREF(bytes);
	return size;
}

ptrdiff_t __nanocode_read(void *file, int fd, unsigned char *data, size_t length)
{
	enter();
	ptrdiff_t done;
	if (!file) {
		PyObject *stream = standard(fd);
		PyObject *buffer = stream ? PyObject_GetAttrString(stream, "buffer") : 0;
		if (stream && !buffer) fail();
		done = buffer ? copy(PyObject_CallMethod(buffer, "read1", "n", (Py_ssize_t)length), data, length) : -1;
		Py_XDECREF(buffer);
		Py_XDECREF(stream);
	} else {
		done = copy(PyObject_CallMethod(file, "read", "n", (Py_ssize_t)length), data, length);
	}
	leave();
	return done;
}

off_t __nanocode_seek(void *file, off_t offset, int whence)
{
	if (!file) {
		errno = ESPIPE;
		return -1;
	}
	enter();
	PyObject *result = PyObject_CallMethod(file, "seek", "Li", (long long)offset, whence);
	off_t position = -1;
	if (result) {
		position = PyLong_AsLongLong(result);
		Py_DECREF(result);
		if (position == -1 && PyErr_Occurred()) fail();
	} else {
		fail();
	}
	leave();
	return position;
}

int __nanocode_close(void *file)
{
	enter();
	PyObject *result = PyObject_CallMethod(file, "close", 0);
	int status = 0;
	if (!result) {
		fail();
		status = -1;
	}
	Py_XDECREF(result);
	Py_DECREF((PyObject *)file);
	leave();
	return status;
}

void *__nanocode_open(const char *path, int fd, const char *mode)
{
	enter();
	PyObject *module = PyImport_ImportModule("io");
	PyObject *file = 0;
	if (module && path)
		file = PyObject_CallMethod(module, "open", "ysi", path, mode, 0);
	else if (module)
		file = PyObject_CallMethod(module, "open", "isiOOOO", fd, mode, 0, Py_None, Py_None, Py_None, Py_False);
	if (!file) fail();
	Py_XDECREF(module);
	leave();
	return file;
}

int __nanocode_fileno(void *file)
{
	enter();
	PyObject *result = PyObject_CallMethod(file, "fileno", 0);
	int fd = -1;
	if (result) {
		fd = (int)PyLong_AsLong(result);
		Py_DECREF(result);
	}
	PyErr_Clear();
	leave();
	return fd;
}

void __nanocode_flush(void)
{
	enter();
	for (int fd = 1; fd <= 2; fd++) {
		PyObject *stream = PySys_GetObject(fd == 1 ? "stdout" : "stderr");
		PyObject *result = stream && stream != Py_None ? PyObject_CallMethod(stream, "flush", 0) : 0;
		Py_XDECREF(result);
		PyErr_Clear();
	}
	leave();
}

static PyObject *os(const char *name)
{
	PyObject *module = PyImport_ImportModule("os");
	PyObject *function = module ? PyObject_GetAttrString(module, name) : 0;
	Py_XDECREF(module);
	return function;
}

static long long number(PyObject *result)
{
	if (!result) {
		fail();
		return -1;
	}
	long long value = result == Py_None ? 0 : PyLong_AsLongLong(result);
	Py_DECREF(result);
	if (value == -1 && PyErr_Occurred()) {
		fail();
		return -1;
	}
	return value;
}

static long long os_call(const char *name, const char *format, ...)
{
	PyObject *function = os(name);
	if (!function) {
		fail();
		return -1;
	}
	va_list arguments;
	va_start(arguments, format);
	PyObject *values = Py_VaBuildValue(format, arguments);
	va_end(arguments);
	PyObject *result = values ? PyObject_CallObject(function, values) : 0;
	Py_XDECREF(values);
	Py_DECREF(function);
	return number(result);
}

ssize_t write(int fd, const void *data, size_t length)
{
	if (fd == 1 || fd == 2) return __nanocode_write(0, fd, data, length);
	enter();
	ssize_t done = os_call("write", "(iy#)", fd, (const char *)data, (Py_ssize_t)length);
	leave();
	return done;
}

ssize_t read(int fd, void *data, size_t length)
{
	enter();
	PyObject *function = os("read");
	ssize_t done = copy(function ? PyObject_CallFunction(function, "in", fd, (Py_ssize_t)length) : 0, data, length);
	Py_XDECREF(function);
	leave();
	return done;
}

off_t lseek(int fd, off_t offset, int whence)
{
	enter();
	off_t position = os_call("lseek", "(iLi)", fd, (long long)offset, whence);
	leave();
	return position;
}

int close(int fd)
{
	enter();
	int status = os_call("close", "(i)", fd) < 0 ? -1 : 0;
	leave();
	return status;
}

int unlink(const char *path)
{
	enter();
	int status = os_call("unlink", "(y)", path) < 0 ? -1 : 0;
	leave();
	return status;
}

int remove(const char *path)
{
	enter();
	int status = os_call("unlink", "(y)", path) < 0 ? -1 : 0;
	if (status < 0 && (errno == EISDIR || errno == EPERM))
		status = os_call("rmdir", "(y)", path) < 0 ? -1 : 0;
	leave();
	return status;
}

int rename(const char *old, const char *new)
{
	enter();
	int status = os_call("rename", "(yy)", old, new) < 0 ? -1 : 0;
	leave();
	return status;
}

static int truth(const char *name, const char *format, ...)
{
	PyObject *function = os(name);
	if (!function) {
		fail();
		return -1;
	}
	va_list arguments;
	va_start(arguments, format);
	PyObject *values = Py_VaBuildValue(format, arguments);
	va_end(arguments);
	PyObject *result = values ? PyObject_CallObject(function, values) : 0;
	Py_XDECREF(values);
	Py_DECREF(function);
	int answer = result ? PyObject_IsTrue(result) : -1;
	Py_XDECREF(result);
	if (answer < 0) fail();
	return answer;
}

int isatty(int fd)
{
	enter();
	int answer = truth("isatty", "(i)", fd);
	if (!answer) errno = ENOTTY;
	leave();
	return answer == 1;
}

int access(const char *path, int mode)
{
	enter();
	int answer = truth("access", "(yi)", path, mode);
	if (!answer) errno = EACCES;
	leave();
	return answer == 1 ? 0 : -1;
}

pid_t getpid(void)
{
	enter();
	pid_t pid = (pid_t)os_call("getpid", "()");
	leave();
	return pid;
}

static int flag(PyObject *module, const char *name)
{
	PyObject *value = PyObject_GetAttrString(module, name);
	int bits = value ? (int)PyLong_AsLong(value) : 0;
	Py_XDECREF(value);
	PyErr_Clear();
	return bits;
}

int open(const char *path, int flags, ...)
{
	enter();
	PyObject *module = PyImport_ImportModule("os");
	int fd = -1;
	if (module) {
		int mode = flags & 3;
		int bits = flag(module, mode == O_WRONLY ? "O_WRONLY" : mode == O_RDWR ? "O_RDWR" : "O_RDONLY");
		if (flags & O_CREAT) bits |= flag(module, "O_CREAT");
		if (flags & O_EXCL) bits |= flag(module, "O_EXCL");
		if (flags & O_TRUNC) bits |= flag(module, "O_TRUNC");
		if (flags & O_APPEND) bits |= flag(module, "O_APPEND");
		fd = (int)number(PyObject_CallMethod(module, "open", "yi", path, bits));
		Py_DECREF(module);
	} else {
		fail();
	}
	leave();
	return fd;
}
