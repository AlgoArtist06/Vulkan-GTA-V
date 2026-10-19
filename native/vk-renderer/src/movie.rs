//! Bounded video-frame transfer and the TV render-target scope.
use std::ffi::c_void;

pub type MovieFrameCallback = unsafe extern "C" fn(*mut c_void, u32, *mut u8, usize,
    *mut u64, *mut u32, *mut u32) -> i32;

#[derive(Clone, Copy)]
pub struct MovieFrameSource {
    callback: MovieFrameCallback,
    context: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapturedFrame {
    Inactive,
    Unchanged { width: u32, height: u32 },
    New { width: u32, height: u32, serial: u64 },
}

impl MovieFrameSource {
    /// The context and callback must remain valid until the GPU worker stops.
    /// The callback must respect capacity, report dimensions, and only change
    /// the serial after copying a complete frame. It runs on that worker only.
    pub unsafe fn new(callback: MovieFrameCallback, context: *mut c_void) -> Self {
        Self { callback, context: context as usize }
    }

    pub fn capture(&self, hash: u32, rgba: &mut Vec<u8>, previous: u64) -> Result<CapturedFrame, String> {
        for retry in 0..2 {
            let (mut width, mut height, mut serial) = (0, 0, previous);
            let result = unsafe { (self.callback)(self.context as *mut c_void, hash, rgba.as_mut_ptr(),
                rgba.len(), &mut serial, &mut width, &mut height) };
            if result == 0 && (width == 0 || height == 0) { return Ok(CapturedFrame::Inactive); }
            if width == 0 || height == 0 || width > 2048 || height > 2048 {
                return Err("movie frame dimensions are outside the decoder bound".into());
            }
            let size = width as usize * height as usize * 4;
            match result {
                -1 if retry == 0 && size > rgba.len() => rgba.resize(size, 0),
                0 if serial == previous => return Ok(CapturedFrame::Unchanged { width, height }),
                1 if size <= rgba.len() && serial != previous => return Ok(CapturedFrame::New { width, height, serial }),
                _ => return Err("movie frame callback returned inconsistent pixels or serial".into()),
            }
        }
        unreachable!()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MovieTarget { pub texture: u32, pub mip: u32, pub layer: u32 }

#[derive(Default)]
pub struct MovieScope(Option<MovieTarget>);
impl MovieScope {
    pub fn begin(&mut self, target: MovieTarget) { self.0 = (target.texture != 0).then_some(target); }
    pub fn skip(&self, target: MovieTarget) -> bool { self.0 == Some(target) }
    pub fn end(&mut self) { self.0 = None; }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MovieBlit { pub source: [[i32; 3]; 2], pub target: [[i32; 3]; 2] }

/// Match the video's viewport and clipping rectangle without changing the
/// game's surrounding target, mip, layer, or graphics state.
pub fn blit_region(source: [u32; 2], target: [u32; 2], viewport: Option<[f32; 6]>,
                   scissor: Option<[u32; 4]>) -> Option<MovieBlit> {
    if source.contains(&0) || target.contains(&0) || source.iter().any(|&x| x > 2048) || target.iter().any(|&x| x > 1024) { return None; }
    let v = viewport.unwrap_or([0., 0., target[0] as f32, target[1] as f32, 0., 1.]);
    if v[..4].iter().any(|x| !x.is_finite()) || v[2] <= 0. || v[3] <= 0. { return None; }
    let mut lo = [v[0].ceil().max(0.) as i64, v[1].ceil().max(0.) as i64];
    let mut hi = [(v[0] + v[2]).floor().min(target[0] as f32) as i64,
                  (v[1] + v[3]).floor().min(target[1] as f32) as i64];
    if let Some(s) = scissor {
        for axis in 0..2 { lo[axis] = lo[axis].max(s[axis] as i32 as i64); hi[axis] = hi[axis].min(s[axis + 2] as i32 as i64); }
    }
    if hi[0] <= lo[0] || hi[1] <= lo[1] { return None; }
    let mut src = [[0, 0, 0], [0, 0, 1]];
    for axis in 0..2 {
        src[0][axis] = (((lo[axis] as f64 - f64::from(v[axis])) / f64::from(v[axis + 2]) * f64::from(source[axis])).round() as i32).clamp(0, source[axis] as i32 - 1);
        src[1][axis] = (((hi[axis] as f64 - f64::from(v[axis])) / f64::from(v[axis + 2]) * f64::from(source[axis])).round() as i32).clamp(src[0][axis] + 1, source[axis] as i32);
    }
    Some(MovieBlit { source: src, target: [[lo[0] as i32, lo[1] as i32, 0], [hi[0] as i32, hi[1] as i32, 1]] })
}


