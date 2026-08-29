//! Squarified treemap (Bruls, Huizing, van Wijk 2000), extruded into a terraced city.
//!
//! Footprint area is proportional to bytes, exactly as WinDirStat. Height is a *separate* channel,
//! because a nested treemap needs a directory's weight to equal the sum of its children's, which
//! rules out putting a nonlinear function of size into the area.
//!
//! Elevation encodes nesting: a node's base sits at `depth * level_step`, so a directory is a slab
//! and its children rest on top of it. Walking into a folder is walking up a step, and the whole
//! tree reads as terraces from a distance.

use glam::Vec3;
use traveler_core::{NodeId, Tree};

use crate::{Layout, LayoutCtx, Placements, Rect, layout_weights};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeightMode {
    /// Every file the same thickness. Area is then the only size channel, which is the honest
    /// option: nothing is encoded twice.
    Uniform,
    /// Thickness scales with `log10(bytes)`. Redundant with area and heavily compressed, but it
    /// gives the city a skyline you can read from a distance.
    LogSize,
}

#[derive(Clone, Copy, Debug)]
pub struct CityParams {
    /// Side length of the root footprint in world units. The city always occupies this square
    /// regardless of how many bytes it holds, so the camera framing never depends on drive size.
    pub world_size: f64,
    /// Rise per depth level, and the thickness of a directory slab.
    pub level_step: f32,
    /// File thickness for `Uniform`, and the minimum for `LogSize`.
    pub file_thickness: f32,
    /// Thickness of the largest file under `LogSize`.
    pub max_file_thickness: f32,
    /// Wall around a folder's children, as a fraction of that folder's shorter side. Purely
    /// proportional: an absolute cap here is what made every large folder read as one solid mass,
    /// since a 0.35 unit wall around a 400 unit district is invisible.
    pub margin_ratio: f64,
    /// Street between sibling blocks, as a fraction of the parent's shorter side. Computed once
    /// per folder rather than per child, so the streets between districts run straight and share
    /// one width instead of varying with whatever they happen to separate.
    pub gap_ratio: f64,
    /// Both ratios are multiplied by this once per level of depth. Top-level folders get
    /// boulevards and deep folders get seams, which is what makes the tree legible from above.
    /// It also stops the spacing from compounding: without it every level would surrender the
    /// same fraction and a file ten levels down would keep a fraction of its true area.
    pub spacing_falloff: f64,
    /// Floor under both ratios, so adjacent files never fuse into one surface.
    pub min_spacing_ratio: f64,
    /// A single child is never inset by more than this fraction of its own shorter side, so a
    /// wide street cannot swallow the small tiles it runs past.
    pub max_child_inset: f64,
    /// Rects thinner than this in either axis are not placed, and neither is anything inside
    /// them. Without it, extreme size disparity inside one directory produces slivers thousands
    /// of times longer than they are wide, which are invisible but still cost geometry.
    pub min_side: f64,
    /// Floor on a leaf's layout weight, in bytes, so zero-byte files stay visible.
    pub min_leaf_weight: f64,
    pub height: HeightMode,
}

