use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::Hash;
use std::hash::Hasher;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use sprite_desktop_protocol::pipe::CursorShape;
use tracing::debug;
use tracing::warn;
use xcursor::parser::parse_xcursor;

use super::CursorSize;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(super) struct ImageKey {
    size: CursorSize,
    pixels: u64,
}

impl ImageKey {
    pub(super) fn new(size: CursorSize, pixels: &[u8]) -> Self {
        let mut hasher = DefaultHasher::new();
        pixels.hash(&mut hasher);
        Self {
            size,
            pixels: hasher.finish(),
        }
    }
}

pub(crate) struct ShapeTable(HashMap<ImageKey, CursorShape>);

impl ShapeTable {
    pub(crate) fn load(theme: &str, search_paths: &[PathBuf]) -> Result<Self> {
        let searched = search_paths
            .iter()
            .map(|path| path.join(theme).display().to_string())
            .collect::<Vec<_>>()
            .join(":");
        let directories = theme_directories(theme, search_paths).with_context(|| {
            format!("inspect cursor theme '{theme}'; searched paths: {searched}")
        })?;
        if directories.is_empty() {
            bail!("cursor theme '{theme}' was not found; searched paths: {searched}");
        }

        let mut table: HashMap<ImageKey, CursorShape> = HashMap::new();
        for shape in CursorShape::ALL {
            let name = shape.css_name();
            let fallback = alias(shape);
            let path = match find_icon(theme, name, search_paths)? {
                Some(path) => Some(path),
                None if fallback != name => find_icon(theme, fallback, search_paths)?,
                None => None,
            };
            let Some(path) = path else {
                warn!(
                    theme,
                    shape = name,
                    alias = fallback,
                    "cursor shape file is missing"
                );
                continue;
            };
            let bytes = fs::read(&path)
                .with_context(|| format!("read cursor shape file {}", path.display()))?;
            let images = parse_xcursor(&bytes)
                .with_context(|| format!("parse cursor shape file {}", path.display()))?;
            for image in images {
                let size = match CursorSize::new(image.width, image.height) {
                    Ok(size) => size,
                    Err(error) => {
                        debug!(
                            theme,
                            shape = name,
                            path = %path.display(),
                            width = image.width,
                            height = image.height,
                            %error,
                            "cursor theme image has unsupported dimensions"
                        );
                        continue;
                    }
                };
                // `pixels_rgba` is xcursor's untouched file-order byte vector. Xcursor stores
                // premultiplied ARGB words little-endian, so these bytes are B, G, R, A just
                // like the compositor's wl_shm ARGB8888 mapping on the Sprite.
                let key = ImageKey::new(size, &image.pixels_rgba);
                let Some(first) = table.get(&key).copied() else {
                    table.insert(key, shape);
                    continue;
                };
                if first == shape {
                    continue;
                }
                let shared = shared_image_shape(first);
                if shared == shared_image_shape(shape) {
                    table.insert(key, shared);
                } else {
                    debug!(
                        theme,
                        shape = name,
                        first = first.css_name(),
                        path = %path.display(),
                        width = image.width,
                        height = image.height,
                        "unrelated cursor shapes share an image; keeping the first"
                    );
                }
            }
        }

        if table.is_empty() {
            bail!(
                "cursor theme '{theme}' contains no usable cursor images; searched paths: {searched}"
            );
        }
        Ok(Self(table))
    }

    pub(super) fn get(&self, key: &ImageKey) -> Option<CursorShape> {
        self.0.get(key).copied()
    }
}

fn find_icon(theme: &str, name: &str, search_paths: &[PathBuf]) -> Result<Option<PathBuf>> {
    let mut current = theme.to_owned();
    let mut visited = HashSet::new();
    while visited.insert(current.clone()) {
        let directories = theme_directories(&current, search_paths)?;
        for directory in &directories {
            let path = directory.join("cursors").join(name);
            match fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() => return Ok(Some(path)),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("inspect cursor shape file {}", path.display()));
                }
            }
        }

        let mut inherited = None;
        for directory in directories {
            if let Some(parent) = read_inherited_theme(&directory.join("index.theme"))? {
                inherited = Some(parent);
                break;
            }
        }
        let Some(parent) = inherited else {
            return Ok(None);
        };
        current = parent;
    }
    Ok(None)
}

fn theme_directories(theme: &str, search_paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut directories = Vec::new();
    for search_path in search_paths {
        let directory = search_path.join(theme);
        match fs::metadata(&directory) {
            Ok(metadata) if metadata.is_dir() => directories.push(directory),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect cursor theme path {}", directory.display()));
            }
        }
    }
    Ok(directories)
}

