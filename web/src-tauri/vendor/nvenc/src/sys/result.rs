#[derive(Debug, PartialEq, Eq)]
#[repr(C)]
pub enum NVencResult {
    Success,
    NoDevice,
    UnsupportedDevice,
    InvalidEncoderDevice,
    InvalidDevice,
    DeviceNotExist,
    InvalidPtr,
    InvalidParam,
    InvalidEvent,
    InvalidCall,
    OutOfMemory,
    NotInitialized,
    UnsupportedParam,
    LockBusy,
    NotEnoughBuffer,
    InvalidVersion,
    MapFailed,
    NeedMoreInput,
    EncoderBusy,
    EventNotRegisterd,
    Generic,
    IncompatibleClientKey,
    Unimplemented,
    ResourceRegisterFailed,
    ResourceNotRegistered,
    ResourceNotMapped,
    NeedMoreOutput,
}

impl NVencResult {
    pub fn into_error(self) -> Result<(), NVencError> {
        NVencError::from_result(self)
    }
}

impl NVencError {
    pub fn from_result(value: NVencResult) -> Result<(), Self> {
        if value == NVencResult::Success {
            Ok(())
        } else {
            // Safety: They are one case different, so they always align
            unsafe { Err(std::mem::transmute::<NVencResult, NVencError>(value)) }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
#[repr(C)]
pub enum NVencError {
    NoDevice = NVencResult::NoDevice as isize,
    UnsupportedDevice,
    InvalidEncoderDevice,
    InvalidDevice,
    DeviceNotExist,
    InvalidPtr,
    InvalidEvent,
    InvalidParam,
    InvalidCall,
    OutOfMemory,
    NotInitialized,
    UnsupportedParam,
    LockBusy,
    NotEnoughBuffer,
    InvalidVersion,
    MapFailed,
    NeedMoreInput,
    EncoderBusy,
    EventNotRegisterd,
    Generic,
    IncompatibleClientKey,
    Unimplemented,
    ResourceRegisterFailed,
    ResourceNotRegistered,
    ResourceNotMapped,
    NeedMoreOutput,
    /// [InDesk 加固] nvEncodeAPI64.dll 加载失败（无 NVIDIA 驱动 / 非 NVIDIA 机器）。
    /// 追加在末尾：不参与 from_result 的 transmute 映射，不影响与 NVencResult 的对齐。
    LibraryNotFound,
}
