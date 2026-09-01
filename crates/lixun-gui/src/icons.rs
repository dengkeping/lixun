//! Icon resolution for hits: theme icon names, absolute-path icons,
//! category fallbacks. Returns GdkPaintable for use in
//! `gtk::Image::set_from_paintable`.
//!
//! Two-tier cache (A5 / render fastpath):
//!   * Main-thread `PAINTABLE_CACHE` (`thread_local!`) dedupes the
//!     hot `IconTheme::lookup_icon` path for the same
//!     `(name, size, scale)` across the rows of a render pass.
//!     `IconTheme::lookup_icon` itself is cheap (metadata only, the
//!     pixel work is deferred to paint time), but instantiating one
//!     `IconPaintable` per row when many rows share an icon name
//!     still adds up under burst input.
//!   * Cross-thread texture cache (`Arc<RwLock<HashMap>>`) holds
//!     `gdk::Texture` instances loaded from absolute file paths by
//!     a dedicated worker thread. `Texture::from_filename` reads
//!     the file off disk; performing that read on the GTK main
//!     thread during `bind` was the latency hazard A5 removes.
//!     The worker drains an `std::sync::mpsc` queue, populates the
//!     cache, and signals completion on `async_channel` so the
//!     main loop can react. The boundary mirrors the
//!     `std::thread::spawn` + `async_channel` pattern used by
//!     `ipc::start_ipc_thread`; no `tokio` is involved.
//!
//! `gdk::Texture` is upstream-marked `Send + Sync` (immutable
//! refcounted GObject), which is what makes the cross-thread cache
//! sound. `gdk::Paintable` and `gtk::IconPaintable` are NOT
//! `Send`, so the main-thread paintable cache stays thread-local
//! and the worker only deals in textures.

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, OnceLock, RwLock};

use gtk::gdk;
use gtk::prelude::*;
use lixun_core::{Category, Hit};

/// Cache key for both tiers: `(icon name or absolute path, pixel
/// size, scale factor)`. Scale is currently always `1`; the field
/// exists so future HiDPI support can vary the key without touching
/// the callers or the worker protocol.
type IconKey = (String, i32, i32);

thread_local! {
    static THEME: OnceCell<Option<gtk::IconTheme>> = const { OnceCell::new() };

    /// Main-thread paintable cache. Bounded by the small, stable
    /// set of icon-name/size pairs seen during a session (typical
    /// upper bound: a few hundred entries). Reset only on process
    /// exit. Stores `Option<Paintable>` so a confirmed-miss
    /// (theme has no such icon, file doesn't exist) is recorded
    /// and the lookup is not repeated on every bind.
    static PAINTABLE_CACHE: RefCell<HashMap<IconKey, Option<gdk::Paintable>>> =
        RefCell::new(HashMap::new());
}

fn icon_theme() -> Option<gtk::IconTheme> {
    THEME.with(|cell| {
        cell.get_or_init(|| {
            let theme = gdk::Display::default().map(|d| gtk::IconTheme::for_display(&d));
            if theme.is_none() {
                // Warn once per process: THEME is thread-local, so
                // the OnceCell alone would re-log on every thread
                // that touches icon resolution.
                use std::sync::atomic::{AtomicBool, Ordering};
                static WARNED: AtomicBool = AtomicBool::new(false);
                if !WARNED.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        "icon theme unavailable (no GDK display); \
                         category fallback icon names are in use without theme lookup"
                    );
                }
            }
            theme
        })
        .clone()
    })
}

/// Default icon name for a hit category. Used as a last-resort
/// fallback when a source does not declare its own `icon_name`
/// and no theme-matched icon is available. Category is intentional
/// here because it classifies the hit semantically (app, file,
/// mail, ...) — not its originating plugin. Host code that needs
/// an icon-name fallback should call this function rather than
/// reinventing the mapping, to keep the category → icon contract
/// in one place.
pub(crate) fn category_fallback(cat: &Category) -> &'static str {
    match cat {
        Category::App => "application-x-executable",
        Category::File => "text-x-generic",
        Category::Mail => "mail-message",
        Category::Attachment => "mail-attachment",
        Category::Calculator => "accessories-calculator",
        Category::Shell => "utilities-terminal",
    }
}

fn lookup_theme_icon(theme: &gtk::IconTheme, name: &str, size: i32) -> Option<gdk::Paintable> {
    if !theme.has_icon(name) {
        return None;
    }
    let paintable = theme.lookup_icon(
        name,
        &[],
        size,
        1,
        gtk::TextDirection::Ltr,
        gtk::IconLookupFlags::empty(),
    );
    Some(paintable.upcast::<gdk::Paintable>())
}

// ============================================================
// Off-main-thread texture cache + worker
// ============================================================