fn read_inherited_theme(path: &Path) -> Result<Option<String>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read cursor theme index {}", path.display()));
        }
    };
    Ok(contents.lines().find_map(|line| {
        let (key, values) = line.trim().split_once('=')?;
        if key.trim() != "Inherits" {
            return None;
        }
        values
            .split(|character: char| {
                character.is_whitespace() || character == ',' || character == ';'
            })
            .find(|value| !value.is_empty())
            .map(str::to_owned)
    }))
}

/// Themes such as breeze draw one double-headed arrow for a whole resize axis and link the
/// single-direction names to it. Those pixels only say which axis the cursor resizes along, so
/// when two shapes on one axis share an image the bidirectional shape is the honest name for it.
const fn shared_image_shape(shape: CursorShape) -> CursorShape {
    match shape {
        CursorShape::NeResize | CursorShape::SwResize | CursorShape::NeswResize => {
            CursorShape::NeswResize
        }
        CursorShape::NwResize | CursorShape::SeResize | CursorShape::NwseResize => {
            CursorShape::NwseResize
        }
        CursorShape::EResize | CursorShape::WResize | CursorShape::EwResize => {
            CursorShape::EwResize
        }
        CursorShape::NResize | CursorShape::SResize | CursorShape::NsResize => {
            CursorShape::NsResize
        }
        CursorShape::Default
        | CursorShape::ContextMenu
        | CursorShape::Help
        | CursorShape::Pointer
        | CursorShape::Progress
        | CursorShape::Wait
        | CursorShape::Cell
        | CursorShape::Crosshair
        | CursorShape::Text
        | CursorShape::VerticalText
        | CursorShape::Alias
        | CursorShape::Copy
        | CursorShape::Move
        | CursorShape::NoDrop
        | CursorShape::NotAllowed
        | CursorShape::Grab
        | CursorShape::Grabbing
        | CursorShape::ColResize
        | CursorShape::RowResize
        | CursorShape::AllScroll
        | CursorShape::ZoomIn
        | CursorShape::ZoomOut
        | CursorShape::DndAsk
        | CursorShape::AllResize => shape,
    }
}

const fn alias(shape: CursorShape) -> &'static str {
    match shape {
        CursorShape::Default | CursorShape::ContextMenu => "left_ptr",
        CursorShape::Help => "question_arrow",
        CursorShape::Pointer => "hand2",
        CursorShape::Progress => "left_ptr_watch",
        CursorShape::Wait => "watch",
        CursorShape::Cell => "plus",
        CursorShape::Crosshair => "cross",
        CursorShape::Text | CursorShape::VerticalText => "xterm",
        CursorShape::Alias => "link",
        CursorShape::Copy => "copy",
        CursorShape::Move | CursorShape::AllScroll | CursorShape::AllResize => "fleur",
        CursorShape::NoDrop => "dnd-no-drop",
        CursorShape::NotAllowed => "crossed_circle",
        CursorShape::Grab => "openhand",
        CursorShape::Grabbing => "closedhand",
        CursorShape::EResize => "right_side",
        CursorShape::NResize => "top_side",
        CursorShape::NeResize => "top_right_corner",
        CursorShape::NwResize => "top_left_corner",
        CursorShape::SResize => "bottom_side",
        CursorShape::SeResize => "bottom_right_corner",
        CursorShape::SwResize => "bottom_left_corner",
        CursorShape::WResize => "left_side",
        CursorShape::EwResize | CursorShape::ColResize => "sb_h_double_arrow",
        CursorShape::NsResize | CursorShape::RowResize => "sb_v_double_arrow",
        CursorShape::NeswResize => "fd_double_arrow",
        CursorShape::NwseResize => "bd_double_arrow",
        CursorShape::ZoomIn => "zoom-in",
        CursorShape::ZoomOut => "zoom-out",
        CursorShape::DndAsk => "dnd-ask",
    }
}

