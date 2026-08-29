//! Top-down SVG dump of a layout.
//!
//! Phase 2 has no renderer, so this is how a treemap gets looked at. It draws the ground-plane
//! footprint only: directories as outlines, files filled by extension. Property tests prove the
//! geometry is correct, this shows whether it is any good.

use traveler_core::{NodeId, Tree};

use crate::Placements;

pub struct SvgParams {
    /// Output width and height in pixels.
    pub px: f64,
    /// Skip anything smaller than this many square pixels. Below about one, the file is bigger
    /// than any viewer wants and the extra rects are invisible anyway.
    pub min_px_area: f64,
    /// Hard cap on emitted rects, largest first.
    pub max_rects: usize,
}

impl Default for SvgParams {
    fn default() -> Self {
        Self {
            px: 1400.0,
            min_px_area: 1.5,
            max_rects: 120_000,
        }
    }
}

/// `fill` supplies the colour of a content node. Containers are drawn as outlines and never ask
/// for one, so a caller with no palette can return anything.
pub fn render<T: Tree>(
    tree: &T,
    placements: &Placements,
    params: &SvgParams,
    fill: impl Fn(NodeId) -> [u8; 3],
) -> String {
    let span_x = (placements.max.x - placements.min.x) as f64;
    let span_z = (placements.max.z - placements.min.z) as f64;
    let span = span_x.max(span_z).max(1e-9);
    let scale = params.px / span;
    let (ox, oz) = (placements.min.x as f64, placements.min.z as f64);

    let mut out = String::with_capacity(1 << 20);
    out.push_str(&format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{px}" height="{px}" viewBox="0 0 {px} {px}">
<rect width="{px}" height="{px}" fill="#0b0d10"/>
"##,
        px = params.px
    ));

    // Ids run parent before child, so drawing in id order paints outer boxes first and nesting
    // comes out right with no sorting.
    let mut drawn = 0usize;
    let mut skipped = 0usize;
    for id in tree.nodes() {
        if !placements.is_visible(id) {
            continue;
        }
        let (x0, z0, x1, z1) = placements.footprint(id);
        let (px0, pz0) = ((x0 as f64 - ox) * scale, (z0 as f64 - oz) * scale);
        let (w, h) = ((x1 - x0) as f64 * scale, (z1 - z0) as f64 * scale);
        if w * h < params.min_px_area {
            skipped += 1;
            continue;
        }
        if drawn >= params.max_rects {
            skipped += 1;
            continue;
        }
        drawn += 1;

        if tree.is_group(id) {
            let depth = tree.depth(id).min(9) as f64;
            let alpha = 0.55 - depth * 0.05;
            out.push_str(&format!(
                r##"<rect x="{px0:.2}" y="{pz0:.2}" width="{w:.2}" height="{h:.2}" fill="none" stroke="#7f8ea3" stroke-opacity="{alpha:.2}" stroke-width="0.6"/>
"##
            ));
        } else {
            let [r, g, b] = fill(id);
            out.push_str(&format!(
                r##"<rect x="{px0:.2}" y="{pz0:.2}" width="{w:.2}" height="{h:.2}" fill="#{r:02x}{g:02x}{b:02x}"/>
"##
            ));
        }
    }

    out.push_str(&format!(
        "<!-- drawn {drawn}, skipped {skipped} below {} px^2 -->\n</svg>\n",
        params.min_px_area
    ));
    out
}

/// Aspect ratio stats over placed leaf footprints. This is the numeric read on whether the
/// treemap is actually squarified: values near 1 are square, large values are slivers.
pub fn aspect_stats<T: Tree>(tree: &T, placements: &Placements) -> AspectStats {
    let mut ratios: Vec<f32> = Vec::new();
    for id in tree.nodes() {
        if !placements.is_visible(id) || tree.is_group(id) {
            continue;
        }
        let s = placements.size[id.index()];
        if s.x <= 0.0 || s.z <= 0.0 {
            continue;
        }
        ratios.push((s.x / s.z).max(s.z / s.x));
    }
    if ratios.is_empty() {
        return AspectStats::default();
    }
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f32| ratios[((ratios.len() as f32 * q) as usize).min(ratios.len() - 1)];
    AspectStats {
        count: ratios.len(),
        median: at(0.5),
        p90: at(0.9),
        p99: at(0.99),
        worst: *ratios.last().unwrap(),
        slivers: ratios.iter().filter(|r| **r > 50.0).count(),
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AspectStats {
    pub count: usize,
    pub median: f32,
    pub p90: f32,
    pub p99: f32,
    pub worst: f32,
    /// Tiles longer than 50:1. These read as hairlines rather than blocks.
    pub slivers: usize,
}
