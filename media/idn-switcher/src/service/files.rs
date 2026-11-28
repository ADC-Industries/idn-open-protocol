//! Files: where recordings go, pictures and lists read from files, slideshows' pictures,
//! and waiting for a change of the state.

use super::*;

/// Where a recording named without a folder goes: Videos\IDN Recordings in the user's
/// home. Started from the Start menu, the switcher runs in its install folder, which it
/// can't write (and the file would be hard to find there).
/// A name already taken gets " (2)", " (3)", … before its extension: the muxer opened
/// it for writing and an earlier recording of the same name was lost (deep bug run
/// 2026-11-09).
#[cfg(feature = "ffmpeg")]
pub(super) fn recording_path(f: &str) -> std::path::PathBuf {
    let p = std::path::PathBuf::from(f);
    let p = if p.is_absolute() {
        p
    } else {
        match std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
            Some(home) => {
                let dir = std::path::PathBuf::from(home)
                    .join("Videos")
                    .join("IDN Recordings");
                let _ = std::fs::create_dir_all(&dir);
                dir.join(p)
            }
            None => p,
        }
    };
    free_name(p)
}

#[cfg(feature = "ffmpeg")]
pub(super) fn free_name(p: std::path::PathBuf) -> std::path::PathBuf {
    if std::fs::symlink_metadata(&p).is_err() {
        return p;
    }
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2..10_000)
        .map(|n| p.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|c| std::fs::symlink_metadata(c).is_err())
        .unwrap_or(p)
}

/// A recording's folder (its disk is watched), or `None` for a stream.
#[cfg(feature = "ffmpeg")]
pub(super) fn recording_dir(target: &str) -> Option<std::path::PathBuf> {
    if target.contains("://") {
        return None;
    }
    let p = std::path::Path::new(target);
    Some(match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    })
}

/// A picture file, by its name: shown as a still, not played.
#[cfg(feature = "ffmpeg")]
/// A picture file read whole (any size, its alpha kept).
#[cfg(feature = "ffmpeg")]
pub(super) fn load_picture(path: &str) -> Result<Frame, String> {
    let p = idn_ffmpeg::player::still(path).map_err(|e| e.to_string())?;
    Ok(Frame {
        width: p.width,
        height: p.height,
        y: p.planes[0].data.clone(),
        cb: p.planes[1].data.clone(),
        cr: p.planes[2].data.clone(),
        // Its alpha, for a layer keyed over the whole picture.
        a: p.planes.get(3).map(|a| a.data.clone()),
        coefs: None,
        gpu: None,
    })
}

/// A list's items from the API's JSON: `items` (paths, or `{"path", "seconds"}`) and
/// `folder` (its videos, sound files and pictures, by name), a picture's `seconds`.
#[cfg(feature = "ffmpeg")]
pub(super) fn list_items(
    v: &Value,
    media: &Media,
) -> Result<Vec<idn_ffmpeg::player::ListItem>, String> {
    let made = list_made(v, media)?;
    made["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| {
            Ok(idn_ffmpeg::player::ListItem {
                // A relative or library name: the file it points to (§S.18).
                path: media.resolve(i["path"].as_str().unwrap_or_default())?,
                seconds: i["seconds"].as_f64().unwrap_or(5.0) as f32,
            })
        })
        .collect()
}

/// A list as kept: its items each `{"path", "seconds"}` (a folder read now; `from`: files
/// separated by `|`, or a folder), `loop`, `shuffle`.
pub(super) fn list_made(v: &Value, media: &Media) -> Result<Value, String> {
    let secs = v["seconds"].as_f64().unwrap_or(5.0).clamp(0.1, 3600.0);
    let mut items: Vec<Value> = v["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| match i {
            Value::String(p) => Some(json!({"path": p, "seconds": secs})),
            o if o["path"].is_string() => Some(json!({
                "path": o["path"],
                "seconds": o["seconds"].as_f64().unwrap_or(secs).clamp(0.1, 3600.0),
            })),
            _ => None,
        })
        .collect();
    // `from`: files separated by `|`, or a folder (one name that is a folder, or has no
    // extension: "Easter.2026" is a folder when there is one).
    let (files, from_dir) = v["from"].as_str().map_or((Vec::new(), None), |f| {
        rules::split_from(f, |p| !media.is_dir(p) && rules::has_extension(p))
    });
    items.extend(
        files
            .into_iter()
            .map(|p| json!({"path": p, "seconds": secs})),
    );
    for folder in v["folder"].as_str().into_iter().chain(from_dir.as_deref()) {
        if folder.starts_with("library:") {
            return Err(
                "a list from the library: give its items (library:FOLDER/FILE each)".into(),
            );
        }
        // A folder named inside the media folder keeps its items' names so too.
        let dir = &media.resolve(folder)?;
        let relative = !std::path::Path::new(folder).is_absolute();
        let is_media = |p: &str| {
            let p = p.to_ascii_lowercase();
            is_picture(&p)
                || [
                    ".mp4", ".mov", ".mkv", ".m4v", ".webm", ".avi", ".mpg", ".mpeg", ".ts",
                    ".mxf", ".mp3", ".wav", ".m4a", ".aac", ".flac", ".ogg",
                ]
                .iter()
                .any(|e| p.ends_with(e))
        };
        let mut found: Vec<String> = std::fs::read_dir(dir)
            .map_err(|e| format!("folder {folder}: {e}"))?
            .filter_map(|e| e.ok())
            .map(|e| {
                if relative {
                    format!(
                        "{}/{}",
                        folder.trim_end_matches(['/', '\\']),
                        e.file_name().to_string_lossy()
                    )
                } else {
                    e.path().to_string_lossy().into_owned()
                }
            })
            .filter(|p| is_media(p))
            .collect();
        found.sort_by_key(|p| p.to_lowercase());
        items.extend(
            found
                .into_iter()
                .map(|p| json!({"path": p, "seconds": secs})),
        );
    }
    if items.len() > 1000 {
        return Err("a list: at most 1000 items".into());
    }
    Ok(json!({
        "items": items,
        "loop": v["loop"].as_bool().unwrap_or(true),
        "shuffle": v["shuffle"].as_bool().unwrap_or(false),
    }))
}

