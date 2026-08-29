# Traveler: dynamic analysis

Companion to [PLAN.md](PLAN.md), which is the working record of phases 0-5 (the disk city).
This file is the forward plan for phase 6 onward: **the inside of a running process, as a city**.

PLAN.md is still binding. Everything it settled — pure layout, struct-of-arrays, groups over
pairs, the validated three-hue palette, measure before tuning — carries forward unless this
document explicitly overrides it, and where it does the override says why.

---

## 1. What is being built, and what is not

Point Traveler at a program. It launches it (or attaches to it), stops it, and draws its
**address space** as a city: laid out by address, sized by mapped bytes, drilled down to the
4 KiB page. Take a second capture later and the two are comparable block for block, so the
difference between them is a picture rather than a diff of two text dumps.

The question the view answers is **"what is this unfamiliar thing made of, and what did it just
do."** Every design call below resolves in favour of that question.

Locked decisions, and the alternatives they kill:

| decision | consequence |
| --- | --- |
| **One process at a time, from the inside** | No system-wide process graph. Nodes are address intervals, not processes. |
| **Snapshot and diff, not continuous live** | The target is stopped for each capture. No animation loop, no sampling clock, no boiling treemap. |
| **Read and poke now; breakpoints and stepping later** | Needs `ptrace`/debug-API attach and consistent reads, but no execution-control state machine yet. One single-shot exception, the entry-point breakpoint in §4, which has no state machine and is what buys the linking diff. The seam for the rest is designed in §12. |
| **Symbol-optional, three tiers** | The view is never empty. Symbols sharpen it; they never gate it. |
| **Pages are the floor, objects later** | No allocator interception, no `LD_PRELOAD` shim, no glibc arena walking in phase 6. |
| **Linux and Windows, abstracted from the first commit** | The snapshot struct is designed against *both* capability lists before either backend is written. Capability flags, never lowest common denominator. |
| **Launch and attach, both, launch first** | Launch is the common case and needs no privileges. Attach converges after ~50 lines. |

Explicitly **not** in scope, and each for a reason:

- **A system-wide process graph.** Different app. ~300 nodes carries no scale and no depth.
- **Continuous live animation.** Ruled out by the snapshot model, which is what makes reads
  consistent and pokes safe. See §7.
- **Merging the disk city and the process city into one world.** They share the renderer and the
  layout contract, not the scene. File-backed mappings *name* their file (§11), which is the
  useful part of the link without the mess of the merge.

---

## 2. The core insight: it is all one interval tree

**Every node is an address interval. The tree is interval containment. Weight is length in bytes.**

```
process
 └─ region            contiguous allocation      (Windows AllocationBase; Linux: rule below)
     └─ mapping       one /proc/pid/maps line, or one VirtualQueryEx region
         └─ section   .text .rodata .data .bss   (only when the on-disk file was parsed)
             └─ page  4 KiB                      (an index into a bitset, not a node — §9)
```

That single sentence buys most of what phase 2 had to fight for:

- **Area is exactly additive with no fudge.** A parent's length *is* the sum of its children's
  lengths. No `min_leaf_weight` floor, no `sum(s^k) != (sum s)^k` problem — bytes really do nest
  here, because the thing being measured is an interval and intervals compose.
- **There is an intrinsic total order: the address.** File sizes have no natural order, which is
  why squarified packing sorts by weight and why a live treemap reshuffles. Address order does
  not change between captures. This is what makes the diff possible at all (§8).
- **Containment is real**, so the LOD/picking traversal from `lod.rs` carries over. §7 of
  PLAN.md dropped the graph view because a floating graph has no spatial hierarchy. An
  address-ordered treemap does. That objection does not apply here, and the whole detail pass is
  reused rather than rewritten — with one branch added at the bottom of the walk for pages, which
  §9 costs out.

Two consequences that must be got right or the areas lie:

**Holes inside a parent are nodes, not absences.** A mapping whose sections cover only part of it
has padding; a region with an uncommitted middle has a gap. Those are *facts about the program*
and they are also required for children to sum to the parent. Synthesise them as unnamed `Gap`
nodes.

**Holes between top-level mappings are not nodes.** A 64-bit address space is essentially all
hole. Laying out the VA range would give a city that is 99.9999% empty ground. The root's weight
is defined as the **sum of mapped extents**, and unmapped VA between mappings is collapsed to
nothing. The address *order* survives the collapse, which is all the layout needs.

**A region needs a Linux rule, because `AllocationBase` has no Linux equivalent.** Windows hands
the level over for free: every mapping knows the `VirtualAlloc` that made it. Linux knows nothing
of the sort, and "the run of mappings sharing one file" covers only file-backed memory — it leaves
the heap, every thread stack, and the hundreds of anonymous mappings an allocator makes with no
region at all, which would put a level of the hierarchy on one platform and not on the other. So:
**a region is a maximal run of address-adjacent mappings with the same backing, where "anonymous"
counts as a backing of its own.** A `PROT_NONE` guard and the `rw-` stack beside it fuse into one
region, which is what `AllocationBase` says about the same memory on Windows; a lone `mmap` is a
region of one. One pass over the address-sorted extents, and it needs no `smaps` read to get it.

The rule's limit, found by the fixture test in 6c rather than by argument: **two adjacent
anonymous mappings from unrelated `mmap` calls fuse**, because `maps` records nothing that tells
them apart. Windows does not have the problem because it records the allocation. On Linux a region
is therefore "what is contiguous and alike", not "what one call made", and the test asserts the
weaker true thing rather than the stronger convenient one.

---

## 3. Crate layout

```
traveler-core     NodeId, Tree, human_bytes, thousands.        deps: none
traveler-scan     walk -> FileIndex.                           deps: core, rayon
traveler-proc     acquire + capture -> AddressSpace.           deps: core, (libc | windows-sys)
traveler-sym      on-disk ELF/PE -> sections, symbols.         deps: core, object
traveler-edges    EdgeProvider over either index.              deps: core, scan, blake3, rayon
traveler-layout   Layout trait, city + space impls.            deps: core, glam        (NO bevy)
traveler-app      the bevy app.                                deps: all of the above
```

**`traveler-core` is a new crate and it is a mechanical refactor, not a redesign.** It exists
because `NodeId` and the `Tree` trait are needed by both `traveler-scan` and `traveler-proc`, and
without it `traveler-proc` would have to depend on `traveler-scan` — pulling in rayon and a
directory walker to borrow a `u32` newtype. `traveler-scan` re-exports `NodeId` so nothing
downstream changes. Do this in one commit, before anything else, and touch nothing but imports.

