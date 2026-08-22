# Traveler

A 3D file-space explorer. Scan a folder or drive, lay every file out in space, fly through it
with WASD, see how big things are and which files relate to each other.

Stack: Rust + Bevy 0.19 (released 2026-06-19).

---

## 0. Phase 0 result, measured 2026-08-18

`spike/` renders N boxes as a grid city: one shared cuboid mesh, 12 materials, stock `Mesh3d` +
`MeshMaterial3d`, automatic batching, spawned via `spawn_batch`. It measures two camera profiles,
because they cost very different amounts.

Machine: NVIDIA TITAN Xp (Pascal, 2017), Vulkan backend, 1600x900, vsync off.

| Blocks | Startup to 1st frame | Overview avg | Overview p99 | Street avg | Street p99 |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1M | 0.83 s | 38 fps | 43 ms | 72 fps | 20 ms |
| 2M | 1.50 s | 13 fps | 271 ms | 37 fps | 38 ms |
| 4M | 2.96 s | 0.2 fps | 5747 ms | 6 fps | 7044 ms |
| 1M + shadows | 0.70 s | 34 fps | 49 ms | 47 fps | 31 ms |

**Verdict: green at 1M, and the architecture holds.** No custom instancing pipeline needed. Three
things this actually settled:

- **Spawn is not a problem.** A million entities are up and rendering in well under a second, and
  it scales linearly. The scan will be the slow part of startup, not the renderer.
- **The ceiling is between 2M and 4M, and it is a cliff, not a slope.** At 4M, frame times blow up
  to multiple seconds with catastrophic hitches, consistent with GPU memory pressure or per-frame
  instance buffer churn. Cause not diagnosed, because the fix is LOD either way.
- **The overview camera is the bottleneck, by roughly 2x, at every count.** Street level gets
  frustum culling for free; looking across the whole city does not. That matters because the
  overview is *what you see first* when a scan finishes, so LOD aggregation moves from "later" to
  required, in phase 3.

Shadows cost about 35% at street level and are nearly free in overview. Cheap enough to keep as an
optional toggle, off by default.

Keep `spike/` as the perf regression harness. `cargo run --release -- --interactive` gives a
flyable city for feel; `--count N`, `--materials M`, `--shadows`, `--vsync` for measurement.

## 1. Architecture

The whole point of the foundation is that **layout is a pure function**. Nothing upstream of it
knows what a treemap is; nothing downstream knows where the numbers came from.

```
scan            ->  FileIndex                     (immutable, ids + sizes + tree)
FileIndex       ->  EdgeSet[]                     (one per enabled provider)
Index + Edges   ->  Layout  ->  Placements        (pos + scale + visible, per node id)
Placements      ->  renderer                      (instanced boxes)
```

Node ids are stable across layouts, so switching views is interpolating two `Placements`
arrays. That holds for the *renderer*, which is most of the benefit, but it is not the whole cost
of the city/graph switch: the level-of-detail pass assumes the file tree is also a spatial
hierarchy, which is a property of the treemap rather than of layouts in general. See section 7.

Enforce the boundary with a cargo workspace rather than discipline:

```
traveler/
  crates/
    traveler-scan/     walk -> FileIndex.           deps: ignore, rayon
    traveler-edges/    EdgeProvider trait + impls.  deps: traveler-scan, blake3
    traveler-layout/   Layout trait + impls.        deps: traveler-scan, glam   (NO bevy)
    traveler-app/      bevy app, camera, HUD.       deps: all of the above
```

`traveler-layout` depending on `glam` but not `bevy` keeps it headless-testable while still
handing back `Vec3` that Bevy consumes without conversion.

## 2. Data model

Struct-of-arrays, `NodeId(u32)`, indexed in parallel. At 1M+ files an AoS of `String` paths is
both slow and hundreds of MB.

```rust
pub struct FileIndex {
    parent:      Vec<NodeId>,     // root points at itself
    name:        Vec<StrRef>,     // (offset, len) into one big interned String
    size:        Vec<u64>,        // file: bytes. dir: subtree total, filled in a post pass
    ext:         Vec<ExtId>,      // interned, lowercased extension
    flags:       Vec<NodeFlags>,  // dir, reparse, hidden, unreadable
    depth:       Vec<u16>,        // free here, and the default height channel in the city
    child_start: Vec<u32>,        // children are contiguous, so this plus a length is enough
    child_len:   Vec<u32>,
}
```

