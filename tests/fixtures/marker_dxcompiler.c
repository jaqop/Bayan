/* Marker DLL for the dxcompiler search-order check.
   Copied beside bayan.exe under the name dxcompiler.dll. DllMain creates
   dxcompiler.loaded in that same directory. A startup that reaches the
   renderer without that file did not map this image.
   Build:
     x86_64-w64-mingw32-gcc -shared -O2 -o marker_dxcompiler.dll marker_dxcompiler.c
*/
#include <windows.h>

BOOL WINAPI DllMain(HINSTANCE module, DWORD reason, LPVOID reserved) {
    wchar_t path[MAX_PATH];
    DWORD length;
    wchar_t *slash;
    const wchar_t *name = L"dxcompiler.loaded";
    size_t i;
    HANDLE file;
    (void)reserved;
    if (reason != DLL_PROCESS_ATTACH) {
        return TRUE;
    }
    length = GetModuleFileNameW(module, path, MAX_PATH);
    if (length == 0 || length >= MAX_PATH) {
        return TRUE;
    }
    slash = path + length;
    while (slash > path && slash[-1] != L'\\' && slash[-1] != L'/') {
        slash--;
    }
    for (i = 0; name[i] != 0 && (size_t)(slash - path) + i + 1 < MAX_PATH; i++) {
        slash[i] = name[i];
    }
    slash[i] = 0;
    file = CreateFileW(path, GENERIC_WRITE, 0, NULL, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    if (file != INVALID_HANDLE_VALUE) {
        CloseHandle(file);
    }
    return TRUE;
}