struct TextureCache {
    /// `Some(Some(tex))` = loaded; `Some(None)` = previously
    /// attempted, file missing/invalid (do not retry); absent from
    /// the map = not yet attempted.
    map: RwLock<HashMap<IconKey, Option<gdk::Texture>>>,
}

/// How the worker should turn a cache key into pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadKind {
    /// The key's string is an absolute path to an icon-sized image
    /// (e.g. a `.desktop`-declared icon file). Decoded with a
    /// downscale-to-request so a stray large file never becomes a
    /// full-size texture.
    IconFile,
    /// The key's string is an absolute path to a PHOTO the row
    /// should thumbnail (R1): try the freedesktop thumbnail cache
    /// first (`$XDG_CACHE_HOME/thumbnails/{large,normal}/<md5 of
    /// file URI>.png` per the spec), fall back to a downscaled
    /// direct decode of the image itself.
    ImageThumb,
}

static TEXTURE_CACHE: OnceLock<Arc<TextureCache>> = OnceLock::new();
static REQUEST_TX: OnceLock<mpsc::Sender<(IconKey, LoadKind)>> = OnceLock::new();
static READY_RX: OnceLock<async_channel::Receiver<IconKey>> = OnceLock::new();

fn texture_cache() -> &'static Arc<TextureCache> {
    TEXTURE_CACHE.get_or_init(|| {
        let cache = Arc::new(TextureCache {
            map: RwLock::new(HashMap::new()),
        });
        let (req_tx, req_rx) = mpsc::channel::<(IconKey, LoadKind)>();
        let (rdy_tx, rdy_rx) = async_channel::unbounded::<IconKey>();
        // Order matters: set the sinks BEFORE spawning the worker
        // so that callers racing the worker can always enqueue.
        let _ = REQUEST_TX.set(req_tx);
        let _ = READY_RX.set(rdy_rx);

        let worker_state = Arc::clone(&cache);
        std::thread::Builder::new()
            .name("lixun-icon-loader".into())
            .spawn(move || icon_loader_loop(worker_state, req_rx, rdy_tx))
            .expect("spawn icon loader thread");
        cache
    })
}

fn icon_loader_loop(
    cache: Arc<TextureCache>,
    req_rx: mpsc::Receiver<(IconKey, LoadKind)>,
    ready_tx: async_channel::Sender<IconKey>,
) {
    while let Ok((key, kind)) = req_rx.recv() {
        // De-dupe: another request may already have populated this
        // slot between enqueue and dispatch.
        if cache.map.read().unwrap().contains_key(&key) {
            continue;
        }
        let (name, size, scale) = &key;
        let texture = if PathBuf::from(name).is_absolute() {
            // Only absolute paths are loaded here. Theme icons
            // resolve on the main thread via `IconTheme::lookup_icon`
            // because the theme handle is not `Send` and the lookup
            // itself does no disk I/O (deferred to paint time).
            let px = size * scale;
            match kind {
                LoadKind::IconFile => load_scaled_texture(std::path::Path::new(name), px),
                LoadKind::ImageThumb => load_photo_thumbnail(std::path::Path::new(name), px),
            }
        } else {
            None
        };
        cache.map.write().unwrap().insert(key.clone(), texture);
        // Best-effort: the receiver may have been dropped at
        // shutdown; we don't unwrap.
        let _ = ready_tx.send_blocking(key);
    }
}

/// Decode `path` downscaled to at most `px` on the long edge and
/// upload as a texture. `from_file_at_scale` reads the image header
/// and decodes at target size, so a 40-megapixel JPEG never becomes
/// a full-resolution texture on the row-icon path (R1).
fn load_scaled_texture(path: &std::path::Path, px: i32) -> Option<gdk::Texture> {
    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(path, px, px, true).ok()?;
    Some(gdk::Texture::for_pixbuf(&pixbuf))
}

/// Freedesktop thumbnail spec lookup key: hex MD5 of the file's
/// canonical `file://` URI (gio produces the same percent-encoding
/// the spec mandates).
fn thumbnail_hash(path: &std::path::Path) -> String {
    let uri = gtk::gio::File::for_path(path).uri();
    format!("{:x}", md5::compute(uri.as_bytes()))
}

/// Resolve a photo's row icon (R1): reuse the desktop's existing
/// thumbnail cache when a thumbnailer already produced one, else
/// decode the photo itself, downscaled. Runs on the loader thread.
fn load_photo_thumbnail(path: &std::path::Path, px: i32) -> Option<gdk::Texture> {
    let hash = thumbnail_hash(path);
    if let Some(cache_dir) = dirs::cache_dir() {
        for bucket in ["large", "normal"] {
            let thumb = cache_dir
                .join("thumbnails")
                .join(bucket)
                .join(format!("{hash}.png"));
            if thumb.exists()
                && let Some(tex) = load_scaled_texture(&thumb, px)
            {
                return Some(tex);
            }
        }
    }
    load_scaled_texture(path, px)
}