Full paths are never stored, only reconstructed on demand by walking `parent`.

## 3. Scanning, built in phase 1

**No `ignore` crate, and no sort.** The plan was a parallel walk into flat records, sorted by
path so that parents precede children. That turned out to be unnecessary: a recursive walk already
knows each directory's parent, so ids can be handed out during the walk itself.

One rayon task per directory. Each reads its own directory outside the lock, then takes the
builder lock once to append all of its children as one contiguous block. One lock per directory,
not per file. That yields both invariants for free, with no sort and without ever holding a
million paths in memory:

- ids allocated parent-before-child, so the size rollup is one reverse pass
- children stored contiguously, so `children()` is a range

`ignore` was dropped because every feature it brings (gitignore, hidden filtering, parent ignore
rules) is something a disk analyzer has to turn off anyway, and reparse handling had to be written
by hand regardless. `jwalk` was never an option: deprecated as of 0.9.0.

Measured on this machine, `F:\Projects`, 92k entries:

| | wall | entries/sec |
| --- | ---: | ---: |
| cold cache | 16.13 s | 5,700 |
| warm cache | 0.073 s | 1,258,000 |
| warm, `--no-metadata` | 0.069 s | 1,331,000 |

Two things fall out of that:

- **`DirEntry::metadata()` really is nearly free on Windows.** 5% of warm walk time. The
  hypothesis in the original plan was right: the size and attributes come back with the directory
  enumeration itself. Question closed, and `--no-metadata` stays in as the way to re-check it.
- **Cold cache is 220x warm, and dominates everything else.** A first scan is pure I/O latency;
  nothing in user space will fix it. That makes the NTFS MFT fast path (`ntfs-reader`) much more
  attractive than "optional later", since it is precisely the cold case it wins. It still needs
  elevation, so it stays a fast path rather than the path.

**The junction bug, which is why this phase exists.** `Metadata::is_dir()` returns *false* for a
directory junction, because std classifies every reparse point as a symlink. Junctions were
therefore filed as zero-byte files. On `F:\Projects` that was 1,189 phantom files, invisible in
the byte total (reparse points report length 0) and caught only by diffing the file *count*
against PowerShell. pnpm builds `node_modules` out of junctions, so any repo tree hits this. Fix:
read `FILE_ATTRIBUTE_DIRECTORY` from the raw attributes instead of trusting `is_dir()`.

Also handled and covered by tests: permission errors collected rather than fatal (49 of them on
`C:\Windows`, scan continues), reparse points recorded but never descended into, and paths past
`MAX_PATH` (verified at 624 characters, std applies the verbatim prefix internally).

Sizes are logical file length, not size on disk. Hardlinks are counted once per link, so a tree
full of them over-reports. Both are documented rather than solved.

Verification, and it must stay exact: `F:\Projects` reports 80,044 files and 18,633,838,660 bytes,
byte-identical to `Get-ChildItem -Recurse -File -Force | Measure-Object -Sum Length`. Nine tests
cover a fixture tree with hand-computed totals, structure invariants, junctions, and long paths.

## 4. City layout, built in phase 2

Squarified treemap (Bruls, Huizing, van Wijk 2000) over the tree, extruded. Layout is a pure
function behind a `Layout` trait, with a `LayoutCtx` struct rather than bare arguments so phase 4
can add edges without changing the signature. `traveler-layout` depends on `glam` and not on
bevy, so all of this is tested headlessly.

**Size lives in footprint area, not in height.** Nested treemaps require additive weights: a
directory's weight has to equal the sum of its children's. That holds for raw bytes and fails for
any nonlinear transform, since `sum(s^k) != (sum s)^k`. So the tempting "volume proportional to
bytes" version cannot nest, and is out.

**Elevation encodes nesting, which is the refinement over the original sketch.** The plan said
height would be depth. It is better as *base* height than as thickness: a node's base sits at
`depth * level_step`, so a directory is a slab and its children rest on its top surface. Walking
into a folder is walking up a step, and the tree reads as terraces from across the city. Thickness
is then free for a separate channel: `LogSize` by default for a legible skyline, `Uniform` as the
honest toggle where area is the only size encoding.

