// SPDX-License-Identifier: GPL-3.0-or-later
//
// Reading hyprcursor themes — the cursor format Hyprland ships, which XCursor
// tooling cannot read.
//
// A theme is a directory with `manifest.hl` (or `manifest.toml`) and a cursors
// directory inside it. Each cursor in that directory is a zip archive — named
// after the shape, e.g. `left_ptr.hlc` — holding a `meta.hl`/`meta.toml` and
// the images it names. `hyprcursor-util --create` packs them that way, and the
// reference loader opens each entry as an archive; unpacked shapes are read
// too, because the format's own documentation shows the source layout and a
// theme assembled by hand is a theme.
//
// The manifest:
//
//     name = My theme!
//     description = ...
//     cursors_directory = hyprcursors
//
// The meta:
//
//     resize_algorithm = bilinear   # bilinear | nearest | none
//     hotspot_x = 0.5              # fraction of the image, 0..1
//     hotspot_y = 0.5
//     define_size = 32, image32.png, 40   # size, file[, delay ms]; repeated
//     define_override = arrow              # another name that means this shape
//
// `nominal_size` (default 1) divides the requested size before an image is
// picked, for themes whose artwork is drawn smaller than its pixels.
//
// SVG shapes are the one part of the format this does not implement: the
// compositor has no SVG renderer, so a shape that is SVG-only is reported as
// absent and the same name is answered by the XCursor theme instead. Every
// other part of the theme keeps working.
//
// The decision logic (manifest, meta, size selection, resampling, hotspots) is
// kept here and tested without a compositor; `cursor.rs` turns the pixels into
// something the renderer can draw.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// How an image is scaled to a size the theme did not ship.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resize {
    /// Pick the nearest size and draw it at its own resolution, as XCursor
    /// does. `resize_algorithm = none`.
    None,
    Nearest,
    Bilinear,
}

impl Resize {
    fn parse(name: &str) -> Resize {
        match name.trim().to_ascii_lowercase().as_str() {
            "bilinear" => Resize::Bilinear,
            "none" => Resize::None,
            // The hyprlang default when the key is absent; an empty string is
            // what the TOML parser returns for the same absence.
            _ => Resize::Nearest,
        }
    }
}

/// One image of a shape, as the meta named it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageRef {
    file: String,
    /// The size the artwork was drawn at. Zero for SVG, which has no size.
    side: u32,
    /// Frame delay in milliseconds; 0 for a still image.
    delay: u32,
}

/// What a shape in the cursors directory contains, before any image is read.
#[derive(Debug, Clone)]
struct ShapeInfo {
    /// The name the theme gives this shape — the archive's stem.
    directory: String,
    overrides: Vec<String>,
    hotspot_x: f64,
    hotspot_y: f64,
    nominal: f64,
    resize: Resize,
    svg: bool,
    images: Vec<ImageRef>,
    source: Source,
}

#[derive(Debug, Clone)]
enum Source {
    /// A `.hlc` zip archive holding `meta.hl` and the images.
    Archive(PathBuf),
    /// An unpacked shape directory: `meta.hl` beside the images.
    Directory(PathBuf),
}

/// A decoded frame, straight (not premultiplied) RGBA.
pub struct Frame {
    pub pixels: Vec<u8>,
    pub delay: u32,
}

/// One shape loaded for one requested size.
pub struct Loaded {
    pub frames: Vec<Frame>,
    pub width: u32,
    pub height: u32,
    /// Hotspot in pixels of the drawn image, from the top-left corner.
    pub hotspot: (i32, i32),
}

/// A theme on disk, with every shape's metadata read and no images decoded yet.
pub struct Theme {
    name: String,
    shapes: Vec<ShapeInfo>,
}

/// Bounds a hostile or mistaken theme cannot go past.
///
/// A cursor image is at most a few hundred pixels and a handful of frames;
/// these are far above anything real and far below anything that would take a
/// frame of the compositor to decode.
const MAX_SIDE: u32 = 1024;
const MAX_FRAMES: usize = 256;
const MAX_IMAGE_BYTES: u64 = 4 * 1024 * 1024;
/// The most manifest bytes a shape's meta may be read for.
///
/// Capped where the directory reads are, by the file's own length; the
/// archive's are capped by [`MAX_IMAGE_BYTES`]-style takes instead, because
/// a zip's central directory is the archive's own claim and a theme is
/// untrusted input.
const MAX_META_BYTES: u64 = 1024 * 1024;

