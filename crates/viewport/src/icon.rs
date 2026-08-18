// SPDX-License-Identifier: GPL-3.0-or-later
//
// Turning what a program says its icon is into something a web page can draw.
//
// Two things arrive from the bus and neither is a picture. An icon *name* is a
// key into the installed icon themes — "discord", "nm-signal-75" — and finding
// what it stands for means walking the theme directories the way every toolkit
// does. An icon *pixmap* is raw ARGB, in network byte order, at whatever sizes
// the application felt like sending.
//
// What comes out is a `data:` URL, because that is the one form the shell can
// always show. A path would do for a shell loaded from `file://` and fail for
// one loaded over `http://localhost:3000`, which is a supported way to run
// this and the way the shell is developed; an icon name means nothing to a
// browser at all.
//
// The PNG written here is deliberately not compressed. A tray icon is a few
// kilobytes, it is encoded once and then cached, and a deflate implementation
// — or a dependency carrying one — is a great deal of machinery to save a
// couple of kilobytes on a message sent when an application starts.

use std::path::{Path, PathBuf};

/// The largest file that will be turned into a data URL.
///
/// An icon theme holds sensible PNGs and the occasional enormous SVG, and a
/// megabyte of base64 in a message the shell parses on the main thread is a
/// frame dropped for a picture 22 pixels wide.
const MAX_FILE: u64 = 512 * 1024;

/// How deep the theme walk goes below a theme's own directory.
///
/// `hicolor/48x48/apps/firefox.png` is three, and every layout in use is that
/// or shallower. A bound matters because this walks directories a package
/// manager fills.
const MAX_DEPTH: usize = 3;

/// An icon file as a `data:` URL, or nothing where it cannot be read or is not
/// a format a browser shows.
pub fn data_url(path: &Path) -> Option<String> {
    let mime = match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "svg" | "svgz" => "image/svg+xml",
        // XPM is still in /usr/share/pixmaps and no browser has ever drawn
        // one. Nothing is better than a broken image element.
        _ => return None,
    };
    let size = std::fs::metadata(path).ok()?.len();
    if size > MAX_FILE {
        tracing::debug!("{}: {size} bytes is too large for an icon", path.display());
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    Some(format!("data:{mime};base64,{}", base64(&bytes)))
}

/// Raw image bytes as a `data:` URL.
///
/// For the icon a menu row carries: `com.canonical.dbusmenu` says `icon-data`
/// is a PNG, so unlike a tray item's pixmap there is nothing to encode — the
/// bytes are already a file, and all that is missing is the wrapper a browser
/// wants.
pub fn png_data_url(bytes: &[u8]) -> Option<String> {
    // Not a length check dressed up as a format check: an empty property is
    // how an application says it has no icon, and PNG's signature is what
    // tells that from a property holding something else entirely.
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") || bytes.len() as u64 > MAX_FILE {
        return None;
    }
    Some(format!("data:image/png;base64,{}", base64(bytes)))
}

/// Where an icon name resolves to, searching the installed themes.
///
/// `theme_path` is the item's own `IconThemePath` — the property an
/// application that ships its own icons sets, and the reason a tray icon can
/// exist for a program that installed nothing into the system themes. It is
/// searched first, since an application that names a directory means the icon
/// in it.
///
/// The named theme is searched before `hicolor`, and `hicolor` is always
/// searched: it is where a package installs an icon that belongs to no theme,
/// and skipping it is how an icon that plainly exists is reported missing.
pub fn lookup(name: &str, theme_path: Option<&str>, theme: &str, size: u32) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }

    // An absolute path is not a name. Applications do send one — it is not in
    // the specification and it is what several toolkits do anyway — and
    // treating it as a theme key means searching for a file called
    // "/opt/foo/icon.png" in every icon directory on the machine.
    let direct = Path::new(name);
    if direct.is_absolute() && direct.is_file() {
        return Some(direct.to_path_buf());
    }

    let mut best: Option<(u32, PathBuf)> = None;
    let mut consider = |found: PathBuf, score: u32| {
        if best.as_ref().is_none_or(|(current, _)| score < *current) {
            best = Some((score, found));
        }
    };

    if let Some(dir) = theme_path {
        walk(Path::new(dir), name, size, 0, &mut consider);
    }
    for base in bases() {
        for theme in [theme, "hicolor"] {
            walk(&base.join(theme), name, size, 0, &mut consider);
        }
        // /usr/share/pixmaps and the like: flat, themeless, and where a great
        // many older applications still put their only icon.
        walk(&base, name, size, MAX_DEPTH, &mut consider);
    }
    best.map(|(_, path)| path)
}