**Every leaf gets a weight floor**, `max(bytes, 4096)` by default, applied to leaves only and
summed upward. Without it a zero-byte file is a zero-area rect, and there are thousands of those
on a real disk. The consequence is that area is proportional to the *floored weight* rather than
to raw bytes, so it is WinDirStat parity only approximately. Setting `min_leaf_weight: 0.0`
restores exact byte proportionality, and there is a test for each of those two claims.

**Spacing is what makes the tree legible, and it took two passes to get right.** The first
version capped the margin and gap at absolute world units (0.35 and 0.08). That is backwards: for
any rect bigger than a few units the absolute cap wins, so a 500-unit district got a 0.35-unit
wall and the whole scan fused into one slab, while tiny tiles hit the proportional branch and lost
a third of their area. The worst parent had its children covering 0.1% of its own footprint.

Now both are purely proportional to the parent's shorter side, with three properties that matter:

- **One street width per folder**, computed from the parent rather than per child, so the streets
  between districts run straight and share a width instead of varying with whatever they separate.
- **A falloff per level of depth** (0.72). Top-level folders get boulevards and deep folders get
  seams, which is what lets you pick out individual trees from above. It also stops the spacing
  compounding: without it every level would surrender the same fraction and a file ten levels down
  would keep a small fraction of its true area.
- **A cap per child** at 15% of its own shorter side, so a wide street cannot swallow the small
  tiles it runs past, and a floor under the ratio so adjacent files never fuse into one surface.

Defaults are deliberately generous, since telling one tree from another is the point of the view.
On `F:\Projects` that gives 47 unit streets between districts on a 1000 unit city. `--spacing N`
scales both ratios for taste. The cost is area: the worst parent now gives up 44% of its footprint
to walls and streets, against 29% before. Comparisons *within* a folder stay exact, which is the
property the view actually rests on; comparisons across depth were already compressed.

Tile quality is unaffected by any of this: median aspect 1.13, p90 1.76, p99 4.79, essentially
identical to the tightly packed version.

**Slivers are inherent and get culled, not hidden.** One dominant file in a directory leaves a
thin strip for everything else. Rects thinner than `min_side` are dropped along with their
subtree, and the rest are measured rather than assumed:

| tree | nodes | layout | median aspect | p90 | p99 | over 50:1 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `F:\Projects\Traveler` | 3.9k | 0.3 ms | 1.06 | 1.67 | 12.4 | 0.06% |
| `F:\Projects` | 92k | 4.4 ms | 1.13 | 1.78 | 5.1 | 0.19% |
| `C:\Windows` | 227k | 15.5 ms | 1.03 | 1.96 | 31.2 | 0.67% |

Median near 1.0 is the packing working. The hairline tail is the same artifact WinDirStat has and
is left alone deliberately: distorting areas to hide it would break the one property the whole
view rests on, and phase 3's LOD drops sub-pixel tiles anyway. At ~15M nodes/sec, a million-node
layout is around 60 ms, so re-laying out on a parameter change is interactive.

Color: categorical, assigned to the top ~12 extensions by total bytes, everything else grey. Same
idea as WinDirStat's legend. Critically, **few materials, not one per file**, or batching dies.
The SVG dump currently uses a hashed hue per extension as a placeholder.

Twelve property tests cover containment, sibling non-overlap, exact area conservation, both
proportionality claims, terracing, zero-byte visibility, determinism, and world bounds. There is
also `--svg`, which writes a top-down picture of the footprint, since phase 2 has no renderer.

## 5. The app, built in phase 3

`traveler <path>` scans, lays out, and renders the city. The scan runs before the window opens, so
a cold drive prints progress to the console instead of showing a frozen frame.

Stock `Mesh3d` + `MeshMaterial3d` with automatic batching, as phase 0 established: one shared
cuboid mesh, five materials, no per-entity material. Camera is the first-party
`bevy_camera_controller::free_camera`, rebound to space/ctrl for vertical movement.

**Level of detail, and it earns its place.** Walk the tree from the root; a node whose largest
footprint edge covers fewer than `--lod-px` pixels does not get its children walked, so its whole
subtree stays hidden and the folder is drawn as one solid block spanning everything inside it.
Cost is proportional to what gets drawn rather than to what was scanned, because an early stop
skips a subtree instead of testing it. Measured on `F:\Projects`, 95,594 blocks:

| view | drawn | fps |
| --- | ---: | ---: |
| overview, detail on | 18,979 | 364 |
| overview, detail off | 95,594 | 172 |
| low oblique, detail on | 24,237 | 347 |

**Picking needs no spatial index.** The level-of-detail pass already bounds the visible set, so a
ray-box test against every node it reaches, folded into the same traversal, costs a fraction of a
millisecond and is exact. The crosshair names the file, its size, and its type.

**Vertical exaggeration matters more than expected.** With `level_step` at 0.6 and files topping
out at 24 units, a city 1000 units wide renders as a flat plain: the third dimension was
technically present and visually absent. Raised to 2.2 and 90.

**Color: the plan's "top 12 extensions, one color each" does not survive validation.** A treemap
is an all-pairs form, since any two tiles can end up adjacent, so every *pair* of colors has to be
separable rather than just neighbours in a legend order. Running the palette validator over the
reference eight, Okabe-Ito, Tol bright, and lightness-snapped variants of each, no fourth hue
clears the colorblind and normal-vision floors against a validated first three on the dark
surface: deuteranopia collapses color onto a blue-yellow axis, and pinning lightness to the dark
band removes the remaining degree of freedom. Every candidate fourth failed, most of them badly.

So hue carries a **category** (code & text, media, binaries & archives) with everything else
folded into a neutral, which is what the method prescribes past the limit. Validated all-pairs on
the dark surface: worst CVD ΔE 9.4, worst normal-vision ΔE 20.9, all three at least 3:1 contrast.
Exact extensions never depended on color anyway: the crosshair readout names the type of whatever
you are looking at, and the legend lists the biggest extensions by bytes under their category.

Lighting had to be calibrated down for this to hold, since over-exposure both brightens and
desaturates through the tonemapper. Measured against the targets, a neutral face was rendering
1.5x too bright; at the current illuminance the binaries green renders (65,163,122) against a
target of (25,158,112), close enough that the pairwise separations survive.

**Two Bevy 0.19 details worth remembering**, both of which cost a debugging round:

- `ScreenshotCaptured` is triggered as a *global* event carrying the entity, so the documented
  `.observe(save_to_disk(path))` on the screenshot entity never fires. Use `app.add_observer`.
- A mesh whose render pipeline is still compiling is silently skipped, with no warning. An
  automated screenshot taken a few frames in shows an empty world; it has to wait on wall clock.

Controls: WASD, space/ctrl up and down, shift to run, scroll to change speed, right click or M to
grab the mouse. L toggles detail, `[` and `]` move the threshold, H switches height mode (a full
relayout plus a transform pass, which is the pure-layout claim from section 1 paying off), R
resets the camera. `--shot <file>` renders a few seconds and saves a png, which is how the
screenshots in this plan were checked.

Bigger trees cost more than `F:\Projects` suggests. `C:\Windows`, 226,877 blocks, renders the
overview at 93-125 fps, against 364 at 95k. Startup there breaks down as 3.6 s scan (warm), 15 ms
layout, and about 1.5 s to spawn, so spawning one entity per node rather than batching is **not**
the bottleneck it looked like; the scan is, and only when cold.

Still missing: shadows are off (phase 0 measured them at ~35% and they are a toggle away), and the
scan blocks the window instead of streaming in.

## 6. Edges, built in phase 4

Everything produces **groups**, never pairs. A group of n members implies n^2 lines, which at these
sizes is spaghetti rather than information, so a selected group is drawn as a shared highlight
plus a hub: n lines converging above the group's centre. `Group` carries a label, its members, its
bytes, and how many of those bytes are redundant.

Three providers, and the split between them turned out to be about I/O, not about difficulty:

- **Extension groups.** One pass over the interned extension column. Free.
- **Name families.** Same folder, same stem after dropping the extension and any trailing counter,
  so `parser.c` finds `parser.h` and `IMG_0001.jpg` finds its 98 siblings. Deliberately scoped to
  one directory: matching stems tree-wide finds thousands of unrelated `index` and `main` files
  and says nothing. The normalised stem is always a *prefix* of the name, so the grouping keys are
  borrowed slices of the name blob and the whole pass allocates nothing per file. Labels carry two
  path components, because every build profile has its own `deps` folder and a bare leaf name
  cannot be told apart in a list.
