//! Texture cache for theme images.
//!
//! A theme may point at decorative images (`ImageSpec`), and the UI must not
//! re-decode a PNG every frame.  [`ImageCache`] keeps one
//! [`egui::TextureHandle`] per path, decodes lazily via the `image` crate, and
//! remembers failures so a missing file logs **one** warning per session
//! instead of 25 warnings per second (the VU meters repaint at 25 Hz).
//!
//! Failure is never fatal: [`ImageCache::get`] returns `None` and the strip
//! paints its gradient body, which is exactly what a user expects from a theme
//! that references a file they deleted.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use egui::{ColorImage, Context, TextureHandle, TextureOptions, Vec2};

use super::ImageSpec;

/// Cache of decoded theme images, keyed by resolved path.
#[derive(Default)]
pub struct ImageCache {
    /// `None` = "this path already failed, do not retry this session".
    entries: HashMap<String, Option<TextureHandle>>,
    /// Paths whose failure has already been logged.
    warned: HashSet<String>,
    /// Directory relative paths are resolved against (the theme file's dir).
    base_dir: Option<PathBuf>,
    /// Number of decode attempts, exposed for tests (`warn-once` behaviour).
    attempts: usize,
}

impl ImageCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve relative paths against `dir` (normally the theme file's dir).
    pub fn with_base_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: Some(dir.into()),
            ..Self::default()
        }
    }

    /// Absolute (or base-dir-joined) path of a spec, as used for the cache key.
    pub fn resolve_path(&self, spec: &ImageSpec) -> PathBuf {
        let path = Path::new(&spec.path);
        if path.is_absolute() {
            return path.to_path_buf();
        }
        match &self.base_dir {
            Some(dir) => dir.join(path),
            None => path.to_path_buf(),
        }
    }

    /// Number of decode attempts made so far (successful or not).
    pub fn attempts(&self) -> usize {
        self.attempts
    }

    /// Has this path been seen and failed?
    pub fn failed(&self, key: &str) -> bool {
        matches!(self.entries.get(key), Some(None))
    }

    /// Texture for `spec`, loading it on first use.
    ///
    /// Returns the handle (cheap to clone: it is an `Arc` inside) and the
    /// texture size in points, so the caller can compute a [`Fit`](super::Fit)
    /// mapping without asking egui again.
    pub fn get(&mut self, ctx: &Context, spec: &ImageSpec) -> Option<(TextureHandle, Vec2)> {
        if spec.path.trim().is_empty() {
            return None;
        }
        let key = self.resolve_path(spec).display().to_string();
        if let Some(cached) = self.entries.get(&key) {
            return cached.as_ref().map(|handle| {
                let size = Vec2::new(handle.size()[0] as f32, handle.size()[1] as f32);
                (handle.clone(), size)
            });
        }

        self.attempts += 1;
        let loaded = decode_image(&key);
        let entry = match loaded {
            Ok(image) => {
                let options = match spec.fit {
                    super::Fit::Tile => TextureOptions::LINEAR_REPEAT,
                    _ => TextureOptions::LINEAR,
                };
                let name = format!("theme-image:{key}");
                let handle = ctx.load_texture(name, image, options);
                Some(handle)
            }
            Err(reason) => {
                if self.warned.insert(key.clone()) {
                    log::warn!("theme image {key:?} unavailable ({reason}) — using the body fill");
                }
                None
            }
        };
        let result = entry.as_ref().map(|handle| {
            let size = Vec2::new(handle.size()[0] as f32, handle.size()[1] as f32);
            (handle.clone(), size)
        });
        self.entries.insert(key, entry);
        result
    }

    /// Drop every texture (used when the theme changes).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.warned.clear();
        self.attempts = 0;
    }
}

