pub(crate) fn native_rect(enabled: bool, rect: Option<[u32;4]>, width: u32, height: u32) -> Result<Option<[u32;4]>, String> {
    if width == 0 || height == 0 { return Err("scissor attachment has zero extent".into()); }
    if !enabled { return Ok(None); }
    // The ring carries the bit patterns of signed D3D RECT coordinates,
    // with exclusive right/bottom edges. Native API requires x/y/w/h
    // inside the attachment. Intersect without changing visible pixels.
    let r = rect.unwrap_or([0;4]);
    let clip = |v: u32, bound: u32| (i64::from(v as i32)).clamp(0, i64::from(bound)) as u32;
    let x = clip(r[0], width); let y = clip(r[1], height);
    let right = clip(r[2], width).max(x); let bottom = clip(r[3], height).max(y);
    Ok(Some([x,y,right-x,bottom-y]))
}

