#include "h264_video.h"

#include <mfapi.h>
#include <mferror.h>
#include <mfidl.h>
#include <mfobjects.h>

#include <cstring>

#pragma comment(lib, "mfplat.lib")
#pragma comment(lib, "mfuuid.lib")

namespace {

inline uint8_t Clip255(int v) {
  return static_cast<uint8_t>(v < 0 ? 0 : (v > 255 ? 255 : v));
}

// [20261019e] Forensics log (exe-adjacent h264_cpp.log).
void CppLog(const char* line) {
  FILE* f = nullptr;
  if (fopen_s(&f, "h264_cpp.log", "a") == 0 && f) {
    SYSTEMTIME st{};
    ::GetLocalTime(&st);
    fprintf(f, "[%02d:%02d:%02d.%03d] %s\n", st.wHour, st.wMinute, st.wSecond,
            st.wMilliseconds, line);
    fclose(f);
  }
}

// ITU-R BT.601 NV12 -> BGRA, integer fixed point (same constants as libyuv
// default). Kept scalar for v1: ~4-8ms per 1080p frame on one worker core,
// which is inside the frame budget at 30fps and does not touch the UI thread.
// [20261019j] Two pixels per iteration share one (U,V) pair: the old loop
// re-read chroma and re-ran x&~1 masking for every pixel, burning ~7.3ms
// per 1680x1050 frame (half of the 60fps budget). Pairing pixels halves the
// chroma math and lets the compiler keep the row pointers in registers.
void Nv12ToBgra(const uint8_t* nv12, LONG pitch, uint32_t codedH,
                uint8_t* dst, int w, int h) {
  const uint8_t* yplane = nv12;
  const uint8_t* uvplane = nv12 + static_cast<size_t>(pitch) * codedH;
  for (int row = 0; row < h; ++row) {
    const uint8_t* yrow = yplane + static_cast<size_t>(row) * pitch;
    // NV12 chroma is 2x2 subsampled; row>>1 selects the UV row pair.
    const uint8_t* uvrow = uvplane + static_cast<size_t>(row >> 1) * pitch;
    uint8_t* out = dst + static_cast<size_t>(row) * w * 4;
    int x = 0;
    // Paired fast path: even widths cover all but the last odd pixel.
    for (; x + 1 < w; x += 2) {
      const int yv0 = yrow[x] - 16;
      const int yv1 = yrow[x + 1] - 16;
      const int u = uvrow[x] - 128;      // x even: U sits at uvrow[x]
      const int v = uvrow[x + 1] - 128;  // and V at uvrow[x+1]
      // [20261019g] Engine pixel-buffer path expects R,G,B,A in memory
      // (flutter_webrtc's kABGR output renders correctly through the same
      // path; BGRA order made red/blue swap = "colors completely wrong").
      out[x * 4 + 0] = Clip255((298 * yv0 + 409 * v + 128) >> 8);
      out[x * 4 + 1] = Clip255((298 * yv0 - 100 * u - 208 * v + 128) >> 8);
      out[x * 4 + 2] = Clip255((298 * yv0 + 516 * u + 128) >> 8);
      out[x * 4 + 3] = 255;
      out[x * 4 + 4] = Clip255((298 * yv1 + 409 * v + 128) >> 8);
      out[x * 4 + 5] = Clip255((298 * yv1 - 100 * u - 208 * v + 128) >> 8);
      out[x * 4 + 6] = Clip255((298 * yv1 + 516 * u + 128) >> 8);
      out[x * 4 + 7] = 255;
    }
    for (; x < w; ++x) {
      const int yv = yrow[x] - 16;
      const int u = uvrow[x & ~1] - 128;
      const int v = uvrow[(x & ~1) + 1] - 128;
      out[x * 4 + 0] = Clip255((298 * yv + 409 * v + 128) >> 8);
      out[x * 4 + 1] = Clip255((298 * yv - 100 * u - 208 * v + 128) >> 8);
      out[x * 4 + 2] = Clip255((298 * yv + 516 * u + 128) >> 8);
      out[x * 4 + 3] = 255;
    }
  }
}

}  // namespace

H264Video& H264Video::Instance() {
  static H264Video inst;
  return inst;
}

