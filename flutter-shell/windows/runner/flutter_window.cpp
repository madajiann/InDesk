#include "flutter_window.h"

#include <imm.h>
#include <optional>
#include <windowsx.h>

#include "flutter/generated_plugin_registrant.h"
#include "h264_video.h"

namespace {

// [20261019] Single top-level window in this runner; lets the static child
// thunk reach the instance for band decisions.
FlutterWindow* g_flutter_window = nullptr;

// True on Windows 11 (build 22000+). RtlGetVersion is used because
// VerifyVersionInfo-based helpers are manifest dependent.
bool IsWin11OrGreater() {
  using RtlGetVersionFn = LONG(WINAPI*)(PRTL_OSVERSIONINFOW);
  RTL_OSVERSIONINFOW info{};
  info.dwOSVersionInfoSize = sizeof(info);
  auto fn = reinterpret_cast<RtlGetVersionFn>(::GetProcAddress(
      ::GetModuleHandleW(L"ntdll.dll"), "RtlGetVersion"));
  if (!fn || fn(&info) != 0) {
    return false;
  }
  return info.dwBuildNumber >= 22000;
}

// Physical border width used by the DWM frame for a thickframe window.
LONG WindowFramePx() {
  return static_cast<LONG>(::GetSystemMetrics(SM_CXSIZEFRAME) +
                           ::GetSystemMetrics(SM_CXPADDEDBORDER));
}

}  // namespace

FlutterWindow::FlutterWindow(const flutter::DartProject& project)
    : project_(project) {}

FlutterWindow::~FlutterWindow() {}