/// A slideshow from the API's JSON: `pictures` (files) or `folder` (its pictures, by name)
/// or `from` (files separated by `|`, or a folder), `every` and `fade` seconds, `loop`,
/// `shuffle`. Names are read as a file input's ([`Media::resolve`]: a name without a
/// folder of its own in the media folder, no ".."), and kept so (a folder's pictures by
/// `FOLDER/NAME` when the folder was named inside the media folder), as lists keep theirs.
pub(super) fn slideshow_from_json(v: &Value, media: &Media) -> Result<Slideshow, String> {
    let mut pictures: Vec<String> = v["pictures"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str().map(str::to_owned))
        .collect();
    // `from`: files separated by `|`, or a folder (one name that is a folder, or isn't a
    // picture's).
    let (files, from_dir) = v["from"].as_str().map_or((Vec::new(), None), |f| {
        rules::split_from(f, |p| !media.is_dir(p) && is_picture(p))
    });
    pictures.extend(files);
    // (No "..", a library item there is: checked now, read when shown.)
    for p in &pictures {
        media.resolve(p)?;
    }
    for folder in v["folder"].as_str().into_iter().chain(from_dir.as_deref()) {
        if folder.starts_with("library:") {
            return Err(
                "a slideshow from the library: give its pictures (library:FOLDER/FILE each)".into(),
            );
        }
        let dir = &media.resolve(folder)?;
        let relative = !std::path::Path::new(folder).is_absolute();
        let mut found: Vec<String> = std::fs::read_dir(dir)
            .map_err(|e| format!("folder {folder}: {e}"))?
            .filter_map(|e| e.ok())
            .map(|e| {
                if relative {
                    format!(
                        "{}/{}",
                        folder.trim_end_matches(['/', '\\']),
                        e.file_name().to_string_lossy()
                    )
                } else {
                    e.path().to_string_lossy().into_owned()
                }
            })
            .filter(|p| is_picture(p))
            .collect();
        found.sort_by_key(|p| p.to_lowercase());
        pictures.extend(found);
    }
    if pictures.is_empty() {
        return Err("slideshow: no pictures (give pictures or a folder of them)".into());
    }
    Ok(Slideshow {
        pictures,
        every: v["every"].as_f64().unwrap_or(5.0) as f32,
        fade: v["fade"].as_f64().unwrap_or(1.0) as f32,
        looped: v["loop"].as_bool().unwrap_or(true),
        shuffle: v["shuffle"].as_bool().unwrap_or(false),
    })
}

pub(super) fn is_picture(path: &str) -> bool {
    [
        ".png", ".jpg", ".jpeg", ".bmp", ".gif", ".tif", ".tiff", ".webp",
    ]
    .iter()
    .any(|e| path.to_ascii_lowercase().ends_with(e))
}

/// Waits up to `max` for the switcher's state to really change past `version` (its
/// [`state_version`](crate::Switcher::state_version): the timers' seconds don't count, so
/// a watcher of real changes doesn't build the state again each second while a timer
/// runs), without holding the service between short waits (a host's drop of it isn't held
/// up, nor run on a background thread): the state version now, or `None` once the service
/// is gone.
pub(super) fn wait_change_weak(
    service: &std::sync::Weak<Service>,
    version: u64,
    max: Duration,
) -> Option<u64> {
    let end = Instant::now() + max;
    loop {
        let sw = service.upgrade()?.sw.clone();
        let left = end.saturating_duration_since(Instant::now());
        let v = sw.wait_state_change(version, left.min(Duration::from_millis(250)));
        if v != version || left.is_zero() {
            return Some(v);
        }
    }
}
