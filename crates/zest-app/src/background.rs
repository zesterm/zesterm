//! From a settings path to an uploaded texture.
//!
//! The decoder lives here rather than in `zest-render-wgpu` so the renderer
//! never grows a dependency on the image-format zoo — it takes RGBA8 bytes and
//! nothing else. `image` is already linked into this binary for `--screenshot`,
//! so this costs no new crate.
//!
//! The one rule worth stating: **a picture that cannot be loaded draws
//! nothing.** Not a black pane, not a panic, not a dialog. A mistyped path
//! leaves the window looking exactly as it did before the setting was touched,
//! which is the only behaviour that stays honest while someone is typing one.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::SystemTime;

use zest_render_wgpu::{BackgroundFit, ImageId, ImageStore};

/// The largest picture kept in VRAM, per axis.
///
/// A phone camera's 6000x4000 is 96 MB of RGBA8, uploaded to be sampled onto a
/// pane a fraction of that size. Downscaling costs one resize at load and is
/// invisible on screen; not downscaling is most of a GPU's memory spent on
/// detail no pixel can show.
const MAX_AXIS: u32 = 4096;

/// What decides whether a file has to be read again.
type Stamp = (u64, Option<SystemTime>);

enum Slot {
    Ready { id: ImageId, size: [u32; 2], stamp: Option<Stamp> },
    /// The file is missing or does not decode. Remembered so the warning is
    /// logged once rather than on every frame.
    Failed { stamp: Option<Stamp> },
}

struct Entry {
    slot: Slot,
    /// The generation this path was last asked for. See [`Backgrounds::invalidate`].
    seen: u64,
}

/// The loaded pictures, keyed by the settings value that named them.
#[derive(Default)]
pub struct Backgrounds {
    entries: HashMap<String, Entry>,
    generation: u64,
}

impl Backgrounds {
    /// The picture for a settings path, loading it the first time it is seen.
    ///
    /// Cheap on every later call: a hash lookup, no `stat`. The file is only
    /// re-examined after [`Self::invalidate`], which is what a config reload
    /// calls — so a slider drag on some unrelated setting does not re-decode a
    /// photograph on every keystroke.
    pub fn get(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        store: &mut ImageStore,
        setting: &str,
    ) -> Option<(ImageId, [u32; 2])> {
        if setting.trim().is_empty() {
            return None;
        }

        let generation = self.generation;
        if let Some(entry) = self.entries.get_mut(setting) {
            if entry.seen == generation {
                return ready(&entry.slot);
            }
            entry.seen = generation;
            // A generation has passed, so the file may have moved under us --
            // an image editor saving over it is the case that matters. The
            // stamp decides; an unchanged one reuses the texture already up.
            let stamp = stamp_of(setting);
            if stamp.is_some() && stamp == entry.slot.stamp() {
                return ready(&entry.slot);
            }
        }

        let slot = load(device, queue, store, setting);
        let ready = ready(&slot);
        self.entries.insert(setting.to_string(), Entry { slot, seen: generation });
        ready
    }

    /// Start a new generation: re-examine every file, and forget what the
    /// configuration no longer names.
    ///
    /// Called on a config reload. A path nobody asked for during the generation
    /// just ended is not in the settings any more — replaced while someone
    /// tried three wallpapers, or on a profile they deleted — and each one held
    /// up to 64 MB of VRAM, so dropping it here is not tidiness.
    pub fn invalidate(&mut self, store: &mut ImageStore) {
        let generation = self.generation;
        self.entries.retain(|_, e| e.seen == generation);
        let live: Vec<ImageId> = self.entries.values().filter_map(|e| e.slot.id()).collect();
        store.retain(|id| live.contains(&id));
        self.generation += 1;
    }
}

impl Slot {
    fn stamp(&self) -> Option<Stamp> {
        match self {
            Self::Ready { stamp, .. } | Self::Failed { stamp } => *stamp,
        }
    }

    fn id(&self) -> Option<ImageId> {
        match self {
            Self::Ready { id, .. } => Some(*id),
            Self::Failed { .. } => None,
        }
    }
}

fn ready(slot: &Slot) -> Option<(ImageId, [u32; 2])> {
    match slot {
        Slot::Ready { id, size, .. } => Some((*id, *size)),
        Slot::Failed { .. } => None,
    }
}

