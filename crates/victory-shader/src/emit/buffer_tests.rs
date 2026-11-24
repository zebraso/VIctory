//! Synthetic GFX10 -> DXIL tests for the runtime stride table
//! (docs/runtime-stride.md): a structured buffer access whose stride the
//! analysis lost takes it from the layer's table (`t0, space 2`) and
//! addresses the element as `index * stride + off`, instead of failing or
//! reading the instruction's offset. Checked in the DXIL validator and on
//! the GPU (hardware and WARP), where the table holds two different
//! strides.

use super::host::{Dispatch, Host, Root, Stride, View};
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

/// SOP1 `s_mov_b32 sdst, src0` (an inline constant or SGPR code).
const fn s_mov_b32(sdst: u8, src0: u16) -> u32 {
    0xBE80_0000 | (sdst as u32) << 16 | 0x03 << 8 | (src0 as u32 & 0xFF)
}

/// SOP2 `op sdst, src0, src1` (inline constants or SGPR codes).
const fn sop2(op: u8, sdst: u8, src0: u16, src1: u16) -> u32 {
    0x8000_0000
        | (op as u32 & 0x7F) << 23
        | (sdst as u32 & 0x7F) << 16
        | (src1 as u32 & 0xFF) << 8
        | (src0 as u32 & 0xFF)
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

/// MUBUF `buffer_load_dword vdata, vaddr, s[srsrc:srsrc+3], 0 idxen`.
const fn buffer_load_dword(vdata: u8, vaddr: u8, srsrc: u8, idxen: bool) -> [u32; 2] {
    [
        0x38 << 26 | 0x0C << 18 | (idxen as u32) << 13,
        0x80 << 24 | ((srsrc as u32 >> 2) & 0x1F) << 16 | (vdata as u32) << 8 | vaddr as u32,
    ]
}

/// MUBUF `buffer_store_dword vdata, off, s[srsrc:srsrc+3], 0 offset:0`.
const fn buffer_store_dword(vdata: u8, srsrc: u8) -> [u32; 2] {
    [
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | ((srsrc as u32 >> 2) & 0x1F) << 16 | (vdata as u32) << 8,
    ]
}

/// Inline integer constant.
const fn iconst(n: u8) -> u16 {
    128 + n as u16
}

/// One dword of the test buffer's structured elements: `stride / 4` apart.
const fn srv_dword(i: usize) -> u32 {
    0x1000_0000 + i as u32
}

/// A 1-thread compute shader: `buffer_load_dword v1, v0, s[8:11], 0 idxen`
/// from the table's descriptor at slot 0 (its stride unknown to the
/// analysis), then `buffer_store_dword v1, 0, s[12:15], 0` to the raw UAV
/// at slot 1. `s0` is the element index.
fn synthetic_program() -> NativeProgram {
    let mut words = Vec::new();
    // s[4:5] = table address (slot 0 at byte 0); s[6:7] = slot 1 (+32).
    words.push(s_mov_b32(5, iconst(0)));
    words.push(sop2(0x00, 6, 4, iconst(32))); // s_add_u32 s6, s4, 32
    words.push(s_mov_b32(7, iconst(0)));
    words.extend_from_slice(&s_load_dwordx4(8, 4));
    words.extend_from_slice(&s_load_dwordx4(12, 6));
    words.push(vop1(0x001, 0, 0)); // v_mov_b32 v0, s0 (the element index)
    words.extend_from_slice(&buffer_load_dword(1, 0, 8, true));
    words.extend_from_slice(&buffer_store_dword(1, 12));
    words.push(S_ENDPGM);

    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
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
        used: vec![
            // The loaded view: structured, with the stride the container's
            // declaration lost (0 here).
            UsedResource {
                kind: ResourceType::Srv,
                root: RootType::Table,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 0,
                dwords: 4,
                user_data: 4,
                slot: 1,
            },
            // The storing view: raw.
            UsedResource {
                kind: ResourceType::Uav,
                root: RootType::Table,
                table_offset_dwords: 8,
                lower_bound: 0,
                stride: 0,
                dwords: 4,
                user_data: 4,
                slot: 1,
            },
        ],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1, // 5 user SGPRs: s0..s4
            threads: [1, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// Lowers [`synthetic_program`] and returns it with the DXIL validator's
/// verdict (in-place signed; the container hash must survive).
fn translated() -> HostShader {
    let native = synthetic_program();
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let res = resource::track(&p, &native.layout, native.stage).expect("resource::track");
    let io = io::raise(&p, &native).expect("io::raise");
    let st = structure::structure(&p).expect("structure");
    let l = lower::lower(&lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    })
    .expect("a structured buffer of unknown stride now reads the table");
    assert!(l.stride_table, "the program must declare the stride table");
    let shader = HostShader {
        stage: native.stage,
        dxil: l.dxil,
        user_data: l.user_data,
        samplers: l.samplers,
        static_samplers: Vec::new(),
        root_cbvs: l.root_cbvs,
        root_srvs: l.root_srvs,
        stride_table: l.stride_table,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        let ours = copy[4..20].to_vec();
        v.validate(&mut copy).expect("DXIL validation failed");
        assert_eq!(copy[4..20], ours[..], "container hash mismatch");
    }
    shader
}

/// The table answers the stride, so the loose fallback no longer changes
/// what is emitted for this program.
#[test]
fn the_table_replaces_the_loose_buffer_fallback() {
    lower::LOOSE_FOR_TESTS.store(0, std::sync::atomic::Ordering::Relaxed);
    let strict = translated();
    lower::LOOSE_FOR_TESTS.store(1, std::sync::atomic::Ordering::Relaxed);
    let loose = translated();
    lower::LOOSE_FOR_TESTS.store(-1, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        strict.dxil, loose.dxil,
        "VICTORY_LOOSE_BUFFERS changed a program the table answers"
    );
}

/// The load addresses element `index` at `index * table[slot] + off`: the
/// same buffer with two table strides reads different dwords, and the
/// offset-only fallback would read element 0 every time.
#[test]
fn the_load_uses_the_tables_stride() {
    let shader = translated();
    let data: Vec<u32> = (0..16).map(srv_dword).collect();
    // (element index, table stride in bytes, dword the address lands on)
    let cases = [(0u32, 8u32, 0usize), (2, 8, 4), (1, 12, 3), (2, 4, 2)];
    let mut ran = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host
            .compute_pipeline(&shader)
            .unwrap_or_else(|e| panic!("{label}: compute_pipeline failed: {e}"));
        for (index, table_stride, want) in cases {
            let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
            for u in &shader.user_data {
                ud[u.index as usize] = match (u.root, u.slot) {
                    (RootType::Constants, 0) => index,
                    (RootType::Table, 1) => 0,
                    other => panic!("unexpected user data {other:?}"),
                };
            }
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![
                            (0, Root::Constants(ud)),
                            (
                                1,
                                Root::View {
                                    srv: true,
                                    data: vec![table_stride, 0],
                                },
                            ),
                        ],
                        views: &[
                            View::Buffer {
                                data: &data,
                                stride: Stride::Raw,
                            },
                            View::RawUav(1),
                        ],
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
            assert_eq!(
                out[0][0],
                srv_dword(want),
                "{label}: index {index} with stride {table_stride} read dword {:#x}, want {}",
                out[0][0],
                want
            );
        }
        ran += 1;
    }
    assert!(ran > 0, "neither hardware nor WARP SM 6.6 device available");
}