impl Theme {
    /// The theme at `root`, read from its manifest and shape metadata.
    ///
    /// `None` when the directory is not a hyprcursor theme or its manifest
    /// cannot be read; a shape that fails on its own is dropped rather than
    /// taking the theme with it, which is the difference between one broken
    /// cursor and no pointer at all.
    pub fn load(root: &Path) -> Option<Theme> {
        let manifest = read_sidecar(root, "manifest")?;
        let fields = parse_manifest(&manifest)?;
        let cursors = fields.get("cursors_directory").map(String::as_str)?;
        if cursors.is_empty() {
            return None;
        }
        let cursors = root.join(cursors);
        let entries = std::fs::read_dir(&cursors).ok()?;

        let mut shapes = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(info) = read_shape(&path) else {
                continue;
            };
            shapes.push(info);
        }
        if shapes.is_empty() {
            return None;
        }
        let name = fields
            .get("name")
            .filter(|name| !name.is_empty())
            .cloned()
            .unwrap_or_else(|| {
                root.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
        Some(Theme { name, shapes })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The first shape that answers to `requested`, loaded at `wanted` pixels.
    ///
    /// Mirrors the reference loader: a shape matches when its directory is the
    /// requested name or the meta lists it in `define_override`, and the first
    /// match in directory order wins. Overrides are how a theme that ships
    /// `default` also answers `left_ptr`.
    pub fn shape(&self, requested: &str, wanted: u32) -> Option<Loaded> {
        let wanted = wanted.clamp(1, MAX_SIDE);
        let info = self.shapes.iter().find(|shape| {
            shape.directory == requested || shape.overrides.iter().any(|o| o == requested)
        })?;
        if info.svg || info.images.is_empty() {
            return None;
        }

        // Which size the artwork is asked for, and which size it is drawn at.
        //
        // `nominal_size` exists because a theme may draw its artwork smaller
        // than the pixels it represents: with nominal 2 a request for 48px
        // looks for 24px images and draws them at 48. The reference loader asks
        // for `size / nominal` and draws at the requested size, which is what
        // the two branches below reproduce.
        let art = ((wanted as f64 / info.nominal).round() as u32).clamp(1, MAX_SIDE);
        let available: Vec<u32> = info.images.iter().map(|image| image.side).collect();
        let exact: Vec<&ImageRef> = info
            .images
            .iter()
            .filter(|image| image.side == art)
            .collect();

        let (chosen, draw): (Vec<&ImageRef>, u32) = if !exact.is_empty() {
            (exact, wanted)
        } else if info.resize == Resize::None {
            // No resampling: use the nearest size at its own resolution, so a
            // pointer drawn at a size the theme ships is never blurred.
            let nearest = *available.iter().min_by_key(|side| side.abs_diff(art))?;
            let chosen = info
                .images
                .iter()
                .filter(|image| image.side == nearest)
                .collect();
            (chosen, nearest)
        } else {
            // The reference loader takes the smallest size at or above the one
            // asked for, and the closest when nothing is big enough.
            let leader = available
                .iter()
                .copied()
                .filter(|side| *side >= art)
                .min()
                .or_else(|| {
                    available
                        .iter()
                        .copied()
                        .min_by_key(|side| side.abs_diff(art))
                })?;
            let chosen = info
                .images
                .iter()
                .filter(|image| image.side == leader)
                .collect();
            (chosen, wanted)
        };

        let mut frames = Vec::with_capacity(chosen.len());
        for image in chosen.iter().take(MAX_FRAMES) {
            let bytes = read_image(&info.source, &image.file)?;
            let (pixels, width, height) = decode_png(&bytes)?;
            let pixels = if width == draw && height == draw {
                pixels
            } else {
                resample(&pixels, width, height, draw, info.resize)
            };
            frames.push(Frame {
                pixels,
                delay: image.delay,
            });
        }
        if frames.is_empty() {
            return None;
        }
        // The hotspot is a fraction of the drawn image, rounded as the
        // reference loader rounds it. Its own doc comment:
        //   (round(size * hotspot_x), round(size * hotspot_y))
        let hotspot = (
            (info.hotspot_x * f64::from(draw)).round() as i32,
            (info.hotspot_y * f64::from(draw)).round() as i32,
        );
        Some(Loaded {
            frames,
            width: draw,
            height: draw,
            hotspot,
        })
    }
}

/// The directories themed cursors are looked for in, in the order the
/// reference loader looks.
///
/// The two under `$HOME` come first, as they do there: a user theme installed
/// in either overrides the system's. `XDG_DATA_HOME` is deliberately not
/// consulted — hyprcursor's own loader hard-codes these, and a theme found in
/// a place the reference implementation would not look is a theme that works
/// here and nowhere else.
fn theme_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        roots.push(home.join(".local/share/icons"));
        roots.push(home.join(".icons"));
    }
    match std::env::var_os("XDG_DATA_DIRS") {
        Some(dirs) => {
            roots.extend(std::env::split_paths(&dirs).map(|dir| dir.join("icons")));
        }
        None => roots.push(PathBuf::from("/usr/share/icons")),
    }
    roots
}