int64_t H264Video::Start(flutter::PluginRegistrarWindows* registrar,
                         flutter::BinaryMessenger* messenger) {
  if (running_.load()) {
    return texture_id_;
  }
  if (!registrar || !messenger) {
    return -1;
  }
  texreg_ = registrar->texture_registrar();
  // Note: the channel is created with the raw messenger so the worker thread
  // can push onFirstFrame later (FlutterDesktopMessengerSend is thread-safe).
  channel_ = std::make_unique<flutter::MethodChannel<flutter::EncodableValue>>(
      messenger, "indesk/h264", &flutter::StandardMethodCodec::GetInstance());

  // [20261019f] PixelBufferTexture - the same presentation path flutter_webrtc
  // uses on Windows and the one proven to render in this app (RTCVideoView).
  // The callback runs on the raster thread; it just hands over the front
  // buffer (nullptr before the first frame, engine keeps the last frame).
  texture_variant_ = std::make_unique<flutter::TextureVariant>(
      flutter::PixelBufferTexture(
          [this](size_t w, size_t h) { return CopyPixelBuffer(w, h); }));
  texture_id_ = texreg_->RegisterTexture(texture_variant_.get());
  if (texture_id_ < 0) {
    texture_variant_.reset();
    texreg_ = nullptr;
    channel_.reset();
    return -1;
  }

  first_frame_sent_ = false;
  stop_flag_.store(false);
  {
    std::lock_guard<std::mutex> lk(pb_mtx_);
    free_slots_ = {0, 1, 2};
    has_frame_ = false;
    pixel_buffer_ = {};
  }
  running_.store(true);
  worker_ = std::thread(&H264Video::WorkerLoop, this);
  return texture_id_;
}

void H264Video::Feed(const uint8_t* data, size_t size, bool key,
                     int64_t tsMs, int width, int height) {
  if (!running_.load() || !data || size == 0) {
    return;
  }
  // [20261019h] Carry the key flag down: after a queue drop we gate on IDR.
  Frame f;
  f.data.assign(data, data + size);
  f.tsMs = tsMs;
  f.width = width;
  f.height = height;
  f.key = key;
  {
    std::lock_guard<std::mutex> lk(queue_mtx_);
    // Bound the queue: if the decoder falls behind, drop the OLDEST frame
    // (we always prefer freshest pixels over backlog). Dropping breaks the
    // reference chain -> gate non-key frames until the next IDR.
    while (queue_.size() >= 5) {
      queue_.pop_front();
      ref_lost_.store(true);
    }
    queue_.push_back(std::move(f));
  }
  queue_cv_.notify_one();
}

void H264Video::Stop() {
  if (!running_.load()) {
    return;
  }
  {
    std::lock_guard<std::mutex> lk(queue_mtx_);
    stop_flag_.store(true);
    queue_.clear();
  }
  queue_cv_.notify_all();
  if (worker_.joinable()) {
    worker_.join();
  }
  ReleaseDecoder();
  if (texreg_ && texture_id_ >= 0) {
    texreg_->UnregisterTexture(texture_id_, []() {});
  }
  texture_id_ = -1;
  texture_variant_.reset();
  channel_.reset();
  texreg_ = nullptr;
  {
    std::lock_guard<std::mutex> lk(pb_mtx_);
    has_frame_ = false;
    pixel_buffer_ = {};
    free_slots_.clear();
    free_slots_ = {0, 1, 2};
  }
  slot_cv_.notify_all();
  running_.store(false);
}

void H264Video::WorkerLoop() {
  ::CoInitializeEx(nullptr, COINIT_MULTITHREADED);
  if (InitDecoder()) {
    while (!stop_flag_.load()) {
      Frame f;
      {
        std::unique_lock<std::mutex> lk(queue_mtx_);
        queue_cv_.wait(lk, [&] {
          return stop_flag_.load() || !queue_.empty();
        });
        if (stop_flag_.load()) {
          break;
        }
        f = std::move(queue_.front());
        queue_.pop_front();
      }
      // [20261019h] Reference-chain guard: after a drop only IDR resumes.
      if (ref_lost_.load()) {
        if (!f.key) {
          continue;
        }
        ref_lost_.store(false);
      }
      DecodeFrame(f);
    }
  } else if (channel_) {
    // Tell Dart the native decoder is unavailable so it can fall back to the
    // JPEG/WebRTC track and never send h264Ready.
    channel_->InvokeMethod("onDecoderFailed",
                           std::make_unique<flutter::EncodableValue>(true));
  }
  ReleaseDecoder();
  ::CoUninitialize();
}