**`traveler-sym` is separate from `traveler-proc` on a clean line: `proc` is what the *operating
system* says about a live process, `sym` is what the *file on disk* says about an image.** They
answer different questions from different sources and the second one is optional and heavy
(`object` now, `gimli`/`pdb` later). Keeping them apart means the process view builds and runs
with the symbol crate absent or failing.

### The `Tree` trait

`traveler-layout` currently takes `&FileIndex` directly. Generalise it to the minimum both index
types can satisfy:

```rust
pub trait Tree {
    fn len(&self) -> usize;
    fn root(&self) -> NodeId;
    fn parent(&self, id: NodeId) -> NodeId;
    /// Children are always a contiguous id range. Enforced by construction in both impls.
    fn children(&self, id: NodeId) -> Range<u32>;
    fn depth(&self, id: NodeId) -> u16;
    fn weight(&self, id: NodeId) -> u64;
}
```

Six methods, all `#[inline]`, all a single array index. Layouts take `T: Tree` **generically, not
as `&dyn Tree`** — the treemap calls these once per node per layout and a vtable hop in that loop
is measurable at a million nodes.

Returning `Range<u32>` rather than an iterator is deliberate: it keeps the contiguous-children
invariant in the *type*, so the new index cannot quietly drop the property the whole layout
depends on.

`LayoutCtx` keeps its shape from PLAN.md §1 and gains the tree by generic parameter.

---

## 4. Acquisition

Two ways in, one thing out. `AttachedTarget` is "a stopped process I may read and write."

```
launch:  fork -> PTRACE_TRACEME -> execve  -> stopped at the execve stop  -\
                                                                            >- AttachedTarget
attach:  PTRACE_ATTACH(pid)               -> stopped                      -/
```

Everything downstream — maps, pagemap, memory reads, ELF parsing, the snapshot, the layout, the
diff — sits behind `AttachedTarget` and does not know which path produced it. The divergence is
about fifty lines. **Build launch first; attach is not a rewrite of anything.**

Why launch is first, concretely:

- **`ptrace_scope` is 1 on the dev machine** (and on most distros). A process may only trace its
  descendants. Attaching to an arbitrary PID is `EPERM` without `CAP_SYS_PTRACE`, root, or the
  target calling `prctl(PR_SET_PTRACER)`. A child you launched is always traceable.
- **You get a real t=0.** Stopping at the `execve` stop means capture 0 is the address space
  *before the dynamic linker has run*: the executable, the interpreter, a stack, and nothing else.
  That frame is unobtainable if you arrive late, and it is the cleanest baseline the app can have.

  **The matching t=1 is not free, and phase 6 as first scoped did not have it.** "Capture 1 after
  linking" needs a stop placed *between* `ld.so` finishing and `main` starting, and the only ways
  to place one are a breakpoint at the ELF entry point or the `_dl_debug_state` rendezvous — both
  execution control, which §1 otherwise defers to phase 9. Without one, capture 1 lands wherever
  the user presses the key: the libraries have still arrived, but it is not a clean before and
  after. The cheap version is a single `0xCC` at `e_entry`, taken from the header `traveler-sym`
  already parses in 6g, hit once and removed — about fifty lines, one `waitpid` return to
  classify, and no state machine. **Schedule it at the end of 6h**, because the diff of those two
  frames is the picture the launch path exists to produce and it should not wait for phase 9.
- **Write tracking needs that zero point.** Soft-dirty (§6) is a *relative* measurement: you
  clear at A and read at B. Attach late and the first capture has no baseline.
- **Reproducibility.** Launching owns argv, env, and cwd, and can disable ASLR
  (`personality(ADDR_NO_RANDOMIZE)` / equivalent), so two runs are comparable. Offer ASLR-off as
  a checkbox, default on, and label it — it changes the program's behaviour and hiding that
  would be dishonest.

**Attach still ships**, and it is what you need for a daemon that cannot be restarted, a process
someone else spawned, or something already misbehaving. It lands right after launch works.

### The multithreaded gotcha, which will bite

**On Linux, ptrace is per-thread.** Seizing the thread-group leader stops one thread; the others
keep running and keep mutating memory underneath your "consistent" read. Every capture must
enumerate `/proc/<pid>/task/*` and seize each TID, and must handle threads created or exited
between the enumeration and the seize (loop until the task list is stable across one pass). Get
this wrong and the failure is a torn snapshot that looks fine.

Windows has the same requirement with different plumbing: suspend every thread from a Toolhelp32
thread snapshot, or use the debug-event stop, which stops the whole process.

### What the stop actually is, which is not what was sketched

**`SIGSTOP` and `PTRACE_CONT`, not `SEIZE` and `INTERRUPT`.** `PTRACE_INTERRUPT` works only on a
tracee that arrived by `PTRACE_SEIZE`, and a tracee that arrived by `PTRACE_TRACEME` cannot be
seized afterwards — so the sketch above quietly asked for two stop mechanisms, one per acquisition
path, for no gain. Both paths freeze the same way instead, and so do helper threads.

**And the signal has to be thread-directed.** `kill(pid, SIGSTOP)` is process-directed: the kernel
gives it to whichever thread will take it, which on a multithreaded target is usually not the
leader, and `waitpid` on the leader then blocks forever. It hangs rather than failing, which is
exactly the kind of thing this file exists to write down. Use `tgkill(pid, pid, SIGSTOP)`.

**A launched target is killed when Traveler drops it.** `PTRACE_O_EXITKILL` covers a crash, but a
clean drop detaches first and clears it, which left a parked fixture behind after every test until
the drop was made to kill and reap what it launched.

### Being traced is observable

`TracerPid` in `/proc/self/status` is nonzero, a second `PTRACE_TRACEME` fails, `IsDebuggerPresent`
returns true. Programs that check will behave differently under Traveler, and for "unfamiliar
binary" some of the most interesting ones check. This is a property of the view, not a bug:
**surface it in the HUD** ("observed under trace") rather than leaving it implicit.

---

## 5. The snapshot

A capture is immutable, exactly as `FileIndex` is immutable. Struct-of-arrays, `NodeId`-indexed,
parents before children, children contiguous — the same shape and for the same reasons, so
`subtree_tops`, the size rollup, and the LOD walk all work verbatim.