/// Where a settings value points.
///
/// A relative path resolves against the config directory, so a config that
/// travels with its pictures keeps working on another machine — the same
/// courtesy `themes_dir` already extends to theme files.
#[must_use]
pub fn resolve_path(setting: &str) -> Option<PathBuf> {
    let raw = PathBuf::from(setting.trim());
    if raw.as_os_str().is_empty() {
        return None;
    }
    if raw.is_absolute() {
        return Some(raw);
    }
    Some(zest_config::paths::config_dir()?.join(raw))
}

fn stamp_of(setting: &str) -> Option<Stamp> {
    let meta = std::fs::metadata(resolve_path(setting)?).ok()?;
    Some((meta.len(), meta.modified().ok()))
}

/// The id a path always gets.
///
/// Hashed from the path alone rather than from the path *and* its stamp, so an
/// edited file replaces the texture already uploaded under that id instead of
/// leaving the old one stranded with nothing left pointing at it.
fn id_of(setting: &str) -> ImageId {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    setting.hash(&mut h);
    ImageId(h.finish())
}

fn load(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    store: &mut ImageStore,
    setting: &str,
) -> Slot {
    let stamp = stamp_of(setting);
    let Some(path) = resolve_path(setting) else {
        return Slot::Failed { stamp };
    };

    let decoded = image::ImageReader::open(&path)
        .and_then(image::ImageReader::with_guessed_format)
        .map_err(|e| e.to_string())
        .and_then(|r| r.decode().map_err(|e| e.to_string()));
    let decoded = match decoded {
        Ok(img) => img,
        Err(err) => {
            // Once per path per generation, because this is reached while
            // someone is typing a path into a settings field and every prefix
            // of it is a file that does not exist.
            tracing::warn!(path = %path.display(), %err, "background picture not loaded");
            return Slot::Failed { stamp };
        }
    };

    let decoded = if decoded.width() > MAX_AXIS || decoded.height() > MAX_AXIS {
        // `resize` fits inside the box and keeps the aspect ratio, so the
        // placement maths downstream still sees the picture's real shape.
        decoded.resize(MAX_AXIS, MAX_AXIS, image::imageops::FilterType::Triangle)
    } else {
        decoded
    };

    let rgba = decoded.to_rgba8();
    let size = [rgba.width(), rgba.height()];
    let id = id_of(setting);
    if !store.upload(device, queue, id, size, rgba.as_raw()) {
        return Slot::Failed { stamp };
    }
    tracing::debug!(path = %path.display(), w = size[0], h = size[1], "background picture loaded");
    Slot::Ready { id, size, stamp }
}

/// The renderer's placement mode for a settings one.
#[must_use]
pub fn fit_of(fit: zest_config::settings::BackgroundFit) -> BackgroundFit {
    match fit {
        zest_config::settings::BackgroundFit::Fill => BackgroundFit::Fill,
        zest_config::settings::BackgroundFit::Fit => BackgroundFit::Fit,
        zest_config::settings::BackgroundFit::Watermark => BackgroundFit::Watermark,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_setting_points_nowhere() {
        // The everyday case -- almost nobody sets a picture -- and it must not
        // reach the filesystem at all.
        assert!(resolve_path("").is_none());
        assert!(resolve_path("   ").is_none());
    }

    #[test]
    fn an_absolute_path_is_taken_as_it_stands() {
        let abs = if cfg!(windows) { r"C:\pictures\a.png" } else { "/pictures/a.png" };
        assert_eq!(resolve_path(abs), Some(PathBuf::from(abs)));
    }

    #[test]
    fn a_relative_path_hangs_off_the_config_directory() {
        // The property that lets a config directory be copied to another
        // machine with its pictures beside it.
        let Some(dir) = zest_config::paths::config_dir() else { return };
        assert_eq!(resolve_path("wall.png"), Some(dir.join("wall.png")));
    }

    #[test]
    fn a_path_keeps_one_id_however_often_it_is_asked_for() {
        // The id must not fold in the file's stamp: an edited picture has to
        // *replace* the texture under its id, or every save leaves the previous
        // one in VRAM with nothing pointing at it.
        assert_eq!(id_of("a.png"), id_of("a.png"));
        assert_ne!(id_of("a.png"), id_of("b.png"));
    }

    #[test]
    fn every_fit_maps_to_its_own_mode() {
        use zest_config::settings::BackgroundFit as S;
        // A match arm copied and not edited is the whole failure mode here, and
        // it looks like a placement bug rather than a typo.
        let all = [S::Fill, S::Fit, S::Watermark].map(fit_of);
        assert_eq!(all, [BackgroundFit::Fill, BackgroundFit::Fit, BackgroundFit::Watermark]);
    }
}