bool H264Video::InitDecoder() {
  HRESULT hr = ::MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET);
  if (FAILED(hr)) {
    return false;
  }
  mf_inited_ = true;

  MFT_REGISTER_TYPE_INFO in_type{MFMediaType_Video, MFVideoFormat_H264};
  MFT_REGISTER_TYPE_INFO out_type{MFMediaType_Video, MFVideoFormat_NV12};
  IMFActivate** acts = nullptr;
  UINT32 count = 0;
  hr = ::MFTEnumEx(MFT_CATEGORY_VIDEO_DECODER,
                   MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                   &in_type, &out_type, &acts, &count);
  if (FAILED(hr) || count == 0) {
    if (acts) {
      ::CoTaskMemFree(acts);
    }
    return false;
  }
  hr = acts[0]->ActivateObject(IID_PPV_ARGS(
      decoder_.ReleaseAndGetAddressOf()));
  for (UINT32 i = 0; i < count; ++i) {
    acts[i]->Release();
  }
  ::CoTaskMemFree(acts);
  if (FAILED(hr)) {
    return false;
  }

  Microsoft::WRL::ComPtr<IMFMediaType> in_mt;
  if (FAILED(::MFCreateMediaType(&in_mt))) {
    return false;
  }
  in_mt->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
  in_mt->SetGUID(MF_MT_SUBTYPE, MFVideoFormat_H264);
  if (FAILED(decoder_->SetInputType(0, in_mt.Get(), 0))) {
    return false;
  }

  // Pick the NV12 output type from the available list.
  bool out_set = false;
  for (DWORD i = 0; i < 64; ++i) {
    Microsoft::WRL::ComPtr<IMFMediaType> ot;
    if (FAILED(decoder_->GetOutputAvailableType(0, i, &ot))) {
      break;
    }
    GUID st{};
    if (FAILED(ot->GetGUID(MF_MT_SUBTYPE, &st))) {
      continue;
    }
    if (st == MFVideoFormat_NV12) {
      out_set = SUCCEEDED(decoder_->SetOutputType(0, ot.Get(), 0));
      break;
    }
  }
  if (!out_set) {
    return false;
  }
  decoder_->ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
  return true;
}

void H264Video::ReleaseDecoder() {
  if (decoder_) {
    decoder_->ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
    decoder_.Reset();
  }
  if (mf_inited_) {
    ::MFShutdown();
    mf_inited_ = false;
  }
}

