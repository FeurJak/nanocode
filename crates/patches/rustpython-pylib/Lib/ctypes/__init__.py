"""ctypes without a foreign function interface.

This interpreter (RustPython in WebAssembly) cannot load shared libraries or
call C functions, but many packages import ctypes for its types alone: to
size C integers, to pass a buffer address as a c_void_p, or to declare
callback signatures. This module keeps those parts. Loading a library raises
OSError, and calling a foreign function raises NotImplementedError.
"""

import struct as _struct
import sys as _sys

__version__ = "1.1.0"

RTLD_LOCAL = 0
RTLD_GLOBAL = 256
DEFAULT_MODE = RTLD_LOCAL


class ArgumentError(Exception):
    pass


_NO_FFI = "ctypes cannot load or call C code in this interpreter"


class _CDataType(type):
    def __mul__(cls, length):
        if not isinstance(length, int) or length < 0:
            raise ValueError("array length must be a non-negative int")
        return _array_type(cls, length)

    __rmul__ = __mul__

    def from_param(cls, obj):
        return obj if isinstance(obj, cls) else cls(obj)

    def in_dll(cls, library, name):
        raise ValueError(_NO_FFI)

    def from_address(cls, address):
        raise ValueError(_NO_FFI)

    def from_buffer(cls, source, offset=0):
        raise TypeError(_NO_FFI)

    from_buffer_copy = from_buffer


class _CData(metaclass=_CDataType):
    _b_base_ = None
    _b_needsfree_ = True
    _objects = None


class _SimpleCData(_CData):
    _type_ = "i"

    def __init__(self, value=None):
        self.value = self._default() if value is None else self._convert(value)

    @classmethod
    def _default(cls):
        return 0

    @classmethod
    def _convert(cls, value):
        bits = 8 * _sizeof_code(cls._type_)
        if cls._type_ in "bhilqnP?":
            value = int(value) & ((1 << bits) - 1)
            if cls._type_ not in "P?" and cls._type_.islower() and value >= 1 << (bits - 1):
                value -= 1 << bits
            return bool(value) if cls._type_ == "?" else value
        if cls._type_ in "BHILQN":
            return int(value) & ((1 << bits) - 1)
        return value

    def __bool__(self):
        return bool(self.value)

    def __repr__(self):
        return "%s(%r)" % (type(self).__name__, self.value)


def _sizeof_code(code):
    if code in "nN":
        return _struct.calcsize("n")
    if code == "z" or code == "Z" or code == "P":
        return _struct.calcsize("P")
    if code == "u":
        return 4
    if code == "g":
        return _struct.calcsize("d")
    return _struct.calcsize(code)


def _simple(name, code, default=0, convert=None):
    ns = {"_type_": code}
    if default != 0:
        ns["_default"] = classmethod(lambda cls: default)
    if convert is not None:
        ns["_convert"] = classmethod(lambda cls, v: convert(v))
    return _CDataType(name, (_SimpleCData,), ns)


c_byte = _simple("c_byte", "b")
c_ubyte = _simple("c_ubyte", "B")
c_short = _simple("c_short", "h")
c_ushort = _simple("c_ushort", "H")
c_int = _simple("c_int", "i")
c_uint = _simple("c_uint", "I")
c_long = _simple("c_long", "l")
c_ulong = _simple("c_ulong", "L")
c_longlong = _simple("c_longlong", "q")
c_ulonglong = _simple("c_ulonglong", "Q")
c_ssize_t = _simple("c_ssize_t", "n")
c_size_t = _simple("c_size_t", "N")
c_bool = _simple("c_bool", "?", False)
c_float = _simple("c_float", "f", 0.0, float)
c_double = _simple("c_double", "d", 0.0, float)
c_longdouble = _simple("c_longdouble", "g", 0.0, float)
c_char = _simple("c_char", "c", b"\x00", lambda v: bytes([v]) if isinstance(v, int) else bytes(v))
c_wchar = _simple("c_wchar", "u", "\x00", str)
c_char_p = _simple("c_char_p", "z", None, lambda v: v)
c_wchar_p = _simple("c_wchar_p", "Z", None, lambda v: v)
c_void_p = _simple("c_void_p", "P", None, lambda v: None if v is None else int(v))
c_int8, c_uint8 = c_byte, c_ubyte
c_int16, c_uint16 = c_short, c_ushort
c_int32, c_uint32 = c_int, c_uint
c_int64, c_uint64 = c_longlong, c_ulonglong
c_time_t = c_int64
c_voidp = c_void_p
py_object = _CDataType("py_object", (_SimpleCData,), {"_type_": "O", "_default": classmethod(lambda cls: None), "_convert": classmethod(lambda cls, v: v)})


