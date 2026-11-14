"""Native ConPTY output fixture; no commands or user input are executed."""
import ctypes
import pathlib
import sys
import time

kernel = ctypes.windll.kernel32
kernel.SetConsoleOutputCP(65001)
mode = ctypes.c_uint32()
handle = kernel.GetStdHandle(-11)
if not kernel.GetConsoleMode(handle, ctypes.byref(mode)):
    raise OSError("GetConsoleMode")
if not kernel.SetConsoleMode(handle, mode.value | 4):
    raise OSError("SetConsoleMode")
for index in range(1200):
    print(f"HISTORY-OLD-{index:04d}")
print(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"), flush=True)
print("BAYAN-CORPUS-DONE", flush=True)
time.sleep(10)  # The test owns and terminates this child after draining output.