It splits in two, and the split is the fix for a claim the first draft of this file got wrong. A
**frame** is the ordered set of extents that ids are assigned over; an **`AddressSpace`** is one
capture's state projected onto a frame.

```rust
/// What ids belong to, and what a layout is a pure function of. Built from one capture, or from
/// the union of several (§8). This is what implements `Tree`.
pub struct Frame {
    // Tree, parallel to NodeId. Same discipline as FileIndex.
    parent: Vec<NodeId>,
    child_start: Vec<u32>,
    child_len: Vec<u32>,
    depth: Vec<u16>,

    // The interval. This is the weight.
    addr: Vec<u64>,
    len: Vec<u64>,

    kind: Vec<NodeKind>,      // Region Mapping Section Gap
    label: Vec<StrRef>,       // interned, same blob trick as FileIndex::names
    backing: Vec<FileRef>,    // interned path + file offset, or NONE
}

/// One capture, over a frame. Every column is parallel to that frame's ids.
pub struct AddressSpace {
    frame: Arc<Frame>,

    /// False where the frame holds an extent this capture did not. "Added" and "removed" in the
    /// diff are exactly this bit differing between two captures over one frame.
    present: Vec<bool>,
    /// Per capture, not per frame: `mprotect` changes it without moving a boundary.
    prot: Vec<Prot>,          // r w x, plus private/shared

    // Page state, held per mapping as bitsets rather than per page as nodes. See §9.
    page_bits: PageBits,
    /// Mappings too wide to read pages for. See §7 — deliberately not the same thing as a
    /// mapping whose pages are all absent.
    pages_elided: Vec<bool>,

    pub caps: Caps,           // what this backend could actually answer
    pub meta: CaptureMeta,    // pid, wall clock, monotonic, stop duration, tid count
}
```

**Node ids belong to the frame, not to the capture, and the difference is load-bearing.**

Within a frame ids come from address order rather than discovery order: sort the extents, then
assign ids breadth-first down the containment tree. Deterministic, and identical inputs give
identical ids. What that does *not* give — and the first draft claimed it did — is stability
across captures. One new mapping at a lower address shifts every id above it, and the `Tree`
contract (children contiguous, parents before children) means a new extent cannot be appended to
a frame, only renumbered into it. So "the mapping at `0x7f2a1000` keeps its `NodeId`" is false of
any scheme that numbers each capture on its own, and the diff cannot be built on it.

The diff therefore never compares ids across captures. A capture is a set of **address-keyed
extents**; matching one onto a frame is done by address; and an id means nothing except relative
to the frame that minted it. Growing a frame is a renumber, and a renumber invalidates
`Placements`, `subtree_top`, `City::entities`, and every painted id — a full rebuild rather than
an incremental one, which is why §8 treats framing as something done once for a set of captures
rather than once per capture.

### Capability flags, not lowest common denominator

The two platforms genuinely differ, and flattening to the intersection would throw away the best
data Linux has. `Caps` is a bitset the backend fills in and the HUD reads:

```
PAGE_RESIDENCY    both
PAGE_SWAPPED      linux
SOFT_DIRTY        linux, and only with CONFIG_MEM_SOFT_DIRTY
PAGE_SHARED       both
WRITE_TRACKING    linux only  (see §6 — Windows has no equivalent for another process)
BACKING_PATH      both
```

A view that needs a capability the backend lacks says so in the panel. It does not silently
render zeros, and it does not get compiled out.

---

## 6. Collection, per platform

### Linux

| what | source | notes |
| --- | --- | --- |
| mappings | `/proc/<pid>/maps` | text, ~10k lines on a big process. Parse it yourself; the format is fixed and a hand parser beats a regex by an order of magnitude. |
| per-mapping RSS | count present bits in `pagemap` | **not** `smaps`: smaps makes the kernel walk page tables per mapping and is slow. `smaps_rollup` is fast but whole-process only. Since pagemap is being read anyway, derive RSS from it. |
| page state | `/proc/<pid>/pagemap` | 8 bytes per page at offset `(vaddr/4096)*8`. Bit 63 present, 62 swapped, 61 file/shared-anon, 56 exclusive, 55 soft-dirty. PFN bits are zeroed without `CAP_SYS_ADMIN` and are not needed. One `pread` per mapping extent. **Priced by mapped VA, not by RSS, which is not a detail — see §7.** |
| write tracking | write `4` to `/proc/<pid>/clear_refs`, read bit 55 later | Costs the target: it write-protects the page tables, so the program takes extra faults afterwards. Make it **opt-in per capture** and say what it costs. |
| memory read | `process_vm_readv` | One syscall, many iovecs. Fall back to `pread` on `/proc/<pid>/mem`. |
| memory write | `pwrite` on `/proc/<pid>/mem` | Deliberately **not** `process_vm_writev`, which cannot write a read-only page. `/proc/pid/mem` goes through `FOLL_FORCE` and can, which is how gdb patches `.text`. |
| threads, stacks | `/proc/<pid>/task/*`, `PTRACE_GETREGSET` | Stack extent is `[sp, stack_top]`; the rest of the stack mapping is untouched guard space and should read as such. |

**Transparent huge pages cannot be detected from `pagemap`, and the first draft of this file said
they could.** A pagemap entry carries present, swapped, file/shared-anon, exclusive, soft-dirty
and a PFN that is zeroed without `CAP_SYS_ADMIN`. There is no huge-page bit. `KPF_THP` lives in
`kpageflags`, which needs both the real PFN and `CAP_SYS_ADMIN`, and the only unprivileged source
is `AnonHugePages` in `smaps` — which the row above rejects for RSS because it walks page tables
per mapping, and the objection is the same here. So THP is **not detected**: a 2 MiB huge page
reads as 512 present 4 KiB pages, which is honest rather than wrong, and the LOD budget in §9
already bounds what that costs to draw.

### Windows

| what | source | notes |
| --- | --- | --- |
| mappings | `VirtualQueryEx` loop | Walk by `BaseAddress + RegionSize`. `MEMORY_BASIC_INFORMATION` gives State (COMMIT/RESERVE/FREE), Protect, Type (IMAGE/MAPPED/PRIVATE), and **AllocationBase — which gives the region level of the hierarchy for free.** |
| backing file | `GetMappedFileNameW` + `QueryDosDevice` | Returns a device path; convert to a DOS path or the label is unreadable. |
| page residency | `QueryWorkingSetEx` | Batched array of `PSAPI_WORKING_SET_EX_INFORMATION`: Valid, Shared, ShareCount, Win32Protection, Locked. This is the `pagemap` analogue. |
| write tracking | **none** | `GetWriteWatch`/`ResetWriteWatch` only work on memory the target itself allocated with `MEM_WRITE_WATCH`. There is no per-page dirty bit for an arbitrary process. Clear `WRITE_TRACKING` in `Caps`. |
| memory read | `ReadProcessMemory` | |
| memory write | `WriteProcessMemory`, `VirtualProtectEx` around read-only pages | Restore protection afterwards. |
| stop | debug event, or `SuspendThread` over a Toolhelp32 thread snapshot | |

