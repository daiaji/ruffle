use crate::avm1::{PropertyMap as Avm1PropertyMap, PropertyMap};
use crate::avm2::{Class as Avm2Class, Domain as Avm2Domain};
use crate::backend::audio::SoundHandle;
use crate::character::Character;

use crate::display_object::{Bitmap, Graphic, MorphShape, Text};
use crate::font::{Font, FontDescriptor, FontLike, FontQuery, FontType};
use crate::prelude::*;
use crate::string::AvmString;
use crate::tag_utils::SwfMovie;
use gc_arena::{Collect, Mutation};
use ruffle_render::backend::RenderBackend;
use ruffle_render::bitmap::BitmapHandle;
use ruffle_render::utils::remove_invalid_jpeg_data;
use ruffle_wstr::{WStr, WString};

use crate::backend::ui::{FontDefinition, UiBackend};
use crate::font::DefaultFont;
use fnv::{FnvHashMap, FnvHashSet};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// A monotonically increasing per-frame tick used to track how long GPU
/// resources (bitmap textures, tessellated meshes) have gone unused.
pub(crate) static TEXTURE_CLOCK: AtomicU64 = AtomicU64::new(0);

/// The texture-clock tick, for stamping GPU resource creation and use.
pub(crate) fn texture_clock() -> u64 {
    TEXTURE_CLOCK.load(Ordering::Relaxed)
}

/// How often (in frames) the GPU resource sweeper runs.
const GPU_SWEEP_INTERVAL: u64 = 1200;

/// GPU resources whose create/use stamp is older than this many frames get
/// evicted; they are lazily re-created if they are ever needed again.
const GPU_SWEEP_MAX_AGE: u64 = GPU_SWEEP_INTERVAL;

/// The default soft budget for GPU memory, used when the
/// `RUFFLE_GPU_TEXTURE_BUDGET_MB` environment variable is not set.
const DEFAULT_GPU_TEXTURE_BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// The soft GPU memory budget, in bytes.
///
/// When reported (or accounted) GPU memory exceeds this, the oldest bitmap
/// textures are evicted even if they are not yet stale, so that bursts of
/// uploads (e.g. the first rendered frame of a bitmap-heavy scene) cannot
/// exhaust VRAM.
///
/// May be overridden with the `RUFFLE_GPU_TEXTURE_BUDGET_MB` environment
/// variable.
fn gpu_texture_budget_bytes() -> u64 {
    static BUDGET: OnceLock<u64> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        std::env::var("RUFFLE_GPU_TEXTURE_BUDGET_MB")
            .ok()
            .and_then(|value| parse_gpu_budget_mb(&value))
            .unwrap_or(DEFAULT_GPU_TEXTURE_BUDGET_BYTES)
    })
}

/// Parses a `RUFFLE_GPU_TEXTURE_BUDGET_MB` value (whole megabytes) into bytes.
///
/// Invalid or zero values are rejected, so the caller falls back to the
/// default budget.
fn parse_gpu_budget_mb(value: &str) -> Option<u64> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(|megabytes| megabytes.saturating_mul(1024 * 1024))
        .filter(|bytes| *bytes > 0)
}

/// Chooses which live textures to evict so that at least `excess` bytes are
/// released, least recently used first. `live` holds `(last_used_tick, bytes)` for each
/// candidate; the returned indices are in eviction order.
fn eviction_plan(live: &[(u64, u64)], excess: u64) -> Vec<usize> {
    let mut plan = Vec::new();
    if excess == 0 {
        return plan;
    }
    let mut order: Vec<usize> = (0..live.len()).collect();
    order.sort_by_key(|index| live[*index].0);
    let mut remaining = excess;
    for index in order {
        if remaining == 0 {
            break;
        }
        remaining = remaining.saturating_sub(live[index].1);
        plan.push(index);
    }
    plan
}

/// Running account of GPU texture bytes held by bitmap characters, updated on
/// handle creation and release.
///
/// This is used as a fallback when the render backend cannot report its own
/// GPU memory usage; it only counts textures uploaded for library bitmap
/// characters, not meshes, render targets, or `BitmapData` textures.
static GPU_TEXTURE_BYTES: AtomicU64 = AtomicU64::new(0);

/// Records that `bytes` of GPU texture memory were uploaded.
pub(crate) fn add_gpu_texture_bytes(bytes: u64) {
    GPU_TEXTURE_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

/// Records that `bytes` of GPU texture memory were released.
pub(crate) fn subtract_gpu_texture_bytes(bytes: u64) {
    GPU_TEXTURE_BYTES.fetch_sub(bytes, Ordering::Relaxed);
}

/// Re-baselines the account to the bytes actually held by live handles.
///
/// A handle dropped without an explicit release - a bitmap character collected
/// by the GC, say - frees its texture without subtracting itself, so the running
/// total would drift upward over a long session. Every sweep re-baselines it to
/// what the live handles actually hold.
fn reconcile_gpu_texture_bytes(bytes: u64) {
    GPU_TEXTURE_BYTES.store(bytes, Ordering::Relaxed);
}

/// An estimate of the GPU texture memory currently held by bitmap characters,
/// in bytes.
///
/// This only counts textures uploaded for library bitmap characters; it does
/// not include tessellated meshes, render targets, or textures owned by
/// `BitmapData` objects. It is intended for diagnostics (e.g. crash reports).
pub fn estimated_gpu_texture_bytes() -> u64 {
    GPU_TEXTURE_BYTES.load(Ordering::Relaxed)
}

#[derive(Clone, Collect)]
#[collect(no_drop)]
struct MovieSymbol<'gc>(SwfMovie<'gc>, CharacterId);

/// The mappings between class objects and library characters defined by
/// `SymbolClass`.
#[derive(Collect)]
#[collect(no_drop)]
pub struct Avm2ClassRegistry<'gc> {
    /// A list of AVM2 class objects and the character IDs they are expected to
    /// instantiate.
    class_map: HashMap<Avm2Class<'gc>, MovieSymbol<'gc>>,
}

impl Default for Avm2ClassRegistry<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'gc> Avm2ClassRegistry<'gc> {
    pub fn new() -> Self {
        Self {
            class_map: HashMap::new(),
        }
    }

