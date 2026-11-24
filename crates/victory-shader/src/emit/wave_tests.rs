//! Synthetic GFX10 -> DXIL -> GPU/WARP tests for Wave32 host compatibility
//! and SGPR spill elision (`v_writelane_b32` / `v_readlane_b32`).

use super::bitcode::{Attr, Term};
use super::container::SigElement;
use super::host::{Dispatch, Draw, Host, Root, View};
use super::shader::Shader;
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};
use windows::Win32::Graphics::Direct3D12::{
    D3D12_FEATURE_D3D12_OPTIONS1, D3D12_FEATURE_DATA_D3D12_OPTIONS1,
};

const S_ENDPGM: u32 = 0xBF81_0000;

/// SOP1 `s_mov_b32 sdst, src0`.
const fn s_mov_b32(sdst: u8, src0: u8) -> u32 {
    0xBE80_0000 | (sdst as u32) << 16 | 0x03 << 8 | src0 as u32
}

/// SOP2 `op sdst, src0, src1`.
const fn sop2(op: u8, sdst: u8, src0: u8, src1: u8) -> u32 {
    0x8000_0000
        | (op as u32 & 0x7F) << 23
        | (sdst as u32 & 0x7F) << 16
        | (src1 as u32) << 8
        | src0 as u32
}

/// SMEM `s_load_dwordx4 s[dst:dst+3], s[base:base+1], 0` (soffset = null).
const fn s_load_dwordx4(dst: u8, base: u8) -> [u32; 2] {
    [
        0x3D << 26 | 0x02 << 18 | (dst as u32) << 6 | (base as u32 >> 1),
        125 << 25,
    ]
}

/// VOP1 `op vdst, src0`.
const fn vop1(op: u16, vdst: u8, src0: u16) -> u32 {
    0x7E00_0000 | (vdst as u32) << 17 | (op as u32) << 9 | (src0 as u32 & 0x1FF)
}

/// VOP2 `op vdst, src0, vsrc1`.
const fn vop2(op: u8, vdst: u8, src0: u16, vsrc1: u8) -> u32 {
    (op as u32 & 0x3F) << 25 | (vdst as u32) << 17 | (vsrc1 as u32) << 9 | (src0 as u32 & 0x1FF)
}

/// VOP3 two-source `op dst, src0, src1`.
const fn vop3_2(op: u16, dst: u8, src0: u16, src1: u16) -> [u32; 2] {
    [
        0x35 << 26 | (op as u32 & 0x3FF) << 16 | dst as u32,
        (src0 as u32 & 0x1FF) | (src1 as u32 & 0x1FF) << 9,
    ]
}

/// MUBUF `buffer_store_dword vdata, vaddr/off, s[srsrc:srsrc+3], 0`.
const fn buffer_store_dword(vdata: u8, vaddr: u8, srsrc: u8, offen: bool) -> [u32; 2] {
    [
        0x38 << 26 | 0x1C << 18 | (offen as u32) << 12,
        0x80 << 24 | ((srsrc as u32 >> 2) & 0x1F) << 16 | (vdata as u32) << 8 | vaddr as u32,
    ]
}

/// EXP `exp mrt0, v0, v1, v2, v3, done`.
const fn exp_mrt0_done(v0: u8, v1: u8, v2: u8, v3: u8) -> [u32; 2] {
    [
        0x3E << 26 | 1 << 11 | 0xF,
        v0 as u32 | (v1 as u32) << 8 | (v2 as u32) << 16 | (v3 as u32) << 24,
    ]
}

const fn sgpr(n: u8) -> u16 {
    n as u16
}

const fn vgpr(n: u8) -> u16 {
    256 + n as u16
}

/// Inline integer constant 0..=64 (GFX10 operand codes 128..=192).
const fn iconst(n: u8) -> u16 {
    128 + n as u16
}

