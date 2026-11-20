"""subprocess for matplotlib and PIL when python3 here has no shell to run commands in: importing works,
running a command fails with OSError(ENOSYS) the way a missing program does, which they already handle."""

import errno


class SubprocessError(Exception):
    pass


class CalledProcessError(SubprocessError):
    pass


class TimeoutExpired(SubprocessError):
    pass


PIPE, STDOUT, DEVNULL = -1, -2, -3


def _none(*args, **kwargs):
    raise OSError(errno.ENOSYS, 'this Python has no processes to run a command in')


Popen = run = call = check_call = check_output = getoutput = getstatusoutput = _none
