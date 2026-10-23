#ifndef RUNNER_H264_VIDEO_H_
#define RUNNER_H264_VIDEO_H_

// [20261019e H264 direct] Native decode of the controlled-side NVENC H.264
// stream (the same DataChannel "VideoChannel" direct-send path the old
// Electron/web master used; see web/src/utils/h264Packet.ts for the wire
// format). Replaces the JPEG relay chain (screen capture -> JPEG -> canvas ->
// re-encode -> WebRTC) that added ~100-150ms of structural latency.
//
// Pipeline: Dart assembles Annex-B frames from DataChannel packets and feeds
// them here via the "indesk/h264" MethodChannel. A worker thread decodes each
// frame with the OS Media Foundation H.264 decoder MFT (sync mode, NV12 out),
// converts NV12 -> BGRA in system memory (matches the web master's own
// finding that software decode has far lower latency than DXVA's ~85ms
// output pipeline), then hands the BGRA buffer to Flutter via the
// PixelBufferTexture path.
//
// [20261019f] Presentation switched from GpuSurfaceTexture
// (kFlutterDesktopGpuSurfaceTypeDxgiSharedHandle) to PixelBufferTexture:
// forensics (h264_cpp.log) proved we wrote the shared textures AND the engine
// pulled our descriptors with matching handles, yet the screen stayed black -
// the shared-handle path is unreliable in this environment. PixelBufferTexture
// is exactly what flutter_webrtc uses on Windows (common/cpp/src/
// flutter_video_renderer.cc) and is proven working in this very app
// (RTCVideoView renders fine).

#include <atomic>
#include <condition_variable>
#include <cstdint>
#include <deque>
#include <memory>
#include <mutex>
#include <thread>
#include <vector>

#include <mfapi.h>
#include <mfidl.h>
#include <mfobjects.h>
#include <wrl/client.h>

#include <flutter/method_channel.h>
#include <flutter/plugin_registrar_windows.h>
#include <flutter/standard_method_codec.h>
#include <flutter/texture_registrar.h>

class H264Video {
 public:
  static H264Video& Instance();

  // Must be called on the platform thread. Returns the registered external
  // texture id (>= 0), or -1 if registration failed (Dart then stays on the
  // JPEG/WebRTC fallback and never sends h264Ready).
  int64_t Start(flutter::PluginRegistrarWindows* registrar,
                flutter::BinaryMessenger* messenger);

  // Enqueue one complete Annex-B frame for decoding. Cheap (copy + notify);
  // call from any thread.
  void Feed(const uint8_t* data, size_t size, bool key, int64_t tsMs,
            int width, int height);

  // Stop the worker, release MF resources and unregister the texture.
  // Must be called on the platform thread.
  void Stop();

  bool running() const { return running_.load(); }

 private:
  H264Video() = default;
  ~H264Video() = default;

  struct Frame {
    std::vector<uint8_t> data;
    int64_t tsMs = 0;
    int width = 0;
    int height = 0;
    bool key = false;
  };

  void WorkerLoop();
  bool InitDecoder();
  void ReleaseDecoder();
  // Decode one frame; may produce 0..N output frames (stream-change events).
  void DecodeFrame(const Frame& f);
  // NV12 -> BGRA (scalar, integer math) into the back buffer, then flip it
  // in front of the engine's CopyPixelBuffer callback.
  bool PresentFrame(const uint8_t* nv12, LONG pitch, uint32_t codedH,
                    int w, int h, int64_t tsMs);
  // Engine-side callback (raster thread): expose the current front buffer.
  const FlutterDesktopPixelBuffer* CopyPixelBuffer(size_t w, size_t h);
  void NotifyFirstFrame(int w, int h);

  std::thread worker_;
  std::atomic<bool> running_{false};
  std::atomic<bool> stop_flag_{false};
  // [20261019h] After a queue-overflow drop the reference chain is broken;
  // decoding further P frames would corrupt the picture. Gate non-key
  // frames until the next IDR arrives.
  std::atomic<bool> ref_lost_{false};
  std::mutex queue_mtx_;
  std::condition_variable queue_cv_;
  std::deque<Frame> queue_;

  flutter::TextureRegistrar* texreg_ = nullptr;
  std::unique_ptr<flutter::TextureVariant> texture_variant_;
  int64_t texture_id_ = -1;
  std::unique_ptr<flutter::MethodChannel<flutter::EncodableValue>> channel_;

  Microsoft::WRL::ComPtr<IMFTransform> decoder_;
  uint32_t coded_w_ = 0;
  uint32_t coded_h_ = 0;

  // [20261019h] Triple-buffered BGRA frames with engine release tracking.
  // The engine's raster thread copies the buffer handed back by
  // CopyPixelBuffer at its own pace (can lag >2 frames); the
  // release_callback returns the slot, and the worker waits for a free slot
  // before converting. This both prevents use-after-free crashes (the
  // 2-buffer version crashed the master) and gives natural backpressure.
  std::mutex pb_mtx_;
  std::condition_variable slot_cv_;
  std::deque<int> free_slots_;
  struct Slot {
    std::vector<uint8_t> data;
  };
  Slot slots_[3];
  FlutterDesktopPixelBuffer pixel_buffer_{};
  bool has_frame_ = false;

  static void OnSlotReleased(void* release_context);

  bool first_frame_sent_ = false;
  bool mf_inited_ = false;
  bool dumped_ = false;
  int conv_count_ = 0;
  int present_count_ = 0;
  int pb_pulls_ = 0;
};

#endif  // RUNNER_H264_VIDEO_H_
