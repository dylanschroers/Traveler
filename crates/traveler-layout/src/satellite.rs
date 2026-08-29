//! A cluster of one group's members, to float above the city it came from.
//!
//! The city already answers "how big is this" and "where does it sit in the tree". Highlighting a
//! group answers "where are its members". The one thing neither shows is how the members relate to
//! *each other*, and for a group of files the relationship that matters is which folder each one
//! came from: 47 shims from a single `.bin` is a completely different fact from 47 copies strewn
//! across 47 projects, and the highlight alone cannot tell you which you are looking at.
//!
//! So members are bucketed by parent folder and each bucket becomes a disc. Where that disc goes
//! is not arbitrary: it keeps the folder's real bearing from the middle of the group, and its
//! distance from the centre tracks how far out that folder actually is. The cluster is a
//! compressed map of the region it hangs over. One tight clump means one source, many clumps mean
//! many, and a clump on the left of the cluster came from the left of the city.
//!
//! Packing the discs by an arbitrary spiral index instead, which is what this did first, looks
//! fine in isolation and is obviously wrong the moment the threads are drawn: a folder on the
//! right of the city gets its disc in the middle of the cluster, and every thread crosses every
//! other one on the way down.
//!
//! Every block is the same size on purpose. Byte size is already encoded by the city below, and
//! re-encoding it here would only compete with the arrangement, which is the sole thing this view
//! is for.
//!
//! Deterministic and non-iterative: no force simulation, no settling, no tuning constants beyond
//! spacing. The same group always produces the same cluster.

use glam::{Vec2, Vec3};
use traveler_core::{NodeId, Tree};

use crate::Placements;

/// Angle between successive points in a phyllotaxis disc, in radians. Successive points land
/// nowhere near each other, which is what keeps the disc evenly filled at any count.
const GOLDEN_ANGLE: f64 = 2.399_963_229_728_653;

#[derive(Clone, Copy, Debug)]
pub struct SatelliteParams {
    /// Cap on blocks placed. A group can hold every `.rs` file on a drive, and past a few thousand
    /// dots the arrangement stops being readable anyway.
    pub max_members: usize,
    /// Distance between neighbouring members.
    pub spacing: f64,
    /// Clear space left between two folder discs.
    pub bucket_gap: f64,
    /// Edge length of each block.
    pub block: f32,
}

impl Default for SatelliteParams {
    fn default() -> Self {
        Self {
            max_members: 2000,
            spacing: 6.0,
            bucket_gap: 9.0,
            block: 4.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SatelliteBucket {
    /// The folder these members came from.
    pub dir: NodeId,
    pub count: usize,
    pub center: Vec2,
    pub radius: f32,
    /// Bearing this bucket was placed on, in radians, taken from the city and never adjusted.
    pub bearing: f32,
}

#[derive(Clone, Debug, Default)]
pub struct Satellite {
    /// Members actually placed, largest bucket first.
    pub members: Vec<NodeId>,
    /// Position of each member relative to the cluster centre, in the y = 0 plane.
    pub offsets: Vec<Vec3>,
    pub buckets: Vec<SatelliteBucket>,
    pub block: f32,
    /// Distance from the centre to the outermost block.
    pub radius: f32,
    /// The point in the city, in ground coordinates, that every bearing was measured from.
    ///
    /// The caller **must** hang the cluster directly over this. Centring it anywhere else, for
    /// instance over the mean of the members rather than the mean of the folders, skews every
    /// thread by the difference between the two points, which is exactly what "the lines do not
    /// line up" looks like.
    pub anchor: Vec2,
    /// Members dropped by `max_members`.
    pub omitted: usize,
}

impl Satellite {
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }
}

/// Arranges `members` into folder clumps that keep their bearing from the city below.
///
/// `city` supplies where each member actually sits, which is what lets the cluster preserve
/// direction. The order of `members` on the way in does not matter.
pub fn lay_out<T: Tree>(
    tree: &T,
    members: &[NodeId],
    city: &Placements,
    params: &SatelliteParams,
) -> Satellite {
    if members.is_empty() {
        return Satellite {
            block: params.block,
            ..Default::default()
        };
    }

    // Bucket by parent folder. Members arrive in id order, which is walk order, so the members of
    // one folder are already contiguous and a sort by parent is nearly free.
    let mut by_parent: Vec<(NodeId, NodeId)> =
        members.iter().map(|m| (tree.parent(*m), *m)).collect();
    by_parent.sort_unstable();

    let mut buckets: Vec<(NodeId, Vec<NodeId>)> = Vec::new();
    for (dir, member) in by_parent {
        match buckets.last_mut() {
            Some((d, list)) if *d == dir => list.push(member),
            _ => buckets.push((dir, vec![member])),
        }
    }

    // Largest first, so the dominant folder gets the spot it asks for and the rest give way. Ties
    // break on the folder's id to keep the whole thing reproducible.
    buckets.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));

    // Trim to the cap by dropping whole buckets from the tail, which keeps every clump complete
    // rather than showing a partial folder.
    let mut kept = 0usize;
    let mut cut = buckets.len();
    for (i, (_, list)) in buckets.iter().enumerate() {
        if kept + list.len() > params.max_members && kept > 0 {
            cut = i;
            break;
        }
        kept += list.len();
    }
    let omitted: usize = buckets[cut..].iter().map(|(_, l)| l.len()).sum();
    buckets.truncate(cut);

