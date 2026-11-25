//! One frame from two: the title's presented texture (its UI and video) and
//! the render target this frame drew the world into, summed on the CPU and
//! uploaded as the frame to present. While the title's own composite of the
//! two does not reach the screen, this is what shows the world with its UI
//! (`VICTORY_PRESENT_COMPOSITE=1`).

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// Whether [`frame`] composes the presented frame.
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PRESENT_COMPOSITE").is_some())
}

/// The frame to present instead of `ui`: `ui` summed over the world target
/// this frame drew, both 8-bit RGBA. None if either is missing or another
/// format (the caller presents the title's own frame then).
pub(crate) fn frame(
    host: &crate::host::Host,
    ui: &ID3D12Resource,
    ui_state: u32,
) -> Option<(ID3D12Resource, u32)> {
    use std::sync::atomic::Ordering::Relaxed;
    let world = crate::capture::take_present_target()?;
    // SAFETY: live textures.
    let (ud, wd) = unsafe { (ui.GetDesc(), world.texture.GetDesc()) };
    let uf = ud.Format.0 as u32;
    let wf = wd.Format.0 as u32;
    // 27/28/29: 8-bit RGBA (typeless, unorm, sRGB). 10/11: 16-bit RGBA halves
    // (float, unorm) - the world's HDR scene is one of these.
    let known = |f: u32| matches!(f, 27..=29 | 10 | 11);
    if !known(uf)
        || !known(wf)
        || ud.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D
        || wd.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D
    {
        return None;
    }
    // Bytes per pixel and a reader to 8-bit RGB.
    let bpp = |f: u32| if f >= 27 { 4 } else { 8 };
    let rgb = |format: u32, row: &[u8], x: usize, o: &mut [u8; 3]| match format {
        10 => {
            // HDR: Reinhard, so a bright scene does not sum with the UI to
            // white.
            let at = &row[x * 8..][..8];
            for (c, slot) in o.iter_mut().enumerate() {
                let h = u16::from_le_bytes([at[c * 2], at[c * 2 + 1]]);
                let v = half(h).max(0.0);
                *slot = (v / (1.0 + v) * 255.0) as u8;
            }
        }
        11 => {
            let at = &row[x * 8..][..8];
            for (c, slot) in o.iter_mut().enumerate() {
                let v = u16::from_le_bytes([at[c * 2], at[c * 2 + 1]]);
                *slot = (u32::from(v) * 255 / 65535) as u8;
            }
        }
        _ => {
            let at = &row[x * 4..][..3];
            o.copy_from_slice(at);
        }
    };
    let (uw, uh) = (ud.Width as usize, ud.Height as usize);
    let (ww, wh) = (wd.Width as usize, wd.Height as usize);
    if uw == 0 || uh == 0 || ww == 0 || wh == 0 {
        return None;
    }
    let list = host.new_list().ok()?;
    let world_state = D3D12_RESOURCE_STATES(world.state.load(Relaxed) as i32);
    let ui_state = D3D12_RESOURCE_STATES(crate::host::legal_state(ui, ui_state) as i32);
    let copy = D3D12_RESOURCE_STATE_COPY_SOURCE;
    // SAFETY: an open list, live textures, read-back buffers of our device.
    let read = unsafe {
        if ui_state != copy {
            list.list
                .ResourceBarrier(&[crate::host::transition(ui, ui_state, copy)]);
        }
        if world_state != copy {
            list.list.ResourceBarrier(&[crate::host::transition(
                &world.texture,
                world_state,
                copy,
            )]);
        }
        let read = (
            crate::capture::read_back(&host.device, &list.list, ui),
            crate::capture::read_back(&host.device, &list.list, &world.texture),
        );
        if world_state != copy {
            list.list.ResourceBarrier(&[crate::host::transition(
                &world.texture,
                copy,
                world_state,
            )]);
        }
        if ui_state != copy {
            list.list
                .ResourceBarrier(&[crate::host::transition(ui, copy, ui_state)]);
        }
        read
    };
    list.close();
    host.execute(&[&list]);
    let (Some((ui_buffer, ui_fp)), Some((world_buffer, world_fp))) = read else {
        return None;
    };
    let mut out = vec![0u8; uw * uh * 4];
    // SAFETY: the copies finished; both mappings span their footprints.
    unsafe {
        let mut ui_at = std::ptr::null_mut();
        let mut world_at = std::ptr::null_mut();
        if ui_buffer.Map(0, None, Some(&mut ui_at)).is_err()
            || world_buffer.Map(0, None, Some(&mut world_at)).is_err()
        {
            return None;
        }
        let ui_pitch = ui_fp.Footprint.RowPitch as usize;
        let world_pitch = world_fp.Footprint.RowPitch as usize;
        let (ub, wb) = (bpp(uf), bpp(wf));
        let ui_rows = std::slice::from_raw_parts(ui_at.cast::<u8>(), ui_pitch * uh);
        let world_rows = std::slice::from_raw_parts(world_at.cast::<u8>(), world_pitch * wh);
        for y in 0..uh {
            let wy = y * wh / uh;
            let ui_row = &ui_rows[y * ui_pitch..][..uw * ub];
            let world_row = &world_rows[wy * world_pitch..][..ww * wb];
            let out_row = &mut out[y * uw * 4..][..uw * 4];
            for x in 0..uw {
                let wx = x * ww / uw;
                let (mut u, mut w) = ([0u8; 3], [0u8; 3]);
                rgb(uf, ui_row, x, &mut u);
                rgb(wf, world_row, wx, &mut w);
                for c in 0..3 {
                    out_row[x * 4 + c] = u[c].saturating_add(w[c]);
                }
                out_row[x * 4 + 3] = 0xFF;
            }
        }
        ui_buffer.Unmap(0, None);
        world_buffer.Unmap(0, None);
    }
    // The upload: a mapped buffer of the frame, copied into a texture kept
    // for this size.
    static OUTPUT: std::sync::Mutex<Option<(ID3D12Resource, u32, u32)>> =
        std::sync::Mutex::new(None);
    let mut output = OUTPUT.lock().unwrap_or_else(|e| e.into_inner());
    let texture = match output.as_ref() {
        Some((texture, w, h)) if *w == uw as u32 && *h == uh as u32 => texture.clone(),
        _ => {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: uw as u64,
                Height: uh as u32,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
                ..Default::default()
            };
            let mut made: Option<ID3D12Resource> = None;
            // SAFETY: plain creation of a texture in the copy destination state.
            let ok = unsafe {
                host.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                        None,
                        &mut made,
                    )
                    .is_ok()
            };
            if !ok {
                return None;
            }
            let texture = made?;
            *output = Some((texture.clone(), uw as u32, uh as u32));
            texture
        }
    };
    // SAFETY: a mapped upload buffer of the frame, an open list of ours.
    unsafe {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: (uw * uh * 4) as u64,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut upload: Option<ID3D12Resource> = None;
        host.device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_GENERIC_READ,
                None,
                &mut upload,
            )
            .ok()?;
        let upload = upload?;
        let mut at = std::ptr::null_mut();
        upload.Map(0, None, Some(&mut at)).ok()?;
        std::ptr::copy_nonoverlapping(out.as_ptr(), at.cast::<u8>(), out.len());
        upload.Unmap(0, None);
        let list = host.new_list().ok()?;
        let src = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&upload),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                        Width: uw as u32,
                        Height: uh as u32,
                        Depth: 1,
                        RowPitch: (uw * 4) as u32,
                    },
                },
            },
        };
        let dst = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&texture),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        list.list
            .CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        list.close();
        host.execute(&[&list]);
        // The upload buffer's list is done; the texture is in COPY_DEST.
    }
    Some((texture, D3D12_RESOURCE_STATE_COPY_DEST.0 as u32))
}

/// A 16-bit half as a float.
fn half(h: u16) -> f32 {
    let (sign, exp, man) = ((h >> 15) as u32, ((h >> 10) & 0x1f) as i32, (h & 0x3ff) as f32);
    let v = match exp {
        0 => man * 2f32.powi(-24),
        31 => f32::INFINITY,
        e => (1.0 + man / 1024.0) * 2f32.powi(e - 15),
    };
    if sign == 1 { -v } else { v }
}