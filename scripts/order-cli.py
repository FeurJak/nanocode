#!/usr/bin/env python3
import os
import sys


def executed(directory):
    names = set()
    for entry in os.listdir(directory):
        for line in open(os.path.join(directory, entry)):
            fields = line.split()
            if len(fields) == 8 and int(fields[7]) > 0:
                names.update((fields[1], fields[4]))
    return names


order = [line.strip() for line in open(sys.argv[1]) if line.strip()]
startup, oneshot, tools, codemode = (executed(directory) for directory in sys.argv[2:])
for group in (startup & oneshot, oneshot - startup, tools - oneshot, codemode - tools - oneshot, startup - oneshot - tools - codemode):
    for name in order:
        if name in group:
            print(name)
for name in order:
    if name not in startup and name not in oneshot and name not in tools and name not in codemode:
        print(name)