/// The directories icon themes are installed into, most specific first.
fn bases() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(&home).join(".local/share/icons"));
        // Deprecated for twenty years and still full on real machines.
        dirs.push(PathBuf::from(&home).join(".icons"));
    }
    let data =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".to_owned());
    for dir in data.split(':').filter(|d| !d.is_empty()) {
        dirs.push(PathBuf::from(dir).join("icons"));
        dirs.push(PathBuf::from(dir).join("pixmaps"));
    }
    dirs
}

/// Walk one theme directory, offering every `name.png` and `name.svg` under it.
///
/// The score handed to `consider` is how far the icon is from the size asked
/// for, so a 24-pixel bar picks the 22 or 24 pixel icon rather than the 512
/// one a search that stopped at the first hit would find. Scalable wins
/// outright — it is the right icon at every size — which is why it scores
/// zero.
fn walk(dir: &Path, name: &str, want: u32, depth: usize, consider: &mut impl FnMut(PathBuf, u32)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if depth < MAX_DEPTH {
                walk(&path, name, want, depth + 1, consider);
            }
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem != name {
            continue;
        }
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        match extension.as_deref() {
            Some("svg") => consider(path, 0),
            Some("png") => {
                let score = size_in(&path).map_or(1024, |size| size.abs_diff(want).max(1));
                consider(path, score);
            }
            _ => {}
        }
    }
}

/// The pixel size a theme path announces, from the `48x48` in it.
fn size_in(path: &Path) -> Option<u32> {
    path.components().rev().find_map(|component| {
        let text = component.as_os_str().to_str()?;
        let (width, height) = text.split_once('x')?;
        let width: u32 = width.parse().ok()?;
        (width == height.parse::<u32>().ok()?).then_some(width)
    })
}

/// One ARGB32 image as the tray hands it over: width, height, and the pixels
/// in network byte order.
pub struct Pixmap {
    pub width: i32,
    pub height: i32,
    pub argb: Vec<u8>,
}

/// The pixmap nearest the size wanted, encoded as a PNG data URL.
///
/// Applications send several sizes and the specification does not order them,
/// so the choice is made here rather than by taking the first — GNOME's own
/// items send 16, 22, 24 and 32 pixel copies in whatever order the toolkit
/// built them.
pub fn pixmap_url(pixmaps: &[Pixmap], want: u32) -> Option<String> {
    let best = pixmaps
        .iter()
        .filter(|p| p.width > 0 && p.height > 0)
        .filter(|p| p.argb.len() >= (p.width as usize) * (p.height as usize) * 4)
        .min_by_key(|p| (p.width as u32).abs_diff(want))?;
    let rgba = argb_to_rgba(&best.argb, best.width as usize * best.height as usize);
    let png = png(best.width as u32, best.height as u32, &rgba);
    Some(format!("data:image/png;base64,{}", base64(&png)))
}

/// ARGB in network byte order — which is what the specification says and what
/// every implementation sends — rearranged into the RGBA a PNG stores.
fn argb_to_rgba(argb: &[u8], pixels: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels * 4);
    for pixel in argb.chunks_exact(4).take(pixels) {
        out.extend_from_slice(&[pixel[1], pixel[2], pixel[3], pixel[0]]);
    }
    out
}

/// A PNG, stored rather than compressed.
///
/// The zlib stream is deflate's "stored" block type: no compression, a length
/// and its complement, and the data. Every decoder handles it because it is
/// the format's own escape hatch for incompressible input, and it means this
/// file contains no compressor.
fn png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    // Eight bits per channel, truecolour with alpha, deflate, no filter, no
    // interlace.
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);

    // Every row is prefixed with its filter type, which is zero: none.
    let stride = width as usize * 4;
    let mut raw = Vec::with_capacity(rgba.len() + height as usize);
    for row in rgba.chunks(stride) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    chunk(&mut png, b"IEND", &[]);
    png
}

/// A zlib stream whose deflate blocks are all stored.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    // Deflate, 32K window, no preset dictionary, and a header the check byte
    // makes a multiple of 31.
    let mut out = vec![0x78, 0x01];
    let mut chunks = data.chunks(0xffff).peekable();
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    }
    while let Some(block) = chunks.next() {
        let last = u8::from(chunks.peek().is_none());
        let len = block.len() as u16;
        out.push(last);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    let mut crc = crc32(kind);
    crc = crc32_continue(crc, body);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    crc32_continue(0, data)
}