/// The first hyprcursor theme named `name`, or the first one there is when the
/// name is empty or the default.
///
/// Matching follows the reference loader: the manifest's human-readable `name`
/// or the directory's own name both count. The fallback to any theme is only
/// for an unset or default name — substituting an unrelated theme for a
/// configured XCursor one would hand the user a pointer they did not choose.
pub fn named(name: &str) -> Option<Theme> {
    find(&theme_roots(), name)
}

/// The same, over an explicit list of roots, which is what the tests use.
fn find(roots: &[PathBuf], name: &str) -> Option<Theme> {
    let name = name.trim();
    let fallback = name.is_empty() || name == "default";
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(theme) = Theme::load(&path) else {
                continue;
            };
            if fallback {
                return Some(theme);
            }
            let directory = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if theme.name == name || directory == name {
                return Some(theme);
            }
        }
    }
    None
}

/// Read `manifest.hl` or, failing that, `manifest.toml` beside a base path.
fn read_sidecar(base: &Path, stem: &str) -> Option<(String, bool)> {
    let hl = base.join(format!("{stem}.hl"));
    if let Ok(text) = std::fs::read_to_string(&hl) {
        return Some((text, true));
    }
    let toml = base.join(format!("{stem}.toml"));
    if let Ok(text) = std::fs::read_to_string(&toml) {
        return Some((text, false));
    }
    None
}

/// The manifest as key/value pairs, without the hyprlang-specific bits this
/// does not need (`#` comments and `key = value` lines).
fn parse_manifest(contents: &(String, bool)) -> Option<HashMap<String, String>> {
    let (text, hyprlang) = contents;
    let mut fields = HashMap::new();
    if *hyprlang {
        for line in text.lines() {
            let line = strip_comment(line).trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            fields.insert(key.trim().to_owned(), unquote(value.trim()));
        }
    } else {
        // A TOML subset: the `[General]` table, plain `key = "value"` pairs.
        // hyprcursor's own TOML files use nothing else.
        for line in text.lines() {
            let line = strip_comment(line).trim();
            if line.is_empty() || line.starts_with('[') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            fields.insert(key.trim().to_owned(), unquote(value.trim()));
        }
    }
    if fields.is_empty() {
        return None;
    }
    Some(fields)
}

