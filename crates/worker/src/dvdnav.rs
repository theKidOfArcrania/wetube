//! Opaque libdvdnav public ABI; disc parsing runs only in a resource-limited child.
use anyhow::{Context, Result, ensure};
use libloading::Library;
use std::{
    ffi::{c_char, c_void},
    fs::File,
    ptr,
};
use wetube_core::{DvdScanReport, DvdTitle};

type Handle = *mut c_void;
type Open = unsafe extern "C" fn(*mut Handle, *const c_char) -> i32;
type Close = unsafe extern "C" fn(Handle) -> i32;
type Titles = unsafe extern "C" fn(Handle, *mut i32) -> i32;
type Parts = unsafe extern "C" fn(Handle, i32, *mut i32) -> i32;
type Play = unsafe extern "C" fn(Handle, i32) -> i32;
type Chapters = unsafe extern "C" fn(Handle, i32, *mut *mut u64, *mut u64) -> u32;
type Angles = unsafe extern "C" fn(Handle, *mut i32, *mut i32) -> i32;
type Language = unsafe extern "C" fn(Handle, u8) -> u16;
type Logical = unsafe extern "C" fn(Handle, u8) -> i8;

struct Api {
    open: Open,
    close: Close,
    titles: Titles,
    parts: Parts,
    play: Play,
    chapters: Chapters,
    angles: Angles,
    audio_language: Language,
    subtitle_language: Language,
    audio_logical: Logical,
    subtitle_logical: Logical,
    _library: Library,
}

impl Api {
    fn load() -> Result<Self> {
        // Function signatures are from libdvdnav's public dvdnav.h, not its
        // private struct layouts. Keeping the library owns every symbol lifetime.
        unsafe {
            let library =
                Library::new("libdvdnav.so.4").context("load installed libdvdnav ABI 4")?;
            Ok(Self {
                open: *library.get(b"dvdnav_open\0")?,
                close: *library.get(b"dvdnav_close\0")?,
                titles: *library.get(b"dvdnav_get_number_of_titles\0")?,
                parts: *library.get(b"dvdnav_get_number_of_parts\0")?,
                play: *library.get(b"dvdnav_title_play\0")?,
                chapters: *library.get(b"dvdnav_describe_title_chapters\0")?,
                angles: *library.get(b"dvdnav_get_angle_info\0")?,
                audio_language: *library.get(b"dvdnav_audio_stream_to_lang\0")?,
                subtitle_language: *library.get(b"dvdnav_spu_stream_to_lang\0")?,
                audio_logical: *library.get(b"dvdnav_get_audio_logical_stream\0")?,
                subtitle_logical: *library.get(b"dvdnav_get_spu_logical_stream\0")?,
                _library: library,
            })
        }
    }
}

struct Disc<'a> {
    handle: Handle,
    api: &'a Api,
}
impl Drop for Disc<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.api.close)(self.handle);
        }
    }
}

pub fn available() -> Result<()> {
    Api::load().map(|_| ())
}

fn language(code: u16) -> Option<String> {
    let bytes = code.to_be_bytes();
    bytes
        .iter()
        .all(u8::is_ascii_alphabetic)
        .then(|| String::from_utf8_lossy(&bytes).into_owned())
}

fn streams(disc: &Disc<'_>, logical: Logical, lang: Language, maximum: u8) -> (u16, Vec<String>) {
    let mut seen = std::collections::BTreeSet::new();
    let mut languages = Vec::new();
    for physical in 0..maximum {
        let index = unsafe { (logical)(disc.handle, physical) };
        if index >= 0
            && seen.insert(index)
            && let Some(language) = language(unsafe { (lang)(disc.handle, index as u8) })
        {
            languages.push(language);
        }
    }
    (seen.len() as u16, languages)
}

/// Caller must be the single-purpose worker child, not a thread in the online worker.
pub fn scan_stdin() -> Result<DvdScanReport> {
    for (resource, limit) in [
        (libc::RLIMIT_AS, 256 * 1024 * 1024),
        (libc::RLIMIT_CPU, 120),
    ] {
        let limit = libc::rlimit {
            rlim_cur: limit,
            rlim_max: limit,
        };
        ensure!(
            unsafe { libc::setrlimit(resource, &limit) } == 0,
            "apply DVD discovery resource limit"
        );
    }
    let file = File::open("/proc/self/fd/0")?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() > 0,
        "scanner stdin must be a nonempty regular ISO"
    );
    let before = wetube_backend::incoming::identity(&file.metadata()?);
    let api = Api::load()?;
    let mut handle = ptr::null_mut();
    ensure!(
        unsafe { (api.open)(&mut handle, c"/proc/self/fd/0".as_ptr()) } == 1 && !handle.is_null(),
        "libdvdnav could not open the staged DVD"
    );
    let disc = Disc { handle, api: &api };
    let mut count = 0;
    ensure!(
        unsafe { (api.titles)(handle, &mut count) } == 1 && (1..=99).contains(&count),
        "invalid DVD title inventory"
    );
    let mut titles = Vec::new();
    for title in 1..=count {
        let mut parts = 0;
        ensure!(
            unsafe { (api.parts)(handle, title, &mut parts) } == 1 && (1..=999).contains(&parts),
            "invalid DVD chapter count"
        );
        let mut times = ptr::null_mut();
        let mut ticks = 0;
        let chapters = unsafe { (api.chapters)(handle, title, &mut times, &mut ticks) };
        // Public API returns a malloc-owned array. We never dereference it;
        // chapter count and total duration suffice for selection.
        let has_times = !times.is_null();
        unsafe {
            libc::free(times.cast());
        }
        ensure!(
            has_times && chapters > 0 && chapters <= 999 && ticks > 0 && ticks <= 86400 * 90000,
            "invalid DVD chapter duration inventory"
        );
        ensure!(
            unsafe { (api.play)(handle, title) } == 1,
            "DVD title cannot be selected"
        );
        let mut current = 0;
        let mut angles = 0;
        ensure!(
            unsafe { (api.angles)(handle, &mut current, &mut angles) } == 1
                && (1..=9).contains(&angles)
                && (1..=angles).contains(&current),
            "invalid DVD angle inventory"
        );
        let (audio_count, audio_languages) =
            streams(&disc, api.audio_logical, api.audio_language, 8);
        let (subtitle_count, subtitle_languages) =
            streams(&disc, api.subtitle_logical, api.subtitle_language, 32);
        titles.push(DvdTitle {
            title: title as u8,
            duration_seconds: ticks as f64 / 90000.0,
            chapter_count: chapters as u16,
            audio_count,
            subtitle_count,
            audio_languages,
            subtitle_languages,
            angles: (1..=angles as u8).collect(),
            main_feature_candidate: false,
        });
    }
    ensure!(
        wetube_backend::incoming::identity(&file.metadata()?) == before,
        "staged input changed during DVD discovery"
    );
    let longest = titles
        .iter()
        .map(|title| title.duration_seconds)
        .fold(0.0, f64::max);
    for title in &mut titles {
        title.main_feature_candidate = title.duration_seconds == longest;
    }
    Ok(DvdScanReport { titles })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_language_is_not_invented() {
        assert_eq!(language(u16::from_be_bytes(*b"en")), Some("en".into()));
        assert_eq!(language(0xffff), None);
        assert_eq!(language(0), None);
    }
}