**The write-tracking asymmetry is the one real platform gap, and it must not be papered over.**
The fallback, if it is ever wanted, is hashing pages at capture time and comparing hashes — exact,
but it reads the entire working set per capture and is far too slow to be a default. Offer it for
one selected mapping, never for the whole space, and only if someone actually asks for it.

---

## 7. Stop time is the budget

The target is frozen for the duration of a capture. That is the cost the user pays, so it is the
number to optimise, and it produces one non-negotiable rule:

> **While stopped: read raw bytes only. Parse nothing. Resume, then parse.**

With one exception the rule cannot avoid: `pagemap` is indexed by address, so the ranges have to
be known before the read the stop exists for. What runs inside the window is a scan of the first
field of each `maps` line and nothing else, and it accepts exactly the lines the full parser
accepts, so the two can never disagree about how many mappings there are — which would silently
shift every mapping after the disagreement. There is a test for that.

Slurp `maps` into a `String` and the pagemap extents into a `Vec<u64>`, resume the target, and do
all parsing, tree building, interning, and layout afterwards on data you already own. Parsing
`maps` while the target waits is pure dead time and it is the easy mistake.

And one rule about what a capture is allowed to change:

> **A capture observes; it does not decide whether the target runs.**

Whether the target is running after a capture is exactly whether it was running before. A capture
of a running target stops it, reads, and resumes it inside the window below; a capture at the
`execve` stop leaves it stopped, so capture 0 stays inspectable. Any other policy either makes the
launch path's t=0 unusable or leaves the target frozen behind the user's back.

Budget for a target with 1 GB resident and 4 GB mapped, to be measured and recorded here the way
PLAN.md records everything else:

| step | target |
| --- | --- |
| seize all threads | < 5 ms |
| read `maps` | ~1 ms |
| read pagemap (1M *mapped* pages = 8 MB) | 60-140 ms, measured |
| resume | < 1 ms |
| **total stop** | **< 50 ms** |
| parse + build + lay out, after resume | < 100 ms |

If total stop exceeds ~100 ms on a realistic target, something is being parsed inside the window.

Measured at 6c, before pages, on Linux 7.0 with `ptrace_scope` 1:

| target | threads | mappings | mapped | stop |
| --- | ---: | ---: | ---: | ---: |
| `python3` holding a 1 GiB `bytearray` | 1 | 44 | 1.0 GiB | 0.39-0.58 ms |
| the same, with 8 spinning threads | 9 | 76 | 1.6 GiB | 0.38-0.63 ms |
| the capture fixture | 5 | 21 | 1.3 MiB | under 0.1 ms |

Stop time at this phase is the `maps` read plus one seize per thread, and neither scales with
mapped bytes — nine threads cost no more than one. The number that will actually move is the
pagemap read in 6e, which is why the cap below exists before the code that needs it.

### Pagemap is priced by address space, not by working set

The pagemap read costs `mapped_bytes / 4096 * 8`, and **mapped is not resident**. That is the one
number in this file it is easy to size against the wrong quantity, and getting it wrong does not
produce a slow capture, it produces one that never returns. A 4 GB mapping holding 40 MB costs
8 MB of pagemap; a JVM or a Go runtime reserves its arena up front; an ASan-instrumented binary
reserves roughly 20 TB of shadow, which is 40 GB of pagemap for a single line of `maps`. §8 treats
"4 GB reserved and 40 MB resident" as a picture worth drawing, so these are not exotic targets —
they are the ones the view is for.

So the page read is capped twice, and both caps are visible rather than silent:

- **Per mapping.** A mapping wider than `page_detail_bytes` does not get its pages read. It keeps
  its extent, permissions, label, sections and its place in the city, and sets `pages_elided`
  (§5), so the readout says "pages not read" rather than showing an empty plate. Start at 1 GiB,
  which is 2 MiB of pagemap for the widest mapping that still qualifies.
- **Per capture.** Mappings are read **smallest first** until the budget is gone, which maximises
  how many mappings get detail instead of letting one reservation eat the window.

Raising either for one selected mapping is an explicit action on a key, exactly as the duplicate
scan is: the cost is the user's to spend once they know what it buys.

**Both caps are counted in pages, not in bytes of pagemap, because pages are what the read costs.**
The kernel walks page tables to answer, so the price is per entry:

| target | mappings | pages read | stop |
| --- | ---: | ---: | ---: |
| `gdb`, 51 MiB mapped | 283 | 13k | 4.5 ms |
| `python3`, 1 GiB resident | 45 | 262k | 15-36 ms |
| the same, 9 threads | 77 | 262k | 15-16 ms |

Two things fall out of those numbers, and both changed the code:

- **The estimate above was optimistic by about five times.** 262k pages cost 15-36 ms, so a
  million would be 60-140 ms rather than 10-25, which is past the point §7 calls a red line. The
  defaults are set from the measurement rather than from the estimate: **1<<19 pages of budget**
  (2 GiB of address space, about 68 ms at the slow end) and **1<<20 pages of detail cap**.
- **The spread on one target is transparent huge pages.** The same `python3` with the same
  resident set costs 15 ms on one run and 36 ms on another; a 2 MiB huge page is one page-table
  entry to walk instead of 512. It is not something to tune against, only to know about, and it is
  the same THP the file used to claim was detectable (§6).

There is also a per-mapping term: `gdb`'s 283 mappings cost 4.5 ms for only 13k pages, because
each mapping is its own `pread`. Nothing needs doing about it yet — it is well inside the budget —
but it is why the cost model is two terms and not one.

**A first version of the cap was set at 1 GiB per mapping and was wrong in the interesting
direction:** it elided the gigabyte heap, which is the mapping anyone would have opened the view
to look at, while costing only 2 MiB of pagemap. A cap that excludes the subject is worse than no
cap. The budget, taken smallest-first, is what actually bounds the stop; the detail cap only
excludes reservations no budget could afford.