def _align(t):
    if isinstance(t, type) and issubclass(t, _SimpleCData):
        return min(_sizeof_code(t._type_), 8) if t._type_ != "O" else _struct.calcsize("P")
    if isinstance(t, type) and issubclass(t, Array):
        return _align(t._type_)
    if isinstance(t, type) and issubclass(t, (Structure, Union)):
        return max([_align(f[1]) for f in t.__dict__.get("_fields_", ())] or [1])
    return _struct.calcsize("P")


def sizeof(obj_or_type):
    t = obj_or_type if isinstance(obj_or_type, type) else type(obj_or_type)
    if not isinstance(t, _CDataType):
        raise TypeError("this type has no size")
    if issubclass(t, _SimpleCData):
        return _sizeof_code(t._type_) if t._type_ != "O" else _struct.calcsize("P")
    if issubclass(t, Array):
        return sizeof(t._type_) * t._length_
    if issubclass(t, (_Pointer, _CFuncPtr)):
        return _struct.calcsize("P")
    if issubclass(t, (Structure, Union)):
        fields = t.__dict__.get("_fields_", ())
        size = 0
        for f in fields:
            fs = sizeof(f[1])
            if issubclass(t, Union):
                size = max(size, fs)
            else:
                a = _align(f[1])
                size = (size + a - 1) // a * a + fs
        a = _align(t)
        return (size + a - 1) // a * a
    return 0


alignment = _align


class _StructBase(_CData):
    def __init__(self, *args, **kwargs):
        fields = [f[0] for f in type(self).__dict__.get("_fields_", ())]
        for f in type(self).__dict__.get("_fields_", ()):
            t = f[1]
            object.__setattr__(self, f[0], t() if isinstance(t, type) and issubclass(t, (Structure, Union, Array)) else (t._default() if isinstance(t, type) and issubclass(t, _SimpleCData) else None))
        if len(args) > len(fields):
            raise TypeError("too many initializers")
        for name, value in zip(fields, args):
            setattr(self, name, value)
        for name, value in kwargs.items():
            setattr(self, name, value)


class Structure(_StructBase):
    pass


class Union(_StructBase):
    pass


BigEndianStructure = LittleEndianStructure = Structure
BigEndianUnion = LittleEndianUnion = Union


class Array(_CData):
    _type_ = c_int
    _length_ = 0

    def __init__(self, *args):
        if len(args) > self._length_:
            raise IndexError("too many initializers")
        t = self._type_
        empty = t._default() if issubclass(t, _SimpleCData) else None
        self._items = list(args) + [empty if issubclass(t, _SimpleCData) else t() for _ in range(self._length_ - len(args))]

    def __len__(self):
        return self._length_

    def __getitem__(self, i):
        return self._items[i]

    def __setitem__(self, i, v):
        self._items[i] = v

    def __iter__(self):
        return iter(self._items)


_array_types = {}


def _array_type(t, length):
    key = (t, length)
    if key not in _array_types:
        _array_types[key] = _CDataType("%s_Array_%d" % (t.__name__, length), (Array,), {"_type_": t, "_length_": length})
    return _array_types[key]


class _Pointer(_CData):
    _type_ = None

    def __init__(self, value=None):
        self.value = value

    @property
    def contents(self):
        raise ValueError(_NO_FFI)

    def __bool__(self):
        return bool(self.value)

    def __getitem__(self, i):
        raise ValueError(_NO_FFI)


_pointer_type_cache = {}


