# Wave64 emulation in `victory-shader` (design note)

Status: plan, based on the implementation that already exists in **Leonida**
(`tools/rdna2spirv.py`, `tools/wave64_emulate_opt.py`, `docs/design.md`).
No code of the emulation itself is in this branch yet.

## Why

Grand Theft Auto VI's console shaders were compiled for RDNA2 wave64. The
translator declares `WaveSize(64)` for compute programs whose lane layout
needs it (`lower.rs`: `console_lanes || one_wave && lds_dwords > 0 && uses_lds`),
and D3D12 refuses such a pipeline on a device whose maximum wave lane count is
below 64:

- NVIDIA (RTX 5080, driver 621.1): max **32** → 111 compute pipelines refused;
- WARP: max **4** (debug layer: "maximum wave lane count for device is 4") → 197 refused.

Forcing `WaveSize(32)` (experiment `VICTORY_WAVE32=1`) makes them run: refusals
111 → 0, but the lane layout is then wrong. The world's GPU-driven passes write
culling/indirect data from wrong lanes, the next passes read garbage, and the
device is removed (`DEVICE_HUNG`, no page fault, the same `u4=0xa6ad00` table in
every DRED report). Issue `Marvisak/VIctory#71` has the same finding.

So the only correct path on a 32-wide host is to **emulate 64 lanes with two
32-lane waves**, exactly as Leonida already does for SPIR-V.

## Reference (Leonida)

`docs/design.md`, "Execution model": one invocation = one lane; a wave = a
subgroup; cross-lane ops (`ds_swizzle`, `v_permlane*`, `v_readlane`/`v_writelane`,
`v_mbcnt`, DPP, ballots) become subgroup shuffles; under `--wave64-emulate`
cross-half exchanges go through the **workgroup buffer** with a barrier
(`Gen.x_round`), and lane-mask control flow is split into continuation segments
(`Gen.x_split`, `wave64_emulate_opt.plan_structured_emulation`). Compute and
mesh shaders are exact on any subgroup size; vertex/pixel/geometry cross-half
ops are still approximate. Tests: `tests/semantic/test_wave64_cross_half.py`,
`runtime/test/shaders/cs_wave64_ballot.asm`, `scripts/bench_wave64.py`.

## Mapping to victory's emitter

victory already models the parts the emulation needs:

| Need | Where |
|---|---|
| 64-bit EXEC / mask model, saveexec, `v_cmp` | `emit/lower.rs` (`Structured`, `mask_dword`, `sections`) |
| ballot of a lane predicate | `waveActiveBallot` (`mask_dword`, `emit/wave.rs`) |
| per-lane values, DPP, `ds_swizzle` | `lower.rs` (`Fields::Valu.dpp`, `Fields::Ds`) |
| workgroup memory | LDS already allocated per program (`lds_dwords`) |
| continuation segments | `sections()` models whole-wave sections today |

Emulation mode (new), enabled when `host_max_wave_lanes < 64` and the program
would declare 64:

1. **Host wave size** = `min(device max, 32)` (32 on NVIDIA, 4 on WARP as a
   diagnostic). Plumb it into the translation cache key.