---

## 8. Layout: ordered treemap, and the union frame

New layout in `traveler-layout`, alongside `city`. Same `Layout` trait, same `Placements` out.

**Squarified is wrong here and must not be reused.** It sorts by weight, which throws away the
address order that is the whole point, and its row breaks move when weights change, which makes
two captures incomparable.

**Use a strip treemap ordered by address.** Items are laid in address order into strips, and
nothing is ever sorted. Properties:

- **Placement order is address order, and the picture never doubles back.** That is a genuine
  analytic property, not decoration: you can see that the heap sits below the libraries, that the
  mmap arena is one contiguous band, that a region grew *into* the gap after it.
- Aspect ratios are worse than squarified, so they get measured rather than assumed.

Hilbert-curve layout was considered and rejected: better 2D locality, but the reading order is
unlearnable, and "where is this address" stops having an answer a person can follow.

### What the strips had to become, measured in 6d

The textbook strip treemap — full-width rows, broken on mean aspect ratio — reads literally left
to right and top to bottom, and on a real address space it is not good enough: **median aspect
6.61, p99 1293, 11.6% hairlines** on a `python3` holding a gigabyte. The cause is structural
rather than a tuning miss. One region holding 99.9% of the bytes takes a full-width row, and
everything else is left to tile a strip one unit tall.

Two changes, each kept because the numbers improved and for no other reason:

- **Strips run along the shorter side of what is left of the rect**, which is what squarify does
  with its leftover. Median 6.61 → 2.79, worst 1293 → 80. The cost is that a strip may now be a
  column as easily as a row, so the promise weakens from "left to right, top to bottom" to
  "along a strip, then the strip beyond it, never backwards". The order is still total and there
  is a test for it; the reading path is one turn less obvious.
- **Breaks are chosen on the worst aspect in the strip, not the mean.** On `gdb` that is median
  2.02 against 2.12, p99 48 against 66, and 0.7% hairlines against 1.4%. Squarify's break rule and
  squarify's sort are separable, and only the sort is the half that would lose the address order.

Measured with `--no-aslr` so the runs are comparable, at the mapping level:

| target | tiles | median | p90 | p99 | worst | over 50:1 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `gdb`, 52 libraries | 284 | 2.02 | 7.9 | 48.3 | 322.9 | 0.70% |
| `python3`, 1 GiB `bytearray` | 44 | 2.73 | 34.9 | 97.3 | 97.3 | 6.8% |
| `node` | 103 | 3.54 | 52.3 | 2630 | 3713 | 11.7% |
| `gdb` again, with 6g's sections | 650 | 2.22 | 21.1 | 321.8 | 1321 | 5.1% |

The last row is the same target one level deeper, and it is the honest cost of tier 2: the leaves
are sections now, and within one mapping a `.text` and a `.plt` differ by three orders of
magnitude. Emitting *every* section gives median 4.50 and 8.7% hairlines; a **one-page floor**,
below which a section falls into the gap around it rather than getting its own tile, gives the
2.22 above while still keeping two and a half times as many nodes as the mapping level alone. A
page is the granularity of the whole view, so a section smaller than one could never have a tile
worth looking at.

**The remaining spread is the data, not the heuristic, and that is worth being precise about.**
The city reaches median 1.03 because a folder holds hundreds of files of similar size, which gives
the packer room. A region holds three to five mappings whose sizes differ by three orders of
magnitude — a 4 KiB guard beside a 1.5 MiB `.text` — and no area-proportional layout of four such
items in one rect produces squares. `gdb`, the target with the most tiles and therefore the most
freedom, is the one that reaches 2.0.