fn lower_and_validate(native: &NativeProgram, outputs: Vec<SigElement>) -> HostShader {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let res = resource::track(&p, &native.layout, native.stage).expect("resource::track");
    let io = io::raise(&p, native).expect("io::raise");
    let st = structure::structure(&p).expect("structure");
    let lowered = lower::lower(&lower::Parts {
        native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs,
        prims: Vec::new(),
        mesh_stride: 0,
    })
    .expect("lower");

    let shader = HostShader {
        stage: native.stage,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        stride_table: lowered.stride_table,
    };

    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        let ours = copy[4..20].to_vec();
        v.validate(&mut copy).expect("DXIL validation failed");
        assert_eq!(copy[4..20], ours[..], "container hash mismatch");
    }

    shader
}

fn build_cs(wave64: bool, threads: [u32; 3], lds_granules: u32, body: &[u32]) -> HostShader {
    let mut words = Vec::new();
    // s[4:5] = descriptor table address (heap index 0 -> byte offset 0).
    words.push(s_mov_b32(5, 0x80));
    words.extend_from_slice(&s_load_dwordx4(8, 4));
    words.extend_from_slice(body);
    words.push(S_ENDPGM);

    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    // rsrc2: 5 user SGPRs (s0..s4), thread_id x+y enabled (bits 12:11 = 1), LDS granules at bit 15.
    let rsrc2 = (5 << 1) | (1 << 11) | (lds_granules << 15);
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64,
        vgprs: 16,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo { rsrc2, threads }),
        code,
    };
    lower_and_validate(&native, Vec::new())
}

fn supports_wave(host: &Host, wave: u32) -> bool {
    let mut opt = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
    // SAFETY: plain D3D12 feature query with valid out pointer.
    let ok = unsafe {
        host.device.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS1,
            (&raw mut opt).cast(),
            size_of_val(&opt) as u32,
        )
    };
    ok.is_ok()
        && opt.WaveOps.as_bool()
        && opt.WaveLaneCountMin <= wave
        && wave <= opt.WaveLaneCountMax
}

fn dispatch_on_all(
    shader: &HostShader,
    constants: [u32; 4],
    out_dwords: u32,
    required_wave: Option<u32>,
) -> Vec<(String, Vec<u32>)> {
    let mut results = Vec::new();
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        if let Some(w) = required_wave
            && !supports_wave(&host, w)
        {
            eprintln!("{label}: does not support wave size {w}; skipped");
            continue;
        }
        let (root, pso) = host
            .compute_pipeline(shader)
            .unwrap_or_else(|e| panic!("{label}: compute_pipeline failed: {e}"));
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            ud[u.index as usize] = match (u.root, u.slot) {
                (RootType::Constants, 0) => constants[u.dword as usize],
                (RootType::Table, 1) => 0,
                other => panic!("unexpected user data {other:?}"),
            };
        }
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(out_dwords)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
        results.push((label.to_string(), out[0].clone()));
    }
    assert!(!results.is_empty(), "no compatible SM 6.6 device available");
    results
}

/// Minimal pass-through vertex shader forwarding `POSITION` to `SV_Position`.
fn passthrough_vs() -> HostShader {
    let mut s = Shader::new(false);
    s.inputs.push(SigElement {
        name: "POSITION".into(),
        index: 0,
        kind: 0,
        comp: 9,
        interp: 0,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 15,
        half: false,
    });
    s.outputs.push(SigElement {
        name: "SV_Position".into(),
        index: 0,
        kind: 3,
        comp: 9,
        interp: 4,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 15,
        half: false,
    });
    let b = s.entry;
    for col in 0..4u8 {
        let undef = s.m.undef(s.t.i32);
        let args = [s.m.i32(0), s.m.i32(0), s.m.i8(col), undef];
        let v = s.op(b, "loadInput.f32", Attr::ReadNone, s.t.f32, 4, &args);
        let out = [s.m.i32(0), s.m.i32(0), s.m.i8(col), v];
        s.op(b, "storeOutput.f32", Attr::NoUnwind, s.t.void, 5, &out);
    }
    s.m.terminate(b, Term::Ret);
    HostShader {
        stage: HwStage::Vs,
        dxil: s.finish(),
        user_data: Vec::new(),
        samplers: Vec::new(),
        static_samplers: Vec::new(),
        root_cbvs: Vec::new(),
        root_srvs: Vec::new(),
        stride_table: false,
    }
}