bool FlutterWindow::OnCreate() {
  if (!Win32Window::OnCreate()) {
    return false;
  }

  RECT frame = GetClientArea();

  // The size here must match the window dimensions to avoid unnecessary surface
  // creation / destruction in the startup path.
  flutter_controller_ = std::make_unique<flutter::FlutterViewController>(
      frame.right - frame.left, frame.bottom - frame.top, project_);
  // Ensure that basic setup of the controller was successful.
  if (!flutter_controller_->engine() || !flutter_controller_->view()) {
    return false;
  }
  RegisterPlugins(flutter_controller_->engine());
  SetChildContent(flutter_controller_->view()->GetNativeWindow());

  // [20261019] Drag fix: subclass the FLUTTERVIEW child window. The real
  // mouse WM_NCHITTEST goes to the child (probed: child answers HTCLIENT
  // while our top-level band logic answers HTCAPTION - the latter never
  // runs). Inside the caption band / resize frame the child now answers
  // HTTRANSPARENT so the hit-test falls through to the top-level handler.
  g_flutter_window = this;
  child_wnd_ = flutter_controller_->view()->GetNativeWindow();
  child_proc_orig_ = reinterpret_cast<WNDPROC>(::SetWindowLongPtrW(
      child_wnd_, GWLP_WNDPROC,
      reinterpret_cast<LONG_PTR>(&FlutterWindow::ChildWndProc)));

  // [20261017c] Disable the system IME while a password field has focus:
  // Dart calls setImeEnabled(false) -> ImmAssociateContext(hwnd, NULL) so
  // typing goes straight to English output (no IME switching needed);
  // setImeEnabled(true) on blur restores the original IME context.
  ime_channel_ =
      std::make_unique<flutter::MethodChannel<flutter::EncodableValue>>(
          flutter_controller_->engine()->messenger(), "indesk/window",
          &flutter::StandardMethodCodec::GetInstance());
  ime_channel_->SetMethodCallHandler(
      [this](const auto& call, auto result) {
        if (call.method_name() == "setImeEnabled") {
          bool enable = true;
          const auto* args = std::get_if<bool>(call.arguments());
          if (args) enable = *args;
          // [20261017d] IME context lives on the window that owns keyboard
          // focus, which is the Flutter child HWND created by
          // SetChildContent - NOT the top-level window. Associating on the
          // top-level window has no effect (2.3.5 bug: the password dialog
          // still went through the system IME).
          const HWND target =
              flutter_controller_->view()->GetNativeWindow();
          if (enable) {
            if (original_himc_ && ime_target_) {
              ::ImmAssociateContext(ime_target_,
                                    static_cast<HIMC>(original_himc_));
              original_himc_ = nullptr;
              ime_target_ = nullptr;
            }
          } else {
            if (!original_himc_ && target) {
              ime_target_ = target;
              original_himc_ = ::ImmAssociateContext(ime_target_, nullptr);
            }
          }
          result->Success(flutter::EncodableValue(true));
        } else if (call.method_name() == "setDragBand") {
          // [20261018] Per-page caption drag band reserves (logical px).
          const auto* map =
              std::get_if<flutter::EncodableMap>(call.arguments());
          if (map) {
            auto li = map->find(flutter::EncodableValue("left"));
            auto ri = map->find(flutter::EncodableValue("right"));
            if (li != map->end()) {
              const auto* lv = std::get_if<int32_t>(&li->second);
              if (lv) drag_band_left_ = static_cast<LONG>(*lv);
            }
            if (ri != map->end()) {
              const auto* rv = std::get_if<int32_t>(&ri->second);
              if (rv) drag_band_right_ = static_cast<LONG>(*rv);
            }
          }
          result->Success(flutter::EncodableValue(true));
        } else {
          result->NotImplemented();
        }
      });

  // [20261019e H264 direct] Native decoder bridge channel. init -> texture id
  // (or null => Dart stays on the JPEG/WebRTC fallback and never sends
  // h264Ready), feed -> enqueue one Annex-B frame, dispose -> stop decoder.
  h264_registrar_ = std::make_unique<flutter::PluginRegistrarWindows>(
      flutter_controller_->engine()->GetRegistrarForPlugin("InDeskH264Video"));
  h264_channel_ =
      std::make_unique<flutter::MethodChannel<flutter::EncodableValue>>(
          flutter_controller_->engine()->messenger(), "indesk/h264",
          &flutter::StandardMethodCodec::GetInstance());
  h264_channel_->SetMethodCallHandler(
      [this](const auto& call, auto result) {
        if (call.method_name() == "init") {
          const int64_t id = H264Video::Instance().Start(
              h264_registrar_.get(),
              flutter_controller_->engine()->messenger());
          if (id >= 0) {
            result->Success(flutter::EncodableValue(id));
          } else {
            result->Success(flutter::EncodableValue());
          }
        } else if (call.method_name() == "feed") {
          const auto* map = std::get_if<flutter::EncodableMap>(call.arguments());
          bool ok = false;
          if (map) {
            auto di = map->find(flutter::EncodableValue("data"));
            auto ki = map->find(flutter::EncodableValue("key"));
            auto ti = map->find(flutter::EncodableValue("ts"));
            auto wi = map->find(flutter::EncodableValue("w"));
            auto hi = map->find(flutter::EncodableValue("h"));
            if (di != map->end() && wi != map->end() && hi != map->end()) {
              const auto* bytes =
                  std::get_if<std::vector<uint8_t>>(&di->second);
              const auto* keyv =
                  ki != map->end() ? std::get_if<bool>(&ki->second) : nullptr;
              int64_t ts = 0;
              if (ti != map->end()) {
                if (const auto* v = std::get_if<int32_t>(&ti->second)) ts = *v;
                if (const auto* v = std::get_if<int64_t>(&ti->second)) ts = *v;
              }
              int32_t w = 0, h = 0;
              if (const auto* v = std::get_if<int32_t>(&wi->second)) w = *v;
              if (const auto* v = std::get_if<int32_t>(&hi->second)) h = *v;
              if (bytes && w > 0 && h > 0) {
                H264Video::Instance().Feed(bytes->data(), bytes->size(),
                                           keyv ? *keyv : false, ts, w, h);
                ok = true;
              }
            }
          }
          result->Success(flutter::EncodableValue(ok));
        } else if (call.method_name() == "dispose") {
          H264Video::Instance().Stop();
          result->Success(flutter::EncodableValue(true));
        } else {
          result->NotImplemented();
        }
      });

  flutter_controller_->engine()->SetNextFrameCallback([&]() {
    this->Show();
  });

  // Flutter can complete the first frame before the "show window" callback is
  // registered. The following call ensures a frame is pending to ensure the
  // window is shown. It is a no-op if the first frame hasn't completed yet.
  flutter_controller_->ForceRedraw();

  return true;
}