/// One shape's metadata, from either a zip archive or a directory.
fn read_shape(path: &Path) -> Option<ShapeInfo> {
    let file_name = path.file_name()?.to_string_lossy().into_owned();
    if path.is_dir() {
        let directory = file_name;
        let (meta, hyprlang) = read_sidecar(path, "meta")?;
        let source = Source::Directory(path.to_path_buf());
        return shape_from_meta(&directory, &(meta, hyprlang), &source);
    }
    if !path.is_file() {
        return None;
    }
    // The archive is named after the shape, with or without `.hlc`.
    let directory = file_name
        .strip_suffix(".hlc")
        .unwrap_or(&file_name)
        .to_owned();
    let mut archive = open_archive(path)?;
    let (meta, hyprlang) = {
        let mut text = String::new();
        let mut found = None;
        for (name, hyprlang) in [("meta.hl", true), ("meta.toml", false)] {
            if let Ok(mut file) = archive.by_name(name) {
                // Capped: the central directory's size field is the archive's
                // own claim about itself, and a file that lies about it is
                // read for as long as the truth runs. A manifest is a few
                // lines; a megabyte is more than any of them.
                std::io::Read::take(&mut file, MAX_META_BYTES)
                    .read_to_string(&mut text)
                    .ok()?;
                found = Some(hyprlang);
                break;
            }
        }
        (text, found?)
    };
    let source = Source::Archive(path.to_path_buf());
    shape_from_meta(&directory, &(meta, hyprlang), &source)
}

fn open_archive(path: &Path) -> Option<zip::ZipArchive<std::fs::File>> {
    let file = std::fs::File::open(path).ok()?;
    zip::ZipArchive::new(file).ok()
}

fn shape_from_meta(directory: &str, meta: &(String, bool), source: &Source) -> Option<ShapeInfo> {
    let (text, hyprlang) = meta;
    let mut hotspot_x = 0.0f64;
    let mut hotspot_y = 0.0f64;
    let mut nominal = 1.0f64;
    let mut resize = if *hyprlang {
        Resize::Nearest
    } else {
        Resize::None
    };
    let mut overrides = Vec::new();
    let mut images = Vec::new();
    let mut svg = false;
    let mut kind_seen = false;

    for line in text.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() || line.starts_with('[') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = unquote(value.trim());
        match key {
            // A hotspot is a fraction of the drawn image and feeds
            // `(fraction * draw).round() as i32`; a NaN parse survives every
            // clamp below (`f64::NAN.clamp` is still NaN) and poisons the
            // `as i32` at the end into zero-width arithmetic. Anything not
            // finite is a mistyped file, not a position.
            "hotspot_x" => {
                hotspot_x = value
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .unwrap_or(0.0)
            }
            "hotspot_y" => {
                hotspot_y = value
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .unwrap_or(0.0)
            }
            "nominal_size" => {
                nominal = value
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .unwrap_or(1.0)
                    .clamp(0.1, 2.0);
            }
            "resize_algorithm" => resize = Resize::parse(&value),
            "define_override" => {
                for name in value.split(';') {
                    let name = name.trim();
                    if !name.is_empty() {
                        overrides.push(name.to_owned());
                    }
                }
            }
            "define_size" => {
                for clause in value.split(';') {
                    let Some(image) = parse_define_size(clause) else {
                        continue;
                    };
                    // A shape is entirely PNG or entirely SVG; the reference
                    // loader refuses a mix, and refusing here means falling
                    // back to XCursor rather than drawing half a cursor.
                    let is_svg = image.file.ends_with(".svg");
                    if kind_seen && is_svg != svg {
                        return None;
                    }
                    kind_seen = true;
                    svg = is_svg;
                    images.push(image);
                }
            }
            _ => {}
        }
    }
    if images.is_empty() {
        return None;
    }
    Some(ShapeInfo {
        directory: directory.to_owned(),
        overrides,
        hotspot_x,
        hotspot_y,
        nominal,
        resize,
        svg,
        images,
        source: source.clone(),
    })
}

/// `size, file[, delay]`, where size is left out for an SVG.
fn parse_define_size(clause: &str) -> Option<ImageRef> {
    let mut parts = clause.split(',');
    let side = parts.next()?.trim();
    let file = parts.next()?.trim();
    if file.is_empty() || file.contains('/') || file.contains('\\') || file.starts_with('.') {
        return None;
    }
    let delay = parts
        .next()
        .and_then(|delay| delay.trim().parse::<u32>().ok())
        .unwrap_or(0);
    let side = if file.ends_with(".svg") {
        0
    } else {
        // Capped at the same bound the decoder and the draw path hold
        // everything else to. A `define_size` past it is a lie about the data
        // that follows — with no resample algorithm set, the theme asks for a
        // buffer of side² × 4 bytes, and the multiplication was done in `u32`
        // where a large enough lie wrapped to nothing and sliced out of
        // bounds, or close to it asked for four gibibytes a frame.
        side.parse::<u32>()
            .ok()
            .filter(|side| *side > 0 && *side <= MAX_SIDE)?
    };
    Some(ImageRef {
        file: file.to_owned(),
        side,
        delay,
    })
}

