use std::fs::File;
use std::io::Write;
use std::time::Instant;

use nvenc::bitstream::BitStream;
use nvenc::session::InitParams;
use nvenc::{
    session::Session,
    sys::guids::{NV_ENC_CODEC_H264_GUID, NV_ENC_PRESET_P3_GUID},
};
#[cfg(windows)]
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

#[cfg(windows)]
pub fn setup() -> ID3D11Device {
    let dxgi_factory: windows::Win32::Graphics::Dxgi::IDXGIFactory =
        unsafe { windows::Win32::Graphics::Dxgi::CreateDXGIFactory() }.unwrap();
    let dxgi_adapter = unsafe { dxgi_factory.EnumAdapters(0) }.unwrap();

    let desc = unsafe { dxgi_adapter.GetDesc() }.unwrap();
    println!("{:?}", windows::core::HSTRING::from_wide(&desc.Description));

    let mut device = None;
    let mut device_context = None;
    unsafe {
        use windows::Win32::{
            Foundation::HMODULE,
            Graphics::{
                Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0},
                Direct3D11::{D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11CreateDevice},
            },
        };

        D3D11CreateDevice(
            &dxgi_adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE(std::ptr::null_mut()),
            D3D11_CREATE_DEVICE_FLAG(0),
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&raw mut device),
            None,
            Some(&raw mut device_context),
        )
    }
    .unwrap();
    let device = device.unwrap();
    let _device_context = device_context.unwrap();

    device
}

#[cfg(windows)]
fn image(device: &ID3D11Device) -> ID3D11Texture2D {
    let data: [u8; 1920 * 4] = std::array::from_fn(|idx| match idx % 4 {
        0 => 255,
        1 => 0,
        2 => 0,
        3 => 255,
        _ => unreachable!(),
    });

    let mut texture = None;
    let desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC {
        Width: 1920,
        Height: 1080,
        MipLevels: 1,
        ArraySize: 1,
        Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE_DEFAULT,
        BindFlags: windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: windows::Win32::Graphics::Dxgi::Common::DXGI_CPU_ACCESS_NONE,
        MiscFlags: 0,
    };
    let data = windows::Win32::Graphics::Direct3D11::D3D11_SUBRESOURCE_DATA {
        pSysMem: data.as_ptr() as _,
        SysMemPitch: 1920 * 4,
        SysMemSlicePitch: 0,
    };
    unsafe {
        device.CreateTexture2D(
            &raw const desc,
            Some(&raw const data),
            Some(&raw mut texture),
        )
    }
    .unwrap();
    texture.unwrap()
}

fn main() {
    let device = setup();

    #[cfg(target_os = "linux")]
    compile_error!("Linux is currently unsupported in this example");
    #[cfg(windows)]
    let session: Session<nvenc::session::NeedsConfig> = Session::open_dx(&device).unwrap();
    assert!(
        session
            .get_encode_codecs()
            .unwrap()
            .contains(&NV_ENC_CODEC_H264_GUID)
    );
    assert!(
        session
            .get_encode_presets(NV_ENC_CODEC_H264_GUID)
            .unwrap()
            .contains(&NV_ENC_PRESET_P3_GUID)
    );
    let (session, mut config) = session
        .get_encode_preset_config_ex(
            NV_ENC_CODEC_H264_GUID,
            NV_ENC_PRESET_P3_GUID,
            nvenc::sys::enums::NVencTuningInfo::LowLatency,
        )
        .unwrap();

    let texture = image(&device);

    config.preset_cfg.rc_params.rate_control_mode = nvenc::sys::enums::NVencParamsRcMode::VBR;
    config.preset_cfg.rc_params.average_bit_rate = 10_000_000;
    config.preset_cfg.gop_len = 0xffffffff;
    config.preset_cfg.frame_interval_p = 1;
    println!("P-Frames: {}", config.preset_cfg.frame_interval_p);
    println!("Bitrate: {}", config.preset_cfg.rc_params.average_bit_rate);

    let init_params = InitParams {
        encode_guid: NV_ENC_CODEC_H264_GUID,
        preset_guid: NV_ENC_PRESET_P3_GUID,
        aspect_ratio: [16, 9],
        encode_config: &mut config.preset_cfg,
        tuning_info: nvenc::sys::enums::NVencTuningInfo::LowLatency,
        buffer_format: nvenc::sys::enums::NVencBufferFormat::ARGB,
        frame_rate: [30, 1],
        resolution: [1920, 1080],
        enable_ptd: true,
        max_encoder_resolution: [0, 0],
    };

    println!("FPS: 30");
    println!("Len: 600 frames");
    println!("Expected time: {}", 600 / 30);
    println!("Codec: H264");
    println!("Tuning Info {:?}:", init_params.tuning_info);

    let encoder = session.init_encoder(init_params).unwrap();
    let registered = encoder
        .register_resource_dx11(&texture, nvenc::sys::enums::NVencBufferFormat::ARGB, 0)
        .unwrap();

    let (processed, to_use) = std::sync::mpsc::sync_channel::<BitStream>(2);
    let (re_use, to_process) = std::sync::mpsc::sync_channel::<BitStream>(2);
    processed
        .send(encoder.create_bitstream_buffer().unwrap())
        .unwrap();
    processed
        .send(encoder.create_bitstream_buffer().unwrap())
        .unwrap();

    let handle = std::thread::spawn({
        move || {
            let mut out = File::create("output.h264").unwrap();

            while let Ok(output) = to_process.recv() {
                let lock = output.try_lock(true).unwrap();
                out.write_all(lock.as_slice()).unwrap();
                drop(lock);
                if let Err(_) = processed.send(output) {
                    break;
                }
            }
        }
    });

    let instant = Instant::now();
    let mut i = 0;
    while i < 600 {
        if let Ok(output) = to_use.recv() {
            encoder
                .encode_picture(
                    &registered,
                    &output,
                    i,
                    instant.duration_since(Instant::now()).as_millis() as u64,
                    nvenc::sys::enums::NVencBufferFormat::ARGB,
                    nvenc::sys::enums::NVencPicStruct::Frame,
                    nvenc::sys::enums::NVencPicType::P,
                    None,
                )
                .unwrap();
            re_use.send(output).unwrap();
            i += 1;
        }
    }

    drop(to_use);
    drop(re_use);
    handle.join().unwrap();
}