def POINTER(t):
    if t not in _pointer_type_cache:
        name = "LP_" + (t if isinstance(t, str) else t.__name__)
        _pointer_type_cache[t] = _CDataType(name, (_Pointer,), {"_type_": t})
    return _pointer_type_cache[t]


def pointer(obj):
    return POINTER(type(obj))(obj)


class _CFuncPtr(_CData):
    _argtypes_ = ()
    _restype_ = c_int
    _flags_ = 0

    def __init__(self, *args):
        self.value = args[0] if args and isinstance(args[0], int) else None
        self.argtypes = self._argtypes_
        self.restype = self._restype_
        self.errcheck = None

    def __call__(self, *args, **kwargs):
        raise NotImplementedError(_NO_FFI)


_c_functype_cache = {}


def CFUNCTYPE(restype, *argtypes, use_errno=False, use_last_error=False):
    key = (restype, argtypes)
    if key not in _c_functype_cache:
        _c_functype_cache[key] = _CDataType("CFunctionType", (_CFuncPtr,), {"_argtypes_": argtypes, "_restype_": restype})
    return _c_functype_cache[key]


PYFUNCTYPE = CFUNCTYPE


def _address(obj):
    if obj is None or isinstance(obj, int):
        return obj
    if isinstance(obj, (_SimpleCData, _Pointer, _CFuncPtr)):
        v = obj.value
        return v if v is None or isinstance(v, int) else id(v)
    raise ArgumentError("cannot take the address of %r here" % (obj,))


def cast(obj, typ):
    if not (isinstance(typ, type) and (issubclass(typ, (_Pointer, _CFuncPtr)) or typ in (c_void_p, c_char_p, c_wchar_p))):
        raise TypeError("cast() argument 2 must be a pointer type, not %s" % getattr(typ, "__name__", typ))
    return typ(_address(obj))


def byref(obj, offset=0):
    return obj


def addressof(obj):
    raise TypeError(_NO_FFI)


def resize(obj, size):
    raise ValueError(_NO_FFI)


def create_string_buffer(init, size=None):
    if isinstance(init, (bytes, bytearray)):
        n = len(init) + 1 if size is None else size
        buf = (c_char * n)()
        buf._items[: len(init)] = [bytes([b]) for b in init]
        buf.value = bytes(init)
        buf.raw = bytes(init).ljust(n, b"\x00")
        return buf
    buf = (c_char * int(init))()
    buf.value = b""
    buf.raw = b"\x00" * int(init)
    return buf


c_buffer = create_string_buffer


def create_unicode_buffer(init, size=None):
    n = (len(init) + 1 if size is None else size) if isinstance(init, str) else int(init)
    buf = (c_wchar * n)()
    buf.value = init if isinstance(init, str) else ""
    return buf


def string_at(ptr, size=-1):
    raise ValueError(_NO_FFI)


def wstring_at(ptr, size=-1):
    raise ValueError(_NO_FFI)


def memmove(dst, src, count):
    raise ValueError(_NO_FFI)


def memset(dst, c, count):
    raise ValueError(_NO_FFI)


_errno = 0


def get_errno():
    return _errno


def set_errno(value):
    global _errno
    old, _errno = _errno, value
    return old


class CDLL:
    _func_flags_ = 0
    _func_restype_ = c_int

    def __init__(self, name, mode=DEFAULT_MODE, handle=None, use_errno=False, use_last_error=False, winmode=None):
        self._name = name
        raise OSError("%s: cannot load %s" % (_NO_FFI, name))


class PyDLL(CDLL):
    pass


class LibraryLoader:
    def __init__(self, dlltype):
        self._dlltype = dlltype

    def __getattr__(self, name):
        if name[0] == "_":
            raise AttributeError(name)
        return self._dlltype(name)

    __getitem__ = __getattr__

    def LoadLibrary(self, name):
        return self._dlltype(name)


cdll = LibraryLoader(CDLL)
pydll = LibraryLoader(PyDLL)


class _NoLibrary:
    def __getattr__(self, name):
        if name[0] == "_":
            raise AttributeError(name)
        raise AttributeError("%s: no %s" % (_NO_FFI, name))


pythonapi = _NoLibrary()
SetPointerType = None
ARRAY = _array_type