If this needs to improve further the next thing to try is the pivot algorithm (Shneiderman and
Wattenberg's ordered treemap), which is still order-preserving and reports better aspect ratios
than strips. It is not free: its reading path is a recursive arrangement rather than a sequence of
strips, which spends more of exactly the property §8 exists to keep. Measure before adopting it.

### The union frame is what makes diff cheap

A layout is a pure function of a **frame** (§5): the ordered set of extents ids are assigned over.
For a diff, build one frame covering both captures and lay out once. Both captures then render
into byte-identical geometry — not because two layouts agree, but because there is only one.

**The frame is built over the union of both captures' extent *boundaries*, not over the union of
their extents.** The distinction does real work. A mapping that grew from `[a,b)` to `[a,c)` is
not one extent that changed size; in the frame it is two, `[a,b)` present in both captures and
`[b,c)` present only in the later one. Growth and shrinkage then stop being states that need an
encoding of their own and become what they actually are: an unchanged block with a new sliver
welded to its edge, drawn at the address the memory actually appeared at. Splitting on boundaries
is also what keeps area additive across a diff, since every frame extent is then wholly present
or wholly absent in each capture and there is no partial node to account for.

- **The diff is a recolour pass over one `Placements`.** No second layout, no interpolation, no
  correspondence problem. This is `paint_selection` doing the same job it already does.
- Added and removed fall straight out of `present[i]` differing between two captures over one
  frame. Grown and shrunk are read off adjacency, per the split above.

For a sequence of captures, frame the union of all of them. **Extending a frame is a renumber and
therefore a full rebuild** — new ids, new `Placements`, respawned entities, repainted state (§5)
— so it is not something to do per capture. Take the captures first and frame them together, and
accept one rebuild when a capture brings a boundary the frame has never seen. Either way **layout
is per-frame, never per-capture.**

Elevation reuses the city's scheme: base at `depth * level_step`, so region slabs carry mapping
slabs carry pages. Thickness is a free channel again; default it to resident fraction, so a
mapping that is 4 GB reserved and 40 MB resident reads as a wide flat plate rather than a tower.
Keep it a toggle, as `HeightMode` already is; in diff mode written-since takes the channel instead
(§10), which is a third setting of that same toggle rather than a second claim on it.

---

## 9. Rendering: pages come from a pool

A 1 GB working set is 262k pages; 4 GB mapped is a million. PLAN.md §5 measured spawning at about
1.5 s per 227k entities, and a capture must not cost that.

**Pages are not materialised as nodes.** The snapshot holds regions, mappings, and sections
explicitly — thousands of nodes, spawned per capture as today — and page state as bitsets per
mapping. Page *blocks* are drawn from a **recycled entity pool sized to the LOD budget**, exactly
the pattern `satellite.rs` already uses for cluster blocks: spawned once, hidden, repositioned as
you fly. The LOD walk already bounds how many nodes are on screen, so the pool bound and the LOD
bound are the same number.

This is the one place the "one entity per node" model from phase 3 does not carry over, and it is
worth the exception: it turns a per-capture 1.5 s spawn into a per-frame transform write over a
few thousand pooled blocks.

**Pages therefore have no `NodeId`, and that reaches further into the app than the pool does.**
`Hover`, `Pinned`, `paint_selection` and the HUD readout are every one of them keyed on `NodeId`
today, and a page is not one. The honest fix is a single type threaded through all of them:

```rust
pub enum Target {
    Node(NodeId),
    /// Page `page` of mapping `mapping`. Resolved against that mapping's bitsets on demand,
    /// never stored, so a million pages cost nothing until one is pointed at.
    Page { mapping: NodeId, page: u32 },
}
```

`Hover.node: Option<NodeId>` becomes `Hover.target: Option<Target>`, the pin holds a `Target`, and
the readout matches on it. `City::entities[]` stays a plain `NodeId` lookup and simply has no
entry for a page; pool slots carry their own `Target`, the way `SatelliteBlock` slots already
carry the member they stand for.

**The LOD traversal is *not* unchanged**, and the first draft of this file was wrong to say so.
Containment holds (§2), so the top-down early-out is still valid and picking is still exact and
free — that much transfers verbatim. What is new is one branch at the bottom of the walk: a
mapping expanded past the pixel threshold has no child nodes to push, so it synthesises page rects
from its bitsets into pool slots and ray-tests those instead. That is a bounded, self-contained
addition to `update_lod` rather than a rework of it, but it is an addition, and 6e is costed with
it in.

---

## 10. Colour

**The palette conclusion from PLAN.md §5 is binding and must not be re-litigated.** No fourth hue
clears the colourblind and normal-vision floors against a validated first three on this dark
surface. Three hues plus neutrals, and identity comes from the crosshair readout.

The three slots map onto **permissions**, which is the most informative three-way split for an
unfamiliar program, and they reuse the existing validated colours rather than picking new ones:

| slot | means | existing colour |
| --- | --- | --- |
| executable | `r-x`, `.text`, PLT | code blue |
| writable | heap, stack, `.data`, `.bss`, private anon | media orange |
| read-only | `.rodata`, file-backed read-only, constants | binary green |
| neutral | reserved, `PROT_NONE` guards, gaps, uncommitted | the existing neutral |

Gold highlight and violet pin keep their meanings from phases 4 and 8.

**Diff mode replaces the categorical encoding rather than layering on it.** The permission hues
switch off entirely and a **diverging scale** takes over. Boundary splitting (§8) is what makes it
fit: there are not six diff states needing an encoding, there are three that belong on a ramp —
**added, unchanged, removed** — with unchanged at the neutral midpoint, because grown and shrunk
are already drawn as added and removed slivers beside an unchanged block. Written-since rides a
separate channel (thickness, or a border), never a hue. A permission change moves no boundary and
so is not on the ramp at all; the crosshair reports it, as identity is reported everywhere else
in this design.

A diverging ramp is a different encoding channel from categorical hue and is not subject to the
all-pairs constraint, because the ordering does the work that separation would otherwise have to.
That was an argument rather than a measurement, and §18 does not allow the difference, so the ramp
went through the same validator the three hues did. It did not need the exemption:

| pair | normal ΔE | deuteranope ΔE |
| --- | ---: | ---: |
| added / unchanged | 22.2 | 23.9 |
| added / removed | 48.0 | 65.2 |
| unchanged / removed | 31.6 | 33.7 |

Contrast on the surface is 4.79, 3.45 and 4.48 to one. The floors are the ones PLAN.md §5 recorded
for the categorical set — 20.9 normal, 9.4 CVD, 3:1 contrast — and every pair clears all three.

**It reads better to a deuteranope than to normal vision, and that is the reason rather than a
fluke.** The ends of the ramp are code blue and media orange, the two of the three validated hues
that sit on the blue-yellow axis, which is the axis deuteranopia preserves; collapsing the other
axis loses nothing that this ramp was using. The validator is checked in with a negative control
that asserts it rejects a red-to-green ramp, because three passing tests prove nothing if the
thing cannot fail.

---

## 11. Symbols, in three tiers

`traveler-sym` parses the file on disk, never the live process. Everything is lazy and cached per
image, keyed by (path, build-id or timestamp) — a process maps 30-100 files and each is parsed
once.

**Tier 1 — always available.** No symbol work at all: mappings, permissions, sizes, page state,
and the backing path. The view is complete and useful at this tier and must remain so.

**Tier 2 — nearly always available, and the best value in the whole plan.** Parse the ELF or PE
itself with `object`:

- **Sections.** `.text`, `.rodata`, `.data`, `.bss` become children of the mapping, so a library
  stops being one grey slab and becomes a readable strip of code, constants, and data.
  `.bss` is not in the file — it is anon memory beyond `p_filesz`, so compute it from
  `p_memsz - p_filesz` or it goes missing exactly where it matters.
- **Exported symbols.** `.symtab` if present, else **`.dynsym`, which survives stripping** in any
  dynamically linked binary. Sort by address, binary search on the crosshair address, and you get
  function names inside `.text` with no debug info anywhere on the machine.
- **Separate debug files.** `.note.gnu.build-id` points at `/usr/lib/debug/.build-id/...`. Cheap
  to check, and on distro binaries it upgrades tier 2 to tier 3 for free.

**Tier 3 — when debug info exists.** `gimli` + `addr2line` for DWARF, `pdb` for PDB: types,
struct layouts, locals, line numbers. This is what makes "poke a variable" mean a variable rather
than an address. **Deferred to phase 8** — it is a large subsystem and tiers 1 and 2 must be solid
and shipped first.

The tier in use is shown in the HUD. A person needs to know whether they are reading a name or a
guess.

---

## 12. Poking

Snapshot semantics make this nearly free, which is the payoff for choosing them.

A poke happens **at a stop**, so there is no race with the target's threads, no torn write, and no
need to reason about memory ordering against a running program. The flow is: stop (already
stopped, if a capture is being viewed), write, resume, and mark the affected extent dirty in the
view.

- Linux: `pwrite` to `/proc/<pid>/mem`, which can write read-only pages.
- Windows: `WriteProcessMemory`, wrapped in `VirtualProtectEx` if the page is not writable, with
  protection restored afterwards.

Without tier 3 symbols a poke is "write these bytes at this address", typed by the user
(u8/u16/u32/u64/f32/f64/bytes/utf8). With tier 3 it becomes "set this field to this value".
Build the untyped version first; the typed one is a formatting layer over it.

**The seam for full debugger control (phase 9)** is `AttachedTarget`. It already owns stop, resume,
read, write, and the thread list. Breakpoints add: an instruction-patch table (`0xCC` on x86-64),
a stop-reason enum on resume, single-step, and stack unwinding. None of that changes the capture,
layout, or render path — which is the test of whether this seam is in the right place.

---

## 13. The pointer graph — phase 7

This is where the Obsidian half lives, and it is the feature that makes the process view more than
a prettier `pmap`.

Scan a captured region for 8-byte aligned words whose value falls inside another mapped extent.
Each hit is a candidate pointer. Aggregate to **(source mapping -> target mapping) edges with
weights**, which is a grouping and therefore stays inside PLAN.md §6's n² rule; drill into a
selected mapping to see individual pointers as lines, where the count is bounded by the selection.

Why it matters: for a binary with no symbols, **the pointer topology is the clearest available
statement of what the program's data actually looks like.** Stacks into heap, heap into heap,
GOT entries into libraries, vtables into `.rodata`. Structure, from raw bytes, with no debug info.

It costs a full read of the resident set, so it follows the duplicate finder's precedent exactly:
an explicit action on a key, run on a worker thread, result dropped in when ready, nothing blocks.
That machinery already exists in `edges.rs` and should be reused rather than re-invented.

False positives: a 47-bit sparse address space makes a random aligned word landing inside a mapped
extent unlikely, so precision is good without any heuristics. Report the rate rather than assuming
it.

---

## 14. App restructure

The scan currently runs before `App::new()` and blocks the window. A launcher makes that
untenable, so the limitation PLAN.md §5 recorded gets fixed here because it is now forced.

**Ask at startup what the user intends, then collect what that choice needs.**

```
traveler                        launcher
                                  ├─ disk        -> path
                                  └─ process     ├─ launch  -> command, args, cwd, env, ASLR
                                                 └─ attach  -> process list / pid

traveler --scan <path>          straight to the disk city
traveler --run <cmd> [args]     straight to process mode, launched
traveler --attach <pid>         straight to process mode, attached
```

Flags short-circuit the launcher entirely, which keeps `--shot` and the rest of the screenshot
automation from PLAN.md §5 working unchanged. **That is a hard requirement, not a nicety** — the
measurement workflow depends on it.

Use `bevy::state` for the mode machine (`Launcher / Loading / Disk / Process`) and gate the
existing systems with `.run_if(in_state(...))`. Scans move onto a worker thread with a `Loading`
state, following the `DupeState` pattern already in `edges.rs`: an `Arc<Mutex<Option<T>>>` slot
polled each frame.

**Captures do not, and cannot.** ptrace attaches a tracee to exactly one tracer *thread*, and a
request from any other thread is `ESRCH`, so a target acquired on a worker could never be captured,
resumed, or poked from anywhere else. Acquisition and every later capture therefore run on the main
thread, which costs what §7 measures — half a millisecond, a fraction of a frame. If 6e's pagemap
read makes that a dropped frame, the answer is one thread that owns the target for its lifetime and
talks over a channel, not a worker per capture.

Process enumeration for the attach picker: `/proc/*/comm` plus `/proc/*/cmdline`, or Toolhelp32.
Cheap, and it does not need ptrace, so the list is populated even where attaching would fail —
mark the ones that would fail rather than hiding them.

---

## 15. Testing

Keep the discipline from phases 1-5: headless, property-based, and asserting the actual claim
rather than the implementation.

- **A fixture target.** A tiny helper binary that mmaps a known set of regions with known
  permissions, touches a known subset of pages, and parks. Then assert the snapshot sees *exactly*
  those. This is the analogue of "byte-exact against PowerShell" from phase 1, and it is the check
  that will catch the real bugs — the ones where a plausible-looking city is quietly wrong.
- **Layout invariants**, mirroring `tests/city.rs`: containment, sibling non-overlap, exact area
  conservation, determinism, world bounds, and two new ones —
  **order preservation** (reading order equals address order) and
  **frame completeness** (the union frame of A and B carries every boundary of both, and each
  capture projects onto it with every one of its bytes landing in some extent and no extent
  straddling a boundary the other capture has).

  The obvious third test does not exist, and the reason belongs in the file. "Lay out A, lay out
  B, lay out their union, and check the shared extents land in the same place" **cannot pass**: a
  treemap scales weights to fill its rect, so the moment the union holds an extent A does not,
  A's own layout and the union's disagree everywhere, by construction and not by bug. Writing that
  test would cost an afternoon and teach nothing. Completeness plus a lossless projection is the
  property that actually matters, and geometric identity then follows for free from §8, because
  there is only ever one layout to be identical to.
- **Diff correctness** over synthetic captures: added and removed asserted directly on
  `present`, and grown and shrunk asserted as the split they are supposed to become — a mapping
  that grows must yield an unchanged extent and an added one that are address-adjacent, not one
  extent that changed length.
- **Symbols**: parse a known ELF and a known PE, assert section VAs and a few symbol addresses.
- **Multithreaded stop**: a fixture with N threads writing continuously; capture, and assert the
  snapshot is self-consistent. This is the test that catches the per-thread ptrace gotcha (§4).

---

## 16. Phases

| | | |
| --- | --- | --- |
| ~~**6a**~~ | `traveler-core` + `Tree` | **Done.** Mechanical. Move `NodeId` and the formatters, add the trait, make `FileIndex` implement it, generalise the layouts. No behaviour change, tests stay green. |
| ~~**6b**~~ | Launcher, states, non-blocking scan | **Done.** Mode machine, worker-thread scan, flag short-circuits. Disk mode must behave exactly as it does today. |
| ~~**6c**~~ | `traveler-proc`: acquire + capture, Linux | **Done.** Launch and attach, all-threads stop, `maps` -> `AddressSpace` at region/mapping level. No pages. The fixture target and its tests landed here, and caught three things argument had not: the process-directed signal hang (§4), the anonymous region limit (§2), and that a dropped target has to be killed. |
| ~~**6d**~~ | Ordered treemap + render | **Done.** Strip layout, the permission palette, the address space on screen, and the launcher's third choice. Aspect ratios measured, and the heuristic changed twice because of them (§8). |
| ~~**6e**~~ | Pages | **Done.** pagemap under caps counted in pages (§7), bitsets, `Target` threaded through hover/pin/HUD and the extra branch on the detail pass (§9), pooled page blocks, soft-dirty opt-in, resident fraction on the thickness channel. Stop time measured, and the cost model rewritten because of it. |
| ~~**6f**~~ | Windows backend | **Written and compile-checked, not run.** `VirtualQueryEx`, `QueryWorkingSetEx`, Toolhelp32 suspend, `GetMappedFileNameW` with the DOS-path conversion. Cross-checked with `cargo check --target x86_64-pc-windows-msvc`, which is what proves the abstraction; runtime correctness needs a Windows machine and is unverified. |
| ~~**6g**~~ | `traveler-sym` tiers 1-2 | **Done.** Sections as a third level with gaps filling what they do not cover, `.dynsym` names on the crosshair, images parsed once and keyed by path and mtime, the tier stated in the HUD. `gdb` goes from 361 nodes to 892. |
| ~~**6h**~~ | Capture and diff | **Done, less the breakpoint.** Multiple captures on a key, boundary-split union frame, the diverging encoding measured against the same floors as the categorical hues (§10), the view named in the HUD. The single-shot entry-point breakpoint from §4 is **not** built: without it capture 0 is where the user pressed the key rather than the `execve` stop, so the linking diff is available from `--run` only by taking a capture immediately. |
| ~~**6i**~~ | Poke | **Done.** Untyped writes at a stop, on both backends. `P` on a pinned node or page, with the width said out loud and a value too wide for its type refused rather than truncated. |
| **7** | Pointer graph | Worker thread, aggregated mapping-to-mapping edges, drill-down within a selection. |
| **8** | Tier 3 symbols | DWARF and PDB. Types, locals, line numbers. Typed pokes. |
| **9** | Full debugger control | Breakpoints, stepping, watchpoints, unwinding, behind the existing `AttachedTarget` seam. |

**On 6f's position:** "abstracted from day one" means the trait and the snapshot struct are
designed against *both* capability lists from 6c (which §6 above does), not that both backends are
written simultaneously. Writing Linux first and Windows immediately after the shape stabilises —
and before 6g/6h build on it — is what keeps the abstraction real instead of a Linux shape with
Windows stuffed into it.

**It did its job, and it cost the shape one field.** The snapshot needed a `group` on a mapping:
the allocation the operating system says it came from, `AllocationBase` on Windows and 0 on Linux,
which falls back to §2's adjacency rule. That is the shape of the whole design in one field — the
platform that records something says it, the platform that does not degrades to an inference,
and neither is flattened to the intersection. Had 6g and 6h been built first, that field would
have arrived as a retrofit.

**And it is unrun.** Cross-compiling proves it builds and that the types fit; nothing here has
executed a Windows syscall. Everything in this file that says "measured" is Linux.

---

## 17. Risks and known asymmetries

| risk | mitigation |
| --- | --- |
| Windows has no per-page write tracking | Capability flag, and 6f sets neither `SOFT_DIRTY` nor `WRITE_TRACKING`. Degrade the view honestly; optional page-hash diff for one selected mapping only. |
| `ptrace_scope` blocks attach | Launch path needs no privileges and is the common case. Detect and explain, do not just fail. |
| Per-thread ptrace produces torn snapshots | Seize every TID, loop until the task list is stable, and test it against a fixture that is actively writing. |
| `clear_refs` slows the target | Opt-in per capture, with the cost stated in the UI. |
| Soft-dirty absent (`CONFIG_MEM_SOFT_DIRTY` off) | Runtime probe, capability flag, degrade. |
| Anti-debug targets behave differently | Unavoidable. Surface "observed under trace" in the HUD. |
| Strip treemap aspect ratios worse than squarified | Measured in 6d and improved twice on the numbers: median 6.6 → 2.0-3.5 depending on target (§8). The rest is extreme size disparity across few siblings, not a heuristic failure. Pivot is the next thing to try, and costs reading order. |
| A million pages per capture | Pages are bitsets, not nodes; blocks come from an LOD-bounded pool (§9). |
| Pagemap is priced by mapped VA, not by RSS | Per-mapping width cap and a per-capture budget, smallest first (§7). `pages_elided` per node so the gap is stated, and raisable for one selected mapping. |
| A new extent renumbers the whole frame | Ids belong to the frame, not the capture (§5). Extending one is a deliberate full rebuild, done for a set of captures rather than per capture. |
| ASLR makes runs incomparable | Offer `ADDR_NO_RANDOMIZE` on the launch path, default on, clearly labelled. |

---

## 18. Code standards for this work

The existing code sets the bar and the bar is the point. Matching it is not optional.

- **Struct-of-arrays, `NodeId`-indexed, parents before children, children contiguous.** These are
  not stylistic; the size rollup, `subtree_tops`, and the LOD walk are all one-pass *because* of
  them.
- **Intern strings into one blob** with `(offset, len)` refs. No `PathBuf` per mapping, no
  `String` per node.
- **No allocation inside per-node loops.** Reuse scratch buffers across nodes, as `city.rs` does
  with `items` and `placed_rects`.
- **Nothing parses while the target is stopped.** §7.
- **Capability flags over lowest common denominator.** A backend says what it can do; callers
  degrade explicitly.
- **Comments record why, and what did not survive contact.** The existing comments are the most
  valuable thing in the repo for anyone picking it up. Every non-obvious constant should say what
  it was before and why it moved.
- **Measure, then tune, then record the numbers in this file.** Every table in PLAN.md is a real
  measurement. Do not add one that is not.
- **Minimal surface.** A trait gets the methods two implementors both need, and no more. Generic
  over `T: Tree` in hot paths, `dyn` only where the call is rare.
- **Tests assert the claim, not the implementation**, and the threshold goes in after measuring,
  as `tests/satellite.rs` did with its 1.4x guard.

---

## 19. Deferred, with the reason

- **Heap object granularity.** Needs allocator interception or arena walking: invasive, and
  version-fragile against glibc internals. Revisit once tier 3 symbols exist, since types are what
  would make individual objects legible anyway.
- **Continuous live view.** Ruled out by the snapshot model. If it ever returns it is a *third*
  mode, not a change to this one, and it brings back every stability problem §8 avoids.
- **System-wide process graph.** Different app.
- **Merging the disk and process cities.** File-backed mappings naming their file gives the useful
  half without the mess.
- **Near-duplicate and code-reference work** from PLAN.md §6. Still deferred, unchanged.