    /// Retrieve the library symbol for a given AVM2 class object.
    ///
    /// A value of `None` indicates that this AVM2 class is not associated with
    /// a library symbol.
    pub fn class_symbol(&self, class_def: Avm2Class<'gc>) -> Option<(SwfMovie<'gc>, CharacterId)> {
        match self.class_map.get(&class_def) {
            Some(MovieSymbol(movie, symbol)) => Some((*movie, *symbol)),
            None => None,
        }
    }

    /// Associate an AVM2 class definition with a given library symbol.
    pub fn set_class_symbol(
        &mut self,
        class_def: Avm2Class<'gc>,
        movie: SwfMovie<'gc>,
        symbol: CharacterId,
    ) {
        if let Some(old) = self.class_map.get(&class_def) {
            if SwfMovie::ptr_eq(&movie, &old.0) && symbol != old.1 {
                // Flash player actually allows using the same class in multiple SymbolClass
                // entries in the same swf, with *different* symbol ids. Whichever one
                // is processed first will *win*, and the second one will be ignored.
                // We still log a warning, since we wouldn't expect this to happen outside
                // of deliberately crafted SWFs.
                tracing::warn!(
                    "Tried to overwrite class {:?} id={:?} with symbol id={:?} from same movie",
                    class_def,
                    old.1,
                    symbol,
                );
            }
            // If we're trying to overwrite the class with a symbol from a *different* SwfMovie,
            // then just ignore it. This handles the case where a Loader has a class that shadows
            // a class in the main swf (possibly with a different ApplicationDomain). This will
            // result in the original class from the parent being used, even when the child swf
            // instantiates the clip on the timeline.
            return;
        }
        self.class_map.insert(class_def, MovieSymbol(movie, symbol));
    }
}

/// Symbol library for a single given SWF.
#[derive(Collect)]
#[collect(no_drop)]
pub struct MovieLibrary<'gc> {
    swf: SwfMovie<'gc>,
    characters: HashMap<CharacterId, Character<'gc>>,
    export_characters: Avm1PropertyMap<'gc, CharacterId>,
    imported_assets: HashMap<AvmString<'gc>, CharacterId>,
    jpeg_tables: Option<Vec<u8>>,
    fonts: FontMap<'gc>,
    avm2_domain: Option<Avm2Domain<'gc>>,
}

impl<'gc> MovieLibrary<'gc> {
    pub fn new(swf: SwfMovie<'gc>) -> Self {
        Self {
            swf,
            characters: HashMap::new(),
            imported_assets: HashMap::new(),
            export_characters: Avm1PropertyMap::new(),
            jpeg_tables: None,
            fonts: Default::default(),
            avm2_domain: None,
        }
    }

    /// Frees cached GPU-side resources (uploaded bitmap textures and
    /// tessellated shape meshes) for every character in this library.
    ///
    /// The underlying character data is kept, so anything that still renders
    /// this movie's characters lazily re-creates the GPU resources on demand.
    /// This is called when a movie is discarded (e.g. `Loader.unload()`), so
    /// that repeated load/unload cycles don't accumulate GPU memory.
    pub fn release_gpu_resources(&mut self) {
        for character in self.characters.values_mut() {
            match *character {
                Character::Bitmap(bitmap) => {
                    bitmap.release_gpu_handle();
                }
                Character::Graphic(graphic) => {
                    graphic.release_gpu_resources();
                }
                Character::MorphShape(morph) => {
                    morph.release_gpu_resources();
                }
                _ => {}
            }
        }
    }

    /// Registers a character; returns `true` if successful, or `false` if a character with
    /// the given ID already exists.
    pub fn register_character(&mut self, id: CharacterId, character: Character<'gc>) -> bool {
        use std::collections::hash_map::Entry;
        match self.characters.entry(id) {
            Entry::Vacant(e) => {
                if let Character::Font(font) = character {
                    self.fonts.register(font);
                }
                e.insert(character);
                true
            }
            Entry::Occupied(_) => {
                tracing::error!("Character ID collision: Tried to register ID {} twice", id);
                false
            }
        }
    }

    /// Registers an export name for a given character ID.
    /// This character will then be instantiable from AVM1.
    pub fn register_export(&mut self, id: CharacterId, export_name: AvmString<'gc>) {
        let character_exists = self.contains_character(id);
        debug_assert!(character_exists);
        if !character_exists {
            tracing::error!(
                "Tried to register export '{export_name}' for a non-existent character {id}"
            );
            return;
        }

        self.export_characters.insert(export_name, id, false);
    }

    pub fn register_font_name(&mut self, character_id: u16, font_name: &str) {
        let Some(Character::Font(font)) = self.character_by_id(character_id) else {
            return;
        };

        let descriptor = FontDescriptor::from_parts(
            font_name,
            font.descriptor().bold(),
            font.descriptor().italic(),
        );
        self.fonts.register_with_descriptor(font, &descriptor);
    }

