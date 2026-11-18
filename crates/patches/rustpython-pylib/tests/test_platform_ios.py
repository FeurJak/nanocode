import builtins
import pathlib
import sys
import types
import unittest


SOURCE = pathlib.Path(__file__).resolve().parents[1] / 'Lib' / 'platform.py'


def load_platform(behavior, target='ios'):
    def imported(name, *args, **kwargs):
        if name == '_ios_support':
            if isinstance(behavior, BaseException):
                raise behavior
            return types.SimpleNamespace(get_platform_ios=behavior)
        return builtins.__import__(name, *args, **kwargs)

    namespace = {
        '__name__': 'platform_ios_test',
        '__builtins__': dict(vars(builtins), __import__=imported),
    }
    exec(compile(SOURCE.read_text(), str(SOURCE), 'exec'), namespace)

    class Sys:
        platform = target

        def __getattr__(self, name):
            return getattr(sys, name)

    namespace['sys'] = Sys()
    return types.SimpleNamespace(**namespace)


class IOSVersionTests(unittest.TestCase):
    def test_import_error_returns_empty_defaults(self):
        platform = load_platform(ImportError('runtime unavailable'))
        self.assertEqual(platform.ios_ver(), ('', '', '', False))

    def test_import_error_preserves_explicit_defaults(self):
        platform = load_platform(ImportError('runtime unavailable'))
        self.assertEqual(platform.ios_ver('a', 'b', 'c', True), ('a', 'b', 'c', True))

    def test_missing_module_preserves_defaults(self):
        platform = load_platform(ModuleNotFoundError('_ios_support'))
        self.assertEqual(platform.ios_ver('a', 'b', 'c', False), ('a', 'b', 'c', False))

    def test_supported_runtime_result_is_unchanged(self):
        platform = load_platform(lambda: ('iOS', '26.5', 'iPhone', True))
        self.assertEqual(platform.ios_ver('a', 'b', 'c', False), ('iOS', '26.5', 'iPhone', True))

    def test_missing_runtime_result_preserves_defaults(self):
        platform = load_platform(lambda: None)
        self.assertEqual(platform.ios_ver('a', 'b', 'c', True), ('a', 'b', 'c', True))

    def test_unexpected_import_error_type_is_not_hidden(self):
        platform = load_platform(RuntimeError('unexpected failure'))
        with self.assertRaises(RuntimeError):
            platform.ios_ver()

    def test_getter_import_error_is_not_hidden(self):
        def getter():
            raise ImportError('getter failure')

        platform = load_platform(getter)
        with self.assertRaises(ImportError):
            platform.ios_ver()

    def test_non_ios_does_not_import_support(self):
        platform = load_platform(AssertionError('must not import'), 'linux')
        self.assertEqual(platform.ios_ver('a', 'b', 'c', True), ('a', 'b', 'c', True))

    def test_uname_and_system_do_not_fabricate_os_name(self):
        platform = load_platform(ImportError('runtime unavailable'))
        platform.uname.__globals__['os'] = types.SimpleNamespace(
            uname=lambda: ('Darwin', 'node', '24', 'version', 'arm64')
        )
        self.assertEqual(platform.system(), '')
        self.assertEqual(platform.uname().machine, 'arm64')


if __name__ == '__main__':
    unittest.main()
