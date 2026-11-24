# Runtime stride for structured buffers

Status: implemented (2026-11-24). Closes the translator's "a structured
buffer of unknown stride" (36 programs in the world run) with the
descriptor's real stride instead of the loose fallback (raw view at the
instruction offset, which reads the wrong element).

## Why a channel is needed

The console's `buffer_load_dword` with `idxen` addresses a structured buffer
as `base + index * stride + off`. The stride is a field of the title's S#
descriptor, known only at bind time. In DXIL the access is
`dx.op.rawBufferLoad(handle, address, off, mask, align)` with a byte
`address`, so the emitted code needs the stride as a value:

- today, `lower.rs` uses `used.stride` from the shader analysis; when it is
  `None` the program fails (`a structured buffer of unknown stride`) or, under
  `VICTORY_LOOSE_BUFFERS`, is viewed raw at `off` (wrong element);
- the stride cannot be baked in the view: `dx.op.bufferLoad` takes an element
  index only for *typed* views, never a structure stride;
- the console descriptor's own dwords are virtualized away (the emitter turns
  the S# into a heap handle, it never holds the raw words).

## The channel

**A stride table**: a root SRV (`RawBuffer` of `uint`), one dword per host
view-heap slot. The layer owns the heap (`descriptor/host.rs`) and already
parses each view's `StructureByteStride` (`element()` at `descriptor/host.rs`
reads `Anonymous.Buffer.StructureByteStride`), so it can keep the table in sync
with the heap. The shader reads the stride of the view it is about to address:

```
stride = strideTable[heapSlot]           // emitted raw buffer load
address = index * stride + off           // replaces the loose fallback
```

`heapSlot` is the same value the emitter already computes with
`heap_index(load, first)` (`lower.rs`) to form the descriptor handle.

## Data flow

1. **Emitter** (`victory-shader`, as built)
   - `lower.rs::runtime_stride()` takes the slot from `heap_index(load,
     first)` (loaded descriptors and phis of them), loads `slot * 4` from the
     table (`rawBufferLoad`, mask 1) and `at_stride()` forms
     `index * stride + off`; the three access sites (load, store, atomic)
     then use that byte address on a raw view of the same slot. Only a
     descriptor that is not a heap descriptor falls back to the old
     `VICTORY_LOOSE_BUFFERS` path (or fails).
   - `shader.rs::stride_table_handle()` declares `t0, space 2`; the
     container's `dx.resources` and `PSV0` list it, and
     `HostShader::stride_table` tells the layer to bind it.
2. **Layer** (`victory-gpu`, as built)
   - `descriptor/host.rs::HostHeap` keeps a 4-byte-per-slot upload buffer
     (`Strides`) and writes a view's `StructureByteStride`
     (`View::stride()`, the view description's dword 3) when `mirror()`
     makes the slot, or 0 when it nulls it.
   - `pipeline/host.rs` declares the root SRV (`t0, space 2`) for each stage
     whose shader reads it; `draw.rs::bind()` binds the table of the view
     heap after the draw's table entries were mirrored.
3. **Cache**: the table is a binding, not part of the translation; the
   cached `HostShader` carries the flag (`VSH1` gained one word).

## Tests

- `victory-shader` `emit/buffer_tests.rs`: a synthetic program with
  `buffer_load_dword ... idxen` and an unknown stride emits the table load
  and `index * stride + off` (validated by dxil.dll); `VICTORY_LOOSE_BUFFERS`
  no longer changes the emitted code for it; on hardware and WARP the same
  buffer reads different dwords for table strides 8, 12 and 4.
- On the AMD iGPU: rerun with `VICTORY_D3D12_ADAPTER=amd` and
  `VICTORY_D3D12_DRED=1`; count `shader does not translate` for this reason
  (was 36 programs) and check the world submission's page fault (was
  `0x3598000`) is gone or moves to a different producer.