    pub fn characters(&self) -> &HashMap<CharacterId, Character<'gc>> {
        &self.characters
    }

    pub fn export_characters(&self) -> &PropertyMap<'gc, CharacterId> {
        &self.export_characters
    }

    pub fn contains_character(&self, id: CharacterId) -> bool {
        self.characters.contains_key(&id)
    }

    pub fn character_by_id(&self, id: CharacterId) -> Option<Character<'gc>> {
        self.characters.get(&id).copied()
    }

    pub fn character_by_export_name(&self, name: &WStr) -> Option<(CharacterId, Character<'gc>)> {
        if let Some(id) = self.export_characters.get(name, false)
            && let Some(character) = self.characters.get(id)
        {
            return Some((*id, *character));
        }
        None
    }

    pub fn character_id_by_import_name(&self, name: &WStr) -> Option<CharacterId> {
        self.imported_assets.get(name).copied()
    }

    pub fn register_import(&mut self, name: AvmString<'gc>, id: CharacterId) {
        self.imported_assets.insert(name, id);
    }

    /// Instantiates the library item with the given character ID into a display object.
    /// The object must then be post-instantiated before being used.
    pub fn instantiate_by_id(
        &self,
        id: CharacterId,
        mc: &Mutation<'gc>,
    ) -> Option<DisplayObject<'gc>> {
        if let Some(&character) = self.characters.get(&id) {
            self.instantiate_display_object(id, character, mc)
        } else {
            tracing::error!("Tried to instantiate a non-registered character ID {id}");
            None
        }
    }

    /// Instantiates the library item with the given export name into a display object.
    /// The object must then be post-instantiated before being used.
    pub fn instantiate_by_export_name(
        &self,
        export_name: &WStr,
        mc: &Mutation<'gc>,
    ) -> Option<DisplayObject<'gc>> {
        if let Some((id, character)) = self.character_by_export_name(export_name) {
            self.instantiate_display_object(id, character, mc)
        } else {
            tracing::error!("Tried to instantiate a non-registered character {export_name}");
            None
        }
    }

    /// Instantiates the given character into a display object.
    /// The object must then be post-instantiated before being used.
    fn instantiate_display_object(
        &self,
        id: CharacterId,
        character: Character<'gc>,
        mc: &Mutation<'gc>,
    ) -> Option<DisplayObject<'gc>> {
        match character {
            Character::Bitmap(bitmap) => {
                let avm2_class = bitmap.avm2_class();
                let bitmap = bitmap.compressed().decode().unwrap();
                let bitmap = Bitmap::new(mc, id, bitmap, self.swf);
                bitmap.set_avm2_bitmapdata_class(mc, avm2_class);
                Some(bitmap.instantiate(mc).into())
            }
            Character::EditText(edit_text) => Some(edit_text.instantiate(mc).into()),
            Character::Graphic(graphic) => Some(graphic.instantiate(mc).into()),
            Character::MorphShape(morph_shape) => Some(morph_shape.instantiate(mc).into()),
            Character::MovieClip(movie_clip) => Some(movie_clip.instantiate(mc).into()),
            Character::Avm1Button(button) => Some(button.instantiate(mc).into()),
            Character::Avm2Button(button) => Some(button.instantiate(mc).into()),
            Character::Text(text) => Some(text.instantiate(mc).into()),
            Character::Video(video) => Some(video.instantiate(mc).into()),
            _ => {
                // Cannot instantiate non-display object
                None
            }
        }
    }

    pub fn get_font(&self, id: CharacterId) -> Option<Font<'gc>> {
        if let Some(&Character::Font(font)) = self.characters.get(&id) {
            Some(font)
        } else {
            None
        }
    }

    pub fn embedded_fonts(&self) -> Vec<Font<'gc>> {
        self.fonts.all()
    }

    /// Returns the `Graphic` with the given character ID.
    /// Returns `None` if the ID does not exist or is not a `Graphic`.
    pub fn get_graphic(&self, id: CharacterId) -> Option<Graphic<'gc>> {
        if let Some(&Character::Graphic(graphic)) = self.characters.get(&id) {
            Some(graphic)
        } else {
            None
        }
    }

    /// Returns the `MorphShape` with the given character ID.
    /// Returns `None` if the ID does not exist or is not a `MorphShape`.
    pub fn get_morph_shape(&self, id: CharacterId) -> Option<MorphShape<'gc>> {
        if let Some(&Character::MorphShape(morph_shape)) = self.characters.get(&id) {
            Some(morph_shape)
        } else {
            None
        }
    }

    pub fn get_sound(&self, id: CharacterId) -> Option<SoundHandle> {
        if let Some(Character::Sound(sound)) = self.characters.get(&id) {
            Some(*sound)
        } else {
            None
        }
    }

    /// Returns the `Text` with the given character ID.
    /// Returns `None` if the ID does not exist or is not a `Text`.
    pub fn get_text(&self, id: CharacterId) -> Option<Text<'gc>> {
        if let Some(&Character::Text(text)) = self.characters.get(&id) {
            Some(text)
        } else {
            None
        }
    }

    pub fn set_jpeg_tables(&mut self, data: &[u8]) {
        if self.jpeg_tables.is_some() {
            // SWF spec says there should only be one JPEGTables tag.
            // TODO: What is the behavior when there are multiples?
            tracing::warn!("SWF contains multiple JPEGTables tags");
            return;
        }
        // Some SWFs have a JPEGTables tag with 0 length; ignore these.
        // (Does this happen when there is only a single DefineBits tag?)
        self.jpeg_tables = if data.is_empty() {
            None
        } else {
            Some(remove_invalid_jpeg_data(data).to_vec())
        }
    }

    pub fn jpeg_tables(&self) -> Option<&[u8]> {
        self.jpeg_tables.as_ref().map(|data| &data[..])
    }

    pub fn set_avm2_domain(&mut self, avm2_domain: Avm2Domain<'gc>) {
        self.avm2_domain = Some(avm2_domain);
    }

    /// Get the AVM2 domain this movie runs under.
    ///
    /// Note that the presence of an AVM2 domain does *not* indicate that this
    /// movie provides AVM2 code. For example, a movie may have been loaded by
    /// AVM2 code into a particular domain, even though it turned out to be
    /// an AVM1 movie, and thus this domain is unused.
    pub fn avm2_domain(&self) -> Avm2Domain<'gc> {
        self.avm2_domain.unwrap()
    }

    pub fn try_avm2_domain(&self) -> Option<Avm2Domain<'gc>> {
        self.avm2_domain
    }
}

pub struct MovieLibrarySource<'a, 'gc> {
    pub library: &'a MovieLibrary<'gc>,
}

impl ruffle_render::bitmap::BitmapSource for MovieLibrarySource<'_, '_> {
    fn bitmap_size(&self, id: u16) -> Option<ruffle_render::bitmap::BitmapSize> {
        if let Some(Character::Bitmap(bitmap)) = self.library.characters.get(&id) {
            Some(bitmap.compressed().size())
        } else {
            None
        }
    }

    fn bitmap_handle(&self, id: u16, backend: &mut dyn RenderBackend) -> Option<BitmapHandle> {
        let Some(Character::Bitmap(bitmap)) = self.library.characters.get(&id) else {
            return None;
        };

        match bitmap.bitmap_handle(backend) {
            Ok(handle) => Some(handle),
            Err(e) => {
                tracing::error!("Failed to register bitmap character {id}: {e}");
                None
            }
        }
    }
}

#[derive(Collect)]
#[collect(no_drop)]
struct MovieLibraries<'gc>(HashMap<SwfMovie<'gc>, MovieLibrary<'gc>>);

impl<'gc> MovieLibraries<'gc> {
    fn new() -> Self {
        Self(HashMap::new())
    }

