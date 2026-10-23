#include "cursor_capture_hook.h"

#include <windows.h>
#include <tlhelp32.h>

#include <cstdarg>
#include <cstdio>
#include <cstring>
#include <cwchar>

namespace {

using GetCursorInfoFn = BOOL(WINAPI*)(PCURSORINFO);

GetCursorInfoFn g_real_get_cursor_info = nullptr;
volatile LONG g_hits = 0;

// Append-only log next to the exe. Separate file from shell.log: the Dart side
// keeps that one open with append mode, two writers would fight over sharing.
void LogLine(const char* format, ...) {
  char path[MAX_PATH] = {0};
  if (::GetModuleFileNameA(nullptr, path, MAX_PATH) == 0) {
    return;
  }
  char* slash = std::strrchr(path, '\\');
  if (slash == nullptr) {
    return;
  }
  const size_t tail = static_cast<size_t>(MAX_PATH) - static_cast<size_t>(slash + 1 - path);
  if (strcpy_s(slash + 1, tail, "cursor_hook.log") != 0) {
    return;
  }
  FILE* file = nullptr;
  if (fopen_s(&file, path, "a") != 0 || file == nullptr) {
    return;
  }
  SYSTEMTIME now;
  ::GetLocalTime(&now);
  std::fprintf(file, "[%02d:%02d:%02d] ", now.wHour, now.wMinute, now.wSecond);
  va_list args;
  va_start(args, format);
  std::vfprintf(file, format, args);
  va_end(args);
  std::fprintf(file, "\n");
  std::fclose(file);
}

// Only the capture chain is lied to: clearing CURSOR_SHOWING makes webrtc's
// MouseCursorMonitorWin report "no cursor", so DesktopAndCursorComposer stops
// drawing it into frames. DWM's on-screen cursor does not go through this call.
BOOL WINAPI HookGetCursorInfo(PCURSORINFO info) {
  if (g_real_get_cursor_info == nullptr) {
    return FALSE;
  }
  const BOOL ok = g_real_get_cursor_info(info);
  if (ok != 0 && info != nullptr) {
    info->flags &= ~CURSOR_SHOWING;
    const LONG hits = ::InterlockedIncrement(&g_hits);
    if (hits == 1 || (hits % 600) == 0) {
      LogLine("capture chain asked GetCursorInfo #%ld -> reported hidden", hits);
    }
  }
  return ok;
}

// Replace USER32!GetCursorInfo entries in the module's import table.
// IAT patching only: no code bytes are touched, the module's own code stays
// untouched, and every other import keeps working.
int PatchModuleImports(HMODULE module, const char* function_name) {
  if (module == nullptr) {
    return 0;
  }
  auto* base = reinterpret_cast<BYTE*>(module);
  auto* dos = reinterpret_cast<IMAGE_DOS_HEADER*>(base);
  if (dos->e_magic != IMAGE_DOS_SIGNATURE) {
    return 0;
  }
  auto* nt = reinterpret_cast<IMAGE_NT_HEADERS*>(base + dos->e_lfanew);
  if (nt->Signature != IMAGE_NT_SIGNATURE) {
    return 0;
  }
  const IMAGE_DATA_DIRECTORY& dir =
      nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT];
  if (dir.VirtualAddress == 0 || dir.Size == 0) {
    return 0;
  }
  int patched = 0;
  auto* desc = reinterpret_cast<IMAGE_IMPORT_DESCRIPTOR*>(base + dir.VirtualAddress);
  for (; desc->Name != 0; ++desc) {
    const char* dll = reinterpret_cast<const char*>(base + desc->Name);
    if (_stricmp(dll, "USER32.dll") != 0) {
      continue;
    }
    if (desc->OriginalFirstThunk == 0) {
      continue;  // no name table -> cannot tell which function it is
    }
    auto* first = reinterpret_cast<IMAGE_THUNK_DATA*>(base + desc->FirstThunk);
    auto* orig = reinterpret_cast<IMAGE_THUNK_DATA*>(base + desc->OriginalFirstThunk);
    for (size_t i = 0; first[i].u1.Function != 0; ++i) {
      if ((orig[i].u1.Ordinal & IMAGE_ORDINAL_FLAG) != 0) {
        continue;
      }
      auto* by_name =
          reinterpret_cast<IMAGE_IMPORT_BY_NAME*>(base + orig[i].u1.AddressOfData);
      if (std::strcmp(reinterpret_cast<const char*>(by_name->Name), function_name) != 0) {
        continue;
      }
      DWORD old_protect = 0;
      if (::VirtualProtect(&first[i].u1.Function, sizeof(ULONG_PTR), PAGE_READWRITE,
                           &old_protect) == 0) {
        continue;
      }
      first[i].u1.Function = reinterpret_cast<ULONG_PTR>(&HookGetCursorInfo);
      DWORD ignored = 0;
      ::VirtualProtect(&first[i].u1.Function, sizeof(ULONG_PTR), old_protect, &ignored);
      ++patched;
    }
  }
  return patched;
}

bool PatchLoadedModules() {
  const HANDLE snapshot = ::CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, ::GetCurrentProcessId());
  if (snapshot == INVALID_HANDLE_VALUE) {
    return false;
  }
  bool patched_any = false;
  MODULEENTRY32W entry;
  std::memset(&entry, 0, sizeof(entry));
  entry.dwSize = sizeof(entry);
  if (::Module32FirstW(snapshot, &entry) != 0) {
    do {
      const bool is_capturer = _wcsicmp(entry.szModule, L"libwebrtc.dll") == 0 ||
                               _wcsicmp(entry.szModule, L"flutter_webrtc_plugin.dll") == 0;
      if (!is_capturer) {
        continue;
      }
      const int count = PatchModuleImports(entry.hModule, "GetCursorInfo");
      if (count > 0) {
        LogLine("hooked USER32!GetCursorInfo in %ls (%d slot)", entry.szModule, count);
        patched_any = true;
      }
    } while (::Module32NextW(snapshot, &entry) != 0);
  }
  ::CloseHandle(snapshot);
  return patched_any;
}

DWORD WINAPI WatcherProc(LPVOID /*unused*/) {
  // libwebrtc.dll is loaded by the engine/plugin later than wWinMain, so poll
  // for up to 5 minutes instead of patching once at startup.
  for (int i = 0; i < 600; ++i) {
    if (PatchLoadedModules()) {
      return 0;
    }
    ::Sleep(500);
  }
  LogLine("libwebrtc.dll not found / no GetCursorInfo import -> hook skipped");
  return 0;
}

}  // namespace

void InstallCursorCaptureHook() {
  const HMODULE user32 = ::GetModuleHandleW(L"user32.dll");
  if (user32 == nullptr) {
    return;
  }
  g_real_get_cursor_info = reinterpret_cast<GetCursorInfoFn>(
      reinterpret_cast<void*>(::GetProcAddress(user32, "GetCursorInfo")));
  if (g_real_get_cursor_info == nullptr) {
    LogLine("GetProcAddress(GetCursorInfo) failed -> hook skipped");
    return;
  }
  LogLine("cursor capture hook armed, real GetCursorInfo=%p",
          reinterpret_cast<void*>(g_real_get_cursor_info));
  const HANDLE thread = ::CreateThread(nullptr, 0, WatcherProc, nullptr, 0, nullptr);
  if (thread != nullptr) {
    ::CloseHandle(thread);
  }
}