/// The bytes of one image, from whichever kind of source the shape has.
fn read_image(source: &Source, file: &str) -> Option<Vec<u8>> {
    match source {
        Source::Directory(path) => {
            let path = path.join(file);
            let metadata = std::fs::metadata(&path).ok()?;
            if metadata.len() > MAX_IMAGE_BYTES {
                return None;
            }
            std::fs::read(path).ok()
        }
        Source::Archive(path) => {
            let mut archive = open_archive(path)?;
            let mut image = archive.by_name(file).ok()?;
            if image.size() > MAX_IMAGE_BYTES {
                return None;
            }
            // Read against the cap, not the claimed size: the size is the
            // archive's own field, and `read_to_end` under a lying one would
            // allocate what the truth turns out to be. One byte past the cap
            // is how a refusal is told from an exactly-full read.
            let mut bytes = Vec::new();
            std::io::Read::take(&mut image, MAX_IMAGE_BYTES + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() as u64 > MAX_IMAGE_BYTES {
                return None;
            }
            Some(bytes)
        }
    }
}

/// A PNG as straight RGBA8, with its size.
fn decode_png(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let size = reader.output_buffer_size()?;
    let mut buffer = vec![0; size];
    let info = reader.next_frame(&mut buffer).ok()?;
    let (width, height) = (info.width, info.height);
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return None;
    }
    let bytes = &buffer[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => bytes.to_vec(),
        png::ColorType::Rgb => bytes
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => bytes
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|pixel| [pixel[0], pixel[0], pixel[0], pixel[1]])
            .collect(),
        png::ColorType::Grayscale => bytes
            .iter()
            .flat_map(|value| [*value, *value, *value, 255])
            .collect(),
        // EXPAND resolves palettes and tRNS before this point.
        png::ColorType::Indexed => return None,
    };
    Some((rgba, width, height))
}

/// Scale straight RGBA to `target` square pixels.
///
/// Nearest is the format's default and what a cursor drawn at a size the theme
/// did not ship should use by default: a pointer is a small image where
/// sharpness matters more than smoothness, and bilinear on a straight-alpha
/// image blends the colour of transparent pixels into the edge.
fn resample(pixels: &[u8], width: u32, height: u32, target: u32, how: Resize) -> Vec<u8> {
    if width == target && height == target {
        return pixels.to_vec();
    }
    let (tw, th) = (target, target);
    // Widened before the multiply: `u32` wrapped on a large enough target,
    // and a wrapped length is a small allocation indexed far past its end.
    let mut out = vec![0u8; tw as usize * th as usize * 4];
    match how {
        Resize::None | Resize::Nearest => {
            for y in 0..th {
                let sy = (y * height / th).min(height - 1);
                for x in 0..tw {
                    let sx = (x * width / tw).min(width - 1);
                    let src = ((sy * width + sx) * 4) as usize;
                    let dst = ((y * tw + x) * 4) as usize;
                    out[dst..dst + 4].copy_from_slice(&pixels[src..src + 4]);
                }
            }
        }
        Resize::Bilinear => {
            for y in 0..th {
                // Sample at pixel centres, as every resampler does, so the
                // image is not shifted half a pixel at the edges.
                let fy =
                    ((y as f64 + 0.5) * f64::from(height) / f64::from(th) - 0.5).max(0.0) as u32;
                let fy_next = (fy + 1).min(height - 1);
                let wy =
                    ((y as f64 + 0.5) * f64::from(height) / f64::from(th) - 0.5) - f64::from(fy);
                for x in 0..tw {
                    let fx =
                        ((x as f64 + 0.5) * f64::from(width) / f64::from(tw) - 0.5).max(0.0) as u32;
                    let fx_next = (fx + 1).min(width - 1);
                    let wx =
                        ((x as f64 + 0.5) * f64::from(width) / f64::from(tw) - 0.5) - f64::from(fx);
                    let mut pixel = [0u8; 4];
                    for channel in 0..4 {
                        let at = |px: u32, py: u32| {
                            f64::from(pixels[((py * width + px) * 4 + channel) as usize])
                        };
                        let top = at(fx, fy) * (1.0 - wx) + at(fx_next, fy) * wx;
                        let bottom = at(fx, fy_next) * (1.0 - wx) + at(fx_next, fy_next) * wx;
                        pixel[channel as usize] =
                            (top * (1.0 - wy) + bottom * wy).round().clamp(0.0, 255.0) as u8;
                    }
                    let dst = ((y * tw + x) * 4) as usize;
                    out[dst..dst + 4].copy_from_slice(&pixel);
                }
            }
        }
    }
    out
}