void H264Video::DecodeFrame(const Frame& f) {
  if (!decoder_) {
    return;
  }
  Microsoft::WRL::ComPtr<IMFMediaBuffer> buf;
  if (FAILED(::MFCreateMemoryBuffer(static_cast<DWORD>(f.data.size()),
                                    &buf))) {
    return;
  }
  BYTE* p = nullptr;
  if (FAILED(buf->Lock(&p, nullptr, nullptr))) {
    return;
  }
  std::memcpy(p, f.data.data(), f.data.size());
  buf->Unlock();
  buf->SetCurrentLength(static_cast<DWORD>(f.data.size()));

  Microsoft::WRL::ComPtr<IMFSample> sample;
  if (FAILED(::MFCreateSample(&sample))) {
    return;
  }
  sample->AddBuffer(buf.Get());
  // Media Foundation time unit = 100ns; packet ts is milliseconds.
  sample->SetSampleTime(static_cast<INT64>(f.tsMs) * 10000);
  sample->SetSampleDuration(0);
  if (FAILED(decoder_->ProcessInput(0, sample.Get(), 0))) {
    return;
  }

  // Drain every output frame / stream-change event.
  for (int guard = 0; guard < 8; ++guard) {
    MFT_OUTPUT_STREAM_INFO osi{};
    decoder_->GetOutputStreamInfo(0, &osi);
    MFT_OUTPUT_DATA_BUFFER db{};
    db.dwStreamID = 0;
    Microsoft::WRL::ComPtr<IMFSample> outs;
    if (!(osi.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES)) {
      if (FAILED(::MFCreateSample(&outs))) {
        return;
      }
      Microsoft::WRL::ComPtr<IMFMediaBuffer> ob;
      if (FAILED(::MFCreateMemoryBuffer(
              osi.cbSize > 0 ? osi.cbSize : 1, &ob))) {
        return;
      }
      outs->AddBuffer(ob.Get());
      db.pSample = outs.Get();
    }
    DWORD status = 0;
    HRESULT hr = decoder_->ProcessOutput(0, 1, &db, &status);
    if (hr == MF_E_TRANSFORM_NEED_MORE_INPUT) {
      return;
    }
    if (hr == MF_E_TRANSFORM_STREAM_CHANGE) {
      // Resolution / format change: re-select NV12 and track the new coded
      // size (NVENC pads heights to mod-16; display size comes from the
      // packet header).
      Microsoft::WRL::ComPtr<IMFMediaType> ot;
      if (SUCCEEDED(decoder_->GetOutputAvailableType(0, 0, &ot))) {
        GUID st{};
        if (SUCCEEDED(ot->GetGUID(MF_MT_SUBTYPE, &st)) &&
            st == MFVideoFormat_NV12) {
          decoder_->SetOutputType(0, ot.Get(), 0);
        }
      }
      UINT32 cw = 0, ch = 0;
      Microsoft::WRL::ComPtr<IMFMediaType> cur;
      if (SUCCEEDED(decoder_->GetOutputCurrentType(0, &cur)) &&
          SUCCEEDED(::MFGetAttributeSize(cur.Get(), MF_MT_FRAME_SIZE, &cw,
                                         &ch))) {
        coded_w_ = cw;
        coded_h_ = ch;
      }
      continue;
    }
    if (FAILED(hr)) {
      return;
    }
    IMFSample* s = db.pSample ? db.pSample : outs.Get();
    if (!s) {
      return;
    }
    Microsoft::WRL::ComPtr<IMFMediaBuffer> mb;
    if (FAILED(s->ConvertToContiguousBuffer(&mb))) {
      return;
    }
    BYTE* src = nullptr;
    LONG pitch = 0;
    Microsoft::WRL::ComPtr<IMF2DBuffer> b2;
    bool locked2d = SUCCEEDED(mb.As(&b2)) && SUCCEEDED(b2->Lock2D(&src, &pitch));
    if (!locked2d) {
      DWORD maxLen = 0, curLen = 0;
      if (FAILED(mb->Lock(&src, &maxLen, &curLen))) {
        return;
      }
      pitch = static_cast<LONG>(coded_w_ > 0 ? coded_w_ : f.width);
    }
    // [20261019g] Dump the first decoded frame raw (layout forensics).
    if (!dumped_) {
      dumped_ = true;
      DWORD maxLen = 0, curLen = 0;
      mb->GetMaxLength(&maxLen);
      mb->GetCurrentLength(&curLen);
      FILE* fp = nullptr;
      if (fopen_s(&fp, "nv12_dump.bin", "wb") == 0 && fp) {
        // Full locked region: Y plane rows = 2*codedH rows of pitch bytes is
        // an over-estimate; write pitch*(codedH*3/2) which covers Y+UV.
        fwrite(src, 1, static_cast<size_t>(pitch) * coded_h_ * 3 / 2, fp);
        fclose(fp);
      }
      char dbuf[192];
      snprintf(dbuf, sizeof(dbuf),
               "Dump pitch=%ld codedW=%u codedH=%u dispW=%d dispH=%d "
               "bufMax=%lu bufCur=%lu locked2d=%d",
               pitch, coded_w_, coded_h_, f.width, f.height,
               (unsigned long)maxLen, (unsigned long)curLen, locked2d ? 1 : 0);
      CppLog(dbuf);
    }
    PresentFrame(src, pitch, coded_h_ > 0 ? coded_h_ : static_cast<uint32_t>(f.height),
                 f.width, f.height, f.tsMs);
    if (locked2d) {
      b2->Unlock2D();
    } else {
      mb->Unlock();
    }
  }
}

