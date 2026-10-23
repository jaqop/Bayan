/* Bait DLL for the ConPTY search-order test.
   Build (no CRT, no imported DLLs, one DIR64 reloc so ASLR can map it):
     x86_64-w64-mingw32-gcc -shared -nostdlib -nodefaultlibs -O1 -s \
       -Wl,--entry,DllMain -Wl,--exclude-all-symbols \
       -o fake_conpty.dll fake_conpty.c
   CreatePseudoConsole and ResizePseudoConsole return E_FAIL (0x80004005).
   A loader that prefers this image cannot create a pseudoconsole, and the
   file shows up in the module list. DllMain returns TRUE. */
int __stdcall DllMain(void *h, unsigned long reason, void *reserved);

static void *anchor = (void *)&DllMain;

int __stdcall DllMain(void *h, unsigned long reason, void *reserved) {
    (void)h;
    (void)reason;
    (void)reserved;
    void *volatile *p = &anchor;
    return *p != (void *)0;
}

__declspec(dllexport) int __stdcall CreatePseudoConsole(
    void *a, void *b, void *c, unsigned long d, void **e) {
    (void)a;
    (void)b;
    (void)c;
    (void)d;
    (void)e;
    return (int)0x80004005;
}

__declspec(dllexport) int __stdcall ResizePseudoConsole(void *a, void *b) {
    (void)a;
    (void)b;
    return (int)0x80004005;
}

__declspec(dllexport) void __stdcall ClosePseudoConsole(void *a) { (void)a; }