fn crc32_continue(crc: u32, data: &[u8]) -> u32 {
    let mut value = !crc;
    for byte in data {
        value ^= u32::from(*byte);
        for _ in 0..8 {
            value = if value & 1 != 0 {
                (value >> 1) ^ 0xedb8_8320
            } else {
                value >> 1
            };
        }
    }
    !value
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for byte in data {
        a = (a + u32::from(*byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// Standard base64, which is what a `data:` URL carries.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for group in data.chunks(3) {
        let mut bits = 0u32;
        for (i, byte) in group.iter().enumerate() {
            bits |= u32::from(*byte) << (16 - 8 * i);
        }
        for i in 0..4 {
            if i <= group.len() {
                out.push(ALPHABET[((bits >> (18 - 6 * i)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_examples_from_the_rfc() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    /// The two checksums a PNG carries, against values published with their
    /// definitions. Both are written out here rather than pulled in, and a
    /// wrong one produces a file every decoder rejects.
    #[test]
    fn the_checksums_are_the_ones_the_formats_define() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    /// A one-pixel image, checked field by field: the signature, the header
    /// chunk's dimensions and colour type, and the trailer. What this is
    /// really asserting is that the file is a PNG at all — the encoder is
    /// hand-written, and an image no decoder accepts would look exactly like
    /// an application with no icon.
    #[test]
    fn a_png_is_a_png() {
        let file = png(1, 1, &[1, 2, 3, 4]);
        assert_eq!(&file[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&file[12..16], b"IHDR");
        assert_eq!(&file[16..24], &[0, 0, 0, 1, 0, 0, 0, 1]);
        assert_eq!(&file[24..29], &[8, 6, 0, 0, 0], "8-bit RGBA, uncompressed");
        assert_eq!(&file[file.len() - 8..file.len() - 4], b"IEND");
    }

    /// The stored deflate stream a decoder has to be able to read: the zlib
    /// header, one final block, its length and the complement of it.
    #[test]
    fn the_zlib_stream_is_one_stored_block() {
        let stream = zlib_stored(b"hi");
        assert_eq!(&stream[..2], &[0x78, 0x01]);
        assert_eq!(stream[2], 1, "the last block");
        assert_eq!(
            &stream[3..7],
            &[2, 0, 0xfd, 0xff],
            "length, then its inverse"
        );
        assert_eq!(&stream[7..9], b"hi");
        assert_eq!(&stream[9..], &adler32(b"hi").to_be_bytes());
    }

    /// ARGB in network byte order is what the bus carries; RGBA is what a PNG
    /// stores. Getting this backwards is a tray full of icons in the wrong
    /// colour, which reads as a theme problem rather than a bug here.
    #[test]
    fn pixels_are_reordered_rather_than_reinterpreted() {
        assert_eq!(argb_to_rgba(&[0xff, 1, 2, 3], 1), vec![1, 2, 3, 0xff]);
    }

    /// The pixmap nearest the size asked for, not the first one sent.
    #[test]
    fn the_closest_pixmap_wins() {
        let pixmaps = vec![
            Pixmap {
                width: 512,
                height: 512,
                argb: vec![0; 512 * 512 * 4],
            },
            Pixmap {
                width: 24,
                height: 24,
                argb: vec![0; 24 * 24 * 4],
            },
        ];
        let url = pixmap_url(&pixmaps, 22).expect("a pixmap");
        // The 24-pixel one: its header says 24, and the 512 one would be
        // three orders of magnitude longer.
        assert!(url.starts_with("data:image/png;base64,"));
        assert!(url.len() < 8000, "the 512-pixel pixmap was encoded instead");
    }

    /// A pixmap whose declared size does not match the bytes behind it is
    /// dropped rather than read past.
    #[test]
    fn a_short_pixmap_is_refused() {
        let pixmaps = vec![Pixmap {
            width: 16,
            height: 16,
            argb: vec![0; 4],
        }];
        assert!(pixmap_url(&pixmaps, 22).is_none());
    }

    /// The size in a theme path, which is how the closest icon is chosen.
    #[test]
    fn a_theme_path_says_what_size_it_holds() {
        assert_eq!(
            size_in(Path::new("/usr/share/icons/hicolor/48x48/apps/a.png")),
            Some(48)
        );
        assert_eq!(
            size_in(Path::new("/usr/share/icons/hicolor/scalable/apps/a.svg")),
            None
        );
    }

    /// A menu row's icon is a PNG already; anything else is refused rather
    /// than wrapped in a URL that says it is one.
    #[test]
    fn png_data_is_recognised_by_its_signature() {
        let png = png(1, 1, &[0, 0, 0, 0]);
        assert!(png_data_url(&png)
            .expect("a data URL")
            .starts_with("data:image/png;base64,iVBOR"));
        assert_eq!(png_data_url(b""), None);
        assert_eq!(png_data_url(b"GIF89a"), None);
    }

    /// An absolute path is the icon, not a name to search for.
    #[test]
    fn an_absolute_path_is_taken_as_one() {
        let file = std::env::current_exe().expect("this test binary");
        let found = lookup(file.to_str().unwrap(), None, "hicolor", 22);
        assert_eq!(found.as_deref(), Some(file.as_path()));
    }
}