/// A Wave64 compute shader whose thread group has at most 32 lanes leaves
/// lanes 32..63 permanently inactive on hardware, so lowering declares
/// WaveSize(32) and runs on Wave32-only GPUs (e.g. NVIDIA) bit-for-bit.
#[test]
fn wave64_small_group_runs_on_wave32_host() {
    let mut body = Vec::new();
    // Compute lane index via v_mbcnt_lo/hi(-1, 0) -> v2 (requires console_lanes).
    body.extend_from_slice(&vop3_2(0x365, 2, 193, iconst(0))); // v_mbcnt_lo_u32_b32 v2, -1, 0
    body.extend_from_slice(&vop3_2(0x366, 2, 193, vgpr(2))); // v_mbcnt_hi_u32_b32 v2, -1, v2
    // v3 = lane + s0 (root constant).
    body.push(vop2(0x25, 3, sgpr(0), 2)); // v_add_nc_u32 v3, s0, v2
    // Broadcast lane 5's v3 to s6 via v_readlane_b32.
    body.extend_from_slice(&vop3_2(0x360, 6, vgpr(3), iconst(5))); // v_readlane_b32 s6, v3, 5
    // Pack (lane << 16) | s6 into v4.
    body.push(vop2(0x1A, 4, iconst(16), 2)); // v_lshlrev_b32 v4, 16, v2
    body.push(vop2(0x1C, 4, sgpr(6), 4)); // v_or_b32 v4, s6, v4
    // Byte offset = lane << 2 in v5; store v4 to UAV[lane].
    body.push(vop2(0x1A, 5, iconst(2), 2)); // v_lshlrev_b32 v5, 2, v2
    body.extend_from_slice(&buffer_store_dword(4, 5, 8, true));

    let shader = build_cs(true, [8, 4, 1], 0, &body);
    let base = 100u32;
    for (label, words) in dispatch_on_all(&shader, [base, 0, 0, 0], 32, Some(32)) {
        for (lane, &got) in words.iter().enumerate() {
            let want = ((lane as u32) << 16) | (base + 5);
            assert_eq!(
                got, want,
                "{label}: lane {lane} = {got:#010x}, want {want:#010x}"
            );
        }
    }
}

/// A 64-thread Wave64 compute shader that only uses `v_writelane_b32` /
/// `v_readlane_b32` to spill SGPRs into VGPR lanes (even lanes >= 32) needs
/// neither `WaveSize(64)` nor wave operations once spills resolve in SSA.
#[test]
fn wave64_64thread_sgpr_spills_run_on_wave32_host() {
    let mut body = Vec::new();
    // Spill s0 to v5 lane 3 and s1 to v5 lane 40 (>= 32!).
    body.extend_from_slice(&vop3_2(0x361, 5, sgpr(0), iconst(3))); // v_writelane_b32 v5, s0, 3
    body.extend_from_slice(&vop3_2(0x361, 5, sgpr(1), iconst(40))); // v_writelane_b32 v5, s1, 40
    // Restore both spilled SGPRs and sum them.
    body.extend_from_slice(&vop3_2(0x360, 6, vgpr(5), iconst(3))); // v_readlane_b32 s6, v5, 3
    body.extend_from_slice(&vop3_2(0x360, 7, vgpr(5), iconst(40))); // v_readlane_b32 s7, v5, 40
    body.push(sop2(0x00, 6, 6, 7)); // s_add_u32 s6, s6, s7
    body.push(vop1(0x001, 2, sgpr(6))); // v_mov_b32 v2, s6
    body.extend_from_slice(&buffer_store_dword(2, 0, 8, false));

    let shader = build_cs(true, [8, 8, 1], 0, &body);
    for (label, words) in dispatch_on_all(&shader, [0x1234_0000, 0x0000_5678, 0, 0], 1, None) {
        assert_eq!(words[0], 0x1234_5678, "{label}: spilled SGPR sum mismatch");
    }
}

