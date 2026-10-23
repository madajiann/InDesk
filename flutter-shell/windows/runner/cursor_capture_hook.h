#ifndef RUNNER_CURSOR_CAPTURE_HOOK_H_
#define RUNNER_CURSOR_CAPTURE_HOOK_H_

// [20261021 r14] Make libwebrtc's desktop capture believe the system cursor is
// hidden, so it is NOT composited into the video frames -- while the real
// cursor on the local desktop keeps working normally.
//
// Why: flutter_webrtc on Windows captures via DXGI Desktop Duplication +
// DesktopAndCursorComposer, which bakes the system cursor into every frame.
// That forces a bad choice on the controller side: show the local cursor and
// get two arrows, or hide it and follow the lagging in-frame cursor.
// (Replacing the system cursor with a transparent one is NOT an option: the
// local user loses the mouse, which is exactly what went wrong in r12.)
//
// This hooks USER32!GetCursorInfo *inside this process only*: the capture
// chain asks "is the cursor shown?" and gets "no", so no cursor is drawn into
// the frame. DWM still composites the real cursor on screen -- unaffected.
void InstallCursorCaptureHook();

#endif  // RUNNER_CURSOR_CAPTURE_HOOK_H_
