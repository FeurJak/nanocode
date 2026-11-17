"""select for hosts without file descriptor polling (WebAssembly).

Only select() exists, and it can wait for nothing: with no descriptors it
sleeps for the timeout, which is what an asyncio event loop with only timers
needs. Waiting on descriptors raises OSError.
"""

import errno as _errno
import time as _time

error = OSError


def _select(rlist, wlist, xlist, timeout=None):
    if rlist or wlist or xlist:
        raise OSError(_errno.ENOSYS, 'select() on file descriptors is not available')
    if timeout is None:
        raise OSError(_errno.EDEADLK, 'select() with no file descriptors and no timeout would wait forever')
    if timeout < 0:
        raise ValueError('timeout must be non-negative')
    if timeout:
        _time.sleep(timeout)
    return [], [], []


class _Builtin:
    # A plain function stored as a class attribute (selectors.SelectSelector
    # keeps select.select that way) would bind like a method; C builtins don't.
    __slots__ = ('_f', '__doc__', '__name__')

    def __init__(self, f):
        self._f = f
        self.__doc__ = f.__doc__
        self.__name__ = f.__name__.lstrip('_')

    def __call__(self, *args, **kwargs):
        return self._f(*args, **kwargs)


select = _Builtin(_select)
del _Builtin
