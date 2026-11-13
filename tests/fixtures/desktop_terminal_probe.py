"""Isolated live desktop fixture; input is recorded as data, never executed."""
import ctypes, json, os, pathlib, sys, threading, time
root = pathlib.Path(sys.argv[1]); root.mkdir(parents=True, exist_ok=True)
corpus = pathlib.Path(sys.argv[2]).read_text(encoding='utf-8')
kernel = ctypes.windll.kernel32
kernel.SetConsoleCP(65001); kernel.SetConsoleOutputCP(65001)
mode = ctypes.c_uint32()
for stream, add, remove in [(-11, 4, 0), (-10, 0x200, 7)]:
    handle = kernel.GetStdHandle(stream)
    if not kernel.GetConsoleMode(handle, ctypes.byref(mode)): raise OSError('GetConsoleMode')
    if not kernel.SetConsoleMode(handle, (mode.value | add) & ~remove): raise OSError('SetConsoleMode')
pid = os.getpid()
(root/f'state-{pid}.json').write_text(json.dumps(dict(pid=pid, cwd=os.getcwd(), argv=sys.argv)), encoding='utf-8')
def read_input():
    while True:
        data = os.read(0, 4096)
        if not data: break
        with (root/f'input-{pid}.bin').open('ab') as f: f.write(data)
threading.Thread(target=read_input, daemon=True).start()
for i in range(1200): print(f'HISTORY-OLD-{i:04d}', flush=False)
print(corpus, flush=True)
print(f'LIVE-READY pid={pid}', flush=True)
seen = ''
while True:
    path = root/f'request-{pid}.json'
    if path.exists():
        req = json.loads(path.read_text(encoding='utf-8-sig'))
        if req['id'] != seen:
            seen = req['id']
            action = req['action']
            if action == 'exit': break
            if action == 'modes': os.write(1, b'\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[?1h\x1b=\x1b[?2004h')
            if action == 'corpus': print(corpus, flush=True)
            (root/f'ack-{pid}.json').write_text(json.dumps(req), encoding='utf-8')
    time.sleep(.03)
