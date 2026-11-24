//! Xbox Series shaders to host shaders.
//!
//! An Xbox Series shader is a standard DXBC container ([`dxbc`]). Its `DXIL`
//! part ([`dxil`]) may carry Xbox additions around the LLVM bitcode: an
//! `XDAT` table before it indexing blobs after it, among them `xbop`
//! (compiler options) and the zlib-compressed GFX10 code. The bitcode may use
//! console-only intrinsics, so the GFX10 code is translated in general.

pub mod dxbc;
pub mod dxil;
pub mod emit;
pub mod gfx10;
pub mod io;
pub mod ir;
pub mod native;
pub mod resource;
pub mod structure;

pub use dxbc::{Container, FourCC, Part};
pub use dxil::{DxilProgram, ShaderKind, XboxExtras};
pub use native::{
    HwStage, NativeProgram, ResourceType, RootParam, RootType, UsedResource, native_programs,
};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShaderError {
    #[error("not a DXBC container")]
    NotDxbc,
    #[error("malformed shader: {0}")]
    Malformed(&'static str),
    #[error("GFX10 code at {offset:#x}: {reason}")]
    Isa { offset: usize, reason: &'static str },
    /// Valid code the translator cannot handle yet: an opcode name or a construct.
    #[error("GFX10 code at {offset:#x}: {what} is not supported")]
    Unsupported { offset: usize, what: &'static str },
}

/// The host GPU's largest wave (`D3D12_OPTIONS1::WaveLaneCountMax`), 0 while
/// unknown (translation then declares the console's own wave size).
static HOST_WAVE_LANES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Tells the translator how many lanes the host's waves have (32 on NVIDIA,
/// 4 on WARP, 64 on RDNA); translations are keyed by it.
pub fn set_host_wave_lanes(lanes: u32) {
    HOST_WAVE_LANES.store(lanes.max(1), std::sync::atomic::Ordering::Relaxed);
}

/// The host's largest wave, 0 while unknown.
pub fn host_wave_lanes() -> u32 {
    HOST_WAVE_LANES.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
mod host_wave_tests {
    /// The probe the host GPU feeds the translator (milestone 1 of
    /// docs/wave64-emulation.md): a 32-lane host makes the emitter declare
    /// 32, and translations are keyed by it in `shader_cache`.
    #[test]
    fn host_wave_roundtrips() {
        super::set_host_wave_lanes(32);
        assert_eq!(super::host_wave_lanes(), 32);
        super::set_host_wave_lanes(4);
        assert_eq!(super::host_wave_lanes(), 4);
        super::set_host_wave_lanes(64);
        assert_eq!(super::host_wave_lanes(), 64);
        super::set_host_wave_lanes(0); // 0 means unknown, stored as 1
        assert_eq!(super::host_wave_lanes(), 1);
    }
}