/// A 64-thread Wave64 compute shader with a non-zero LDS header field but no
/// LDS instructions does not depend on single-wave LDS lockstep.
#[test]
fn wave64_unused_lds_header_does_not_force_wave64() {
    let mut body = Vec::new();
    body.push(vop1(0x001, 2, sgpr(0))); // v_mov_b32 v2, s0
    body.extend_from_slice(&buffer_store_dword(2, 0, 8, false));

    let shader = build_cs(true, [8, 8, 1], 1, &body); // 512 bytes LDS declared, unused
    for (label, words) in dispatch_on_all(&shader, [0xDEAD_BEEF, 0, 0, 0], 1, None) {
        assert_eq!(words[0], 0xDEAD_BEEF, "{label}: output mismatch");
    }
}

/// Pixel shaders that spill SGPRs into VGPR lanes (`v_writelane_b32` /
/// `v_readlane_b32`) must lower without failing as cross-lane operations.
#[test]
fn pixel_shader_sgpr_spill_lowers_and_draws() {
    let mut words = Vec::new();
    // Spill root constants s0, s1 into v5 lanes 2 and 35, then restore and export.
    words.extend_from_slice(&vop3_2(0x361, 5, sgpr(0), iconst(2)));
    words.extend_from_slice(&vop3_2(0x361, 5, sgpr(1), iconst(35)));
    words.extend_from_slice(&vop3_2(0x360, 6, vgpr(5), iconst(2)));
    words.extend_from_slice(&vop3_2(0x360, 7, vgpr(5), iconst(35)));
    words.push(vop1(0x001, 0, sgpr(6)));
    words.push(vop1(0x001, 1, sgpr(7)));
    words.push(vop1(0x001, 2, iconst(0)));
    words.push(vop1(0x001, 3, iconst(0)));
    words.extend_from_slice(&exp_mrt0_done(0, 1, 2, 3));
    words.push(S_ENDPGM);

    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let native = NativeProgram {
        stage: HwStage::Ps,
        wave64: true,
        vgprs: 8,
        layout: vec![RootParam {
            root: RootType::Constants,
            table_offset_dwords: 0,
            sgprs: 4,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: Vec::new(),
        io: StageIo::Ps {
            inputs: Vec::new(),
            input_addr: 0,
            db_shader_control: 0,
            patches: Vec::new(),
        },
        code,
    };
    let outputs = vec![SigElement {
        name: "SV_Target".into(),
        index: 0,
        kind: 16,
        comp: 9,
        interp: 0,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 15,
        half: false,
    }];
    let ps = lower_and_validate(&native, outputs);
    let vs = passthrough_vs();
    let vertices = [
        -1.0f32, -1.0, 0.5, 1.0, //
        -1.0, 3.0, 0.5, 1.0, //
        3.0, -1.0, 0.5, 1.0,
    ];
    let k = [0.25f32.to_bits(), 0.75f32.to_bits(), 0, 0];
    let mut ud = vec![0u32; ps.user_data_dwords().max(1) as usize];
    for u in &ps.user_data {
        ud[u.index as usize] = k[u.dword as usize];
    }

    let mut ran = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            continue;
        };
        let (root, pso) = host
            .pipeline(&vs, &ps)
            .unwrap_or_else(|e| panic!("{label}: pipeline failed: {e}"));
        let texels = host
            .draw(
                &root,
                &pso,
                &Draw {
                    roots: vec![(1, Root::Constants(ud.clone()))],
                    textures: &[],
                    samplers: &[],
                    vertices: &vertices,
                    stride: 16,
                    size: (1, 1),
                    mesh_groups: 0,
                },
            )
            .unwrap_or_else(|e| panic!("{label}: draw failed: {e}"));
        assert_eq!(texels[0][0], 0.25, "{label}: R channel");
        assert_eq!(texels[0][1], 0.75, "{label}: G channel");
        ran += 1;
    }
    assert!(ran > 0, "no SM 6.6 device available");
}
