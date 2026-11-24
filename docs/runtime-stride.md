# Runtime stride for structured buffers (design note)

Status: design. Not implemented in this branch. Closes the translator's
"a structured buffer of unknown stride" (36 programs in the world run) with
the descriptor's real stride instead of the loose fallback (raw view at the
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

1. **Emitter** (`victory-shader`)
   - `handle()` knows the heap slot value; for a structured access with an
     unknown stride, emit a load from the stride table (a root SRV in a new
     space, e.g. `t0, space STRIDE_SPACE`) at `slot * 4`.
   - `HostShader` gains the stride table as a root SRV (like `root_srvs`).
2. **Layer** (`victory-gpu`)
   - when a view is written into a heap slot, store its stride at
     `slot * 4` in a small upload buffer (updated with the slot);
   - bind the buffer as the stride-table root SRV for every pipeline whose
     shader declares it (the pipeline already binds root SRVs).
3. **Cache**: the table is a binding, not part of the translation, so the
   shader cache needs no key change.

## Tests

- `victory-shader`: a synthetic program with `buffer_load_dword ... idxen`
  and an unknown stride emits the table load and `index * stride + off`;
  `VICTORY_LOOSE_BUFFERS` no longer changes the emitted code for it.
- On the AMD iGPU: rerun with `VICTORY_D3D12_ADAPTER=amd` and
  `VICTORY_D3D12_DRED=1`; count `shader does not translate` for this reason
  (was 36 programs) and check the world submission's page fault (was
  `0x3598000`) is gone or moves to a different producer.

## Why not done now

It crosses three modules (emitter DXIL, root signature, heap mirror) and needs
the heap-slot/stride table kept coherent under the title's dynamic descriptor
writes; a partial version would leave the tree unbuildable or, worse, read a
wrong stride silently. The anchors above are the whole change surface.
