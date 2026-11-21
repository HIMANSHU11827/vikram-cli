//! Image blocks in the transcript.
//!
//! A terminal has no general way to paint arbitrary image bytes — kitty, iTerm2 and sixel each
//! speak their own protocol and none is negotiated here — so the transcript is honest about what
//! it holds: inline data is cached under `$VIKRAM_HOME/cache/images/` and the line names the
//! file, and a URL is named as a link. Nothing claims pixels that were not drawn.

use std::path::PathBuf;
#[cfg(test)]
use std::sync::Mutex;

use ratatui::text::Line;
use vikram_contracts::ImageSource;

use crate::theme::Theme;
use crate::widgets::indent;

/// Decoded bytes larger than this are reported but not written to the cache.
const MAX_CACHED_IMAGE_BYTES: usize = 32 * 1024 * 1024;

/// One transcript row for an image block: `[image: …]` in the muted style every other
/// non-speaking row uses.
pub fn line(source: &ImageSource, theme: &Theme) -> Line<'static> {
    Line::styled(
        format!(
            "{}[image: {}]",
            indent::row_prefix(indent::TURN),
            label(source)
        ),
        theme.muted(),
    )
}

/// The text inside the brackets. For `Data` it is `media_type · size → path` when the bytes were
/// cached, and says plainly when they were not; for `Url` it is the file name plus `linked image`,
/// because the bytes were never fetched.
pub fn label(source: &ImageSource) -> String {
    match source {
        ImageSource::Url { url } => match url_file_name(url) {
            Some(name) => format!("{name} · linked image"),
            None => "linked image".to_owned(),
        },
        ImageSource::Data { media_type, data } => {
            let Some(bytes) = base64_decode(data) else {
                return format!("{media_type} · unreadable data");
            };
            let size = bytes.len();
            if size > MAX_CACHED_IMAGE_BYTES {
                return format!("{media_type} · {} · too large to cache", size_bytes(size));
            }
            match cache_artifact(media_type, data, &bytes) {
                Some(path) => format!("{media_type} · {} → {}", size_bytes(size), path.display()),
                None => format!("{media_type} · {} · not cached", size_bytes(size)),
            }
        }
    }
}

fn url_file_name(url: &str) -> Option<&str> {
    let path = url.split(['?', '#']).next().unwrap_or("");
    path.rsplit('/').next().filter(|part| !part.is_empty())
}

fn size_bytes(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let kib = bytes as f64 / KIB;
    if kib < 1024.0 {
        return format!("{kib:.1}KiB");
    }
    format!("{:.1}MiB", kib / 1024.0)
}

/// Write the decoded bytes to `$VIKRAM_HOME/cache/images/<hash>.<ext>` and return the path.
/// The name is content-addressed, so repeat renders of the same block are a no-op.
fn cache_artifact(media_type: &str, data: &str, bytes: &[u8]) -> Option<PathBuf> {
    let dir = vikram_runtime::config::vikram_home()
        .join("cache")
        .join("images");
    let mut hash = fnv1a(media_type.as_bytes());
    hash = fnv1a_continue(hash, data.as_bytes());
    let path = dir.join(format!("{hash:016x}.{}", extension(media_type)));
    if !path.exists() {
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::write(&path, bytes).ok()?;
    }
    Some(path)
}

fn extension(media_type: &str) -> String {
    let suffix = media_type
        .rsplit('/')
        .next()
        .unwrap_or("")
        .split(['+', ';'])
        .next()
        .unwrap_or("");
    let clean: String = suffix
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    if clean.is_empty() {
        "img".to_owned()
    } else {
        clean
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    fnv1a_continue(0xcbf29ce484222325, bytes)
}

fn fnv1a_continue(mut hash: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: [i8; 256] = build_table();
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    let mut padding = 0usize;
    for byte in input.bytes() {
        if byte.is_ascii_whitespace() {
            continue;
        }
        let value = TABLE[byte as usize];
        if value < 0 {
            if byte == b'=' {
                padding += 1;
                continue;
            }
            return None;
        }
        if padding > 0 {
            return None;
        }
        acc = (acc << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    if padding > 2 || bits >= 6 {
        return None;
    }
    Some(out)
}

/// Serializes tests that point `VIKRAM_HOME` at a tempdir; the env var is process-global.
#[cfg(test)]
pub(crate) static HOME_LOCK: Mutex<()> = Mutex::new(());

const fn build_table() -> [i8; 256] {
    let mut table = [-1i8; 256];
    let mut i = 0usize;
    while i < 26 {
        table[b'A' as usize + i] = i as i8;
        table[b'a' as usize + i] = (i + 26) as i8;
        i += 1;
    }
    let mut i = 0usize;
    while i < 10 {
        table[b'0' as usize + i] = (i + 52) as i8;
        i += 1;
    }
    table[b'+' as usize] = 62;
    table[b'/' as usize] = 63;
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    fn home_guard(dir: &tempfile::TempDir) -> MutexGuard<'static, ()> {
        let guard = HOME_LOCK.lock().unwrap();
        unsafe { std::env::set_var("VIKRAM_HOME", dir.path()) };
        guard
    }

    fn png_source() -> ImageSource {
        // "PNG" magic plus a few bytes; the cache writes whatever it is given.
        ImageSource::data("image/png", "UE5HAAAA")
    }

    #[test]
    fn a_data_image_is_cached_and_the_label_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = home_guard(&dir);
        let source = png_source();
        let text = label(&source);
        assert!(text.contains("image/png"), "got {text}");
        assert!(text.contains("→"), "got {text}");
        let path = text.rsplit('→').next().unwrap().trim().to_owned();
        let written = std::fs::read(&path).unwrap();
        assert_eq!(written, base64_decode("UE5HAAAA").unwrap());
        assert!(std::path::Path::new(&path).starts_with(dir.path().join("cache").join("images")));
        assert!(path.ends_with(".png"));
        // A second render reuses the same file rather than writing again.
        assert_eq!(label(&source), text);
    }

    #[test]
    fn a_url_image_is_named_as_a_link_and_nothing_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = home_guard(&dir);
        let source = ImageSource::url("https://example.test/shots/screen shot.png?w=1");
        let text = label(&source);
        assert!(text.contains("linked image"), "got {text}");
        assert!(text.contains("screen shot.png"), "got {text}");
        assert!(!dir.path().join("cache").exists());
    }

    #[test]
    fn unreadable_and_oversized_data_are_reported_honestly() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = home_guard(&dir);
        let bad = ImageSource::data("image/png", "!!!not-base64!!!");
        assert_eq!(label(&bad), "image/png · unreadable data");
        let big = ImageSource::data("image/png", "A".repeat(MAX_CACHED_IMAGE_BYTES * 2));
        let text = label(&big);
        assert!(text.contains("too large to cache"), "got {text}");
        assert!(!dir.path().join("cache").exists());
    }

    #[test]
    fn several_images_each_get_their_own_row_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = home_guard(&dir);
        let theme = Theme::new(false);
        let mut lines = Vec::new();
        for data in ["UE5HAAAA", "SlBFRwAA", "R0lGAAAA"] {
            lines.push(line(&ImageSource::data("image/png", data), &theme));
        }
        assert_eq!(lines.len(), 3);
        let files: Vec<_> = std::fs::read_dir(dir.path().join("cache").join("images"))
            .unwrap()
            .collect();
        assert_eq!(files.len(), 3);
    }
}