/// Decode an image file into an egui [`ColorImage`].
///
/// Kept separate from [`ImageCache`] so it is testable without a `Context`:
/// the only egui type involved is a plain struct.
pub fn decode_image(path: impl AsRef<Path>) -> Result<ColorImage, String> {
    let path = path.as_ref();
    let reader = image::ImageReader::open(path).map_err(|e| format!("open: {e}"))?;
    let reader = reader
        .with_guessed_format()
        .map_err(|e| format!("format: {e}"))?;
    let decoded = reader.decode().map_err(|e| format!("decode: {e}"))?;
    let rgba = decoded.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Fit;

    /// Write a tiny PNG into a unique temp path and return it.
    fn temp_png(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "holaf-ui-imgcache-{}-{name}.png",
            std::process::id()
        ));
        let img = image::RgbaImage::from_pixel(4, 3, image::Rgba([10u8, 20, 30, 255]));
        img.save(&path).expect("write test png");
        path
    }

    #[test]
    fn decode_reads_size_and_pixels() {
        let path = temp_png("decode");
        let image = decode_image(&path).expect("decode");
        assert_eq!(image.size, [4, 3]);
        assert_eq!(image.pixels.len(), 12);
        assert_eq!(image.pixels[0].r(), 10);
        assert_eq!(image.pixels[0].g(), 20);
        assert_eq!(image.pixels[0].b(), 30);
        assert_eq!(image.pixels[0].a(), 255);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn decode_missing_file_is_an_error() {
        let err = decode_image("/nonexistent/holaf/nope.png").unwrap_err();
        assert!(err.contains("open"), "{err}");
    }

    #[test]
    fn decode_non_image_file_is_an_error() {
        let path = std::env::temp_dir().join(format!(
            "holaf-ui-imgcache-{}-notanimage.txt",
            std::process::id()
        ));
        std::fs::write(&path, b"this is not a png").unwrap();
        assert!(decode_image(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn relative_paths_resolve_against_the_base_dir() {
        let cache = ImageCache::with_base_dir("/themes/holaf");
        let spec = ImageSpec::new("bg.png");
        assert_eq!(cache.resolve_path(&spec), PathBuf::from("/themes/holaf/bg.png"));

        let absolute = ImageSpec::new("/elsewhere/bg.png");
        assert_eq!(cache.resolve_path(&absolute), PathBuf::from("/elsewhere/bg.png"));

        let no_base = ImageCache::new();
        assert_eq!(no_base.resolve_path(&spec), PathBuf::from("bg.png"));
    }

    #[test]
    fn empty_path_never_touches_the_context() {
        let ctx = Context::default();
        let mut cache = ImageCache::new();
        assert!(cache.get(&ctx, &ImageSpec::default()).is_none());
        assert_eq!(cache.attempts(), 0);
    }

    #[test]
    fn missing_file_warns_only_once_and_is_remembered() {
        // A `Context` can be created without any renderer: `load_texture` only
        // registers the texture in the texture manager.
        let ctx = Context::default();
        let mut cache = ImageCache::new();
        let spec = ImageSpec::new("/nonexistent/holaf/nope.png");
        assert!(cache.get(&ctx, &spec).is_none());
        assert_eq!(cache.attempts(), 1);
        // Second and third calls must hit the negative cache (no retry, no log).
        assert!(cache.get(&ctx, &spec).is_none());
        assert!(cache.get(&ctx, &spec).is_none());
        assert_eq!(cache.attempts(), 1, "a failed path must not be retried");
        let key = cache.resolve_path(&spec).display().to_string();
        assert!(cache.failed(&key));
    }

    #[test]
    fn existing_file_is_loaded_then_served_from_cache() {
        let ctx = Context::default();
        let path = temp_png("load");
        let mut cache = ImageCache::new();
        let spec = ImageSpec {
            fit: Fit::Cover,
            ..ImageSpec::new(path.display().to_string())
        };
        let (handle, size) = cache.get(&ctx, &spec).expect("texture");
        assert_eq!(size, Vec2::new(4.0, 3.0));
        assert_eq!(handle.size(), [4, 3]);
        assert_eq!(cache.attempts(), 1);
        let (again, size2) = cache.get(&ctx, &spec).expect("cached texture");
        assert_eq!(again.id(), handle.id());
        assert_eq!(size2, size);
        assert_eq!(cache.attempts(), 1, "decoded once, then cached");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn clear_forgets_failures_so_a_fixed_theme_reloads() {
        let ctx = Context::default();
        let mut cache = ImageCache::new();
        let spec = ImageSpec::new("/nonexistent/holaf/nope.png");
        assert!(cache.get(&ctx, &spec).is_none());
        cache.clear();
        assert_eq!(cache.attempts(), 0);
        assert!(cache.get(&ctx, &spec).is_none());
        assert_eq!(cache.attempts(), 1, "after clear the path is retried");
    }
}