    fn get(&self, key: SwfMovie<'gc>) -> Option<&MovieLibrary<'gc>> {
        self.0.get(&key)
    }

    fn get_or_insert_mut(&mut self, movie: SwfMovie<'gc>) -> &mut MovieLibrary<'gc> {
        self.0
            .entry(movie)
            .or_insert_with(|| MovieLibrary::new(movie))
    }

    fn known_movies(&self) -> impl Iterator<Item = SwfMovie<'gc>> {
        self.0.keys().copied()
    }
}

/// Symbol library for multiple movies.
#[derive(Collect)]
#[collect(no_drop)]
pub struct Library<'gc> {
    /// All the movie libraries.
    movie_libraries: MovieLibraries<'gc>,

    /// A cache of seen device fonts.
    // TODO: Descriptors shouldn't be stored in fonts. Fonts should be a list that we iterate and ask "do you match". A font can have zero or many names.
    device_fonts: FontMap<'gc>,

    /// "Global" embedded fonts, registered through AVM2 `Font.registerFont`.
    /// These should be checked before any Movie-specific library's own fonts.
    global_fonts: FontMap<'gc>,

    /// A set of which fonts we've asked from the backend already, to help with negative caching.
    /// If we've asked for a specific font, record it here and don't ask again.
    font_lookup_cache: FnvHashSet<FontQuery>,

    /// Cached font sort queries.
    font_sort_cache: FnvHashMap<FontQuery, Vec<Font<'gc>>>,

    /// The implementation names of each default font.
    default_font_names: FnvHashMap<DefaultFont, Vec<String>>,

    /// The cached list of implementations per default font.
    default_font_cache: FnvHashMap<(DefaultFont, bool, bool), Vec<Font<'gc>>>,

    /// A list of the symbols associated with specific AVM2 constructor
    /// prototypes.
    avm2_class_registry: Avm2ClassRegistry<'gc>,
}

impl<'gc> Library<'gc> {
    pub fn empty() -> Self {
        Self {
            movie_libraries: MovieLibraries::new(),
            device_fonts: Default::default(),
            global_fonts: Default::default(),
            font_lookup_cache: Default::default(),
            font_sort_cache: Default::default(),
            default_font_names: Default::default(),
            default_font_cache: Default::default(),
            avm2_class_registry: Default::default(),
        }
    }

    pub fn library_for_movie(&self, movie: SwfMovie<'gc>) -> Option<&MovieLibrary<'gc>> {
        self.movie_libraries.get(movie)
    }

    pub fn library_for_movie_mut(&mut self, movie: SwfMovie<'gc>) -> &mut MovieLibrary<'gc> {
        self.movie_libraries.get_or_insert_mut(movie)
    }

    pub fn known_movies(&self) -> impl Iterator<Item = SwfMovie<'gc>> {
        self.movie_libraries.known_movies()
    }

    /// Called once per player frame. Advances the texture clock, and
    /// periodically sweeps all movie libraries for GPU resources (uploaded
    /// bitmap textures, tessellated meshes) that haven't been used in
    /// `GPU_SWEEP_MAX_AGE` frames, releasing them to bound VRAM usage.
    ///
    /// In addition to the age-based eviction, the sweep enforces a soft memory
    /// budget: if the render backend reports a GPU memory reading (Vulkan and
    /// DX12 can), that real value is used; otherwise we fall back to our own
    /// accounting of bitmap textures. When over budget, the oldest textures
    /// are evicted first.
    ///
    /// Evicted resources are lazily re-created on demand, so nothing breaks;
    /// frequently rendered resources keep their stamps fresh and are never
    /// evicted.
    pub fn tick_gpu_resources(&mut self, renderer: &dyn RenderBackend) {
        let now = TEXTURE_CLOCK.fetch_add(1, Ordering::Relaxed) + 1;

        if !now.is_multiple_of(GPU_SWEEP_INTERVAL) {
            return;
        }
        let cutoff = now.saturating_sub(GPU_SWEEP_MAX_AGE);

        // A single pass over every character: evict stale textures and meshes,
        // and collect the surviving textures (least recently used first) so the byte budget
        // can be enforced below.
        let mut live: Vec<(u64, u64, SwfMovie<'gc>, CharacterId)> = Vec::new();
        for movie in self.movie_libraries.known_movies() {
            let Some(library) = self.movie_libraries.get(movie) else {
                continue;
            };
            for (id, character) in library.characters.iter() {
                match *character {
                    Character::Bitmap(bitmap) => {
                        if !bitmap.has_gpu_handle() {
                            continue;
                        }
                        if bitmap.last_used() < cutoff {
                            bitmap.release_gpu_handle();
                        } else {
                            live.push((bitmap.last_used(), bitmap.bytes(), movie, *id));
                        }
                    }
                    // Only release meshes that haven't been rendered since
                    // the cutoff; rendering refreshes `last_used`.
                    Character::Graphic(graphic) if graphic.last_used() < cutoff => {
                        graphic.release_gpu_resources();
                    }
                    Character::MorphShape(morph) if morph.last_used() < cutoff => {
                        morph.release_gpu_resources();
                    }
                    _ => {}
                }
            }
        }

        // Re-baseline the account now that we know which bitmaps still hold a
        // handle, discarding any drift from handles released without an
        // explicit release (and therefore never subtracted).
        reconcile_gpu_texture_bytes(live.iter().map(|(_, bytes, ..)| *bytes).sum());

        // Prefer a real reading of GPU memory from the backend; fall back to
        // our own accounting when the backend cannot report usage (e.g. GL).
        //
        // Budget against the *allocated* bytes (live GPU allocations), not the
        // reserved bytes: a suballocating backend keeps the memory blocks of
        // freed allocations around, so `reserved_bytes` would not fall as we
        // evict, and every sweep would evict everything.
        let backend_usage = renderer.gpu_memory_usage();
        let pressure = backend_usage
            .map(|usage| usage.allocated_bytes)
            .unwrap_or_else(estimated_gpu_texture_bytes);
        let budget = gpu_texture_budget_bytes();

        if pressure > budget {
            let candidates: Vec<(u64, u64)> = live
                .iter()
                .map(|(created, bytes, ..)| (*created, *bytes))
                .collect();
            for index in eviction_plan(&candidates, pressure - budget) {
                let (_, _, movie, id) = live[index];
                let Some(library) = self.movie_libraries.get(movie) else {
                    continue;
                };
                if let Some(Character::Bitmap(bitmap)) = library.characters.get(&id)
                    && bitmap.has_gpu_handle()
                {
                    bitmap.release_gpu_handle();
                }
            }
        }

        tracing::debug!(
            "GPU resource sweep: backend allocated={:?} MB reserved={:?} MB, own estimate={} MB, {} live bitmap textures, budget {} MB",
            backend_usage.map(|usage| usage.allocated_bytes / (1024 * 1024)),
            backend_usage.map(|usage| usage.reserved_bytes / (1024 * 1024)),
            estimated_gpu_texture_bytes() / (1024 * 1024),
            live.len(),
            budget / (1024 * 1024),
        );
    }

    /// Returns the default Font implementations behind the built in names (ie `_sans`)
    pub fn default_font(
        &mut self,
        name: DefaultFont,
        is_bold: bool,
        is_italic: bool,
        ui: &dyn UiBackend,
        renderer: &mut dyn RenderBackend,
        gc_context: &Mutation<'gc>,
    ) -> Vec<Font<'gc>> {
        // Can't use entry api here as we want to use self for `load_device_font`.
        // Cache the value as this will be looked up a lot, and font lookup by name can be expensive if lots of fonts exist.
        if let Some(cache) = self.default_font_cache.get(&(name, is_bold, is_italic)) {
            return cache.clone();
        }

        let mut result = vec![];
        // First try to find any exactly matching fonts.
        for name in self.default_font_names.entry(name).or_default().clone() {
            let query = FontQuery::new(FontType::Device, name, is_bold, is_italic);
            if let Some(font) = self.get_or_load_exact_device_font(&query, ui, renderer, gc_context)
            {
                result.push(font);
                break; // TODO: Return multiple fonts when it's needed.
            }
        }

        // Nothing found, try a compatible font.
        if result.is_empty() {
            for name in self.default_font_names.entry(name).or_default().clone() {
                let query = FontQuery::new(FontType::Device, name, is_bold, is_italic);
                if let Some(font) = self.device_fonts.find(&query) {
                    result.push(font);
                    break; // TODO: Return multiple fonts when it's needed.
                }
            }
        }

        self.default_font_cache
            .insert((name, is_bold, is_italic), result.clone());
        result
    }

    /// Returns the device font exactly matching the requested options.
    fn get_or_load_exact_device_font(
        &mut self,
        query: &FontQuery,
        ui: &dyn UiBackend,
        renderer: &mut dyn RenderBackend,
        gc_context: &Mutation<'gc>,
    ) -> Option<Font<'gc>> {
        // If we have the exact matching font already, use that
        // TODO: We should instead ask each font if it matches a given name. Partial matches are allowed, and fonts may have any amount of names.
        if let Some(font) = self.device_fonts.get(query) {
            return Some(font);
        }

        // We don't have this font already. Did we ask for it before?
        let new_request = self.font_lookup_cache.insert(query.clone());
        if new_request {
            // First time asking for this font, see if our backend can provide anything relevant
            ui.load_device_font(query, &mut |definition| {
                self.register_device_font(gc_context, renderer, definition)
            });

            // Check again. A backend may or may not have provided some new fonts,
            // and they may or may not be relevant to the one we're asking for.
            if let Some(font) = self.device_fonts.get(query) {
                return Some(font);
            }

            let name = &query.name;
            let is_bold = query.is_bold;
            let is_italic = query.is_italic;
            warn!("Unknown device font \"{name}\" (bold: {is_bold}, italic: {is_italic})");
        }

        None
    }

