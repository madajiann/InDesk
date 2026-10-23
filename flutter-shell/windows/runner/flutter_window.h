#ifndef RUNNER_FLUTTER_WINDOW_H_
#define RUNNER_FLUTTER_WINDOW_H_

#include <flutter/dart_project.h>
#include <flutter/flutter_view_controller.h>
#include <flutter/method_channel.h>
#include <flutter/plugin_registrar_windows.h>
#include <flutter/standard_method_codec.h>

#include <memory>

#include "win32_window.h"

// A window that does nothing but host a Flutter view.
class FlutterWindow : public Win32Window {
 public:
  // Creates a new Flutter window hosting a Flutter view running |project|.
  explicit FlutterWindow(const flutter::DartProject& project);
  virtual ~FlutterWindow();

 protected:
  // Win32Window:
  bool OnCreate() override;
  void OnDestroy() override;
  LRESULT MessageHandler(HWND window, UINT const message, WPARAM const wparam,
                         LPARAM const lparam) noexcept override;

 private:
  // The project to run.
  flutter::DartProject project_;

  // The Flutter instance hosted by this window.
  std::unique_ptr<flutter::FlutterViewController> flutter_controller_;

  // [20261017c] Dart -> native channel: disable the system IME while a
  // password field has focus (ImmAssociateContext), restore on blur --
  // English input without switching IME.
  std::unique_ptr<
      flutter::MethodChannel<flutter::EncodableValue>>
      ime_channel_;

  // [20261019e H264 direct] "indesk/h264" channel: init/feed/dispose for the
  // native NVENC-H264 decoder bridge (h264_video.cpp). The registrar wrapper
  // owns the external texture registration and must outlive the engine.
  std::unique_ptr<
      flutter::MethodChannel<flutter::EncodableValue>>
      h264_channel_;
  std::unique_ptr<flutter::PluginRegistrarWindows> h264_registrar_;

  // Original IME context saved before disabling (for restore), plus the
  // Flutter child HWND it was taken from ([20261017d] IME binds to the
  // focused child window, not the top-level window).
  void* original_himc_ = nullptr;
  HWND ime_target_ = nullptr;

  // [20261018] WM_NCHITTEST caption band side reserves (logical px). The
  // top-bar middle zone returns HTCAPTION; the side zones stay HTCLIENT so
  // page-specific buttons keep working. Dart switches these per page
  // (setDragBand): shell page keeps 300/400, remote session page uses
  // 260/180 (its toolbar has buttons on both sides).
  LONG drag_band_left_ = 300;
  LONG drag_band_right_ = 400;

  // [20261019] Drag fix: the REAL mouse hit-test lands on the FLUTTERVIEW
  // child window (which answers HTCLIENT), so the top-level HTCAPTION band
  // in MessageHandler never ran. Subclass the child and answer HTTRANSPARENT
  // inside the drag band / resize frame so the hit falls through to the
  // top-level handler.
  HWND child_wnd_ = nullptr;
  WNDPROC child_proc_orig_ = nullptr;

  // Subclassed child window procedure (static thunk).
  static LRESULT CALLBACK ChildWndProc(HWND hwnd, UINT message,
                                       WPARAM wparam, LPARAM lparam);
  // HTTRANSPARENT decision for the child; everything else delegates.
  LRESULT ChildHitTest(HWND hwnd, LPARAM lparam);
};

#endif  // RUNNER_FLUTTER_WINDOW_H_