impl Default for CityParams {
    fn default() -> Self {
        Self {
            world_size: 1000.0,
            // Vertical exaggeration. The city is ~1000 units wide, so anything under a few tens
            // of units tall reads as a flat plain from the overview camera.
            level_step: 2.2,
            file_thickness: 1.0,
            max_file_thickness: 90.0,
            // Generous on purpose. The point of the view is telling one tree from another, and
            // packing tight enough to preserve every last square unit of area made the whole
            // scan read as a single fused slab.
            margin_ratio: 0.062,
            gap_ratio: 0.054,
            spacing_falloff: 0.72,
            min_spacing_ratio: 0.004,
            max_child_inset: 0.15,
            min_side: 0.01,
            min_leaf_weight: 4096.0,
            height: HeightMode::LogSize,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CityLayout {
    pub params: CityParams,
}

impl CityLayout {
    pub fn new(params: CityParams) -> Self {
        Self { params }
    }
}

impl Layout for CityLayout {
    fn name(&self) -> &'static str {
        "city"
    }

    fn place<T: Tree>(&self, ctx: &LayoutCtx<'_, T>, out: &mut Placements) {
        let index = ctx.tree;
        let p = &self.params;
        let n = index.len();
        out.reset(n);
        if n == 0 {
            return;
        }

        let weights = layout_weights(index, p.min_leaf_weight);
        // Files only. A directory's size is its subtree total, so including directories would
        // make this the root's total bytes and no file would ever reach the maximum thickness.
        let log_denom = {
            let max_file = index
                .nodes()
                .filter(|id| !index.is_group(*id))
                .map(|id| index.weight(id))
                .max()
                .unwrap_or(0);
            (1.0 + max_file as f64).log10().max(1.0)
        };

        let half = p.world_size * 0.5;
        let mut stack = vec![(
            index.root(),
            Rect {
                x: -half,
                z: -half,
                w: p.world_size,
                h: p.world_size,
            },
        )];

        let mut min = Vec3::splat(f32::INFINITY);
        let mut max = Vec3::splat(f32::NEG_INFINITY);
        // Reused across nodes so a million-node layout does not allocate per directory.
        let mut items: Vec<(NodeId, f64)> = Vec::new();
        let mut placed_rects: Vec<(NodeId, Rect)> = Vec::new();

        while let Some((id, rect)) = stack.pop() {
            if rect.is_degenerate() || rect.w < p.min_side || rect.h < p.min_side {
                continue;
            }

            let thickness = if index.is_group(id) {
                p.level_step
            } else {
                self.file_thickness(index.weight(id) as f64, log_denom)
            };
            let base = index.depth(id) as f32 * p.level_step;

            let center = Vec3::new(
                (rect.x + rect.w * 0.5) as f32,
                base + thickness * 0.5,
                (rect.z + rect.h * 0.5) as f32,
            );
            let size = Vec3::new(rect.w as f32, thickness, rect.h as f32);

            out.center[id.index()] = center;
            out.size[id.index()] = size;
            out.visible[id.index()] = true;
            out.placed += 1;
            min = min.min(center - size * 0.5);
            max = max.max(center + size * 0.5);

            if index.child_count(id) == 0 {
                continue;
            }

            // Children pack into the parent's rect minus its wall, which leaves the slab showing
            // as a border.
            let depth = index.depth(id);
            let inner = rect.inset(spacing_ratio(p, p.margin_ratio, depth) * rect.w.min(rect.h));
            if inner.is_degenerate() {
                continue;
            }

            items.clear();
            let mut total = 0.0;
            for child in index.child_ids(id) {
                let w = weights[child.index()];
                total += w;
                items.push((child, w));
            }
            if total <= 0.0 {
                continue;
            }

            // Largest first is what makes the greedy row packing produce square-ish tiles. Ties
            // break on id so the layout is deterministic.
            items.sort_unstable_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.0.cmp(&b.0))
            });

            // Weights become areas that exactly fill the inner rect.
            let scale = inner.area() / total;
            for it in items.iter_mut() {
                it.1 *= scale;
            }

            placed_rects.clear();
            squarify(&items, inner, &mut placed_rects);

            // One street width for the whole folder, so the gaps line up.
            let street = spacing_ratio(p, p.gap_ratio, depth) * inner.w.min(inner.h) * 0.5;
            for (child, r) in placed_rects.iter() {
                let inset = street.min(p.max_child_inset * r.w.min(r.h));
                stack.push((*child, r.inset(inset)));
            }
        }

        out.min = if out.placed > 0 { min } else { Vec3::ZERO };
        out.max = if out.placed > 0 { max } else { Vec3::ZERO };
    }
}