    /// Returns the device font compatible with the requested options.
    pub fn get_or_load_device_font(
        &mut self,
        name: &str,
        is_bold: bool,
        is_italic: bool,
        ui: &dyn UiBackend,
        renderer: &mut dyn RenderBackend,
        gc_context: &Mutation<'gc>,
    ) -> Option<Font<'gc>> {
        let query = FontQuery::new(FontType::Device, name.to_owned(), is_bold, is_italic);

        // Try to find an exactly matching font.
        if let Some(font) = self.get_or_load_exact_device_font(&query, ui, renderer, gc_context) {
            return Some(font);
        }

        // Fallback: Try to find an existing font to re-use instead of giving up.
        self.device_fonts.find(&query)
    }

    fn sort_device_fonts(
        &mut self,
        query: &FontQuery,
        ui: &dyn UiBackend,
        renderer: &mut dyn RenderBackend,
        gc_context: &Mutation<'gc>,
    ) -> Vec<Font<'gc>> {
        // First, ask the backend to sort the fonts for us.
        let fonts = ui.sort_device_fonts(query, &mut |definition| {
            self.register_device_font(gc_context, renderer, definition)
        });

        let fonts: Vec<Font<'gc>> = fonts
            .iter()
            .filter_map(|font_query| self.device_fonts.get(font_query))
            .collect();

        if !fonts.is_empty() {
            return fonts;
        }

        // When the backend failed (or doesn't support sorting fonts), fall back
        // to loading one font only without sorting.
        let font = self.get_or_load_device_font(
            &query.name,
            query.is_bold,
            query.is_italic,
            ui,
            renderer,
            gc_context,
        );
        font.map(|font| vec![font]).unwrap_or_default()
    }

    pub fn get_or_sort_device_fonts(
        &mut self,
        name: &str,
        is_bold: bool,
        is_italic: bool,
        ui: &dyn UiBackend,
        renderer: &mut dyn RenderBackend,
        gc_context: &Mutation<'gc>,
    ) -> Vec<Font<'gc>> {
        // TODO We should be able to return a &Vec here, but (1) the borrow
        //   checker is too strict and doesn't allow if branching, and
        //   (2) there's no way to insert a value and get a reference to
        //   it at the same time.
        let query = FontQuery::new(FontType::Device, name.to_owned(), is_bold, is_italic);
        if let Some(fonts) = self.font_sort_cache.get(&query) {
            return fonts.clone();
        }

        let fonts = self.sort_device_fonts(&query, ui, renderer, gc_context);
        self.font_sort_cache.insert(query, fonts.clone());
        fonts
    }

    pub fn set_default_font(&mut self, font: DefaultFont, names: Vec<String>) {
        self.default_font_names.insert(font, names);
        self.default_font_cache.clear();
    }

    pub fn register_device_font(
        &mut self,
        gc_context: &Mutation<'gc>,
        renderer: &mut dyn RenderBackend,
        definition: FontDefinition<'_>,
    ) {
        match definition {
            FontDefinition::SwfTag(tag, encoding) => {
                let font =
                    Font::from_swf_tag(gc_context, renderer, tag, encoding, FontType::Device);
                let name = font.descriptor().name().to_owned();
                let is_bold = font.descriptor().bold();
                let is_italic = font.descriptor().italic();
                tracing::debug!(
                    "Loaded new device font \"{name}\" (bold: {is_bold}, italic: {is_italic}) from swf tag"
                );
                self.device_fonts.register(font);
            }
            FontDefinition::FontFile {
                name,
                is_bold,
                is_italic,
                data,
                index,
            } => {
                let descriptor = FontDescriptor::from_parts(&name, is_bold, is_italic);
                if let Ok(font) =
                    Font::from_font_file(gc_context, descriptor, data, index, FontType::Device)
                {
                    let name = font.descriptor().name().to_owned();
                    tracing::debug!(
                        "Loaded new device font \"{name}\" (bold: {is_bold}, italic: {is_italic}) from file"
                    );
                    self.device_fonts.register(font);
                } else {
                    warn!("Failed to load device font from file");
                }
            }
            FontDefinition::ExternalRenderer {
                name,
                is_bold,
                is_italic,
                font_renderer,
            } => {
                let descriptor = FontDescriptor::from_parts(&name, is_bold, is_italic);
                let font = Font::from_renderer(gc_context, descriptor, font_renderer);
                tracing::debug!(
                    "Loaded new externally rendered font \"{name}\" (bold: {is_bold}, italic: {is_italic})"
                );
                self.device_fonts.register(font);
            }
        }
        self.default_font_cache.clear();
    }

    /// Find a font by its name and parameters.
    pub fn get_embedded_font_by_name(
        &self,
        name: &str,
        font_type: FontType,
        is_bold: bool,
        is_italic: bool,
        movie: Option<SwfMovie<'gc>>,
    ) -> Option<Font<'gc>> {
        let query = FontQuery::new(font_type, name.to_owned(), is_bold, is_italic);
        if let Some(font) = self.global_fonts.find(&query) {
            return Some(font);
        }
        if let Some(movie) = movie
            && let Some(library) = self.library_for_movie(movie)
        {
            if let Some((_, font)) = library.character_by_export_name(&WString::from_utf8(name)) {
                // Exporting a font seems to override font lookup completely.
                return if let Character::Font(font) = font {
                    Some(font)
                } else {
                    None
                };
            }
            if let Some(font) = library.fonts.find(&query) {
                return Some(font);
            }
        }
        None
    }

    pub fn global_fonts(&self) -> Vec<Font<'gc>> {
        self.global_fonts.all()
    }

    pub fn register_global_font(&mut self, font: Font<'gc>) {
        self.global_fonts.register(font);
    }

    /// Get the AVM2 class registry.
    pub fn avm2_class_registry(&self) -> &Avm2ClassRegistry<'gc> {
        &self.avm2_class_registry
    }

    /// Mutate the AVM2 class registry.
    pub fn avm2_class_registry_mut(&mut self) -> &mut Avm2ClassRegistry<'gc> {
        &mut self.avm2_class_registry
    }

    /// Evicts cached font resources that haven't been used, across all device
    /// fonts. Meant to be called once per rendered frame.
    pub fn sweep_font_caches(&self) {
        for font in self.device_fonts.iter_all() {
            font.sweep_caches();
        }
    }
}