void FlutterWindow::OnDestroy() {
  // [20261019e] Stop the H264 decoder before the engine goes away.
  H264Video::Instance().Stop();
  h264_channel_.reset();
  h264_registrar_.reset();
  // [20261019] Restore the subclassed child proc before teardown.
  if (child_wnd_ && child_proc_orig_) {
    ::SetWindowLongPtrW(child_wnd_, GWLP_WNDPROC,
                        reinterpret_cast<LONG_PTR>(child_proc_orig_));
    child_wnd_ = nullptr;
    child_proc_orig_ = nullptr;
  }
  if (g_flutter_window == this) {
    g_flutter_window = nullptr;
  }
  // [20261017c] Restore the IME context in case a password field left it off
  if (original_himc_ && ime_target_) {
    ::ImmAssociateContext(ime_target_, static_cast<HIMC>(original_himc_));
    original_himc_ = nullptr;
    ime_target_ = nullptr;
  }
  if (flutter_controller_) {
    flutter_controller_ = nullptr;
  }

  Win32Window::OnDestroy();
}

LRESULT
FlutterWindow::MessageHandler(HWND hwnd, UINT const message,
                              WPARAM const wparam,
                              LPARAM const lparam) noexcept {
  // [20261016] Hidden-titlebar frame fix, handled BEFORE the plugin delegate.
  //
  // window_manager (title bar hidden) insets the client area by a hardcoded
  // 8px in the normal state, which leaves a visible blank band between the
  // window edge and the content ("double border" the user reported), and its
  // maximized inset was double-compensated by our Dart-side padding. Fix:
  //  - normal state: client == full window rect (no band);
  //  - maximized: inset by the real frame width so the content stays inside
  //    the monitor (a maximized thickframe window overflows the screen);
  //  - WM_NCHITTEST restores native edge resizing.
  if (message == WM_NCCALCSIZE && wparam) {
    auto* sz = reinterpret_cast<NCCALCSIZE_PARAMS*>(lparam);
    if (::IsZoomed(hwnd)) {
      const LONG frame = WindowFramePx();
      sz->rgrc[0].left += frame;
      sz->rgrc[0].top += frame;
      sz->rgrc[0].right -= frame;
      sz->rgrc[0].bottom -= frame;
    } else {
      // On Windows 10 a 1px DWM artifact line shows at the top when the
      // client starts at 0; nudge the top edge down (window_manager does the
      // same for the hidden title bar style).
      if (!IsWin11OrGreater()) {
        sz->rgrc[0].top += 1;
      }
    }
    return 0;
  }
  if (message == WM_NCHITTEST) {
    POINT pt{GET_X_LPARAM(lparam), GET_Y_LPARAM(lparam)};
    RECT rc;
    ::GetWindowRect(hwnd, &rc);
    if (!::PtInRect(&rc, pt)) {
      return HTNOWHERE;
    }
    if (::IsIconic(hwnd)) {
      // [20261019] While iconic, the icon lives at (-32000,-32000): skip
      // drag/resize hit-testing entirely and let the default path handle
      // everything, keeping the taskbar restore flow untouched.
      return ::DefWindowProc(hwnd, message, wparam, lparam);
    }
    if (!::IsZoomed(hwnd)) {
      const LONG frame = WindowFramePx();
      const bool at_left = pt.x < rc.left + frame;
      const bool at_right = pt.x >= rc.right - frame;
      const bool at_top = pt.y < rc.top + frame;
      const bool at_bottom = pt.y >= rc.bottom - frame;
      if (at_top && at_left) return HTTOPLEFT;
      if (at_top && at_right) return HTTOPRIGHT;
      if (at_bottom && at_left) return HTBOTTOMLEFT;
      if (at_bottom && at_right) return HTBOTTOMRIGHT;
      if (at_left) return HTLEFT;
      if (at_right) return HTRIGHT;
      if (at_top) return HTTOP;
      if (at_bottom) return HTBOTTOM;
    }
    // [20261017d] Native caption drag for the top bar band. Flutter's
    // DragToMoveArea pan path proved unreliable in testing (the window
    // simply would not move), so dragging is handled natively: the middle
    // zone of the top 40 logical px returns HTCAPTION. Side zones stay
    // HTCLIENT so the main shell right-side buttons and the remote
    // toolbar left-side buttons keep working.
    const UINT dpi = ::GetDpiForWindow(hwnd);
    const double scale = static_cast<double>(dpi) / 96.0;
    const LONG band = static_cast<LONG>(40 * scale);
    const LONG leftKeep = static_cast<LONG>(drag_band_left_ * scale);
    const LONG rightKeep = static_cast<LONG>(drag_band_right_ * scale);
    RECT rcClient;
    ::GetClientRect(hwnd, &rcClient);
    ::MapWindowPoints(hwnd, nullptr,
                      reinterpret_cast<POINT*>(&rcClient), 2);
    if (pt.y >= rcClient.top && pt.y < rcClient.top + band &&
        pt.x >= rcClient.left + leftKeep &&
        pt.x <= rcClient.right - rightKeep) {
      return HTCAPTION;
    }
    // Interior points fall through to the plugin / DefWindowProc (HTCLIENT).
  }

  // [20261019] Taskbar-restore fix: the window_manager plugin returns 1 on
  // WM_NCACTIVATE for hidden-titlebar windows WITHOUT calling DefWindowProc.
  // Probes proved every message-level restore API works, yet the user
  // reported "clicking the taskbar button never restores the minimized
  // window". Route WM_NCACTIVATE through DefWindowProc before the plugin
  // delegate so the default activation bookkeeping runs again. Our client
  // area equals the window rect (NCCALCSIZE empties the NC area) and DWM
  // owns the frame, so there is no visible repaint side effect.
  if (message == WM_NCACTIVATE) {
    return ::DefWindowProc(hwnd, message, wparam, lparam);
  }

  // Give Flutter, including plugins, an opportunity to handle window messages.
  if (flutter_controller_) {
    std::optional<LRESULT> result =
        flutter_controller_->HandleTopLevelWindowProc(hwnd, message, wparam,
                                                      lparam);
    if (result) {
      return *result;
    }
  }

  switch (message) {
    case WM_FONTCHANGE:
      flutter_controller_->engine()->ReloadSystemFonts();
      break;
  }

  return Win32Window::MessageHandler(hwnd, message, wparam, lparam);
}