    // A disc of n points spaced `s` apart needs a radius near s*sqrt(n).
    let radii: Vec<f64> = buckets
        .iter()
        .map(|(_, list)| params.spacing * (list.len() as f64).sqrt().max(0.5))
        .collect();

    // Where each folder actually is, and where the group as a whole is, so the cluster can keep
    // the bearings. Members the city could not place carry no position and stay out of the
    // averages rather than dragging them toward the origin.
    let bucket_pos: Vec<Option<Vec2>> = buckets
        .iter()
        .map(|(_, list)| mean_city_pos(city, list))
        .collect();
    let anchor = {
        let known: Vec<Vec2> = bucket_pos.iter().flatten().copied().collect();
        if known.is_empty() {
            Vec2::ZERO
        } else {
            known.iter().copied().sum::<Vec2>() / known.len() as f32
        }
    };
    let total: usize = buckets.iter().map(|(_, l)| l.len()).sum();
    let spread = params.spacing * (total as f64).sqrt() * 1.7;
    let placement = place_by_bearing(&radii, &bucket_pos, anchor, spread, params.bucket_gap);

    let mut out = Satellite {
        block: params.block,
        omitted,
        anchor,
        ..Default::default()
    };
    let mut extent = 0.0f64;

    for (i, (dir, list)) in buckets.iter().enumerate() {
        let (cx, cz, bearing) = placement[i];
        let r = radii[i];
        out.buckets.push(SatelliteBucket {
            dir: *dir,
            count: list.len(),
            center: Vec2::new(cx as f32, cz as f32),
            radius: r as f32,
            bearing: bearing as f32,
        });

        // Ordering inside a clump is by size then id: stable, and it puts the big one on the
        // outside edge rather than scattering by whatever order the scan happened to use.
        let mut sorted = list.clone();
        sorted.sort_unstable_by(|a, b| tree.weight(*b).cmp(&tree.weight(*a)).then(a.cmp(b)));

        let n = sorted.len();
        for (j, member) in sorted.into_iter().enumerate() {
            let (x, z) = if n == 1 {
                (cx, cz)
            } else {
                let angle = j as f64 * GOLDEN_ANGLE;
                let rr = r * ((j as f64 + 0.5) / n as f64).sqrt();
                (cx + rr * angle.cos(), cz + rr * angle.sin())
            };
            extent = extent.max(x.hypot(z));
            out.members.push(member);
            out.offsets.push(Vec3::new(x as f32, 0.0, z as f32));
        }
    }

    out.radius = extent as f32 + params.block;
    out
}

/// Mean ground position of the members the city actually placed.
fn mean_city_pos(city: &Placements, members: &[NodeId]) -> Option<Vec2> {
    let mut sum = Vec2::ZERO;
    let mut n = 0u32;
    for m in members {
        if city.is_visible(*m) {
            let c = city.center[m.index()];
            sum += Vec2::new(c.x, c.z);
            n += 1;
        }
    }
    (n > 0).then(|| sum / n as f32)
}

/// Places each disc on its folder's true bearing from the group's centre, at a radius that tracks
/// how far out the folder is, pushed outward only as far as it takes to stop overlapping.
///
/// Bearing is never adjusted, only radius. That is what keeps the cluster's angles equal to the
/// city's angles, and it is why threads leaving the cluster do not cross.
///
/// Returns `(x, z, bearing)` per input disc.
fn place_by_bearing(
    radii: &[f64],
    positions: &[Option<Vec2>],
    anchor: Vec2,
    spread: f64,
    gap: f64,
) -> Vec<(f64, f64, f64)> {
    let offsets: Vec<Vec2> = positions
        .iter()
        .map(|p| p.map(|v| v - anchor).unwrap_or(Vec2::ZERO))
        .collect();
    let furthest = offsets
        .iter()
        .map(|o| o.length() as f64)
        .fold(0.0f64, f64::max);

    let mut placed: Vec<(f64, f64, f64)> = Vec::with_capacity(radii.len());
    let mut out = vec![(0.0, 0.0, 0.0); radii.len()];

    // Biggest first, so the dominant folder settles at the radius it wants and the rest give way.
    let mut order: Vec<usize> = (0..radii.len()).collect();
    order.sort_by(|a, b| {
        radii[*b]
            .partial_cmp(&radii[*a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(b))
    });

    for i in order {
        let offset = offsets[i];
        let bearing = if offset.length_squared() > 0.0 {
            (offset.y as f64).atan2(offset.x as f64)
        } else {
            0.0
        };
        let want = if furthest > 0.0 {
            offset.length() as f64 / furthest * spread
        } else {
            0.0
        };

        let step = (radii[i] + gap).max(1.0) * 0.25;
        let mut r = want;
        // A single folder legitimately sits dead centre, so this starts at the wanted radius and
        // walks outward along the bearing until nothing is in the way. Bounded because a
        // pathological input must not spin forever.
        for _ in 0..4096 {
            let (x, z) = (r * bearing.cos(), r * bearing.sin());
            let clear = placed
                .iter()
                .all(|(px, pz, pr)| (x - px).hypot(z - pz) >= pr + radii[i] + gap);
            if clear {
                break;
            }
            r += step;
        }
        let (x, z) = (r * bearing.cos(), r * bearing.sin());
        placed.push((x, z, radii[i]));
        out[i] = (x, z, bearing);
    }
    out
}
