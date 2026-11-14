import ctypes, json, os, pathlib, sys, threading, time
ctypes.windll.kernel32.SetConsoleCP(65001)
ctypes.windll.kernel32.SetConsoleOutputCP(65001)
root = pathlib.Path(sys.argv[1]); root.mkdir(parents=True, exist_ok=True)
echo = ''
def read_input():
    global echo
    for line in sys.stdin:
        echo = line.rstrip('\r\n')
threading.Thread(target=read_input, daemon=True).start()
print(pathlib.Path(sys.argv[2]).read_text(encoding='utf-8'), flush=True)
tick = 0
while True:
    tick += 1
    size = os.get_terminal_size()
    state = dict(pid=os.getpid(), tick=tick, echo=echo, cols=size.columns, rows=size.lines)
    staged = root/'counter.tmp'
    staged.write_text(json.dumps(state), encoding='utf-8')
    staged.replace(root/'counter.json')
    print(f'MUX-LIVE pid={os.getpid()} tick={tick}', flush=True)
    time.sleep(.15)
