//! Screen frames arrive from ScreenCaptureKit as BGRA, but GPUI's Metal
//! surface path only paints bi-planar YCbCr. VideoToolbox converts between
//! them on the media hardware, at full size, without touching the CPU. The
//! same pass makes a tiny copy that the `blur` fit stretches as its backdrop.
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use core_video::pixel_buffer::{
    CVPixelBuffer, CVPixelBufferKeys, CVPixelBufferRef, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
};
use std::ffi::c_void;

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    fn VTPixelTransferSessionCreate(allocator: *const c_void, session_out: *mut *mut c_void) -> i32;
    fn VTPixelTransferSessionTransferImage(session: *mut c_void, source: CVPixelBufferRef, destination: CVPixelBufferRef) -> i32;
    fn VTPixelTransferSessionInvalidate(session: *mut c_void);
}

unsafe extern "C" {
    fn CFRelease(object: *const c_void);
}

/// Output buffers in rotation, so the GPU can still be reading the previous
/// frame while the next one is written.
const RING: usize = 4;
/// Width of the backdrop copy. Stretched to the canvas it reads as a blur.
const THUMB_WIDTH: usize = 96;

/// A converted frame: full size for the picture, tiny for the blur backdrop.
pub struct Converted {
    pub full: CVPixelBuffer,
    pub thumb: CVPixelBuffer,
}

struct Ring {
    buffers: Vec<CVPixelBuffer>,
    next: usize,
    size: (usize, usize),
}

impl Ring {
    const fn new() -> Self {
        Self { buffers: Vec::new(), next: 0, size: (0, 0) }
    }

    /// The next buffer of `size`, (re)allocating the ring when the size changes.
    fn take(&mut self, size: (usize, usize)) -> Option<CVPixelBuffer> {
        if size != self.size || self.buffers.is_empty() {
            self.buffers = (0..RING).map(|_| output_buffer(size)).collect::<Option<_>>()?;
            self.size = size;
            self.next = 0;
        }
        let buffer = self.buffers.get(self.next)?.clone();
        self.next = (self.next + 1) % RING;
        Some(buffer)
    }
}

pub struct Converter {
    session: *mut c_void,
    full: Ring,
    thumb: Ring,
}

// SAFETY: the session and buffers are only used from one thread at a time
// (the capture callback holds the surrounding mutex); VideoToolbox sessions
// and CoreVideo buffers have no thread affinity.
unsafe impl Send for Converter {}

impl Converter {
    pub fn new() -> Result<Self, String> {
        let mut session: *mut c_void = std::ptr::null_mut();
        // SAFETY: `session` is a valid out pointer; a null allocator means the default.
        let status = unsafe { VTPixelTransferSessionCreate(std::ptr::null(), &mut session) };
        if status != 0 || session.is_null() {
            return Err(format!("could not create the frame converter (VideoToolbox status {status})"));
        }
        Ok(Self { session, full: Ring::new(), thumb: Ring::new() })
    }

    /// Convert one frame. `None` when the hardware refuses it (the caller
    /// keeps showing the previous frame).
    pub fn convert(&mut self, source: &CVPixelBuffer) -> Option<Converted> {
        let size = (source.get_width(), source.get_height());
        let thumb_height = ((THUMB_WIDTH * size.1 / size.0.max(1)) / 2).max(1) * 2;
        let full = self.full.take(size)?;
        let thumb = self.thumb.take((THUMB_WIDTH, thumb_height))?;
        self.transfer(source, &full)?;
        self.transfer(source, &thumb)?;
        Some(Converted { full, thumb })
    }

    fn transfer(&self, source: &CVPixelBuffer, target: &CVPixelBuffer) -> Option<()> {
        // SAFETY: the session is live and both buffers are valid for the call.
        let status = unsafe { VTPixelTransferSessionTransferImage(self.session, source.as_concrete_TypeRef(), target.as_concrete_TypeRef()) };
        (status == 0).then_some(())
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        // SAFETY: the session was created in `new` and is released exactly once.
        unsafe {
            VTPixelTransferSessionInvalidate(self.session);
            CFRelease(self.session);
        }
    }
}

/// A shareable, Metal-compatible bi-planar buffer of the given size.
fn output_buffer((width, height): (usize, usize)) -> Option<CVPixelBuffer> {
    let no_properties = CFDictionary::<CFString, CFType>::from_CFType_pairs(&[]);
    let options = CFDictionary::<CFString, CFType>::from_CFType_pairs(&[
        (CFString::from(CVPixelBufferKeys::IOSurfaceProperties), no_properties.as_CFType()),
        (CFString::from(CVPixelBufferKeys::MetalCompatibility), CFBoolean::true_value().as_CFType()),
    ]);
    CVPixelBuffer::new(kCVPixelFormatType_420YpCbCr8BiPlanarFullRange, width, height, Some(&options)).ok()
}
