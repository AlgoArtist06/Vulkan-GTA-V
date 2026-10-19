#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum StorageFormat { Rgba8Unorm, Rgba8Snorm, Rgba8Uint, Rgba8Sint, R32Float, R32Uint, R32Sint, Rg32Float, Rgba16Float, Rgba32Float }

pub(crate) fn parse_storage_format(value: &str) -> Result<StorageFormat, String> {
    match value { "rgba8unorm" => Ok(StorageFormat::Rgba8Unorm), "rgba8snorm" => Ok(StorageFormat::Rgba8Snorm), "rgba8uint" => Ok(StorageFormat::Rgba8Uint), "rgba8sint" => Ok(StorageFormat::Rgba8Sint), "r32float" => Ok(StorageFormat::R32Float), "r32uint" => Ok(StorageFormat::R32Uint), "r32sint" => Ok(StorageFormat::R32Sint), "rg32float" => Ok(StorageFormat::Rg32Float), "rgba16float" => Ok(StorageFormat::Rgba16Float), "rgba32float" => Ok(StorageFormat::Rgba32Float), value => Err(format!("unsupported storage texture format {value}")) }
}

impl StorageFormat {
    pub(crate) fn wgsl(self) -> &'static str {
        match self {
            Self::Rgba8Unorm => "rgba8unorm", Self::Rgba8Snorm => "rgba8snorm",
            Self::Rgba8Uint => "rgba8uint", Self::Rgba8Sint => "rgba8sint",
            Self::R32Float => "r32float", Self::R32Uint => "r32uint", Self::R32Sint => "r32sint",
            Self::Rg32Float => "rg32float", Self::Rgba16Float => "rgba16float", Self::Rgba32Float => "rgba32float",
        }
    }
}

// Replace only a reflected global's format token. Keep the immutable asset
// intact: a later dispatch can bind a different format to the same shader.
pub(crate) fn specialize(source: &str, name: &str, from: StorageFormat, to: StorageFormat) -> Result<String, String> {
    let scan = mask_comments(source)?;
    let mut range = None;
    for (offset, _) in scan.match_indices("var") {
        let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
        if offset > 0 && ident(scan.as_bytes()[offset-1]) { continue; }
        if scan.as_bytes().get(offset+3).is_some_and(|b| ident(*b)) { continue; }
        let declaration = scan[offset+3..].trim_start();
        let Some((identifier, type_text)) = declaration.split_once(':') else { continue };
        if identifier.trim() != name { continue; }
        let type_text = type_text.trim_start();
        if !type_text.starts_with("texture_storage_") { continue; }
        let open = type_text.find('<').ok_or_else(|| format!("storage declaration {name} lacks format"))?;
        let comma = type_text[open+1..].find(',').map(|v| v+open+1).ok_or_else(|| format!("storage declaration {name} lacks access"))?;
        let format_text = &type_text[open+1..comma];
        if format_text.trim() != from.wgsl() { return Err(format!("storage declaration {name} format differs from reflection")); }
        let begin = type_text.as_ptr() as usize - scan.as_ptr() as usize + open + 1 + format_text.len() - format_text.trim_start().len();
        let end = begin + format_text.trim().len();
        if range.replace(begin..end).is_some() { return Err(format!("ambiguous storage declaration {name}")); }
    }
    let range = range.ok_or_else(|| format!("storage declaration {name} not found"))?;
    let mut output = source.to_owned();
    output.replace_range(range, to.wgsl());
    Ok(output)
}

fn mask_comments(source: &str) -> Result<String, String> {
    let mut bytes = source.as_bytes().to_vec();
    let mut i = 0;
    while i+1 < bytes.len() {
        if &bytes[i..i+2] == b"//" {
            while i < bytes.len() && bytes[i] != b'\n' { bytes[i] = b' '; i += 1; }
        } else if &bytes[i..i+2] == b"/*" {
            let mut depth = 1;
            bytes[i..i+2].fill(b' '); i += 2;
            while i < bytes.len() && depth > 0 {
                if i+1 < bytes.len() && &bytes[i..i+2] == b"/*" { depth += 1; bytes[i..i+2].fill(b' '); i += 2; }
                else if i+1 < bytes.len() && &bytes[i..i+2] == b"*/" { depth -= 1; bytes[i..i+2].fill(b' '); i += 2; }
                else { if bytes[i] != b'\n' { bytes[i] = b' '; } i += 1; }
            }
            if depth != 0 { return Err("unterminated WGSL block comment".into()); }
        } else { i += 1; }
    }
    String::from_utf8(bytes).map_err(|error| format!("WGSL comment masking: {error}"))
}


