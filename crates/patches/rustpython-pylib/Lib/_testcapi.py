"""The integer limits of CPython's _testcapi, which CPython's own tests import.

nanocode's python3 has no C API to test, so this module holds only the C type
limits (as on a 64-bit Linux build) and the feature flags test.support reads;
every C API test helper is missing, so tests that need one skip or fail with
ImportError on "from _testcapi import <helper>" as on a build without it.
"""
import sys as _sys

CHAR_MAX, CHAR_MIN, UCHAR_MAX = 127, -128, 255
SHRT_MAX, SHRT_MIN, USHRT_MAX = 2**15 - 1, -2**15, 2**16 - 1
INT_MAX, INT_MIN, UINT_MAX = 2**31 - 1, -2**31, 2**32 - 1
INT32_MAX, INT32_MIN, UINT32_MAX = 2**31 - 1, -2**31, 2**32 - 1
INT64_MAX, INT64_MIN, UINT64_MAX = 2**63 - 1, -2**63, 2**64 - 1
LONG_MAX, LONG_MIN, ULONG_MAX = 2**63 - 1, -2**63, 2**64 - 1
LLONG_MAX, LLONG_MIN, ULLONG_MAX = 2**63 - 1, -2**63, 2**64 - 1
PY_SSIZE_T_MAX, PY_SSIZE_T_MIN = _sys.maxsize, -_sys.maxsize - 1
SIZE_MAX = 2 * _sys.maxsize + 1
SIZEOF_VOID_P = SIZEOF_SIZE_T = SIZEOF_TIME_T = 8
SIZEOF_WCHAR_T = 4
FLT_MAX, FLT_MIN = 3.4028234663852886e+38, 1.1754943508222875e-38
DBL_MAX, DBL_MIN = _sys.float_info.max, _sys.float_info.min
PY_SSIZE_T_SIZE = 8
LIMITED_API_AVAILABLE = False
WITH_PYMALLOC = False
Py_GIL_DISABLED = False
