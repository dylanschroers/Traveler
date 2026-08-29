//! Strip treemap over an address space, ordered by address.
//!
//! Squarified is wrong here and is deliberately not reused. It sorts by weight, which throws away
//! the address order that is the whole point, and its row breaks move when weights change, which
//! makes two captures incomparable.
//!
//! A strip treemap lays items in order into rows across the rect, breaking a row when adding to it
//! would worsen the row's mean aspect ratio. Reading order is then address order — left to right,
//! top to bottom — which is a genuine analytic property rather than decoration: you can see that
//! the heap sits below the libraries, that an arena is one contiguous band, that a region grew
//! into the gap after it.
//!
//! Hilbert curves were considered and rejected: better 2D locality, but the reading order is
//! unlearnable and "where is this address" stops having an answer a person can follow.

use glam::Vec3;
use traveler_core::{NodeId, Tree};

use crate::{Layout, LayoutCtx, Placements, Rect, spacing_ratio};

#[derive(Clone, Copy, Debug)]
pub struct SpaceParams {
    pub world_size: f64,
    /// Rise per level, and the thickness of a container's slab.
    pub level_step: f32,
    pub leaf_thickness: f32,
    pub margin_ratio: f64,
    pub gap_ratio: f64,
    pub spacing_falloff: f64,
    pub min_spacing_ratio: f64,
    pub max_child_inset: f64,
    /// Extents span twelve orders of magnitude, from a 4 KiB `vdso` to a gigabyte arena, so the
    /// thin end is culled rather than drawn as invisible geometry.
    pub min_side: f64,
}

impl Default for SpaceParams {
    fn default() -> Self {
        Self {
            world_size: 1000.0,
            level_step: 2.2,
            leaf_thickness: 6.0,
            margin_ratio: 0.052,
            gap_ratio: 0.044,
            spacing_falloff: 0.72,
            min_spacing_ratio: 0.004,
            max_child_inset: 0.15,
            min_side: 0.01,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SpaceLayout {
    pub params: SpaceParams,
}

impl SpaceLayout {
    pub fn new(params: SpaceParams) -> Self {
        Self { params }
    }
}

impl Layout for SpaceLayout {
    fn name(&self) -> &'static str {
        "space"
    }

    fn place<T: Tree>(&self, ctx: &LayoutCtx<'_, T>, out: &mut Placements) {
        let tree = ctx.tree;
        let p = &self.params;
        let n = tree.len();
        out.reset(n);
        if n == 0 {
            return;
        }

        let half = p.world_size * 0.5;
        let mut stack = vec![(
            tree.root(),
            Rect {
                x: -half,
                z: -half,
                w: p.world_size,
                h: p.world_size,
            },
        )];

        let mut min = Vec3::splat(f32::INFINITY);
        let mut max = Vec3::splat(f32::NEG_INFINITY);
        let mut items: Vec<(NodeId, f64)> = Vec::new();
        let mut placed_rects: Vec<(NodeId, Rect)> = Vec::new();

        while let Some((id, rect)) = stack.pop() {
            if rect.is_degenerate() || rect.w < p.min_side || rect.h < p.min_side {
                continue;
            }

            let thickness = if tree.is_group(id) {
                p.level_step
            } else {
                p.leaf_thickness
            };
            let base = tree.depth(id) as f32 * p.level_step;
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

            if tree.child_count(id) == 0 {
                continue;
            }

            let depth = tree.depth(id);
            let inner = rect.inset(
                spacing_ratio(
                    p.margin_ratio,
                    p.spacing_falloff,
                    p.min_spacing_ratio,
                    depth,
                ) * rect.w.min(rect.h),
            );
            if inner.is_degenerate() {
                continue;
            }

            // No sort. Address order is id order is child order, and preserving it is the point.
            items.clear();
            let mut total = 0.0;
            for child in tree.child_ids(id) {
                let w = tree.weight(child) as f64;
                total += w;
                items.push((child, w));
            }
            if total <= 0.0 {
                continue;
            }
            let scale = inner.area() / total;
            for it in items.iter_mut() {
                it.1 *= scale;
            }

            placed_rects.clear();
            strip(&items, inner, &mut placed_rects);

            let street = spacing_ratio(p.gap_ratio, p.spacing_falloff, p.min_spacing_ratio, depth)
                * inner.w.min(inner.h)
                * 0.5;
            for (child, r) in placed_rects.iter() {
                let inset = street.min(p.max_child_inset * r.w.min(r.h));
                stack.push((*child, r.inset(inset)));
            }
        }

        out.min = if out.placed > 0 { min } else { Vec3::ZERO };
        out.max = if out.placed > 0 { max } else { Vec3::ZERO };
    }
}

/// Lays `items`, in order, into strips along the shorter side of whatever is left of `rect`.
///
/// The first version laid full-width rows across the whole rect, which is the textbook strip
/// treemap and reads literally left to right, top to bottom. Measured on a real address space it
/// gave a median aspect of 6.6 and 12% hairlines, because one region holding 99.9% of the bytes
/// takes a full-width row and leaves everything else a strip one unit tall. Taking the shorter
/// side of the *remaining* rect is what squarify does, and it is orthogonal to squarify's other
/// half, the sort — which is the half that would lose the address order.
fn strip(items: &[(NodeId, f64)], rect: Rect, out: &mut Vec<(NodeId, Rect)>) {
    let mut rect = rect;
    let mut i = 0;
    while i < items.len() {
        if rect.is_degenerate() {
            break;
        }
        let side = rect.w.min(rect.h);

        let mut end = i + 1;
        let mut sum = items[i].1;
        let mut best = worst_aspect(&items[i..end], sum, side);
        while end < items.len() {
            let candidate_sum = sum + items[end].1;
            let candidate = worst_aspect(&items[i..end + 1], candidate_sum, side);
            if candidate > best {
                break;
            }
            best = candidate;
            sum = candidate_sum;
            end += 1;
        }

        rect = lay_row(&items[i..end], sum, rect, out);
        i = end;
    }
}

/// Lays one strip along the shorter side and returns what is left over.
fn lay_row(items: &[(NodeId, f64)], sum: f64, rect: Rect, out: &mut Vec<(NodeId, Rect)>) -> Rect {
    let last = items.len() - 1;
    if rect.w >= rect.h {
        let cw = (sum / rect.h).min(rect.w);
        let mut z = rect.z;
        for (n, (id, area)) in items.iter().enumerate() {
            // The last tile takes the exact remainder rather than an accumulated quotient, so a
            // strip covers its band with no float drift.
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

/// Worst aspect ratio in a strip of total area `sum` laid along an edge of length `side`.
///
/// Worst rather than mean, measured rather than assumed: the mean is the textbook strip rule, and
/// on `gdb` it gives median 2.12, p99 65.9 and 1.4% hairlines against 2.02, 41.8 and 0.35% for
/// this one. Squarify's break rule and squarify's sort are separable, and only the sort is the
/// part that would lose the address order.
fn worst_aspect(row: &[(NodeId, f64)], sum: f64, side: f64) -> f64 {
    if sum <= 0.0 || side <= 0.0 || row.is_empty() {
        return f64::INFINITY;
    }
    let thickness = sum / side;
    let mut worst = 0.0f64;
    for (_, area) in row {
        if *area <= 0.0 {
            return f64::INFINITY;
        }
        let extent = area / thickness;
        worst = worst.max((extent / thickness).max(thickness / extent));
    }
    worst
}
