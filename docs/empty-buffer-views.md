# Empty buffer views (`NumElements` 0): leave the slot alone

Status: open (2026-11-24). The finding is recorded here because the first
fix attempt blacked the title's screens out and was reverted.

## What the title does

The world load creates a structured UAV (`StructureByteStride` 16,
`FirstElement` 2599384, `NumElements` 0) and more views like it. D3D12
refuses a buffer view whose count is 0 and **removes the device** for it:

```
ID3D12Device::CreateUnorderedAccessView: The Dimensions of the View are
invalid ... NumElements (value = 0) must be between 1 and 8803880 ...
then
ID3D12Device::RemoveDevice: ... (DXGI_ERROR_INVALID_CALL ...)
```

(The driver reports the removal reason as `0x887A0001` on NVIDIA and AMD;
DRED has no page fault for it, and auto-breadcrumbs are empty or complete,
which is what sent the first investigations after the wrong thing.)

## What the layer did before

`descriptor/host.rs::buffer_srv`/`buffer_uav` passed the 0 straight to
`Create*View`. `windows-rs` returns `()` there, the call failed, the slot
kept whatever descriptor it held (the null view from `fill_null`, or an
earlier view of that slot) and `make` still reported success, so nothing
else happened. The title's video and menus worked in that state.

## What not to do

* **"0 means to the end of the buffer"** (`231be5c`): every presented
  frame went solid black (30 fps, 10 draws a frame still going through;
  `VICTORY_CAPTURE` frames 1..512 are one 6 KB PNG of zeros, mean 0.0).
* **A null view for it** (`2bd7726`): the same black screens.
* Clamping a count past the end (`231be5c`): same class of change, same
  risk.

Bisection that proved it: `2f3836b` (the stride-table commit) presents
content at frames 128/256/512 (mean 109/247/108 with `VICTORY_CAPTURE`);
the two fixes above present black. `VICTORY_KEYS` (the scripted Enter) and
the shader cache are innocent (same captures with and without them).

## What to do

Do not hand D3D12 a view it will refuse, and do not change what the slot
holds for one: leave the descriptor **and the host resource it keeps**
(`HostHeap::kept`) as they are. The mirror needs a "nothing to make, keep
the slot" outcome distinct from both "made" and "failed" (the current
`Option<Kept>` cannot say it), and the check has to happen after the host
copy's offset is folded into `FirstElement` (an invalid view is one that
does not fit the host copy, not just the title's buffer).

`VICTORY_D3D12_DEBUG=1` names it (`CreateUnorderedAccessView`), and
`VICTORY_CAPTURE=<dir>` with a brightness check on the PNGs is the fastest
way to tell a black presentation from a black window.