LRESULT CALLBACK FlutterWindow::ChildWndProc(HWND hwnd, UINT message,
                                             WPARAM wparam, LPARAM lparam) {
  if (message == WM_NCHITTEST && g_flutter_window) {
    return g_flutter_window->ChildHitTest(hwnd, lparam);
  }
  if (g_flutter_window && g_flutter_window->child_proc_orig_) {
    return ::CallWindowProcW(g_flutter_window->child_proc_orig_, hwnd,
                             message, wparam, lparam);
  }
  return ::DefWindowProcW(hwnd, message, wparam, lparam);
}

LRESULT FlutterWindow::ChildHitTest(HWND hwnd, LPARAM lparam) {
  WNDPROC orig = child_proc_orig_;
  auto passthrough = [&]() -> LRESULT {
    return orig ? ::CallWindowProcW(orig, hwnd, WM_NCHITTEST, 0, lparam)
                : ::DefWindowProcW(hwnd, WM_NCHITTEST, 0, lparam);
  };
  HWND top = ::GetAncestor(hwnd, GA_ROOT);
  if (!top || ::IsIconic(top) || ::IsZoomed(top)) {
    return passthrough();
  }
  POINT pt{GET_X_LPARAM(lparam), GET_Y_LPARAM(lparam)};
  RECT rc;
  ::GetWindowRect(top, &rc);
  if (!::PtInRect(&rc, pt)) {
    return passthrough();
  }
  const UINT dpi = ::GetDpiForWindow(top);
  const double scale = static_cast<double>(dpi) / 96.0;
  const LONG frame = WindowFramePx();
  // Resize frame strips (the top-level handler answers HTLEFT/HTTOP/...).
  const bool at_edge = pt.x < rc.left + frame || pt.x >= rc.right - frame ||
                       pt.y < rc.top + frame || pt.y >= rc.bottom - frame;
  // Caption band (top-level answers HTCAPTION). Client == window rect on
  // this frameless window (WM_NCCALCSIZE), so the window rect works for
  // the band test; keep/reserve values mirror MessageHandler exactly.
  const LONG band = static_cast<LONG>(40 * scale);
  const LONG leftKeep = static_cast<LONG>(drag_band_left_ * scale);
  const LONG rightKeep = static_cast<LONG>(drag_band_right_ * scale);
  const bool in_band = pt.y < rc.top + band &&
                       pt.x >= rc.left + leftKeep &&
                       pt.x <= rc.right - rightKeep;
  if (at_edge || in_band) {
    // Fall through to the top-level window (same thread), which owns the
    // real non-client decision.
    return HTTRANSPARENT;
  }
  return passthrough();
}
