use std::cell::{Cell, RefCell};

use crate::backend::audio::SoundHandle;
use crate::binary_data::BinaryData;
use crate::display_object::{
    Avm1Button, Avm2Button, BitmapClass, EditText, Graphic, MorphShape, MovieClip, Text, Video,
};
use crate::font::Font;
use gc_arena::barrier::unlock;
use gc_arena::lock::Lock;
use gc_arena::{Collect, Gc, Mutation};
use ruffle_render::backend::RenderBackend;
use ruffle_render::bitmap::{Bitmap as RenderBitmap, BitmapHandle, BitmapSize};
use ruffle_render::error::Error as RenderError;
use swf::DefineBitsLossless;

#[derive(Copy, Clone, Collect, Debug)]
#[collect(no_drop)]
pub enum Character<'gc> {
    EditText(EditText<'gc>),
    Graphic(Graphic<'gc>),
    MovieClip(MovieClip<'gc>),
    Bitmap(Gc<'gc, BitmapCharacter<'gc>>),
    Avm1Button(Avm1Button<'gc>),
    Avm2Button(Avm2Button<'gc>),
    Font(Font<'gc>),
    MorphShape(MorphShape<'gc>),
    Text(Text<'gc>),
    Sound(#[collect(require_static)] SoundHandle),
    Video(Video<'gc>),
    BinaryData(Gc<'gc, BinaryData>),
}

#[derive(Collect, Debug)]
#[collect(no_drop)]
pub struct BitmapCharacter<'gc> {
    #[collect(require_static)]
    compressed: CompressedBitmap,
    /// A lazily constructed GPU handle, used when performing fills with this bitmap
    #[collect(require_static)]
    handle: RefCell<Option<BitmapHandle>>,
    /// The texture-clock tick at which this bitmap's GPU handle was last
    /// used, used by the GPU resource sweeper to evict long-unused bitmap
    /// textures.
    #[collect(require_static)]
    last_used: Cell<u64>,
    /// Size in bytes of the GPU texture backing `handle` (RGBA8), used for
    /// the GPU memory account.
    #[collect(require_static)]
    bytes: Cell<u64>,
    /// The bitmap class set by `SymbolClass` - this is used when we instantaite
    /// a `Bitmap` displayobject.
    avm2_class: Lock<BitmapClass<'gc>>,
}

impl<'gc> BitmapCharacter<'gc> {
    pub fn new(compressed: CompressedBitmap) -> Self {
        Self {
            compressed,
            handle: RefCell::new(None),
            last_used: Cell::new(0),
            bytes: Cell::new(0),
            avm2_class: Lock::new(BitmapClass::NoSubclass),
        }
    }

    pub fn compressed(&self) -> &CompressedBitmap {
        &self.compressed
    }

    pub fn avm2_class(&self) -> BitmapClass<'gc> {
        self.avm2_class.get()
    }

    pub fn set_avm2_class(this: Gc<'gc, Self>, bitmap_class: BitmapClass<'gc>, mc: &Mutation<'gc>) {
        unlock!(Gc::write(mc, this), Self, avm2_class).set(bitmap_class);
    }

    pub fn bitmap_handle(
        &self,
        backend: &mut dyn RenderBackend,
    ) -> Result<BitmapHandle, RenderError> {
        // Requesting the handle *is* the use: stamp it, so the sweeper only
        // evicts textures that have genuinely gone unused.
        self.last_used.set(crate::library::texture_clock());
        if let Some(handle) = self.handle.borrow().clone() {
            return Ok(handle);
        }
        let decoded = self.compressed.decode()?;
        let bytes = u64::from(decoded.width()) * u64::from(decoded.height()) * 4;
        let new_handle = backend.register_bitmap(decoded)?;
        // The handle is dropped again by the GPU resource sweeper when the
        // bitmap goes unused, and lazily re-created here on the next request.
        self.bytes.set(bytes);
        crate::library::add_gpu_texture_bytes(bytes);
        *self.handle.borrow_mut() = Some(new_handle.clone());
        Ok(new_handle)
    }

    /// Drops the cached GPU handle for this bitmap, if one was created.
    ///
    /// The handle is lazily re-created by [`Self::bitmap_handle`] if this
    /// bitmap is rendered again.
    pub fn release_gpu_handle(&self) {
        if let Some(handle) = self.handle.borrow_mut().take() {
            drop(handle);
            crate::library::subtract_gpu_texture_bytes(self.bytes.get());
            self.bytes.set(0);
        }
    }

    /// The texture-clock tick at which this bitmap's GPU handle was last used.
    pub fn last_used(&self) -> u64 {
        self.last_used.get()
    }

    /// Size in bytes of the GPU texture registered for this bitmap, or 0 if
    /// no handle is currently registered.
    pub fn bytes(&self) -> u64 {
        self.bytes.get()
    }

    /// Whether a GPU handle is currently cached for this bitmap.
    pub fn has_gpu_handle(&self) -> bool {
        self.handle.borrow().is_some()
    }
}

/// Holds a bitmap from an SWF tag, plus the decoded width/height.
/// We avoid decompressing the image until it's actually needed - some pathological SWFS
/// like 'House' have thousands of highly-compressed (mostly empty) bitmaps, which can
/// take over 10GB of ram if we decompress them all during preloading.
#[derive(Clone, Debug)]
pub enum CompressedBitmap {
    Jpeg {
        data: Vec<u8>,
        alpha: Option<Vec<u8>>,
        width: u32,
        height: u32,
    },
    Lossless(DefineBitsLossless<'static>),
}

impl CompressedBitmap {
    pub fn size(&self) -> BitmapSize {
        match self {
            CompressedBitmap::Jpeg { width, height, .. } => BitmapSize {
                width: *width,
                height: *height,
            },
            CompressedBitmap::Lossless(define_bits_lossless) => BitmapSize {
                width: define_bits_lossless.width.into(),
                height: define_bits_lossless.height.into(),
            },
        }
    }
    pub fn decode(&self) -> Result<RenderBitmap<'static>, RenderError> {
        match self {
            CompressedBitmap::Jpeg {
                data,
                alpha,
                width: _,
                height: _,
            } => ruffle_render::utils::decode_define_bits_jpeg(data, alpha.as_deref()),
            CompressedBitmap::Lossless(define_bits_lossless) => {
                ruffle_render::utils::decode_define_bits_lossless(define_bits_lossless)
            }
        }
    }
}
