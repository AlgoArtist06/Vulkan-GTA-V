#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Pass { pub source_mip: u32, pub destination_mip: u32, pub layer: u32 }
pub(crate) fn passes(total_mips: u32, total_layers: u32, first_mip: u32, mip_count: u32, first_layer: u32, layer_count: u32) -> Result<Vec<Pass>, String> {
    if first_mip >= total_mips || first_layer >= total_layers { return Err("mipmap view starts outside texture".into()); }
    let mips = if mip_count == 0 || mip_count == u32::MAX { total_mips-first_mip } else { mip_count };
    let layers = if layer_count == 0 || layer_count == u32::MAX { total_layers-first_layer } else { layer_count };
    let mip_end = first_mip.checked_add(mips).filter(|end| *end<=total_mips).ok_or("mipmap view extends outside texture")?;
    let layer_end = first_layer.checked_add(layers).filter(|end| *end<=total_layers).ok_or("mipmap array view extends outside texture")?;
    let mut result=Vec::new();
    let capacity=(mips.saturating_sub(1) as usize).checked_mul(layers as usize).ok_or("mipmap pass count overflow")?;
    result.try_reserve_exact(capacity).map_err(|error| format!("mipmap pass allocation: {error}"))?;
    for layer in first_layer..layer_end { for destination_mip in first_mip+1..mip_end { result.push(Pass{source_mip:destination_mip-1,destination_mip,layer}); } }
    Ok(result)
}