bool H264Video::PresentFrame(const uint8_t* nv12, LONG pitch, uint32_t codedH,
                             int w, int h, int64_t tsMs) {
  if (w <= 0 || h <= 0) {
    return false;
  }
  // [20261019h] Acquire a free slot: the engine holds slots between
  // CopyPixelBuffer and its release_callback. Waiting here (instead of
  // round-robin overwriting) prevents use-after-free when the raster thread
  // lags, at the cost of natural backpressure on the decode worker.
  int slot = -1;
  {
    std::unique_lock<std::mutex> lk(pb_mtx_);
    if (!slot_cv_.wait_for(lk, std::chrono::milliseconds(200),
                           [&] {
                             return !free_slots_.empty() ||
                                    stop_flag_.load();
                           })) {
      return false;  // engine stalled >200ms; drop this frame
    }
    if (stop_flag_.load()) {
      return false;
    }
    slot = free_slots_.front();
    free_slots_.pop_front();
  }
  std::vector<uint8_t>& back = slots_[slot].data;
  back.resize(static_cast<size_t>(w) * h * 4);
  // [20261019g] Conversion timing forensics (first 5 frames).
  LARGE_INTEGER t0{}, t1{};
  QueryPerformanceCounter(&t0);
  Nv12ToBgra(nv12, pitch, codedH, back.data(), w, h);
  QueryPerformanceCounter(&t1);
  if (conv_count_ < 5) {
    conv_count_++;
    static LARGE_INTEGER freq = [] {
      LARGE_INTEGER f;
      QueryPerformanceFrequency(&f);
      return f;
    }();
    char cbuf[96];
    snprintf(cbuf, sizeof(cbuf), "Convert #%d %dx%d %.1f ms", conv_count_, w,
             h, (double)(t1.QuadPart - t0.QuadPart) * 1000.0 / freq.QuadPart);
    CppLog(cbuf);
  }

  {
    std::lock_guard<std::mutex> lk(pb_mtx_);
    pixel_buffer_.width = static_cast<size_t>(w);
    pixel_buffer_.height = static_cast<size_t>(h);
    pixel_buffer_.buffer = back.data();
    pixel_buffer_.release_context = (void*)(intptr_t)slot;
    pixel_buffer_.release_callback = &H264Video::OnSlotReleased;
    has_frame_ = true;
  }

  if (texreg_) {
    texreg_->MarkTextureFrameAvailable(texture_id_);
  }
  // [20261019f] Forensics: first 3 presents.
  if (present_count_ < 3) {
    present_count_++;
    char pbuf[128];
    snprintf(pbuf, sizeof(pbuf), "Present #%d %dx%d slot=%d", present_count_,
             w, h, slot);
    CppLog(pbuf);
  }
  if (!first_frame_sent_) {
    first_frame_sent_ = true;
    NotifyFirstFrame(w, h);
  }
  (void)tsMs;
  return true;
}

void H264Video::OnSlotReleased(void* release_context) {
  // Called by the engine's raster thread once it has finished copying the
  // buffer, or when overwriting the previously returned pointer.
  H264Video& self = Instance();
  const int slot = static_cast<int>(reinterpret_cast<intptr_t>(release_context));
  if (slot < 0 || slot >= 3) {
    return;
  }
  {
    std::lock_guard<std::mutex> lk(self.pb_mtx_);
    self.free_slots_.push_back(slot);
  }
  self.slot_cv_.notify_one();
}

const FlutterDesktopPixelBuffer* H264Video::CopyPixelBuffer(size_t w,
                                                            size_t h) {
  std::lock_guard<std::mutex> lk(pb_mtx_);
  // [20261019f] Forensics: engine raster pulls. Keep first 5.
  if (pb_pulls_ < 5) {
    pb_pulls_++;
    char buf[128];
    snprintf(buf, sizeof(buf), "CopyPixelBuffer #%d has=%d %zux%zu",
             pb_pulls_, has_frame_ ? 1 : 0, pixel_buffer_.width,
             pixel_buffer_.height);
    CppLog(buf);
  }
  (void)w;
  (void)h;
  if (!has_frame_) {
    return nullptr;
  }
  return &pixel_buffer_;
}

void H264Video::NotifyFirstFrame(int w, int h) {
  if (!channel_) {
    return;
  }
  flutter::EncodableMap m;
  m[flutter::EncodableValue("w")] = flutter::EncodableValue(w);
  m[flutter::EncodableValue("h")] = flutter::EncodableValue(h);
  channel_->InvokeMethod(
      "onFirstFrame", std::make_unique<flutter::EncodableValue>(m));
}