2. **Two halves per logical wave.** `threads <= 32`: keep today's exact
   `WaveSize(32)` path (PR #54). `threads > 32`: lane `l` runs in host wave
   `l / host_wave`; a logical wave covers `64 / host_wave` host waves.
3. **Cross-half read** (`v_readlane`/`v_writelane` of the other half,
   `v_permlane`, DPP across 32, `ds_swizzle` beyond the half): store the value
   into an LDS staging array indexed by `[logical_lane][slot]`, `s_barrier`,
   read the wanted lane, `s_barrier`. This is Leonida's `x_round`.
4. **Ballots**: each host wave ballots its predicate; the 64-bit mask is
   assembled by exchanging the two 32-bit words through LDS.
5. **EXEC masks used as data**: same exchange; masks that only gate lane-local
   work need no exchange (they are per-lane predicates already).
6. **Mask-controlled control flow**: keep DXIL structured control flow; when a
   mask change crosses halves, recompute the mask after the barrier
   (`x_split` equivalent is only needed where the SPIR-V version switches
   continuations; DXIL `if` + recomputed predicates is the first cut).
7. **Mesh**: mesh shaders are compute-like (workgroup + mesh outputs); the same
   emulation applies once compute is exact.
8. **Vertex/pixel**: keep the current path (approximate cross-half ops), as
   Leonida does.

## Tests

Port the Leonida fixtures as `victory-shader` emit tests plus a WARP run:

- `cs_w64x_readlane`, `cs_w64x_writelane`, `cs_w64x_bpermute`
  (`tests/semantic/test_wave64_cross_half.py` expectations),
- `cs_wave64_ballot` (mask counts, first-set lane),
- DPP16/8 `cs_dpp*` (exchange through the staging buffer),
- a 64-thread group with a cross-half ballot under `WaveSize(4)` on WARP.

## Milestones

1. Host wave-size probe (`D3D12_FEATURE_D3D12_OPTIONS1.WaveLaneCountMax`) + cache
   key + `VICTORY_WAVE64_EMULATE=1` switch.
2. Compute: cross-half read/write + ballot (covers culling/indirect builders).
3. EXEC-as-data and DPP exchanges.
4. Mesh.
5. Run the world with DRED: refusals 0 **and** no `DEVICE_HUNG`.

## Evidence this is the blocker

- `VICTORY_WAVE32=1`: compute refusals 111 → 0, then `DEVICE_HUNG` at
  `dispatch {fb332f777bae57fb, 3a79195f06d3ed85, 512416d975ace48b, …}` all with
  `u4=0xa6ad00` (same placed buffer), i.e. data-dependency hangs, not one bad
  program.
- WARP: max 4 lanes, same refusal class, cannot be used as a wave64 reference.
- Issue `Marvisak/VIctory#71` (comment 7118434961) has the same conclusion.

## Appendix: exchange rules from XCL (`docs/wave64-on-wave32.md`)

XCL implements the same emulation in HLSL (RDNA2 -> HLSL -> DXIL through DXC). Its
rules port to this emitter's structures; the text here is a paraphrase of the
technical facts, not copied code (XCL is GPL-2.0, this tree is not).

One console wave64 = two hardware wave32 halves of the same thread group. Console
lane = console thread % 64, half = bit 5. Per-lane code is unchanged; the halves
meet only where a value must be whole-wave:

| operation | exchange |
|---|---|
| any `ballot()` (`v_cmp*`, `v_cmpx*`, carries, EXEC at entry) | one 32-bit word per half; both halves rebuild the 64-bit mask (skip when the mask is only read lane by lane) |
| `v_readlane_b32`, `v_readfirstlane_b32` (uniform lane index) | one word: each half publishes lane `l & 31`, the half owning `l` wins |
| `ds_bpermute_b32` | 32 words (the whole half) |
| DPP16, DPP8, `ds_swizzle`, `v_permlane(x)16` | none: the ISA keeps them inside a 32-lane half (`WaveReadLaneAt(v, l & 31)`) |
| `s_load*` / `s_buffer_load*` with GLC/DLC | up to 16 words: half 0's values for both halves |

So both halves hold the full 64-bit EXEC/VCC and identical SGPRs, and every
branch is taken the same way: the existing scalar lowering (`s_bcnt1_i32_b64`,
`s_ff1`, `s_cbranch_execz`, `v_mbcnt_hi`, ...) needs no change.

Synchronisation:
- one console wave per group (<= 64 threads): `GroupMemoryBarrierWithGroupSync`
  between publishing and reading the slot; both halves run the same exchange
  sequence, so both reach the barrier;
- several console waves per group (mesh 128 threads, compute > 64): a group
  barrier would also wait for other console waves with different control flow,
  so each wave gets its own slot and a per-wave rendezvous (counter in LDS);
- threads are assigned to halves from a groupshared counter (`xcl_x_thread`):
  a group's threads are interchangeable, but console waves are consecutive
  thread indices, which is not guaranteed per hardware wave (e.g. 8x8x4 groups).

Mapping here: `mask_dword()` / `sections()` publish and combine the ballot words
(replace the single `waveActiveBallot` with the two-half exchange); `v_readlane`
uses the one-word slot; `Fields::Ds`/DPP keep `& 31`; `lds_dwords` needs the slot
space (64 lanes x 4 bytes per wave plus a counter). Milestone 2 of this note.

## Implementation anchors found in this tree

The emitter already has the two inputs the exchange needs, but not the barrier:

- **thread id**: `Input::ThreadIdInGroup` -> `Lower::thread_id()` (lower.rs), which uses
  `threadIdInGroup.i32` (op 95) or `flattenedThreadIdInGroup.i32` (op 96) with the wave base.
  Console lane = `flat % 64`, half = bit 5; both available.
- **workgroup memory**: `lds_dwords` and the `ds_*` lowering already address LDS.
- **barrier: missing**. The emitter never emits `dx.op.barrier` (op 80); `s_barrier` is only
  tracked (`no_barrier` for mesh) and barriers appear only in DXC-compared tests
  (diff_tests.rs). The exchange cannot be emitted before adding `Shader::barrier()` (op 80,
  `Attr::NoDuplicate`, mode for group sync + group memory) and a test that validates the
  module with dxil.dll.
- **mask path**: `mask_dword()` (lower.rs) calls `dx.op.waveActiveBallot` (op 116); under
  emulation it becomes: publish the half's 32-bit ballot to LDS, barrier, combine both words.
- `sections()` (emit/wave.rs) models whole-wave sections per console wave; the rendezvous for
  several waves per group needs a per-wave slot and counter in the same LDS block.

Order of work: (1) barrier intrinsic + validation test, (2) ballot exchange for one console
wave per group, (3) readlane/readfirstlane, (4) ds_bpermute, (5) several waves per group,
(6) mesh.
