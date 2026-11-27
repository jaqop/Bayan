"""Minimal raw-input fixture: append hex of stdin bytes to argv[1].

Used by the ConPTY kitty-CSI u test to assert exactly what the pseudoconsole
delivered to the program. Appends per read so a reader can poll mid-run.
"""
import os
import sys
import time

out = sys.argv[1]
with open(out, "ab") as f:
    f.write(b"start")
deadline = time.time() + float(sys.argv[2] if len(sys.argv) > 2 else 5)
while time.time() < deadline:
    chunk = os.read(0, 4096)
    if not chunk:
        break
    with open(out, "ab") as f:
        f.write(chunk.hex().encode("ascii"))