#[derive(Collect, Default)]
#[collect(no_drop)]
struct FontMap<'gc>(FnvHashMap<FontQuery, Font<'gc>>);

impl<'gc> FontMap<'gc> {
    pub fn register(&mut self, font: Font<'gc>) {
        self.register_with_descriptor(font, font.descriptor());
    }

    pub fn register_with_descriptor(&mut self, font: Font<'gc>, descriptor: &FontDescriptor) {
        self.0
            .entry(FontQuery::from_descriptor(font.font_type(), descriptor))
            .or_insert(font);
    }

    pub fn get(&self, font_query: &FontQuery) -> Option<Font<'gc>> {
        self.0.get(font_query).copied()
    }

    pub fn find(&self, font_query: &FontQuery) -> Option<Font<'gc>> {
        // The order here is specific, and tested in `tests/swfs/fonts/embed_matching/fallback_preferences`

        // Exact match
        if let Some(font) = self.get(font_query) {
            return Some(font);
        }

        let is_italic = font_query.is_italic;
        let is_bold = font_query.is_bold;

        let mut fallback_query = font_query.clone();
        if is_italic ^ is_bold {
            // If one is set (but not both), then try upgrading to bold italic...
            fallback_query.is_bold = true;
            fallback_query.is_italic = true;
            if let Some(font) = self.get(&fallback_query) {
                return Some(font);
            }

            // and then downgrading to regular
            fallback_query.is_bold = false;
            fallback_query.is_italic = false;
            if let Some(font) = self.get(&fallback_query) {
                return Some(font);
            }

            // and then finally whichever one we don't have set
            fallback_query.is_bold = !is_bold;
            fallback_query.is_italic = !is_italic;
            if let Some(font) = self.get(&fallback_query) {
                return Some(font);
            }
        } else {
            // We don't have an exact match and we were either looking for regular or bold-italic

            if is_italic && is_bold {
                // Do we have regular? (unless we already looked for it)
                fallback_query.is_bold = false;
                fallback_query.is_italic = false;
                if let Some(font) = self.get(&fallback_query) {
                    return Some(font);
                }
            }

            // Do we have bold?
            fallback_query.is_bold = true;
            fallback_query.is_italic = false;
            if let Some(font) = self.get(&fallback_query) {
                return Some(font);
            }

            // Do we have italic?
            fallback_query.is_bold = false;
            fallback_query.is_italic = true;
            if let Some(font) = self.get(&fallback_query) {
                return Some(font);
            }

            if !is_bold && !is_italic {
                // Do we have bold italic? (unless we already looked for it)
                fallback_query.is_bold = true;
                fallback_query.is_italic = true;
                if let Some(font) = self.get(&fallback_query) {
                    return Some(font);
                }
            }
        }

        None
    }

    pub fn all(&self) -> Vec<Font<'gc>> {
        self.0.values().copied().collect()
    }

    pub fn iter_all(&self) -> impl Iterator<Item = Font<'gc>> {
        self.0.values().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GPU_SWEEP_INTERVAL, GPU_SWEEP_MAX_AGE, Library, TEXTURE_CLOCK, add_gpu_texture_bytes,
        estimated_gpu_texture_bytes, eviction_plan, parse_gpu_budget_mb, texture_clock,
    };
    use crate::character::{BitmapCharacter, Character, CompressedBitmap};
    use crate::tag_utils::{SwfMovie, SwfMovieData};
    use gc_arena::lock::GcRefLock;
    use gc_arena::{Collect, Gc, Rootable};
    use ruffle_render::backend::ViewportDimensions;
    use ruffle_render::backend::null::NullRenderer;
    use std::borrow::Cow;
    use std::io::Write;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    /// The GPU accounting statics are process-wide, so the tests that assert on
    /// them must not run concurrently with each other.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    type GpuTestArena = gc_arena::Arena<Rootable![GpuTestRoot<'_>]>;

    #[derive(Collect)]
    #[collect(no_drop)]
    struct GpuTestRoot<'gc> {
        data: GcRefLock<'gc, GpuTestData<'gc>>,
    }

    #[derive(Collect)]
    #[collect(no_drop)]
    struct GpuTestData<'gc> {
        library: Library<'gc>,
        movie: SwfMovie<'gc>,
    }

    /// A 4x4 RGBA32 bitmap tag, fully self-contained: zlib-compressed Rgb32
    /// rows (straight BGRA quads; the decoder swizzles them to RGBA).
    fn test_bitmap() -> CompressedBitmap {
        let mut pixels = Vec::with_capacity(4 * 4 * 4);
        for _ in 0..4 * 4 {
            pixels.extend([0x10, 0x20, 0x30, 0xFF]);
        }
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&pixels).unwrap();
        CompressedBitmap::Lossless(swf::DefineBitsLossless {
            version: 2,
            id: 1,
            format: swf::BitmapFormat::Rgb32,
            width: 4,
            height: 4,
            data: Cow::Owned(encoder.finish().unwrap()),
        })
    }

    fn null_renderer() -> NullRenderer {
        NullRenderer::new(ViewportDimensions {
            width: 64,
            height: 64,
            scale_factor: 1.0,
        })
    }

    #[test]
    fn gpu_accounting_balances_through_bitmap_character() {
        let _guard = TEST_LOCK.lock().unwrap();
        let baseline = estimated_gpu_texture_bytes();
        let bitmap = BitmapCharacter::new(test_bitmap());
        let mut renderer = null_renderer();

        // Requesting the handle uploads the texture and counts its bytes.
        let _handle = bitmap.bitmap_handle(&mut renderer).unwrap();
        assert_eq!(estimated_gpu_texture_bytes() - baseline, 4 * 4 * 4);
        assert!(bitmap.has_gpu_handle());

        // Releasing drops the handle and un-counts the bytes.
        bitmap.release_gpu_handle();
        assert_eq!(estimated_gpu_texture_bytes(), baseline);
        assert!(!bitmap.has_gpu_handle());

        // Re-requesting lazily re-creates the handle and re-counts.
        let _handle = bitmap.bitmap_handle(&mut renderer).unwrap();
        assert_eq!(estimated_gpu_texture_bytes() - baseline, 4 * 4 * 4);

        // Leave the accounting where we found it (there is no Drop that
        // un-counts, so an unreleased handle would leak into later tests).
        bitmap.release_gpu_handle();
        assert_eq!(estimated_gpu_texture_bytes(), baseline);
    }

    #[test]
    fn release_gpu_resources_zeroes_accounting() {
        let _guard = TEST_LOCK.lock().unwrap();
        let baseline = estimated_gpu_texture_bytes();
        let arena: GpuTestArena = gc_arena::Arena::new(|gc| GpuTestRoot {
            data: GcRefLock::new(
                gc,
                GpuTestData {
                    library: Library::empty(),
                    movie: SwfMovie::new(gc, SwfMovieData::empty(41, None)),
                }
                .into(),
            ),
        });
        arena.mutate(|mc, root| {
            let bitmap = Gc::new(mc, BitmapCharacter::new(test_bitmap()));
            let mut data = root.data.borrow_mut(mc);
            let movie = data.movie;
            data.library
                .library_for_movie_mut(movie)
                .register_character(1, Character::Bitmap(bitmap));
            let mut renderer = null_renderer();
            let _ = bitmap.bitmap_handle(&mut renderer).unwrap();
            assert_eq!(estimated_gpu_texture_bytes() - baseline, 4 * 4 * 4);

            // Releasing the movie's GPU resources (the unload path) drops the
            // handle and un-counts its bytes; the character data is kept.
            data.library
                .library_for_movie_mut(movie)
                .release_gpu_resources();
            assert_eq!(estimated_gpu_texture_bytes(), baseline);
            assert!(!bitmap.has_gpu_handle());
        });
    }

    #[test]
    fn tick_gpu_resources_evicts_unused_bitmaps() {
        let _guard = TEST_LOCK.lock().unwrap();
        let baseline = estimated_gpu_texture_bytes();
        let arena: GpuTestArena = gc_arena::Arena::new(|gc| GpuTestRoot {
            data: GcRefLock::new(
                gc,
                GpuTestData {
                    library: Library::empty(),
                    movie: SwfMovie::new(gc, SwfMovieData::empty(41, None)),
                }
                .into(),
            ),
        });
        arena.mutate(|mc, root| {
            let bitmap = Gc::new(mc, BitmapCharacter::new(test_bitmap()));
            let mut data = root.data.borrow_mut(mc);
            let movie = data.movie;
            data.library
                .library_for_movie_mut(movie)
                .register_character(1, Character::Bitmap(bitmap));
            let mut renderer = null_renderer();
            let _ = bitmap.bitmap_handle(&mut renderer).unwrap();
            assert_eq!(estimated_gpu_texture_bytes() - baseline, 4 * 4 * 4);

            // Land the next tick on the *second* sweep boundary after the
            // upload, so the bitmap's stamp is older than the cutoff: the
            // sweep evicts it.
            let stamp = texture_clock();
            let first_boundary = ((stamp / GPU_SWEEP_INTERVAL) + 1) * GPU_SWEEP_INTERVAL;
            TEXTURE_CLOCK.store(first_boundary + GPU_SWEEP_MAX_AGE - 1, Ordering::Relaxed);
            data.library.tick_gpu_resources(&renderer);

            assert_eq!(estimated_gpu_texture_bytes(), baseline);
            assert!(!bitmap.has_gpu_handle());

            // The evicted texture is re-created lazily on the next request,
            // and releasing it again leaves the accounting where we found it.
            let _ = bitmap.bitmap_handle(&mut renderer).unwrap();
            assert_eq!(estimated_gpu_texture_bytes() - baseline, 4 * 4 * 4);
            bitmap.release_gpu_handle();
            assert_eq!(estimated_gpu_texture_bytes(), baseline);
        });
    }

    #[test]
    fn tick_gpu_resources_keeps_fresh_bitmaps() {
        let _guard = TEST_LOCK.lock().unwrap();
        let baseline = estimated_gpu_texture_bytes();
        let arena: GpuTestArena = gc_arena::Arena::new(|gc| GpuTestRoot {
            data: GcRefLock::new(
                gc,
                GpuTestData {
                    library: Library::empty(),
                    movie: SwfMovie::new(gc, SwfMovieData::empty(41, None)),
                }
                .into(),
            ),
        });
        arena.mutate(|mc, root| {
            let bitmap = Gc::new(mc, BitmapCharacter::new(test_bitmap()));
            let mut data = root.data.borrow_mut(mc);
            let movie = data.movie;
            data.library
                .library_for_movie_mut(movie)
                .register_character(1, Character::Bitmap(bitmap));
            let mut renderer = null_renderer();
            let _ = bitmap.bitmap_handle(&mut renderer).unwrap();
            let stamp = texture_clock();

            // Land the next tick on the *first* sweep boundary after the
            // upload, so the bitmap's stamp stays inside the max-age window:
            // the sweep runs but does not evict it.
            let first_boundary = ((stamp / GPU_SWEEP_INTERVAL) + 1) * GPU_SWEEP_INTERVAL;
            TEXTURE_CLOCK.store(first_boundary - 1, Ordering::Relaxed);
            data.library.tick_gpu_resources(&renderer);

            assert_eq!(estimated_gpu_texture_bytes() - baseline, 4 * 4 * 4);
            assert!(bitmap.has_gpu_handle());

            bitmap.release_gpu_handle();
            assert_eq!(estimated_gpu_texture_bytes(), baseline);
        });
    }

    #[test]
    fn tick_gpu_resources_rebaselines_drifted_accounting() {
        let _guard = TEST_LOCK.lock().unwrap();
        let baseline = estimated_gpu_texture_bytes();
        let arena: GpuTestArena = gc_arena::Arena::new(|gc| GpuTestRoot {
            data: GcRefLock::new(
                gc,
                GpuTestData {
                    library: Library::empty(),
                    movie: SwfMovie::new(gc, SwfMovieData::empty(41, None)),
                }
                .into(),
            ),
        });
        arena.mutate(|mc, root| {
            let bitmap = Gc::new(mc, BitmapCharacter::new(test_bitmap()));
            let mut data = root.data.borrow_mut(mc);
            let movie = data.movie;
            data.library
                .library_for_movie_mut(movie)
                .register_character(1, Character::Bitmap(bitmap));
            let mut renderer = null_renderer();
            let _ = bitmap.bitmap_handle(&mut renderer).unwrap();

            // Simulate a handle dropped without an explicit release (a
            // character collected by the GC): the account now over-reports.
            add_gpu_texture_bytes(1024 * 1024);
            assert_eq!(
                estimated_gpu_texture_bytes() - baseline,
                4 * 4 * 4 + 1024 * 1024
            );

            // The sweep re-baselines the account to the live handles, so the
            // stale surplus disappears even though this bitmap survives it.
            let stamp = texture_clock();
            let first_boundary = ((stamp / GPU_SWEEP_INTERVAL) + 1) * GPU_SWEEP_INTERVAL;
            TEXTURE_CLOCK.store(first_boundary - 1, Ordering::Relaxed);
            data.library.tick_gpu_resources(&renderer);

            assert_eq!(estimated_gpu_texture_bytes() - baseline, 4 * 4 * 4);
            assert!(bitmap.has_gpu_handle());

            bitmap.release_gpu_handle();
            assert_eq!(estimated_gpu_texture_bytes(), baseline);
        });
    }

    #[test]
    fn parse_gpu_budget_mb_accepts_positive_values() {
        assert_eq!(parse_gpu_budget_mb("512"), Some(512 * 1024 * 1024));
        assert_eq!(parse_gpu_budget_mb(" 1024 "), Some(1024 * 1024 * 1024));
        // Extremely large values saturate rather than overflowing.
        assert_eq!(parse_gpu_budget_mb("18446744073709551615"), Some(u64::MAX));
    }

    #[test]
    fn parse_gpu_budget_mb_rejects_invalid_values() {
        assert_eq!(parse_gpu_budget_mb(""), None);
        assert_eq!(parse_gpu_budget_mb("0"), None);
        assert_eq!(parse_gpu_budget_mb("abc"), None);
        assert_eq!(parse_gpu_budget_mb("-100"), None);
    }

    #[test]
    fn eviction_plan_is_empty_without_pressure() {
        assert!(eviction_plan(&[(1, 100), (2, 100)], 0).is_empty());
    }

    #[test]
    fn eviction_plan_evicts_least_recently_used_first() {
        let live = [(10, 100), (20, 100), (30, 100)];
        assert_eq!(eviction_plan(&live, 100), vec![0]);
        assert_eq!(eviction_plan(&live, 150), vec![0, 1]);
    }

    #[test]
    fn eviction_plan_handles_unsorted_live_textures() {
        let live = [(30, 50), (10, 50), (20, 50)];
        // Newest entry is first in the list, oldest is in the middle.
        assert_eq!(eviction_plan(&live, 50), vec![1]);
        assert_eq!(eviction_plan(&live, 150), vec![1, 2, 0]);
    }

    #[test]
    fn eviction_plan_evicts_everything_when_pressure_exceeds_total() {
        let live = [(10, 40), (20, 40)];
        assert_eq!(eviction_plan(&live, 1000), vec![0, 1]);
    }

    #[test]
    fn eviction_plan_accounts_for_zero_byte_entries() {
        // A texture that reports no size frees nothing, so eviction continues.
        let live = [(1, 0), (2, 100)];
        assert_eq!(eviction_plan(&live, 100), vec![0, 1]);
    }
}