impl CityLayout {
    fn file_thickness(&self, size: f64, log_denom: f64) -> f32 {
        match self.params.height {
            HeightMode::Uniform => self.params.file_thickness,
            HeightMode::LogSize => {
                let t = ((1.0 + size).log10() / log_denom).clamp(0.0, 1.0) as f32;
                self.params.file_thickness
                    + t * (self.params.max_file_thickness - self.params.file_thickness)
            }
        }
    }
}

/// Spacing as a fraction of the parent's shorter side, shrinking one step per level of depth.
#[inline]
fn spacing_ratio(p: &CityParams, base: f64, depth: u16) -> f64 {
    if base <= 0.0 {
        return 0.0;
    }
    (base * p.spacing_falloff.powi(depth as i32)).max(p.min_spacing_ratio)
}

/// The squarify step. `items` must be sorted by area descending and sum to `rect.area()`.
///
/// Greedily grows a row along the shorter side for as long as doing so improves the worst aspect
/// ratio in that row, then lays the row down and recurses on what is left of the rect.
fn squarify(items: &[(NodeId, f64)], rect: Rect, out: &mut Vec<(NodeId, Rect)>) {
    let mut rect = rect;
    let mut i = 0;
    while i < items.len() {
        if rect.is_degenerate() {
            break;
        }
        let side = rect.w.min(rect.h);

        let mut end = i + 1;
        let mut sum = items[i].1;
        let mut best = worst_ratio(items[i].1, items[i].1, sum, side);
        while end < items.len() {
            let next_sum = sum + items[end].1;
            // Items are sorted descending, so the first in the row is the largest and the
            // candidate is the smallest.
            let candidate = worst_ratio(items[i].1, items[end].1, next_sum, side);
            if candidate > best {
                break;
            }
            best = candidate;
            sum = next_sum;
            end += 1;
        }

        rect = lay_row(&items[i..end], sum, rect, out);
        i = end;
    }
}

/// Worst aspect ratio in a row of total area `sum` laid along an edge of length `side`.
#[inline]
fn worst_ratio(max_area: f64, min_area: f64, sum: f64, side: f64) -> f64 {
    let s2 = sum * sum;
    let w2 = side * side;
    if s2 <= 0.0 || w2 <= 0.0 || min_area <= 0.0 {
        return f64::INFINITY;
    }
    (w2 * max_area / s2).max(s2 / (w2 * min_area))
}

/// Lays one row along the shorter side and returns the rect that is left over.
fn lay_row(items: &[(NodeId, f64)], sum: f64, rect: Rect, out: &mut Vec<(NodeId, Rect)>) -> Rect {
    let last = items.len() - 1;
    if rect.w >= rect.h {
        // Shorter side is z, so the row is a column down the left edge.
        let cw = (sum / rect.h).min(rect.w);
        let mut z = rect.z;
        for (n, (id, area)) in items.iter().enumerate() {
            // The last tile takes the exact remainder rather than an accumulated quotient, so a
            // row covers its strip with no float drift.
            let ch = if n == last {
                rect.z + rect.h - z
            } else {
                area / cw
            };
            out.push((
                *id,
                Rect {
                    x: rect.x,
                    z,
                    w: cw,
                    h: ch,
                },
            ));
            z += ch;
        }
        Rect {
            x: rect.x + cw,
            z: rect.z,
            w: rect.w - cw,
            h: rect.h,
        }
    } else {
        let rh = (sum / rect.w).min(rect.h);
        let mut x = rect.x;
        for (n, (id, area)) in items.iter().enumerate() {
            let cw = if n == last {
                rect.x + rect.w - x
            } else {
                area / rh
            };
            out.push((
                *id,
                Rect {
                    x,
                    z: rect.z,
                    w: cw,
                    h: rh,
                },
            ));
            x += cw;
        }
        Rect {
            x: rect.x,
            z: rect.z + rh,
            w: rect.w,
            h: rect.h - rh,
        }
    }
}
