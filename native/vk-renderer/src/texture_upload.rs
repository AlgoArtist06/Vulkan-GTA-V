//! Checked copy layout shared by the real texture-upload path and regression tests.

pub(crate) fn validate_upload(
    extent: [u32; 3], origin: [u32; 3], copy: [u32; 3],
    block: [u32; 3], bytes_per_row: u32, rows_per_image: u32, payload: usize,
) -> Result<(), &'static str> {
    let [width, height, depth] = copy;
    let [block_bytes, block_width, block_height] = block;
    if width == 0 || height == 0 || depth == 0 || bytes_per_row == 0 || rows_per_image == 0
        || block_bytes == 0 || block_width == 0 || block_height == 0 {
        return Err("empty upload or invalid block layout");
    }
    if origin[0] % block_width != 0 || origin[1] % block_height != 0
        || width % block_width != 0 || height % block_height != 0 {
        return Err("texture upload is not block aligned");
    }
    // Compressed mip tails occupy a physical block even when their logical
    // width/height is less than four texels. Array layers are not rounded.
    for axis in 0..3 {
        let alignment = [block_width, block_height, 1][axis] as u64;
        let physical_extent = (extent[axis] as u64).div_ceil(alignment) * alignment;
        if origin[axis] as u64 + copy[axis] as u64 > physical_extent {
            return Err("upload region is outside texture");
        }
    }
    let rows = height / block_height;
    let row_bytes = u64::from(width / block_width) * u64::from(block_bytes);
    if u64::from(bytes_per_row) < row_bytes || bytes_per_row % block_bytes != 0 || rows_per_image < rows {
        return Err("texture upload row layout is too small or unaligned");
    }
    // Padding after the final row/image is not read by write_texture.
    let required = u64::from(bytes_per_row).checked_mul(u64::from(rows_per_image))
        .and_then(|slice| slice.checked_mul(u64::from(depth - 1)))
        .and_then(|bytes| bytes.checked_add(u64::from(bytes_per_row) * u64::from(rows - 1)))
        .and_then(|bytes| bytes.checked_add(row_bytes))
        .ok_or("texture upload byte range overflows")?;
    if required > payload as u64 { return Err("texture upload payload is shorter than its row layout"); }
    Ok(())
}


