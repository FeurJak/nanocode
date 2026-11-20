ACCESS_READ = 1


class mmap:
    def __init__(self, *args, **kwargs):
        raise OSError("memory maps are not available in this Python")