#[cfg(test)]
mod tests {
    use std::process;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "sprite-desktop-cursor-shapes-{}-{id}",
                process::id()
            ));
            fs::create_dir(&path).expect("test theme root should be created");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("test theme root should be removed");
        }
    }

    fn write_cursor(root: &Path, theme: &str, name: &str, width: u32, height: u32, pixels: &[u8]) {
        let size = CursorSize::new(width, height).expect("test cursor size should be valid");
        assert_eq!(pixels.len(), size.byte_count());
        let mut bytes = Vec::with_capacity(64 + pixels.len());
        bytes.extend_from_slice(b"Xcur");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&0x0001_0000_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&0xfffd_0002_u32.to_le_bytes());
        bytes.extend_from_slice(&u32::from(size.width).to_le_bytes());
        bytes.extend_from_slice(&28_u32.to_le_bytes());
        bytes.extend_from_slice(&36_u32.to_le_bytes());
        bytes.extend_from_slice(&0xfffd_0002_u32.to_le_bytes());
        bytes.extend_from_slice(&u32::from(size.width).to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(pixels);

        let cursors = root.join(theme).join("cursors");
        fs::create_dir_all(&cursors).expect("test cursor directory should be created");
        fs::write(cursors.join(name), bytes).expect("test cursor should be written");
    }

    #[test]
    fn load_maps_xcursor_pixels_to_their_shape() {
        let root = TestDirectory::new();
        let pixels = [1, 2, 3, 4];
        write_cursor(root.path(), "direct", "pointer", 1, 1, &pixels);

        let table = ShapeTable::load("direct", &[root.path().to_owned()])
            .expect("direct theme should load");
        let size = CursorSize::new(1, 1).expect("test cursor size should be valid");

        assert_eq!(
            table.get(&ImageKey::new(size, &pixels)),
            Some(CursorShape::Pointer)
        );
    }

    #[test]
    fn load_follows_the_first_inherited_theme() {
        let root = TestDirectory::new();
        let pixels = [4, 3, 2, 1];
        write_cursor(root.path(), "parent", "pointer", 1, 1, &pixels);
        let child = root.path().join("child");
        fs::create_dir(&child).expect("child theme should be created");
        fs::write(
            child.join("index.theme"),
            "[Icon Theme]\nInherits=parent,ignored\n",
        )
        .expect("child theme index should be written");

        let table = ShapeTable::load("child", &[root.path().to_owned()])
            .expect("inherited theme should load");
        let size = CursorSize::new(1, 1).expect("test cursor size should be valid");

        assert_eq!(
            table.get(&ImageKey::new(size, &pixels)),
            Some(CursorShape::Pointer)
        );
    }

    #[test]
    fn one_image_for_a_resize_axis_maps_to_the_bidirectional_shape() {
        let root = TestDirectory::new();
        let diagonal = [9, 9, 9, 9];
        write_cursor(root.path(), "axis", "ne-resize", 1, 1, &diagonal);
        write_cursor(root.path(), "axis", "sw-resize", 1, 1, &diagonal);
        let horizontal = [8, 8, 8, 8];
        write_cursor(root.path(), "axis", "e-resize", 1, 1, &horizontal);
        write_cursor(root.path(), "axis", "w-resize", 1, 1, &horizontal);
        write_cursor(root.path(), "axis", "ew-resize", 1, 1, &horizontal);
        let unrelated = [7, 7, 7, 7];
        write_cursor(root.path(), "axis", "pointer", 1, 1, &unrelated);
        write_cursor(root.path(), "axis", "n-resize", 1, 1, &unrelated);

        let table =
            ShapeTable::load("axis", &[root.path().to_owned()]).expect("axis theme should load");
        let size = CursorSize::new(1, 1).expect("test cursor size should be valid");

        assert_eq!(
            table.get(&ImageKey::new(size, &diagonal)),
            Some(CursorShape::NeswResize)
        );
        assert_eq!(
            table.get(&ImageKey::new(size, &horizontal)),
            Some(CursorShape::EwResize)
        );
        assert_eq!(
            table.get(&ImageKey::new(size, &unrelated)),
            Some(CursorShape::Pointer)
        );
    }

    #[test]
    fn missing_theme_error_names_theme_and_every_search_path() {
        let root = TestDirectory::new();
        let first = root.path().join("first");
        let second = root.path().join("second");
        let error = ShapeTable::load("absent", &[first.clone(), second.clone()])
            .err()
            .expect("missing theme should fail");
        let message = error.to_string();

        assert!(message.contains("absent"));
        assert!(message.contains(first.to_string_lossy().as_ref()));
        assert!(message.contains(second.to_string_lossy().as_ref()));
    }

    #[test]
    fn partial_theme_still_loads_the_shapes_it_has() {
        let root = TestDirectory::new();
        let pixels = [0, 0, 0, 0];
        write_cursor(root.path(), "partial", "default", 1, 1, &pixels);

        let table = ShapeTable::load("partial", &[root.path().to_owned()])
            .expect("partial theme should load");
        let size = CursorSize::new(1, 1).expect("test cursor size should be valid");

        assert_eq!(
            table.get(&ImageKey::new(size, &pixels)),
            Some(CursorShape::Default)
        );
    }
}
