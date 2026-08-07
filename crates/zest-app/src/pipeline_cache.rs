//! A persistent driver pipeline cache.
//!
//! Creating the render pipelines is ~400-500ms of a cold start — the largest
//! remaining chunk once the window itself appears in ~60ms. It is also entirely
//! re-derivable work: the driver compiles the same shaders to the same machine
//! code every launch. Handing it back what it produced last time removes most of
//! that.
//!
//! # Why this is `unsafe`, and why it is still safe here
//!
//! Feeding a driver a pipeline cache blob is trusting it not to be malicious or
//! corrupt, which is why `create_pipeline_cache` is unsafe. Three things keep it
//! sound in practice: the blob is written by this program into a
//! per-user directory, `fallback: true` tells wgpu to start empty rather than
//! fail if the driver rejects it, and the file is keyed by adapter and driver
//! version so a GPU or driver change starts fresh rather than feeding one
//! driver another's blob.

use std::path::PathBuf;

/// Where the cache lives, keyed so it is never shared across drivers.
///
/// A blob from one driver version handed to another is exactly the case the
/// safety warning is about. Drivers do validate their own headers, but keying
/// the filename means we never even ask.
fn cache_path(info: &wgpu::AdapterInfo) -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("dev", "zesterm", "zesterm")?;
    let dir = dirs.cache_dir().to_path_buf();

    // Sanitize: adapter names contain spaces, brackets and slashes.
    let key: String = format!("{}-{}-{:?}", info.name, info.driver_info, info.backend)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let key: String = key.chars().take(120).collect();

    Some(dir.join(format!("pipeline-{key}.bin")))
}

/// Read a previously saved blob, if one exists for this adapter.
pub fn load(info: &wgpu::AdapterInfo) -> Option<Vec<u8>> {
    let path = cache_path(info)?;
    let data = std::fs::read(&path).ok()?;
    tracing::debug!(bytes = data.len(), ?path, "pipeline cache loaded");
    Some(data)
}

/// Create the cache object, optionally seeded from disk.
///
/// Returns `None` when the adapter cannot do it, in which case pipelines are
/// compiled from scratch as before.
pub fn create(
    device: &wgpu::Device,
    supported: bool,
    data: Option<&[u8]>,
) -> Option<wgpu::PipelineCache> {
    if !supported {
        return None;
    }
    // SAFETY: `data`, when present, was written by `PipelineCache::get_data` on
    // this machine (see `save`), into a per-user cache directory whose filename
    // is keyed by adapter name, driver version and backend. `fallback: true`
    // makes wgpu start with an empty cache rather than fail if the driver
    // rejects the blob anyway.
    Some(unsafe {
        device.create_pipeline_cache(&wgpu::PipelineCacheDescriptor {
            label: Some("zesterm pipelines"),
            data,
            fallback: true,
        })
    })
}

/// Write the cache back out, if the driver produced anything new.
///
/// Failures are logged and ignored: a missing cache costs startup time, never
/// correctness, so it must never be able to stop the terminal from running.
pub fn save(cache: &wgpu::PipelineCache, info: &wgpu::AdapterInfo, previous_len: usize) {
    let Some(data) = cache.get_data() else { return };
    if data.len() == previous_len {
        // Nothing new compiled; rewriting would only churn the disk.
        return;
    }
    let Some(path) = cache_path(info) else { return };

    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::debug!(error = %e, "could not create the cache directory");
            return;
        }
    }
    match std::fs::write(&path, &data) {
        Ok(()) => tracing::debug!(bytes = data.len(), ?path, "pipeline cache saved"),
        Err(e) => tracing::debug!(error = %e, "could not save the pipeline cache"),
    }
}