fn strip_comment(line: &str) -> &str {
    line.split_once('#').map(|(head, _)| head).unwrap_or(line)
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    match value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        Some(inner) => inner.to_owned(),
        None => match value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
            Some(inner) => inner.to_owned(),
            None => value.to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn png_of(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        crate::icon::encode_png(width, height, rgba)
    }

    fn red(width: u32) -> Vec<u8> {
        (0..width * width)
            .flat_map(|_| [255u8, 0, 0, 255])
            .collect()
    }

    /// A manifest and meta in the hyprlang spelling, plus one shape archive.
    fn theme_in(dir: &Path) -> PathBuf {
        let root = dir.join("Test-Theme");
        std::fs::create_dir_all(root.join("hyprcursors")).unwrap();
        std::fs::write(
            root.join("manifest.hl"),
            "name = Test Theme\ndescription = test\nversion = 0.1\ncursors_directory = hyprcursors\n",
        )
        .unwrap();
        // A shape with two sizes, an animation and an override.
        let archive_path = root.join("hyprcursors/arrow.hlc");
        let file = std::fs::File::create(&archive_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("meta.hl", options).unwrap();
        zip.write_all(
            b"resize_algorithm = nearest\nhotspot_x = 0.5\nhotspot_y = 0.25\n\
              define_override = left_ptr\ndefine_size = 24, a24.png, 40\ndefine_size = 24, b24.png, 60\ndefine_size = 48, a48.png\n",
        )
        .unwrap();
        for name in ["a24.png", "b24.png", "a48.png"] {
            zip.start_file(name, options).unwrap();
            zip.write_all(&png_of(2, 2, &red(2))).unwrap();
        }
        zip.finish().unwrap();
        root
    }

    #[test]
    fn a_manifest_and_meta_are_read_the_way_the_reference_loader_reads_them() {
        let dir = std::env::temp_dir().join(format!(
            "viewport-hyprcursor-{}",
            std::process::id() as u64 * 1000 + 7
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = theme_in(&dir);
        let theme = Theme::load(&root).expect("theme loads");
        assert_eq!(theme.name(), "Test Theme");

        // The 24px animation: two frames with a 40ms and a 60ms delay.
        let loaded = theme.shape("arrow", 24).expect("arrow at 24");
        assert_eq!((loaded.width, loaded.height), (24, 24));
        assert_eq!(loaded.frames.len(), 2);
        assert_eq!(loaded.frames[0].delay, 40);
        assert_eq!(loaded.frames[1].delay, 60);
        // 0.5 and 0.25 of 24, rounded.
        assert_eq!(loaded.hotspot, (12, 6));

        // The override answers to the XCursor spelling.
        let alias = theme.shape("left_ptr", 24).expect("override answers");
        assert_eq!(alias.frames.len(), 2);

        // 48 is shipped exactly, so no resampling happens to produce it.
        let large = theme.shape("arrow", 48).expect("arrow at 48");
        assert_eq!((large.width, large.height), (48, 48));
        assert_eq!(large.hotspot, (24, 12));

        // 32 is not shipped: the smallest larger image is resampled to the
        // size actually asked for, which is the point of the resize methods.
        let between = theme.shape("arrow", 32).expect("resampled size");
        assert_eq!(between.width, 32, "nearest leads with 48 and draws at 32");
        assert_eq!(between.hotspot, (16, 8));

        assert!(
            theme.shape("text", 24).is_none(),
            "an absent shape is absent"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_resampled_size_is_drawn_at_the_size_asked_for() {
        let dir = std::env::temp_dir().join(format!(
            "viewport-hyprcursor-bilinear-{}",
            std::process::id() as u64 * 1000 + 11
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = Theme::load(&dir);
        assert!(root.is_none(), "an empty directory is not a theme");

        std::fs::create_dir_all(dir.join("T/hyprcursors")).unwrap();
        std::fs::write(
            dir.join("T/manifest.hl"),
            "name = T\ncursors_directory = hyprcursors\n",
        )
        .unwrap();
        let file = std::fs::File::create(dir.join("T/hyprcursors/pointer.hlc")).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("meta.hl", options).unwrap();
        zip.write_all(b"resize_algorithm = bilinear\nhotspot_x = 0.0\nhotspot_y = 0.0\ndefine_size = 16, p16.png\n")
            .unwrap();
        zip.start_file("p16.png", options).unwrap();
        zip.write_all(&png_of(2, 2, &red(2))).unwrap();
        zip.finish().unwrap();
        let theme = Theme::load(&dir.join("T")).expect("theme");
        let loaded = theme.shape("pointer", 32).expect("resampled");
        assert_eq!((loaded.width, loaded.height), (32, 32));
        assert_eq!(loaded.frames[0].pixels.len(), 32 * 32 * 4);
        assert_eq!(loaded.hotspot, (0, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unpacked_shape_is_read_too() {
        let dir = std::env::temp_dir().join(format!(
            "viewport-hyprcursor-dir-{}",
            std::process::id() as u64 * 1000 + 13
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("T/hyprcursors/hand")).unwrap();
        std::fs::write(
            dir.join("T/manifest.hl"),
            "name = T\ncursors_directory = hyprcursors\n",
        )
        .unwrap();
        // `none` is the no-resampling algorithm: a size the theme does not
        // ship is answered with the nearest one at its own resolution.
        std::fs::write(
            dir.join("T/hyprcursors/hand/meta.hl"),
            "resize_algorithm = none\nhotspot_x = 0.5\nhotspot_y = 0.5\ndefine_size = 20, hand.png\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("T/hyprcursors/hand/hand.png"),
            png_of(2, 2, &red(2)),
        )
        .unwrap();
        let theme = Theme::load(&dir.join("T")).expect("theme");
        let loaded = theme.shape("hand", 20).expect("hand");
        assert_eq!((loaded.width, loaded.height), (20, 20));
        assert_eq!(loaded.hotspot, (10, 10));

        let nearest = theme.shape("hand", 26).expect("nearest at none");
        assert_eq!((nearest.width, nearest.height), (20, 20));
        assert_eq!(nearest.hotspot, (10, 10));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovery_uses_the_directory_name_and_the_manifest_name() {
        let dir = std::env::temp_dir().join(format!(
            "viewport-hyprcursor-find-{}",
            std::process::id() as u64 * 1000 + 17
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("icons")).unwrap();
        theme_in(&dir.join("icons"));

        assert!(
            find(&[dir.join("icons")], "Test Theme").is_some(),
            "manifest name"
        );
        assert!(
            find(&[dir.join("icons")], "Test-Theme").is_some(),
            "directory name"
        );
        assert!(
            find(&[dir.join("icons")], "nonexistent").is_none(),
            "a configured name is not swapped for an unrelated theme"
        );
        assert!(
            find(&[dir.join("icons")], "").is_some(),
            "empty asks for any"
        );
        assert!(find(&[dir.join("icons")], "default").is_some());
        assert!(find(&[dir.join("nope")], "").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_toml_manifest_and_meta_read_the_same() {
        let dir = std::env::temp_dir().join(format!(
            "viewport-hyprcursor-toml-{}",
            std::process::id() as u64 * 1000 + 19
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("T/hyprcursors/text")).unwrap();
        std::fs::write(
            dir.join("T/manifest.toml"),
            "[General]\nname = 'Toml Theme'\ncursors_directory = 'hyprcursors'\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("T/hyprcursors/text/meta.toml"),
            "[General]\nresize_algorithm = 'none'\nhotspot_x = 0.25\nhotspot_y = 0.25\ndefine_override = 'xterm;ibeam'\ndefine_size = '18, t.png'\n",
        )
        .unwrap();
        std::fs::write(dir.join("T/hyprcursors/text/t.png"), png_of(2, 2, &red(2))).unwrap();
        let theme = Theme::load(&dir.join("T")).expect("toml theme");
        assert_eq!(theme.name(), "Toml Theme");
        let loaded = theme.shape("xterm", 18).expect("alias from toml");
        assert_eq!((loaded.width, loaded.height), (18, 18));
        assert_eq!(loaded.hotspot, (5, 5), "0.25 of 18 is 4.5, rounded up");
        assert!(theme.shape("ibeam", 18).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hostile_theme_is_skipped_rather_than_believed() {
        assert!(parse_define_size("24, ../escape.png").is_none());
        assert!(parse_define_size("24, /etc/passwd").is_none());
        assert!(parse_define_size("0, zero.png").is_none());
        assert!(parse_define_size("24").is_none());
        // An SVG shape is reported as absent: the compositor has no SVG
        // renderer, so the XCursor theme answers that name instead.
        let dir = std::env::temp_dir().join(format!(
            "viewport-hyprcursor-svg-{}",
            std::process::id() as u64 * 1000 + 23
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("T/hyprcursors")).unwrap();
        std::fs::write(
            dir.join("T/manifest.hl"),
            "name = T\ncursors_directory = hyprcursors\n",
        )
        .unwrap();
        let file = std::fs::File::create(dir.join("T/hyprcursors/left_ptr.hlc")).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("meta.hl", options).unwrap();
        zip.write_all(b"define_size = 0, cursor.svg\n").unwrap();
        zip.finish().unwrap();
        let theme = Theme::load(&dir.join("T")).expect("theme");
        assert!(theme.shape("left_ptr", 24).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_byte_order_of_the_drawn_buffer_is_premultiplied_bgra() {
        // Straight RGBA, twice: one opaque red, one half-transparent blue.
        let pixels = [255, 0, 0, 255, 0, 0, 255, 128];
        let converted = super::super::cursor::premultiplied_bgra(&pixels);
        assert_eq!(converted[0], 0, "blue");
        assert_eq!(converted[1], 0, "green");
        assert_eq!(converted[2], 255, "red");
        assert_eq!(converted[3], 255, "alpha");
        // Blue at alpha 128 premultiplies to half.
        assert_eq!(converted[4], 128);
        assert_eq!(converted[5], 0);
        assert_eq!(converted[6], 0);
        assert_eq!(converted[7], 128);
    }

    /// A theme's `define_size` names the resolution its artwork ships at, and
    /// anything past what a cursor can be drawn at is a lie about the data
    /// that follows.
    ///
    /// `parse_define_size` used to accept any `u32`, and with
    /// `resize_algorithm` unset — the default for a toml meta, which is the
    /// shape most themes ship — `shape` handed the unclamped value straight
    /// to `resample`, whose `vec![0u8; tw * th * 4]` multiplied it out in
    /// `u32`: `32768` overflowed (a panic in debug, a zero-length vector and
    /// an immediate slice panic in release), and `32767` asked for four
    /// gibibytes per frame. The side is refused at the parse, which is where
    /// the file is still text and the theme is still one line.
    #[test]
    fn a_define_size_past_the_drawn_bound_is_refused() {
        assert!(parse_define_size("24, p.png").is_some());
        assert!(parse_define_size("1024, p.png").is_some(), "the cap itself");
        assert!(
            parse_define_size("1025, p.png").is_none(),
            "one past the cap is not a size this loads"
        );
        assert!(parse_define_size("65536, p.png").is_none());
        assert!(
            parse_define_size("4294967295, p.png").is_none(),
            "and the extreme is not special"
        );
    }

    /// A resample to a sane size cannot overflow: the products are widened
    /// before they are multiplied, so even a theme that ships a 2x2 png
    /// against a 1024 target costs what a 1024 cursor costs.
    #[test]
    fn a_resample_at_the_cap_is_a_cursor_worth_of_pixels() {
        let four = vec![255u8; 4 * 4];
        let out = resample(&four, 2, 2, MAX_SIDE, Resize::None);
        assert_eq!(out.len(), MAX_SIDE as usize * MAX_SIDE as usize * 4);
    }
}
