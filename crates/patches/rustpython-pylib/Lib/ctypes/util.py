"""ctypes.util for an interpreter that cannot load shared libraries."""


def find_library(name):
    return None


def find_msvcrt():
    return None
