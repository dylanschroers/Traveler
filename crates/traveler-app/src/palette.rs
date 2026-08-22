//! Color encoding.
//!
//! The original plan said "top 12 extensions by bytes, each its own color". That does not
//! survive validation. A treemap is an all-pairs form: any two tiles can end up adjacent, so
//! every pair of colors has to be separable, not just neighbours in a legend order. Running the
//! palette validator over candidate sets (the reference eight, Okabe-Ito, Tol bright, and
//! lightness-snapped variants of each) found that **no fourth hue** clears the colorblind and
//! normal-vision floors against a validated first three on the dark surface. Deuteranopia
//! collapses color to a blue-yellow axis, and pinning lightness to the dark band removes the
//! other degree of freedom.
//!
//! So hue carries a *category*, not an extension, and everything else folds into a neutral, which
//! is what the method prescribes past the limit. Exact extensions are still available: the HUD
//! names the type of whatever is under the crosshair, and the legend lists the biggest extensions
//! by bytes under their category. Identity is never color alone.
//!
//! Validated on surface #1a1a19, all-pairs, dark mode:
//!   worst CVD ΔE 9.4 (deutan), worst normal-vision ΔE 20.9, all three ≥ 3:1 contrast.

use bevy::prelude::*;
use traveler_scan::{ExtId, FileIndex};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    /// Source, config, markup, docs, logs.
    Code = 0,
    /// Images, audio, video, fonts.
    Media = 1,
    /// Executables, libraries, object files, archives, databases.
    Binary = 2,
    /// Everything else, including files with no extension.
    Other = 3,
    /// Directory slabs. Not a categorical slot, a surface.
    Directory = 4,
}

pub const CATEGORY_COUNT: usize = 5;

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Code => "code & text",
            Category::Media => "media",
            Category::Binary => "binaries & archives",
            Category::Other => "other",
            Category::Directory => "folders",
        }
    }

    /// Validated categorical slots 1-3, plus two neutrals that are deliberately not hues.
    pub fn color(self) -> Color {
        match self {
            Category::Code => Color::srgb_u8(0x39, 0x87, 0xe5),
            Category::Media => Color::srgb_u8(0xd9, 0x59, 0x26),
            Category::Binary => Color::srgb_u8(0x19, 0x9e, 0x70),
            Category::Other => Color::srgb_u8(0x6b, 0x6f, 0x76),
            Category::Directory => Color::srgb_u8(0x2c, 0x31, 0x38),
        }
    }
}

const CODE: &[&str] = &[
    "rs",
    "ts",
    "tsx",
    "js",
    "jsx",
    "mjs",
    "cjs",
    "py",
    "c",
    "h",
    "cc",
    "cpp",
    "hpp",
    "cs",
    "java",
    "kt",
    "go",
    "rb",
    "php",
    "swift",
    "lua",
    "sh",
    "bash",
    "ps1",
    "bat",
    "cmd",
    "sql",
    "html",
    "htm",
    "css",
    "scss",
    "sass",
    "vue",
    "svelte",
    "json",
    "yaml",
    "yml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "xml",
    "md",
    "txt",
    "rst",
    "csv",
    "tsv",
    "log",
    "lock",
    "gitignore",
    "editorconfig",
    "d",
    "map",
    "patch",
    "diff",
    "ipynb",
    "r",
    "jl",
    "scala",
    "clj",
    "ex",
    "exs",
    "erl",
    "hs",
    "ml",
    "pl",
    "vim",
    "el",
];

const MEDIA: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "ico", "svg", "psd", "ai", "eps",
    "raw", "cr2", "nef", "heic", "avif", "mp3", "wav", "flac", "aac", "ogg", "oga", "m4a", "wma",
    "opus", "mid", "midi", "mp4", "mkv", "mov", "avi", "wmv", "webm", "flv", "m4v", "mpg", "mpeg",
    "ttf", "otf", "woff", "woff2", "eot", "fnt", "blend", "fbx", "obj3d", "gltf", "glb", "dds",
    "ktx", "exr", "hdr",
];

const BINARY: &[&str] = &[
    "exe", "dll", "so", "dylib", "lib", "a", "o", "obj", "pdb", "rlib", "rmeta", "bin", "dat",
    "msi", "cab", "msu", "appx", "deb", "rpm", "dmg", "pkg", "zip", "gz", "bz2", "xz", "zst",
    "tar", "7z", "rar", "iso", "img", "vhd", "vhdx", "wim", "esd", "pack", "idx", "db", "sqlite",
    "sqlite3", "mdb", "wasm", "jar", "war", "class", "pyc", "pyd", "whl", "nupkg", "node", "sys",
    "drv", "ocx", "winmd", "nls", "mui", "mun", "efi",
];

/// Category per interned extension id, built once after a scan.
pub struct CategoryTable {
    by_ext: Vec<Category>,
}

impl CategoryTable {
    pub fn build(index: &FileIndex) -> Self {
        let mut by_ext = Vec::with_capacity(index.ext_count());
        for i in 0..index.ext_count() {
            let ext = index.ext_str(ExtId(i as u32));
            by_ext.push(classify(ext));
        }
        Self { by_ext }
    }

    #[inline]
    pub fn of(&self, ext: ExtId) -> Category {
        self.by_ext
            .get(ext.0 as usize)
            .copied()
            .unwrap_or(Category::Other)
    }
}

fn classify(ext: &str) -> Category {
    if ext.is_empty() {
        return Category::Other;
    }
    if CODE.contains(&ext) {
        Category::Code
    } else if MEDIA.contains(&ext) {
        Category::Media
    } else if BINARY.contains(&ext) {
        Category::Binary
    } else {
        Category::Other
    }
}

/// The biggest extensions by total bytes, with the category each one falls into. Drives the
/// legend, which is what keeps identity from being color alone.
pub fn top_extensions(
    index: &FileIndex,
    table: &CategoryTable,
    n: usize,
) -> Vec<(String, u64, Category)> {
    let mut bytes = vec![0u64; index.ext_count()];
    for id in index.iter() {
        if !index.flags(id).is_dir() {
            bytes[index.ext(id).0 as usize] += index.size(id);
        }
    }
    let mut ranked: Vec<(usize, u64)> = bytes.into_iter().enumerate().collect();
    ranked.sort_unstable_by_key(|r| std::cmp::Reverse(r.1));
    ranked
        .into_iter()
        .filter(|(_, b)| *b > 0)
        .take(n)
        .map(|(e, b)| {
            let name = index.ext_str(ExtId(e as u32));
            let name = if name.is_empty() {
                "(none)".to_string()
            } else {
                name.to_string()
            };
            (name, b, table.of(ExtId(e as u32)))
        })
        .collect()
}