/// Receiver for `(name, size, scale)` keys whose textures have
/// just been populated in the cross-thread cache. The GTK main
/// loop drains this and provokes a ListView re-bind so newly
/// loaded textures appear without requiring the user to scroll
/// or re-type. Mirrors the `async_channel` boundary used by
/// `ipc::start_ipc_thread`.
pub(crate) fn icon_ready_rx() -> async_channel::Receiver<IconKey> {
    let _ = texture_cache();
    READY_RX
        .get()
        .expect("texture cache initialised by texture_cache()")
        .clone()
}

fn lookup_cached_texture(path: &std::path::Path, size: i32, kind: LoadKind) -> Option<gdk::Paintable> {
    let key: IconKey = (path.to_string_lossy().into_owned(), size, 1);
    {
        let map = texture_cache().map.read().unwrap();
        if let Some(slot) = map.get(&key) {
            // Cache hit (loaded or known-bad). Returning None here
            // means "we already tried and the file isn't usable";
            // resolve_icon falls through to the category fallback
            // and does NOT re-enqueue.
            return slot.as_ref().map(|t| t.clone().upcast::<gdk::Paintable>());
        }
    }
    // Cache miss: enqueue the load and return None for this pass.
    // The next bind — provoked by the icon-ready receiver, or by
    // a natural re-render — will see the populated slot.
    if let Some(tx) = REQUEST_TX.get() {
        let _ = tx.send((key, kind));
    }
    None
}

pub(crate) fn resolve_icon(hit: &Hit, size: i32) -> Option<gdk::Paintable> {
    let theme = icon_theme()?;

    // R1: an image-content search engine must not render every photo
    // as the same generic icon. For hits whose (generic) MIME says
    // image/* and whose action points at a real file, the row icon
    // IS the photo — desktop thumbnail cache first, downscaled
    // direct decode as fallback, loaded off the main thread through
    // the existing texture worker. Keying on MIME keeps the host
    // plugin-agnostic (AGENTS.md §1).
    if hit.mime.as_deref().is_some_and(|m| m.starts_with("image/"))
        && let Some(path) = crate::factory::hit_file_path(hit)
        && path.is_absolute()
    {
        let key: IconKey = (path.to_string_lossy().into_owned(), size, 1);
        if let Some(cached) = PAINTABLE_CACHE.with(|c| c.borrow().get(&key).cloned()) {
            if let Some(paintable) = cached {
                return Some(paintable);
            }
            // Known-bad photo: fall through to the normal icon flow.
        } else if let Some(paintable) = lookup_cached_texture(&path, size, LoadKind::ImageThumb) {
            PAINTABLE_CACHE.with(|c| {
                c.borrow_mut().insert(key, Some(paintable.clone()));
            });
            return Some(paintable);
        }
        // Miss on this pass (load enqueued) or known-bad: continue
        // to icon_name / category fallback below. Known-bad is
        // recorded in the texture cache, so the enqueue is a cheap
        // no-op on subsequent binds.
    }

    if let Some(name) = hit.icon_name.as_deref() {
        let key: IconKey = (name.to_string(), size, 1);
        // Hot path: main-thread paintable cache hit?
        if let Some(cached) = PAINTABLE_CACHE.with(|c| c.borrow().get(&key).cloned()) {
            return cached;
        }

        let p = std::path::Path::new(name);
        let resolved = if p.is_absolute() {
            lookup_cached_texture(p, size, LoadKind::IconFile)
        } else {
            lookup_theme_icon(&theme, name, size)
        };

        if let Some(paintable) = resolved {
            PAINTABLE_CACHE.with(|c| {
                c.borrow_mut().insert(key, Some(paintable.clone()));
            });
            return Some(paintable);
        }
        // Resolution failed this pass. For absolute paths this is
        // expected on the first miss (load enqueued, not yet
        // ready). For theme names it means the theme genuinely
        // doesn't carry that icon — still worth caching the
        // negative result so a fixed-string icon_name like
        // `"text-x-generic"` doesn't re-walk the theme on every
        // bind across a session.
        if !p.is_absolute() {
            PAINTABLE_CACHE.with(|c| {
                c.borrow_mut().insert(key, None);
            });
        }
        // For absolute paths we leave the entry absent so the
        // next bind retries (the texture cache itself records
        // the negative result, so re-enqueueing is cheap and
        // idempotent).
    }

    let fallback = category_fallback(&hit.category);
    let fb_key: IconKey = (fallback.to_string(), size, 1);
    if let Some(cached) = PAINTABLE_CACHE.with(|c| c.borrow().get(&fb_key).cloned()) {
        return cached;
    }
    let result = lookup_theme_icon(&theme, fallback, size);
    PAINTABLE_CACHE.with(|c| {
        c.borrow_mut().insert(fb_key, result.clone());
    });
    result
}