- **Exact duplicates.** Bucket by size, screen on a 64 KiB prefix, then full BLAKE3 on whatever
  still collides. Exact rather than probable, because the obvious next feature is deleting things.
  Zero-length files are excluded: they are all identical to each other and there are thousands.

Together on `F:\Projects`: 136 extension groups and 7,242 name families in 11 ms, and on the
Traveler tree the duplicate finder found **1.3 GiB of redundant build artifacts**.

**The duplicate finder is seek-bound, and that shaped the design.** On this machine `F:` is a
7200 rpm disk, and the pass costs one seek per candidate no matter how little of the file is read:
6,882 files took 44 s, which is 154 files per second, almost exactly one seek each. Three
consequences:

- Reads are issued in **walk order rather than size order**. Consecutive ids are neighbours in the
  directory tree and usually neighbours on the platter, which turns a scatter of seeks into
  something closer to a sweep.
- **Four reader threads, not one per core.** Ten concurrent readers just make the head thrash.
- **A 4 KiB size floor**, since a 500 byte duplicate cannot repay a 6 ms seek. The count of
  candidates this skips is reported rather than hidden.

Even so it can take a minute on a build tree, so in the app it runs on a **worker thread** and its
result drops in when ready. Nothing blocks: the two free providers are ready before the window
opens, and `D` starts the duplicate scan. `--dupes` starts it at launch instead.

In the city, tab cycles the group kind, up and down move the selection (shift for ten at a time),
and esc clears. Selected members turn gold and hub lines run to a point above their centroid, so
you can see both where they are and that they belong together. Both are capped, at 20,000 recolors
and 1,200 lines, since a group can hold every `.rs` file on a drive.

Deferred, unchanged: near-duplicates (needs TLSH or per-format perceptual hashing plus a threshold
that takes a long time to tune) and code references (tree-sitter gets the import strings cheaply,
but resolving them to real paths is per-ecosystem work).

## 7. Satellite cluster, built in phase 5

**The full view switch was planned and then deliberately not built.** Reviewing it against the
code found that "switching views is free" only holds for the renderer. The level-of-detail and
picking pass assumes the file tree is also a *spatial* hierarchy in three places: the screen-size
test treats a node's own footprint as its subtree's extent, `collapsed_box` does the same
vertically, and declining to descend is safe only because the parent covers the same pixels. All
three are properties of a treemap, not of layouts in general, so a floating graph would have
needed the detail pass reworked, a transition system, and new camera framing.

What got built instead is the part that says something a highlight cannot: **how a group's members
relate to each other**. The city stays exactly as it is, and the selected group is lifted above it.

**The arrangement is the message.** Members are bucketed by parent folder, each bucket becomes a
disc sized `spacing * sqrt(count)`, and the discs are packed outward from the centre largest
first. One tight clump means the group came from one place; several clumps mean several. On this
repo's own tree the contrast is immediate: `.rlib` reports *658 across 3 folders* and draws as two
dense columns, the debug and release `deps` directories, while `.exe` reports *176 across 75
folders* and scatters. Threads are **one straight line** from each file to its block, and getting there took three
tries. Diagonals crossed the city, so they became an elbow: straight up, then a bend near the top.
That fixed the crossing but only because it hid it, and the bend plane sat just under the cluster,
so a thread from a distant file had to cover its whole horizontal run in a short vertical drop and
came out nearly horizontal, streaking over everything. Keeping the city's bearings removed the
reason the elbow existed, and the straight line came back.

**A bug that only showed up as "the lines do not line up":** the layout measured its bearings from
the mean of the *folder* positions while the app hung the cluster over the mean of the *member*
positions. Two reasonable centres, different whenever folders hold different numbers of files, and
every thread was skewed by the gap between them. The layout now reports the anchor it used and the
app is required to hang the cluster exactly there, so the two cannot drift apart again. The crosshair picks
cluster blocks as well as city blocks, so pointing at a dot names the file.

Deliberately **deterministic and non-iterative**: no force simulation, no settling, no tuning
beyond spacing, and reversing the input order cannot change the output. Blocks are all one size,
because byte size is already encoded by the city below and re-encoding it here would only compete
with the arrangement.

Three things the first render got wrong, all found by looking at it:

- the cluster sat above the *members'* highest point, so a tall building elsewhere speared through
  it. It now clears the whole skyline.
