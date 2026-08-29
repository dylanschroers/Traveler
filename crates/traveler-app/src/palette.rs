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
use traveler_scan::{ExtId, FileIndex, Tree};

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
    for id in index.nodes() {
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

/// Colour separation, checked rather than asserted.
///
/// PLAN.md §5 established the three categorical hues by running every pair through a validator on
/// the dark surface, under normal vision and under simulated colour vision deficiency, and
/// PLAN-DYNAMIC §10 says the diverging diff ramp has to clear the same bars. A ramp a deuteranope
/// reads as flat is the exact failure the categorical work already found once, and it is not a
/// failure that shows up in a screenshot.
///
/// Test-only: it exists to hold the palette to its floors, not to run at render time.
#[cfg(test)]
pub mod separation {
    /// The surface the palette was validated against.
    pub const SURFACE: [u8; 3] = [0x1a, 0x1a, 0x19];

    fn to_linear(c: u8) -> f64 {
        let s = c as f64 / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }

    fn to_srgb(v: f64) -> f64 {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.003_130_8 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        }
    }

    fn linear(rgb: [u8; 3]) -> [f64; 3] {
        [to_linear(rgb[0]), to_linear(rgb[1]), to_linear(rgb[2])]
    }

    /// WCAG relative luminance, for the contrast floor.
    fn luminance(rgb: [u8; 3]) -> f64 {
        let [r, g, b] = linear(rgb);
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    pub fn contrast(a: [u8; 3], b: [u8; 3]) -> f64 {
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    /// Viénot, Brettel and Mollon's dichromat simulation: project onto the plane a deuteranope
    /// cannot leave, which collapses colour onto the blue-yellow axis.
    pub fn deuteranope(rgb: [u8; 3]) -> [u8; 3] {
        let [r, g, b] = linear(rgb);
        let l = 17.8824 * r + 43.5161 * g + 4.11935 * b;
        let s = 0.029_956_6 * r + 0.184_309 * g + 1.46709 * b;

        // The M cone response is not read: a deuteranope does not have one, and the simulation
        // reconstructs it from L and S, which is the collapse itself.
        let (l2, m2, s2) = (l, 0.494207 * l + 1.24827 * s, s);

        let r2 = 0.080_944_447_9 * l2 - 0.130_504_409 * m2 + 0.116_721_066 * s2;
        let g2 = -0.010_248_533_5 * l2 + 0.054_019_326_6 * m2 - 0.113_614_708 * s2;
        let b2 = -0.000_365_296_938 * l2 - 0.004_121_614_69 * m2 + 0.693_511_405 * s2;
        [
            (to_srgb(r2) * 255.0).round() as u8,
            (to_srgb(g2) * 255.0).round() as u8,
            (to_srgb(b2) * 255.0).round() as u8,
        ]
    }

    fn lab(rgb: [u8; 3]) -> [f64; 3] {
        let [r, g, b] = linear(rgb);
        // sRGB to CIEXYZ, D65.
        let x = (0.412_456 * r + 0.357_576 * g + 0.180_438 * b) / 0.950_489;
        let y = 0.212_673 * r + 0.715_152 * g + 0.072_175 * b;
        let z = (0.019_334 * r + 0.119_192 * g + 0.950_304 * b) / 1.088_840;
        let f = |t: f64| {
            if t > 216.0 / 24389.0 {
                t.cbrt()
            } else {
                (841.0 / 108.0) * t + 4.0 / 29.0
            }
        };
        let (fx, fy, fz) = (f(x), f(y), f(z));
        [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
    }

    /// CIEDE2000, which is what a perceptual difference has to be measured in: CIE76 over-reports
    /// differences in the blues, and the blues are exactly where this palette lives.
    pub fn delta_e(a: [u8; 3], b: [u8; 3]) -> f64 {
        let ([l1, a1, b1], [l2, a2, b2]) = (lab(a), lab(b));
        let (c1, c2) = ((a1 * a1 + b1 * b1).sqrt(), (a2 * a2 + b2 * b2).sqrt());
        let cbar = (c1 + c2) / 2.0;
        let g = 0.5 * (1.0 - (cbar.powi(7) / (cbar.powi(7) + 25f64.powi(7))).sqrt());
        let (ap1, ap2) = (a1 * (1.0 + g), a2 * (1.0 + g));
        let (cp1, cp2) = ((ap1 * ap1 + b1 * b1).sqrt(), (ap2 * ap2 + b2 * b2).sqrt());
        let hp = |ap: f64, bp: f64| {
            if ap == 0.0 && bp == 0.0 {
                0.0
            } else {
                bp.atan2(ap).to_degrees().rem_euclid(360.0)
            }
        };
        let (hp1, hp2) = (hp(ap1, b1), hp(ap2, b2));

        let dl = l2 - l1;
        let dc = cp2 - cp1;
        let dh = if cp1 * cp2 == 0.0 {
            0.0
        } else {
            let d = hp2 - hp1;
            if d.abs() <= 180.0 {
                d
            } else if d > 180.0 {
                d - 360.0
            } else {
                d + 360.0
            }
        };
        let dhp = 2.0 * (cp1 * cp2).sqrt() * (dh.to_radians() / 2.0).sin();

        let lbar = (l1 + l2) / 2.0;
        let cbar = (cp1 + cp2) / 2.0;
        let hbar = if cp1 * cp2 == 0.0 {
            hp1 + hp2
        } else if (hp1 - hp2).abs() <= 180.0 {
            (hp1 + hp2) / 2.0
        } else if hp1 + hp2 < 360.0 {
            (hp1 + hp2 + 360.0) / 2.0
        } else {
            (hp1 + hp2 - 360.0) / 2.0
        };

        let t = 1.0 - 0.17 * (hbar - 30.0).to_radians().cos()
            + 0.24 * (2.0 * hbar).to_radians().cos()
            + 0.32 * (3.0 * hbar + 6.0).to_radians().cos()
            - 0.20 * (4.0 * hbar - 63.0).to_radians().cos();
        let sl = 1.0 + (0.015 * (lbar - 50.0).powi(2)) / (20.0 + (lbar - 50.0).powi(2)).sqrt();
        let sc = 1.0 + 0.045 * cbar;
        let sh = 1.0 + 0.015 * cbar * t;
        let rt = -2.0
            * (cbar.powi(7) / (cbar.powi(7) + 25f64.powi(7))).sqrt()
            * (60.0 * (-((hbar - 275.0) / 25.0).powi(2)).exp())
                .to_radians()
                .sin();

        ((dl / sl).powi(2) + (dc / sc).powi(2) + (dhp / sh).powi(2) + rt * (dc / sc) * (dhp / sh))
            .sqrt()
    }
}

#[cfg(test)]
mod tests {
    use super::separation::*;

    /// The diverging ramp §10 puts in place of the categorical hues: added one way, removed the
    /// other, unchanged at the neutral midpoint. It reuses two of the three already-validated
    /// hues, and deliberately the two on the blue-yellow axis, which is the axis deuteranopia
    /// preserves.
    const ADDED: [u8; 3] = [0x39, 0x87, 0xe5];
    const UNCHANGED: [u8; 3] = [0x6b, 0x6f, 0x76];
    const REMOVED: [u8; 3] = [0xd9, 0x59, 0x26];

    /// The floors PLAN.md §5 recorded for the categorical set, which §10 says the ramp must clear
    /// too rather than be exempted from.
    const CVD_FLOOR: f64 = 9.4;
    const NORMAL_FLOOR: f64 = 20.9;
    const CONTRAST_FLOOR: f64 = 3.0;

    fn ramp() -> [([u8; 3], &'static str); 3] {
        [
            (ADDED, "added"),
            (UNCHANGED, "unchanged"),
            (REMOVED, "removed"),
        ]
    }

    #[test]
    fn the_diverging_ramp_separates_under_normal_vision() {
        let ramp = ramp();
        for (i, (a, an)) in ramp.iter().enumerate() {
            for (b, bn) in &ramp[i + 1..] {
                let d = delta_e(*a, *b);
                assert!(d >= NORMAL_FLOOR, "{an} against {bn} is only {d:.1}");
            }
        }
    }

    /// The one that matters. Deuteranopia collapses colour onto a blue-yellow axis, and a ramp
    /// whose ends land on the same point of it is a ramp that reads as flat.
    #[test]
    fn the_diverging_ramp_separates_under_deuteranopia() {
        let ramp = ramp();
        for (i, (a, an)) in ramp.iter().enumerate() {
            for (b, bn) in &ramp[i + 1..] {
                let d = delta_e(deuteranope(*a), deuteranope(*b));
                assert!(
                    d >= CVD_FLOOR,
                    "{an} against {bn} is only {d:.1} to a deuteranope"
                );
            }
        }
    }

    #[test]
    fn every_step_of_the_ramp_stands_off_the_surface() {
        for (colour, name) in ramp() {
            let c = contrast(colour, SURFACE);
            assert!(
                c >= CONTRAST_FLOOR,
                "{name} is only {c:.2}:1 on the surface"
            );
        }
    }

    /// The validator has to actually be able to fail, or the three tests above prove nothing.
    #[test]
    fn the_validator_rejects_a_ramp_a_deuteranope_cannot_read() {
        // Red to green through grey is the textbook mistake, and the textbook reason for it.
        let (red, green) = ([0xd0, 0x30, 0x30], [0x30, 0xa0, 0x30]);
        assert!(
            delta_e(red, green) >= NORMAL_FLOOR,
            "red and green should look fine to normal vision"
        );
        assert!(
            delta_e(deuteranope(red), deuteranope(green)) < CVD_FLOOR,
            "the simulation is not collapsing red and green"
        );
    }
}

#[cfg(test)]
mod measured {
    use super::separation::*;

    /// Prints the numbers §18 asks to be recorded. `cargo test -p traveler-app -- --ignored
    /// --nocapture measured` re-derives them.
    #[test]
    #[ignore]
    fn report() {
        let ramp = [
            ([0x39u8, 0x87, 0xe5], "added"),
            ([0x6b, 0x6f, 0x76], "unchanged"),
            ([0xd9, 0x59, 0x26], "removed"),
        ];
        for (i, (a, an)) in ramp.iter().enumerate() {
            for (b, bn) in &ramp[i + 1..] {
                println!(
                    "{an:>10} / {bn:<10} normal {:5.1}   deutan {:5.1}",
                    delta_e(*a, *b),
                    delta_e(deuteranope(*a), deuteranope(*b))
                );
            }
        }
        for (c, n) in ramp {
            println!("{n:>10} contrast on surface {:.2}:1", contrast(c, SURFACE));
        }
    }
}
