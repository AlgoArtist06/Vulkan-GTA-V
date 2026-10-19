//! Validate and preserve D3D render-target slot numbering for native passes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Extent { pub width: u32, pub height: u32, pub samples: u32 }

pub(crate) fn color_outputs(vs: u32, ps: u32, colors: &[u32], depth: u32) -> Result<bool, String> {
    if vs == 0 { return Err("draw has no vertex shader".into()); }
    if depth == 0 && (ps == 0 || !colors.iter().any(|id| *id != 0)) { return Err("draw has no writable color/depth attachment".into()); }
    // With no pixel shader, D3D rasterization can update depth/stencil but
    // cannot write colors. Leave existing color allocations untouched.
    Ok(ps != 0)
}

pub(crate) fn plan(colors: &[Option<Extent>], depth: Option<Extent>) -> Result<(Extent, usize), String> {
    let reference = colors.iter().flatten().next().copied().or(depth).ok_or("draw has no attachments")?;
    if reference.width == 0 || reference.height == 0 || reference.samples == 0 {
        return Err("invalid attachment extent/sample count".into());
    }
    for extent in colors.iter().flatten().copied().chain(depth) {
        if extent != reference { return Err("render attachment extent/sample counts do not match".into()); }
    }
    let count = colors.iter().rposition(Option::is_some).map_or(0, |slot| slot + 1);
    Ok((reference, count))
}