- a group spread over 75 folders packed into a disc a fifth of the city wide. The whole cluster is
  now scaled to one readable footprint, positions and block size together.
- blocks are drawn from a pool of 2000 spawned once and hidden, so switching groups moves entities
  rather than spawning them, and the cap drops **whole folders** rather than showing one half
  present.

Eight tests, and the one that matters asserts the actual claim: mean distance between same-folder
members must be clearly below mean distance between members of different folders. This fixture
measures 1.9x. The threshold is set at 1.4x as a regression guard, after measuring rather than
before.

**Still deferred:** the full graph view. Section 7 of the git history has the plan if it is ever
wanted, but phase 4's highlight plus this cluster now cover what it was for, while keeping the two
encodings a floating graph throws away: area as bytes, elevation as nesting.

## 8. Clicking, built after phase 5

Left click **pins** whatever the crosshair is on. The readout otherwise changes the instant you
turn your head, which is useless the moment you want to look at what you found. Clicking a folder
also paints its whole subtree, and reports what it holds: file and folder counts, and the largest
single child, which is nearly always the thing you were actually looking for.

The value is in the contrast rather than the highlight itself. Pinning `target
elease\deps`
turns half the city violet and leaves the `debug` district beside it green, which says "this
entire region is one folder" faster than any number could.

Two decisions worth keeping:

- **One system paints both overlays.** A pinned folder can contain members of the selected group,
  so two independent painters would fight over the same block. Precedence is pin over group over
  category, matching what the click meant, and everything is restored to its category colour
  before repainting rather than reasoning about which overlay a block should fall back to.
- **The painter keys off a generation counter, not the pinned node id.** The first version keyed
  off the id and lost the race: it ran before the subtree had been computed, painted nothing,
  and marked itself up to date, so the highlight never appeared even though the readout was
  correct. A counter bumped when the subtree is rebuilt makes it order-independent.

Painting is capped at 20,000 blocks; counting is not, so the summary tells the truth even when the
highlight is showing part of it. Violet is deliberately outside the categorical palette: it means
"the thing you just clicked", so it only has to stand apart from its neighbours rather than carry
an identity of its own.

Considered and not built: drilling into a folder to make it the new root, which is the biggest
remaining win and the fix for tiny folders being unreadable; opening a file preview, which is a
different app bolted on; and reveal-in-Explorer, which is small but should not be a bare click.

## 9. Phases

0. ~~Spike~~ **Done, green.** See section 0. 1M blocks at 38/72 fps, sub-second spawn, ceiling
   between 2M and 4M. `spike/` stays as the perf regression harness.
1. ~~`traveler-scan`~~ **Done.** See section 3. Byte-exact against PowerShell on `F:\Projects`,
   1.26M entries/sec warm, 9 tests.
2. ~~`traveler-layout`~~ **Done.** See section 4. 227k nodes in 15.5 ms, median tile aspect 1.03,
   11 property tests.
3. ~~`traveler-app`~~ **Done.** See section 5. 95k blocks at 364 fps with detail on, 2.1x the
   framerate of drawing everything, crosshair picking, validated palette.
4. ~~`traveler-edges`~~ **Done.** See section 6. Three providers, group panel, highlight and hub
   lines, duplicate scan on a worker thread. Found 1.3 GiB redundant in this repo's own tree.
5. ~~Graph layout~~ **Done, as a satellite cluster.** See section 7. The full view switch was
   reviewed, costed, and dropped in favour of the part that carries the information.
6. Code references, one ecosystem at a time. TypeScript or Rust first, for dogfooding.

## 10. Known risks

| Risk | Mitigation |
| --- | --- |
| ~~Entity count tanks framerate~~ | Retired. 1M measured fine, cliff at 2-4M, LOD covers it |
| ~~Junctions and symlinks cycle the scan~~ | Retired. Reparse points flagged, never followed, tested |
| ~~Windows long paths~~ | Retired. Verified at 624 chars, std handles the verbatim prefix |
| Cold-cache scan is 220x warm | Unfixable in user space. Raises the value of the MFT fast path |
| Hardlinks double-count bytes | Currently counted once per link. Documented, not solved |
| Size spans 12 orders of magnitude | Leaf weight floor plus sliver culling. 0.2-0.7% of tiles are still hairlines |
