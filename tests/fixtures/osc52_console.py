"""Real Windows console process: emit a query and read its VT reply."""
import ctypes
import json
import os
import pathlib
import sys

kernel = ctypes.windll.kernel32
mode = ctypes.c_uint32()
for stream, add, remove in [(-11, 4, 0), (-10, 0x200, 7)]:
    handle = kernel.GetStdHandle(stream)
    if not kernel.GetConsoleMode(handle, ctypes.byref(mode)):
        raise OSError('GetConsoleMode')
    if not kernel.SetConsoleMode(handle, (mode.value | add) & ~remove):
        raise OSError('SetConsoleMode')
os.write(1, b'\x1b]52;c;?\x07')
reply = bytearray()
while not reply.endswith(b'\x07') and not reply.endswith(b'\x1b\\'):
    reply.extend(os.read(0, 1))
    if len(reply) > 100000:
        raise ValueError('reply limit')
pathlib.Path(sys.argv[1]).write_text(json.dumps(reply.decode('ascii')), encoding='utf-8')
