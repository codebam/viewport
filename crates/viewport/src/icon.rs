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

use std::io::Read;
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

/// Open `path` for reading without blocking on a FIFO that has no writer.
///
/// Symlinks are followed on purpose: NixOS assembles an icon theme as a union
/// of store paths, so the size directories, the category directories and every
/// icon under them are links. What the hardening actually needs is not "no
/// links" but "no unexpected inode": the decision is made from the descriptor
/// after it is open, [`data_url`] refuses anything that is not a regular file,
/// reads no further than the size cap, and checks that the bytes are the image
/// the extension claims. A theme that links `icon.png` at an ssh key is
/// refused by the content check; a link at a FIFO cannot block because
/// `O_NONBLOCK` keeps `open` from waiting for a writer, and is refused because
/// the descriptor is not a regular file.
#[cfg(unix)]
fn open_icon(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_icon(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// Whether the bytes really are the format the extension names.
///
/// The extension is not evidence. `lookup` follows symlinks again because a
/// package-manager theme is built from them, and it can also hand back an
/// absolute path an application chose, so a file named `icon.png` may be
/// anything at all. Without this, a theme link from `icon.png` to an ssh key
/// would be read whole, base64'd, and delivered to the shell — which is the
/// leak the symlink refusal was added for, achieved without a symlink at all
/// if an application names the key directly. Checked on the bytes after the
/// bounded read, so it costs a handful of comparisons per icon.
fn content_matches(extension: &str, bytes: &[u8]) -> bool {
    match extension {
        "png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "jpg" | "jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "webp" => bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP",
        "bmp" => bytes.starts_with(b"BM"),
        "avif" => avif_magic(bytes),
        "svg" => svg_magic(bytes),
        // gzip-compressed SVG; no browser draws it, but the old behaviour is
        // a data URL rather than a refusal, so it stays and is checked.
        "svgz" => bytes.starts_with(&[0x1f, 0x8b]),
        _ => false,
    }
}

/// An ISO base media file whose brands name AVIF, not merely `mif1`.
fn avif_magic(bytes: &[u8]) -> bool {
    if bytes.len() < 16 || &bytes[4..8] != b"ftyp" {
        return false;
    }
    let brand = |b: &[u8]| b == b"avif" || b == b"avis";
    // The major brand, or any compatible brand: a file written as `mif1`
    // with `avif` in its compatible list is a valid AVIF.
    brand(&bytes[8..12]) || bytes[16..].chunks_exact(4).any(brand)
}

/// Whether text looks like an SVG document rather than a renamed secret.
///
/// SVG is text, so there is no fixed signature: a BOM, whitespace, an XML
/// declaration, a doctype or a comment may precede the root element. Only the
/// prologue is scanned, and the root must appear in it — an unrelated XML
/// file with an `.svg` name is not a picture and is not sent.
fn svg_magic(bytes: &[u8]) -> bool {
    let text = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let Some(start) = text.iter().position(|b| !b.is_ascii_whitespace()) else {
        return false;
    };
    let text = &text[start..];
    if text.starts_with(b"<svg") {
        return true;
    }
    if text.starts_with(b"<?xml") || text.starts_with(b"<!DOCTYPE") || text.starts_with(b"<!--") {
        let head = &text[..text.len().min(4096)];
        return head.windows(4).any(|window| window == &b"<svg"[..]);
    }
    false
}

/// An icon file as a `data:` URL, or nothing where it cannot be read or is not
/// a format a browser shows.
pub fn data_url(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let mime = match extension.as_str() {
        "png" => "image/png",
        "svg" | "svgz" => "image/svg+xml",
        // Not for icons — a theme holds none of these — but for cover art,
        // which a music library keeps as JPEG almost without exception.
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        // XPM is still in /usr/share/pixmaps and no browser has ever drawn
        // one. Nothing is better than a broken image element.
        _ => return None,
    };
    let file = open_icon(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        tracing::debug!("{}: icon path is not a regular file", path.display());
        return None;
    }
    let size = meta.len();
    if size > MAX_FILE {
        tracing::debug!("{}: {size} bytes is too large for an icon", path.display());
        return None;
    }
    // One byte past the cap, so a file that grew after the metadata check is
    // detected rather than read to the end.
    let mut bytes = Vec::new();
    (&file).take(MAX_FILE + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_FILE {
        tracing::debug!("{}: icon grew past the {MAX_FILE} byte cap", path.display());
        return None;
    }
    if !content_matches(&extension, &bytes) {
        tracing::debug!("{}: contents are not {mime}", path.display());
        return None;
    }
    Some(format!("data:{mime};base64,{}", base64(&bytes)))
}

/// The largest cover-art file that will be turned into a data URL.
///
/// Eight megabytes, against an icon's [`MAX_FILE`], because the two are not
/// the same job. An icon is re-encoded for every menu row and a theme holds
/// hundreds of them; cover art arrives once per track, players ship it as
/// compressed JPEG or PNG, and artwork ripped at any real resolution reaches
/// several megabytes. The cap exists to bound what one bus message can make
/// this read — the path is chosen by whoever published the player, and not
/// every publisher is the player it says it is — rather than to second-guess
/// what a music library keeps.
pub(crate) const MAX_ART: u64 = 8 << 20;

/// Cover art as a `data:` URL, under the looser limits art needs.
///
/// The formats a music library actually keeps covers in, a cap sized for
/// album artwork rather than tray icons, and — because the path arrives off
/// the bus rather than out of a theme walk — a refusal to open anything that
/// is not a regular file. A device, a FIFO or a directory named `cover.png`
/// would otherwise hang or outgrow the reader: `/dev/zero` reports no size
/// at all, so a length check alone never fires on it, and a FIFO with no
/// writer blocks forever. The metadata comes from the open file rather than
/// the name, so what is checked is the inode that would be read.
///
/// SVG is deliberately absent, unlike in [`data_url`]. That one reads paths
/// found in installed themes; this one reads paths published by whatever
/// felt like speaking on the session bus, and a format that can carry
/// markup has no business arriving that way.
pub fn art_data_url(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let mime = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        _ => return None,
    };
    let file = open_icon(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        tracing::debug!("{}: cover art that is not a file", path.display());
        return None;
    }
    if meta.len() > MAX_ART {
        tracing::debug!(
            "{}: {} bytes is too large for cover art",
            path.display(),
            meta.len()
        );
        return None;
    }
    // Bounded by the take even where the size above was measured against a
    // file that has since grown; one byte past the cap says which happened.
    let mut bytes = Vec::new();
    (&file).take(MAX_ART + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_ART {
        tracing::debug!("{}: cover art grew past the cap", path.display());
        return None;
    }
    if !content_matches(&extension, &bytes) {
        tracing::debug!("{}: contents are not {mime}", path.display());
        return None;
    }
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

/// Every theme path that answers for `name`, best candidate first.
///
/// [`lookup`] is the one-answer form and is what most callers want. This is
/// for a caller that has a budget: the closest icon is not always the one it
/// can use. A scalable SVG scores best at every size and is the right answer
/// for a tray, but one large document is tens of kilobytes of base64 in a
/// single message, and a 48-pixel PNG a couple of candidates down may be the
/// icon that actually fits. The order is the score order, and ties keep the
/// order the themes were searched in.
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
pub fn candidates(name: &str, theme_path: Option<&str>, theme: &str, size: u32) -> Vec<PathBuf> {
    if name.is_empty() {
        return Vec::new();
    }

    // An absolute path is not a name. Applications do send one — it is not in
    // the specification and it is what several toolkits do anyway — and
    // treating it as a theme key means searching for a file called
    // "/opt/foo/icon.png" in every icon directory on the machine.
    let direct = Path::new(name);
    if direct.is_absolute()
        && std::fs::metadata(direct)
            .map(|meta| meta.is_file())
            .unwrap_or(false)
    {
        return vec![direct.to_path_buf()];
    }

    let mut found: Vec<(u32, PathBuf)> = Vec::new();
    let mut consider = |path: PathBuf, score: u32| found.push((score, path));

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
    // Stable, so equal scores stay in the order the themes were searched.
    found.sort_by_key(|(score, _)| *score);
    found.into_iter().map(|(_, path)| path).collect()
}

/// Where an icon name resolves to, searching the installed themes.
pub fn lookup(name: &str, theme_path: Option<&str>, theme: &str, size: u32) -> Option<PathBuf> {
    candidates(name, theme_path, theme, size).into_iter().next()
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

/// Whether a symlinked directory is one the icon-theme layout names.
///
/// A package-manager theme is a union of links, and following all of them
/// would let one link to `/` turn a lookup into a walk of the machine. The
/// layout has a fixed vocabulary — a size directory, `scalable`, `symbolic`,
/// or a category — and a link is descended into only when its name is in it.
/// A real directory is not restricted: it is material inside the theme the
/// caller asked for, not an indirection somewhere else.
fn theme_component(name: &str) -> bool {
    const CATEGORIES: &[&str] = &[
        "actions",
        "animations",
        "apps",
        "categories",
        "devices",
        "emblems",
        "emotes",
        "filesystems",
        "intl",
        "legacy",
        "mimetypes",
        "notifications",
        "panel",
        "places",
        "preferences",
        "shortcuts",
        "status",
        "stock",
        "ui",
    ];
    if CATEGORIES.contains(&name) {
        return true;
    }
    if name == "scalable" || name == "symbolic" {
        return true;
    }
    if let Some(rest) = name
        .strip_prefix("scalable@")
        .or_else(|| name.strip_prefix("symbolic@"))
    {
        return !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit());
    }
    // 48x48, 128x128@2 and the like.
    let Some((width, rest)) = name.split_once('x') else {
        return false;
    };
    let height = rest.split_once('@').map_or(rest, |(height, _)| height);
    !width.is_empty()
        && !height.is_empty()
        && width.bytes().all(|b| b.is_ascii_digit())
        && height.bytes().all(|b| b.is_ascii_digit())
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
        // Follows links: a package-manager theme is made of them. What the
        // hardening wanted is not refused here — the open and the content
        // check below are where an unexpected inode is stopped. FIFOs,
        // devices and sockets report as neither file nor directory and are
        // skipped before anything opens them.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            if depth >= MAX_DEPTH {
                continue;
            }
            if entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
                let Some(component) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if !theme_component(component) {
                    continue;
                }
            }
            walk(&path, name, want, depth + 1, consider);
            continue;
        }
        if !meta.is_file() {
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

/// How much larger than the size wanted a pixmap may be encoded as it came.
///
/// Not one, because the shell draws the icon on whatever scale the screen has
/// and a picture handed over at exactly 22 pixels is soft on a 2x display.
/// Four is that headroom with room to spare, and it is what bounds the message
/// the shell is sent: a 22-pixel icon at this multiple is 88 pixels, which is
/// 31KB of pixels and 42KB of data URL however big the item's own copy was.
const OVERSAMPLE: u32 = 4;

/// The pixmap nearest the size wanted, encoded as a PNG data URL.
///
/// Applications send several sizes and the specification does not order them,
/// so the choice is made here rather than by taking the first — GNOME's own
/// items send 16, 22, 24 and 32 pixel copies in whatever order the toolkit
/// built them.
///
/// An item that sends *one* size gets it whatever that size is, which is where
/// picking the nearest stops being enough: Electron's tray publishes a single
/// 512x512 pixmap and nothing else, so "nearest" is a megabyte of pixels. This
/// file's PNG writer does not compress, so that megabyte survived encoding,
/// and the 1.4MB data URL that came out of it was a single control message
/// larger than the shell's whole backlog allowance — the compositor dropped
/// the desktop's connection the moment such an application started, and the
/// shell died with it. So anything above [`OVERSAMPLE`] times the size wanted
/// is scaled down first, and the picture the shell is sent is bounded by what
/// the bar can draw rather than by what the application felt like sending.
pub fn pixmap_url(pixmaps: &[Pixmap], want: u32) -> Option<String> {
    let best = pixmaps
        .iter()
        .filter(|p| p.width > 0 && p.height > 0)
        .filter(|p| p.argb.len() >= (p.width as usize) * (p.height as usize) * 4)
        .min_by_key(|p| (p.width as u32).abs_diff(want))?;
    let width = best.width as usize;
    let height = best.height as usize;
    let rgba = argb_to_rgba(&best.argb, width * height);

    let limit = (want.max(1) * OVERSAMPLE) as usize;
    let (width, height, rgba) = if width > limit || height > limit {
        downscale(&rgba, width, height, limit)
    } else {
        (width, height, rgba)
    };

    let png = png(width as u32, height as u32, &rgba);
    Some(format!("data:image/png;base64,{}", base64(&png)))
}

/// `rgba` fitted inside `limit` on its longer side, keeping its shape.
///
/// A box filter — every destination pixel is the average of the source
/// rectangle it covers — rather than dropping rows and columns, because an
/// icon reduced by nearest neighbour loses the thin strokes that are most of
/// what an icon is.
///
/// The average is taken with the colour premultiplied by alpha and undone
/// afterwards. Averaging colour straight would let whatever is behind the
/// transparent part of an icon — black, in every toolkit that clears its
/// buffer — bleed into the edge of the visible part, which is the dark halo a
/// naive resize puts around a logo.
fn downscale(rgba: &[u8], width: usize, height: usize, limit: usize) -> (usize, usize, Vec<u8>) {
    let longest = width.max(height);
    let dest_width = (width * limit / longest).max(1);
    let dest_height = (height * limit / longest).max(1);

    let mut out = Vec::with_capacity(dest_width * dest_height * 4);
    for y in 0..dest_height {
        let top = y * height / dest_height;
        let bottom = (((y + 1) * height).div_ceil(dest_height))
            .max(top + 1)
            .min(height);
        for x in 0..dest_width {
            let left = x * width / dest_width;
            let right = (((x + 1) * width).div_ceil(dest_width))
                .max(left + 1)
                .min(width);

            let (mut red, mut green, mut blue, mut alpha) = (0u64, 0u64, 0u64, 0u64);
            for row in top..bottom {
                for column in left..right {
                    let at = (row * width + column) * 4;
                    let weight = u64::from(rgba[at + 3]);
                    red += u64::from(rgba[at]) * weight;
                    green += u64::from(rgba[at + 1]) * weight;
                    blue += u64::from(rgba[at + 2]) * weight;
                    alpha += weight;
                }
            }

            let pixels = ((bottom - top) * (right - left)) as u64;
            // A fully transparent block has no colour to recover — its
            // weights are all zero — and black at zero alpha is what every
            // other pixel of empty space in the image already is.
            let colour = |sum: u64| sum.checked_div(alpha).unwrap_or(0) as u8;
            out.extend_from_slice(&[
                colour(red),
                colour(green),
                colour(blue),
                (alpha / pixels) as u8,
            ]);
        }
    }
    (dest_width, dest_height, out)
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
/// A valid PNG containing `rgba` pixels at `width`x`height`.
///
/// The zlib stream is deflate's "stored" block type: no compression, a length
/// and its complement, and the data. Every decoder handles it because it is
/// the format's own escape hatch for incompressible input, and it means this
/// file contains no compressor.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    png(width, height, rgba)
}

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

    /// The one that killed the desktop: an item with a single 512-pixel
    /// pixmap and no smaller copy. Encoded as it came, that is a 1.4MB data
    /// URL in one control message — more than the shell's whole backlog
    /// allowance, so the compositor dropped the shell's connection and the
    /// shell died. It is scaled down instead.
    #[test]
    fn one_enormous_pixmap_is_scaled_rather_than_sent_whole() {
        let pixmaps = vec![Pixmap {
            width: 512,
            height: 512,
            argb: vec![0x80; 512 * 512 * 4],
        }];
        let url = pixmap_url(&pixmaps, 22).expect("a pixmap");
        // 88x88 of pixels, its PNG wrapper and base64's third on top: tens of
        // kilobytes, against the megabyte the pixmap arrived as.
        assert!(url.len() < 64 * 1024, "{} bytes of icon", url.len());
    }

    /// The picture, not just its size: a block of one colour stays that
    /// colour, and the shape it was sent in is kept.
    #[test]
    fn scaling_averages_rather_than_drops_pixels() {
        // Four pixels, one of them opaque red and the rest transparent.
        let rgba = [
            255, 0, 0, 255, // red
            0, 0, 0, 0, // and three of nothing
            0, 0, 0, 0, //
            0, 0, 0, 0, //
        ];
        let (width, height, out) = downscale(&rgba, 2, 2, 1);
        assert_eq!((width, height), (1, 1));
        // Red at a quarter of the alpha — not black, which is what averaging
        // the colour without premultiplying would give.
        assert_eq!(out, vec![255, 0, 0, 63]);

        // A wide icon keeps its shape rather than becoming a square.
        let (width, height, _) = downscale(&vec![0; 64 * 16 * 4], 64, 16, 8);
        assert_eq!((width, height), (8, 2));
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

    /// AVIF may name its brand as the major brand or among the compatible
    /// ones; either is a picture, and neither is a renamed secret.
    #[test]
    fn avif_brands_are_recognised() {
        let mut file = vec![0u8; 32];
        file[4..8].copy_from_slice(b"ftyp");
        file[8..12].copy_from_slice(b"avif");
        assert!(avif_magic(&file));

        file[8..12].copy_from_slice(b"mif1");
        file[16..20].copy_from_slice(b"avif");
        assert!(avif_magic(&file));

        file[16..20].copy_from_slice(b"heic");
        assert!(!avif_magic(&file));
        assert!(!avif_magic(b"ftypavif"));
    }

    /// The best candidate is the scalable one; the size-matched PNGs follow
    /// so a caller with a message budget can fall to the one that fits.
    #[test]
    fn candidates_are_scored_best_first() {
        let dir = scratch_dir("candidates");
        for (size, name) in [
            ("48x48", "viewport-test-cand.png"),
            ("22x22", "viewport-test-cand.png"),
            ("scalable", "viewport-test-cand.svg"),
        ] {
            let where_ = dir.join(size).join("apps");
            std::fs::create_dir_all(&where_).expect("the layout");
            std::fs::write(where_.join(name), b"x").expect("the icon");
        }

        let found = candidates(
            "viewport-test-cand",
            Some(dir.to_str().unwrap()),
            "hicolor",
            48,
        );
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(found[0].ends_with("scalable/apps/viewport-test-cand.svg"));
        assert!(found[1].ends_with("48x48/apps/viewport-test-cand.png"));
        assert!(found[2].ends_with("22x22/apps/viewport-test-cand.png"));

        let _ = std::fs::remove_dir_all(&dir);
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

    /// A scratch directory of this test binary's own, so a symlink or a FIFO
    /// can be made without touching anything real.
    #[cfg(unix)]
    fn scratch_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("viewport-icon-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    /// A symlinked icon resolves — a package-manager theme is links all the
    /// way down — but a link to a regular file that is not an image does not,
    /// because the bytes are checked against the extension before anything is
    /// sent to the shell.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_image_resolves_but_a_symlinked_secret_does_not() {
        use std::os::unix::fs::symlink;

        let dir = scratch_dir("symlink");
        let target = dir.join("real-icon.png");
        std::fs::write(&target, png(1, 1, &[0, 0, 0, 0])).expect("the target");
        let link = dir.join("viewport-test-icon.png");
        symlink(&target, &link).expect("the symlink");

        assert!(data_url(&link).is_some());
        assert!(art_data_url(&link).is_some());
        assert_eq!(
            lookup(
                "viewport-test-icon",
                Some(dir.to_str().unwrap()),
                "hicolor",
                22
            )
            .as_deref(),
            Some(link.as_path())
        );

        // A regular file that is not a PNG, named as one: a theme must not be
        // able to base64 a key into a shell message through the name alone.
        let secret = dir.join("secret");
        std::fs::write(&secret, b"-----BEGIN OPENSSH PRIVATE KEY-----\n").expect("the secret");
        let liar = dir.join("viewport-test-secret.png");
        symlink(&secret, &liar).expect("the second symlink");
        assert_eq!(data_url(&liar), None);
        assert_eq!(art_data_url(&liar), None);

        // The absolute-path arm follows links too, and then fails the same
        // content check rather than reading the file out.
        assert_eq!(
            lookup(liar.to_str().unwrap(), None, "hicolor", 22).as_deref(),
            Some(liar.as_path())
        );
        assert_eq!(data_url(liar.as_path()), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The NixOS union layout: the size directory and the icon file are links
    /// into the store, and a lookup has to arrive at the file through both.
    #[cfg(unix)]
    #[test]
    fn a_package_manager_union_theme_resolves() {
        use std::os::unix::fs::symlink;

        let root = scratch_dir("union");
        let store = scratch_dir("union-store");
        let store_size = store.join("48x48/apps");
        std::fs::create_dir_all(&store_size).expect("the store layout");
        std::fs::write(
            store_size.join("nixos-test-icon.png"),
            png(1, 1, &[0, 0, 0, 0]),
        )
        .expect("the store icon");

        let theme = root.join("hicolor");
        std::fs::create_dir_all(&theme).expect("the theme root");
        symlink(store.join("48x48"), theme.join("48x48")).expect("the size link");
        // A file link beside the real one: the path the walk finds is a link,
        // and the icon has to be read through it.
        symlink(
            store.join("48x48/apps/nixos-test-icon.png"),
            store_size.join("nixos-test-link.png"),
        )
        .expect("the file link");

        let found = lookup(
            "nixos-test-link",
            Some(theme.to_str().unwrap()),
            "hicolor",
            48,
        )
        .expect("the union icon");
        assert!(found.ends_with("48x48/apps/nixos-test-link.png"));
        assert!(
            data_url(&found).is_some(),
            "the linked image must be served"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&store);
    }

    /// Only the layout's own names are followed when a directory is a link,
    /// so a theme cannot point the walk at the machine root.
    #[test]
    fn only_theme_layout_directory_links_are_followed() {
        assert!(theme_component("48x48"));
        assert!(theme_component("128x128@2"));
        assert!(theme_component("scalable"));
        assert!(theme_component("scalable@2"));
        assert!(theme_component("apps"));
        assert!(theme_component("status"));
        assert!(!theme_component(""));
        assert!(!theme_component(".."));
        assert!(!theme_component("not-a-theme-component"));
        assert!(!theme_component("48x"));
        assert!(!theme_component("x48"));
    }

    /// A FIFO named `icon.png` used to block the compositor worker inside
    /// `open` with no writer ever coming. It is refused before any read.
    #[cfg(unix)]
    #[test]
    fn a_fifo_named_like_an_icon_is_refused() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt as _;

        let dir = scratch_dir("fifo");
        let fifo = dir.join("viewport-test-icon.png");
        let path = CString::new(fifo.as_os_str().as_bytes()).expect("a path");
        let made = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
        assert_eq!(made, 0, "mkfifo: {}", std::io::Error::last_os_error());

        assert_eq!(data_url(&fifo), None);
        assert_eq!(art_data_url(&fifo), None);
        assert_eq!(
            lookup(
                "viewport-test-icon",
                Some(dir.to_str().unwrap()),
                "hicolor",
                22
            ),
            None
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The size cap is the boundary, not an approximation: a file that is one
    /// byte over is refused however its extension advertises it.
    #[test]
    fn an_icon_at_the_size_cap_is_accepted_and_one_over_is_not() {
        let dir = std::env::temp_dir().join(format!("viewport-icon-size-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");

        let at_cap = dir.join("at-cap.png");
        let mut at_cap_bytes = vec![0u8; MAX_FILE as usize];
        at_cap_bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        std::fs::write(&at_cap, at_cap_bytes).expect("the at-cap file");
        assert!(data_url(&at_cap).is_some());

        let over_cap = dir.join("over-cap.png");
        let mut over_bytes = vec![0u8; MAX_FILE as usize + 1];
        over_bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        std::fs::write(&over_cap, over_bytes).expect("the over-cap file");
        assert_eq!(data_url(&over_cap), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
