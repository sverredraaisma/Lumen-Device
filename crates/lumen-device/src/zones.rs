//! Zones and projections.
//!
//! A zone is a **selector, evaluated on each device against its own LEDs** —
//! never resolved centrally into a pixel list. So a zone record replicates as
//! its selector rather than as a list of pixels, which keeps it tiny however
//! many LEDs it covers, and a device moved to a new position joins and leaves
//! zones by itself with no publish step.
//!
//! # Two forms, one mechanism
//!
//! Explicit sets are predictable and are what "this specific strip" means.
//! Geometric predicates survive rewiring and pick up new devices, which is what
//! "the bottom of the room" means. Being able to union and subtract them is what
//! makes both usable together, and an explicit set minus a geometric exclusion
//! covers most real cases.
//!
//! # The rule that prevents a recurring mystery
//!
//! **A geometric clause never selects a device with synthetic coordinates.**
//! Every device has coordinates, but a synthetic device's are arbitrary, so
//! `z < 0.3` would select it essentially at random — which is worse than not
//! selecting it, because random is indistinguishable from broken. Naming a
//! device explicitly still selects it, because naming it is an unambiguous
//! statement of intent regardless of where it thinks it is.
//!
//! # Re-evaluation is triggered, not continuous
//!
//! Membership is recomputed on a device root change, a mapping change, or a zone
//! record change — never per frame. A device that has just been moved should
//! re-evaluate once and settle, not flicker between zones while an AR session is
//! still refining its position.

use lumen_proto::Uuid;
use lumen_vm::q16::Q16;

/// Why zone membership might need recomputing.
///
/// Separate causes because they settle differently, which is the whole point of
/// distinguishing them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Resettle {
    /// The device's own root moved.
    ///
    /// The flickery one. An AR session refining a position emits these
    /// continuously for as long as somebody is pointing a phone at the device,
    /// and each one would otherwise re-resolve every zone.
    RootMoved,
    /// The device's LED coordinates or map quality changed.
    ///
    /// Arrives from the same AR session as `RootMoved` and for the same reason,
    /// so it settles the same way.
    MappingChanged,
    /// A zone record was created, edited or deleted.
    ///
    /// Somebody's deliberate act, so it applies at once. Waiting here would make
    /// the editor feel broken - a user who has just changed a zone is watching
    /// the lights to see whether it worked.
    ZoneChanged,
}

/// How long a moved device waits for its position to stop changing.
///
/// Long enough to cover the gaps between updates in an AR session, short enough
/// that a device set down on a shelf joins its zones while the person who moved
/// it is still looking at it.
pub const SETTLE_US: u64 = 500_000;

/// Decides when zone membership is worth recomputing.
///
/// Sans-IO and allocation-free: it takes causes and a clock and answers a
/// question. The caller does the resolving, which is what keeps this testable
/// without a device.
///
/// # Why debouncing rather than rate limiting
///
/// A rate limit would recompute every so often *during* a move, which is the
/// flicker the rule exists to avoid — the intermediate answers are all wrong and
/// showing them is worse than showing the stale one. Waiting for quiet means a
/// device shows its old membership throughout a move and its new membership
/// once, which is both correct and what a person expects to see.
#[derive(Clone, Copy, Default, Debug)]
pub struct Settling {
    pending: bool,
    /// When the work becomes due. Already past for a cause that does not wait.
    due_at_us: u64,
}

impl Settling {
    pub const fn new() -> Self {
        Settling {
            pending: false,
            due_at_us: 0,
        }
    }

    /// Note that something happened which membership depends on.
    pub fn touch(&mut self, now_us: u64, cause: Resettle) {
        let due = match cause {
            Resettle::RootMoved | Resettle::MappingChanged => now_us.saturating_add(SETTLE_US),
            Resettle::ZoneChanged => now_us,
        };
        // The later deadline wins, so a move in progress keeps pushing the work
        // out — that is the debounce. A `ZoneChanged` arriving mid-move
        // therefore does *not* pull the recompute forward, which is deliberate:
        // resolving against a position still being refined would produce an
        // answer that has to be thrown away, and the move's own deadline is
        // moments later anyway.
        self.due_at_us = if self.pending {
            self.due_at_us.max(due)
        } else {
            due
        };
        self.pending = true;
    }

    /// Whether membership should be recomputed now.
    pub fn is_due(&self, now_us: u64) -> bool {
        self.pending && now_us >= self.due_at_us
    }

    /// Whether anything is waiting, due or not.
    pub fn is_pending(&self) -> bool {
        self.pending
    }

    /// When to wake up, for a caller scheduling a timer.
    ///
    /// `None` when nothing is waiting, so a settled device sets no timer and
    /// stays asleep.
    pub fn due_at(&self) -> Option<u64> {
        self.pending.then_some(self.due_at_us)
    }

    /// Record that the caller has recomputed.
    pub fn settled(&mut self) {
        self.pending = false;
        self.due_at_us = 0;
    }
}

/// How much a device's coordinates can be trusted.
///
/// Ordered, so "rough or better" is a comparison rather than a match.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum MapQuality {
    /// Declared topology laid out along an arbitrary axis at an arbitrary
    /// origin. Present so that no feature needs an unmapped code path — but
    /// arbitrary, so nothing geometric may rely on it.
    Synthetic = 0,
    /// Placed by hand: "this strip runs along the desk, roughly 2 m, from here".
    /// A few seconds per device, and enough for volumetric effects to look
    /// right for most purposes.
    Rough = 1,
    /// Measured, from an AR mapping session.
    Mapped = 2,
}

impl MapQuality {
    /// Whether a geometric predicate may select this device.
    pub fn is_geometrically_trustworthy(self) -> bool {
        self >= MapQuality::Rough
    }
}

/// One LED, with the coordinates every device always has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Led {
    /// Index within the device.
    pub index: u16,
    /// World coordinates in metres. Arbitrary when the device is synthetic.
    pub world: [Q16; 3],
    /// Coordinates relative to the device root.
    pub local: [Q16; 3],
}

/// This device's own LEDs, which is all it can evaluate a zone against.
#[derive(Clone, Debug)]
pub struct DeviceLeds {
    pub device: Uuid,
    pub quality: MapQuality,
    pub leds: alloc::vec::Vec<Led>,
}

/// Which coordinate a comparison reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Axis {
    X,
    Y,
    Z,
}

impl Axis {
    pub fn of(self, p: &[Q16; 3]) -> Q16 {
        match self {
            Axis::X => p[0],
            Axis::Y => p[1],
            Axis::Z => p[2],
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmpOp {
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    fn holds(self, a: Q16, b: Q16) -> bool {
        match self {
            CmpOp::Lt => a < b,
            CmpOp::Le => a <= b,
            CmpOp::Gt => a > b,
            CmpOp::Ge => a >= b,
        }
    }
}

/// A geometric test on one LED's world position.
///
/// The evaluated form. On the wire a predicate travels as a cut-down bytecode
/// program — a predicate is an expression returning a boolean, and the VM
/// already exists — but a device needs the evaluated shape either way, and
/// having it separately is what makes zone membership testable without a
/// compiler.
#[derive(Clone, PartialEq, Debug)]
pub enum Predicate {
    Compare {
        axis: Axis,
        op: CmpOp,
        value: Q16,
    },
    /// Within `radius` of a point.
    Near {
        point: [Q16; 3],
        radius: Q16,
    },
    All(alloc::vec::Vec<Predicate>),
    Any(alloc::vec::Vec<Predicate>),
    Not(alloc::boxed::Box<Predicate>),
}

impl Predicate {
    pub fn holds(&self, world: &[Q16; 3]) -> bool {
        match self {
            Predicate::Compare { axis, op, value } => op.holds(axis.of(world), *value),
            Predicate::Near { point, radius } => {
                let d = Q16::len3(
                    world[0].sub(point[0]),
                    world[1].sub(point[1]),
                    world[2].sub(point[2]),
                );
                d < *radius
            }
            Predicate::All(ps) => ps.iter().all(|p| p.holds(world)),
            Predicate::Any(ps) => ps.iter().any(|p| p.holds(world)),
            Predicate::Not(p) => !p.holds(world),
        }
    }
}

/// One term of a zone selector.
#[derive(Clone, PartialEq, Debug)]
pub enum Clause {
    /// Name a device, optionally a range of its LEDs.
    ///
    /// `leds` is `[from, to)`. Naming a device selects it whatever its mapping
    /// quality — that is the whole point of naming it.
    Device {
        device: Uuid,
        leds: Option<(u16, u16)>,
    },
    /// A geometric test. Never matches a synthetic device.
    Where(Predicate),
}

impl Clause {
    fn matches(&self, dev: &DeviceLeds, led: &Led) -> bool {
        match self {
            Clause::Device { device, leds } => {
                if *device != dev.device {
                    return false;
                }
                match leds {
                    Some((from, to)) => led.index >= *from && led.index < *to,
                    None => true,
                }
            }
            Clause::Where(p) => {
                // The rule that stops "why is that strip dark" being a recurring
                // mystery: arbitrary coordinates would match at random.
                dev.quality.is_geometrically_trustworthy() && p.holds(&led.world)
            }
        }
    }
}

/// What `u` means for a zone's pixels.
///
/// An effect written for a 1D strip must still work on a mapped 3D device, or
/// half the community's effects are unusable in the best feature the project
/// has. `x, y, z` stay world coordinates regardless, so genuinely volumetric
/// effects ignore projections entirely.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Projection {
    /// Index along the physical strip. The default, and correct for an unmapped
    /// device — which is why an unmapped device runs 1D effects perfectly.
    Strip,
    /// Position projected onto a vector, normalised over the zone's bounds.
    Axis([Q16; 3]),
    /// Distance from a point, normalised over the zone's bounds.
    Radial([Q16; 3]),
    /// Angle around an axis, in turns. Good for rings and ceilings.
    Angle(Axis),
    /// A rectangle, giving effects a `uv` pair as well as `u`.
    ///
    /// What makes text, images, scrolling and 2D patterns possible with no
    /// separate subsystem: a panel is a zone with this projection, and a set of
    /// strips arranged in a rectangle can declare the same one and run the same
    /// effects.
    Grid { width: u16, height: u16 },
}

/// A zone: what it covers, and what `u` means inside it.
#[derive(Clone, PartialEq, Debug)]
pub struct Zone {
    pub id: Uuid,
    pub include: alloc::vec::Vec<Clause>,
    pub exclude: alloc::vec::Vec<Clause>,
    pub projection: Projection,
}

/// The extent of a zone's members, for normalising a projection.
///
/// Computed once when membership is resolved. Recomputing it per frame would be
/// wasted work and, worse, would make `u` shift under a running effect whenever
/// a device joined.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bounds {
    pub min: [Q16; 3],
    pub max: [Q16; 3],
    /// How many LEDs of this device the zone covers.
    pub count: u16,
}

impl Bounds {
    fn empty() -> Bounds {
        Bounds {
            min: [Q16::MAX; 3],
            max: [Q16::MIN; 3],
            count: 0,
        }
    }

    fn include(&mut self, led: &Led) {
        for k in 0..3 {
            self.min[k] = self.min[k].min(led.world[k]);
            self.max[k] = self.max[k].max(led.world[k]);
        }
        self.count = self.count.saturating_add(1);
    }

    pub fn span(&self, axis: Axis) -> Q16 {
        axis.of(&self.max).sub(axis.of(&self.min))
    }

    pub fn centre(&self) -> [Q16; 3] {
        [
            self.min[0].add(self.max[0]).mul(Q16::HALF),
            self.min[1].add(self.max[1]).mul(Q16::HALF),
            self.min[2].add(self.max[2]).mul(Q16::HALF),
        ]
    }
}

/// Which of this device's LEDs a zone covers, and their extent.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Membership {
    pub zone: Uuid,
    /// Indices into the device's LED list, ascending.
    pub leds: alloc::vec::Vec<u16>,
    pub bounds: Bounds,
}

impl Membership {
    pub fn is_empty(&self) -> bool {
        self.leds.is_empty()
    }

    pub fn contains(&self, index: u16) -> bool {
        self.leds.binary_search(&index).is_ok()
    }
}

impl Zone {
    /// Resolve this zone against one device's LEDs.
    ///
    /// Call on a device root change, a mapping change, or a zone record change —
    /// **not per frame**. A device that has just been moved should re-evaluate
    /// once and settle, not flicker between zones while an AR session refines
    /// its position.
    pub fn resolve(&self, dev: &DeviceLeds) -> Membership {
        let mut leds = alloc::vec::Vec::new();
        let mut bounds = Bounds::empty();
        for led in &dev.leds {
            let included = self.include.iter().any(|c| c.matches(dev, led));
            if !included {
                continue;
            }
            if self.exclude.iter().any(|c| c.matches(dev, led)) {
                continue;
            }
            leds.push(led.index);
            bounds.include(led);
        }
        if leds.is_empty() {
            bounds = Bounds::empty();
        }
        Membership {
            zone: self.id,
            leds,
            bounds,
        }
    }

    /// Whether this zone would select `dev` if only its mapping were better.
    ///
    /// The app needs this to say "the ceiling strip is not in this zone because
    /// it has not been placed", with a route to placing it. Without it, "why is
    /// that strip dark" has no visible answer, because with distributed
    /// evaluation nobody holds one centrally.
    pub fn excluded_for_mapping(&self, dev: &DeviceLeds) -> bool {
        if dev.quality.is_geometrically_trustworthy() {
            return false;
        }
        if !self.resolve(dev).is_empty() {
            return false;
        }
        // Would any LED match if the coordinates were trusted?
        let trusted = DeviceLeds {
            device: dev.device,
            quality: MapQuality::Mapped,
            leds: dev.leds.clone(),
        };
        !self.resolve(&trusted).is_empty()
    }
}

/// Where a pixel sits inside a source's zone.
///
/// **`u` is per-source, not per-pixel.** Zones may overlap and two overlapping
/// zones may declare different projections, so there is no single answer to
/// "what is `u` for this LED". A pixel covered by three sources has three
/// different values of `u` in one frame, and that is correct.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Projected {
    pub u: Q16,
    /// Set only for a `grid` projection. Referencing `uv` without one is an
    /// error the compiler already refuses.
    pub uv: Option<(Q16, Q16)>,
}

impl Projection {
    /// Where `led` sits, given the zone's extent and its position in the
    /// membership list.
    ///
    /// `ordinal` is the LED's position among the zone's members, which is what
    /// [`Projection::Strip`] uses — the physical order along the strip, not the
    /// raw device index, so a zone covering LEDs 40..60 still runs `u` from 0
    /// to 1 across them.
    pub fn project(&self, led: &Led, bounds: &Bounds, ordinal: u16) -> Projected {
        match self {
            Projection::Strip => Projected {
                u: fraction(ordinal, bounds.count),
                uv: None,
            },
            Projection::Axis(v) => {
                // Projection onto the vector, normalised over the zone's extent
                // along that same vector. Normalising over anything else would
                // make `u` depend on how the zone happens to be shaped.
                let dot = |p: &[Q16; 3]| p[0].mul(v[0]).add(p[1].mul(v[1])).add(p[2].mul(v[2]));
                let here = dot(&led.world);
                let lo = dot(&bounds.min);
                let hi = dot(&bounds.max);
                Projected {
                    u: normalise(here, lo.min(hi), lo.max(hi)),
                    uv: None,
                }
            }
            Projection::Radial(p) => {
                let d = Q16::len3(
                    led.world[0].sub(p[0]),
                    led.world[1].sub(p[1]),
                    led.world[2].sub(p[2]),
                );
                // The far corner of the zone's box is the furthest anything in
                // it can be, so it is the natural normaliser.
                let far = Q16::len3(
                    bounds.max[0]
                        .sub(p[0])
                        .abs()
                        .max(bounds.min[0].sub(p[0]).abs()),
                    bounds.max[1]
                        .sub(p[1])
                        .abs()
                        .max(bounds.min[1].sub(p[1]).abs()),
                    bounds.max[2]
                        .sub(p[2])
                        .abs()
                        .max(bounds.min[2].sub(p[2]).abs()),
                );
                Projected {
                    u: normalise(d, Q16::ZERO, far),
                    uv: None,
                }
            }
            Projection::Angle(axis) => {
                let centre = bounds.centre();
                let (a, b) = match axis {
                    Axis::X => (led.world[1].sub(centre[1]), led.world[2].sub(centre[2])),
                    Axis::Y => (led.world[2].sub(centre[2]), led.world[0].sub(centre[0])),
                    Axis::Z => (led.world[0].sub(centre[0]), led.world[1].sub(centre[1])),
                };
                // In turns, not radians: an effect wanting one rotation per
                // second then needs no constant anywhere, and `sin01` takes
                // turns already.
                let angle = Q16::atan2(b, a);
                let turns = angle.div(Q16::TAU).unwrap_or(Q16::ZERO);
                Projected {
                    u: turns.fract(),
                    uv: None,
                }
            }
            Projection::Grid { width, height } => {
                let w = (*width).max(1);
                let h = (*height).max(1);
                let col = ordinal % w;
                let row = (ordinal / w).min(h.saturating_sub(1));
                let u = fraction(col, w);
                let v = fraction(row, h);
                Projected {
                    // `u` stays meaningful on a grid so a 1D effect still runs
                    // on a panel - which is the entire argument for projections.
                    u: fraction(ordinal, bounds.count),
                    uv: Some((u, v)),
                }
            }
        }
    }

    pub fn is_grid(&self) -> bool {
        matches!(self, Projection::Grid { .. })
    }
}

/// `ordinal / (count - 1)`, so the first LED is 0 and the last is exactly 1.
///
/// Dividing by `count` instead would make the last LED fall short of 1, and an
/// effect ending at `u == 1` would never quite reach the end of the strip —
/// visible as a dark final pixel on every gradient.
fn fraction(ordinal: u16, count: u16) -> Q16 {
    if count <= 1 {
        return Q16::ZERO;
    }
    Q16::from_ratio(ordinal as i32, (count - 1) as i32).clamp(Q16::ZERO, Q16::ONE)
}

fn normalise(value: Q16, lo: Q16, hi: Q16) -> Q16 {
    let span = hi.sub(lo);
    if span.is_zero() {
        // A zone with no extent along this axis — one LED, or a flat plane.
        // Zero is the only answer that is not arbitrary.
        return Q16::ZERO;
    }
    value
        .sub(lo)
        .div(span)
        .unwrap_or(Q16::ZERO)
        .clamp(Q16::ZERO, Q16::ONE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn uuid(n: u8) -> Uuid {
        Uuid([n; 16])
    }

    fn m(v: i32) -> Q16 {
        Q16::from_int(v as i16)
    }

    /// A strip of `n` LEDs running along x from 0 to `n-1` metres, at height y.
    fn strip(device: u8, n: u16, y: i32, quality: MapQuality) -> DeviceLeds {
        DeviceLeds {
            device: uuid(device),
            quality,
            leds: (0..n)
                .map(|i| Led {
                    index: i,
                    world: [m(i as i32), m(y), Q16::ZERO],
                    local: [m(i as i32), Q16::ZERO, Q16::ZERO],
                })
                .collect(),
        }
    }

    fn zone(include: Vec<Clause>, exclude: Vec<Clause>) -> Zone {
        Zone {
            id: uuid(99),
            include,
            exclude,
            projection: Projection::Strip,
        }
    }

    #[test]
    fn naming_a_device_selects_all_of_it() {
        let dev = strip(1, 4, 0, MapQuality::Mapped);
        let z = zone(
            vec![Clause::Device {
                device: uuid(1),
                leds: None,
            }],
            vec![],
        );
        assert_eq!(z.resolve(&dev).leds, vec![0, 1, 2, 3]);
    }

    #[test]
    fn naming_a_range_selects_only_that_range() {
        let dev = strip(1, 8, 0, MapQuality::Mapped);
        let z = zone(
            vec![Clause::Device {
                device: uuid(1),
                leds: Some((2, 5)),
            }],
            vec![],
        );
        assert_eq!(
            z.resolve(&dev).leds,
            vec![2, 3, 4],
            "the range is half-open"
        );
    }

    #[test]
    fn another_devices_name_selects_nothing_here() {
        let dev = strip(1, 4, 0, MapQuality::Mapped);
        let z = zone(
            vec![Clause::Device {
                device: uuid(2),
                leds: None,
            }],
            vec![],
        );
        assert!(z.resolve(&dev).is_empty());
    }

    #[test]
    fn a_geometric_clause_selects_by_position() {
        let low = strip(1, 3, 0, MapQuality::Mapped);
        let high = strip(2, 3, 2, MapQuality::Mapped);
        let z = zone(
            vec![Clause::Where(Predicate::Compare {
                axis: Axis::Y,
                op: CmpOp::Lt,
                value: m(1),
            })],
            vec![],
        );
        assert_eq!(z.resolve(&low).leds.len(), 3);
        assert!(z.resolve(&high).is_empty());
    }

    #[test]
    fn a_geometric_clause_never_selects_a_synthetic_device() {
        // Its coordinates are arbitrary, so matching would be random - which is
        // indistinguishable from broken, and worse than not matching at all.
        let dev = strip(1, 3, 0, MapQuality::Synthetic);
        let z = zone(
            vec![Clause::Where(Predicate::Compare {
                axis: Axis::Y,
                op: CmpOp::Lt,
                value: m(100),
            })],
            vec![],
        );
        assert!(z.resolve(&dev).is_empty(), "a synthetic device was matched");

        // Rough is enough, though: a few seconds of manual placement buys it.
        let rough = strip(1, 3, 0, MapQuality::Rough);
        assert_eq!(z.resolve(&rough).leds.len(), 3);
    }

    #[test]
    fn naming_a_synthetic_device_still_selects_it() {
        // Naming a device is an unambiguous statement of intent, regardless of
        // where it thinks it is.
        let dev = strip(1, 3, 0, MapQuality::Synthetic);
        let z = zone(
            vec![Clause::Device {
                device: uuid(1),
                leds: None,
            }],
            vec![],
        );
        assert_eq!(z.resolve(&dev).leds.len(), 3);
    }

    #[test]
    fn an_explicit_set_minus_a_geometric_exclusion_is_the_common_case() {
        // "This strip, but not the bit above the shelf."
        let dev = DeviceLeds {
            device: uuid(1),
            quality: MapQuality::Mapped,
            leds: (0..5u16)
                .map(|i| Led {
                    index: i,
                    world: [Q16::ZERO, m(i as i32), Q16::ZERO],
                    local: [Q16::ZERO; 3],
                })
                .collect(),
        };
        let z = zone(
            vec![Clause::Device {
                device: uuid(1),
                leds: None,
            }],
            vec![Clause::Where(Predicate::Compare {
                axis: Axis::Y,
                op: CmpOp::Gt,
                value: m(2),
            })],
        );
        assert_eq!(z.resolve(&dev).leds, vec![0, 1, 2]);
    }

    #[test]
    fn an_exclusion_cannot_reach_a_device_it_does_not_name() {
        let dev = strip(1, 3, 0, MapQuality::Mapped);
        let z = zone(
            vec![Clause::Device {
                device: uuid(1),
                leds: None,
            }],
            vec![Clause::Device {
                device: uuid(2),
                leds: None,
            }],
        );
        assert_eq!(z.resolve(&dev).leds.len(), 3);
    }

    #[test]
    fn a_near_predicate_selects_a_sphere() {
        let dev = strip(1, 6, 0, MapQuality::Mapped);
        let z = zone(
            vec![Clause::Where(Predicate::Near {
                point: [m(1), Q16::ZERO, Q16::ZERO],
                radius: Q16::from_ratio(3, 2),
            })],
            vec![],
        );
        // LEDs at x = 0, 1, 2 are within 1.5 m of x = 1.
        assert_eq!(z.resolve(&dev).leds, vec![0, 1, 2]);
    }

    #[test]
    fn predicates_compose() {
        let dev = strip(1, 6, 0, MapQuality::Mapped);
        let between = Predicate::All(vec![
            Predicate::Compare {
                axis: Axis::X,
                op: CmpOp::Ge,
                value: m(2),
            },
            Predicate::Compare {
                axis: Axis::X,
                op: CmpOp::Le,
                value: m(4),
            },
        ]);
        assert_eq!(
            zone(vec![Clause::Where(between.clone())], vec![])
                .resolve(&dev)
                .leds,
            vec![2, 3, 4]
        );

        let outside = Predicate::Not(alloc::boxed::Box::new(between.clone()));
        assert_eq!(
            zone(vec![Clause::Where(outside)], vec![])
                .resolve(&dev)
                .leds,
            vec![0, 1, 5]
        );

        let either = Predicate::Any(vec![
            Predicate::Compare {
                axis: Axis::X,
                op: CmpOp::Lt,
                value: m(1),
            },
            Predicate::Compare {
                axis: Axis::X,
                op: CmpOp::Gt,
                value: m(4),
            },
        ]);
        assert_eq!(
            zone(vec![Clause::Where(either)], vec![]).resolve(&dev).leds,
            vec![0, 5]
        );
    }

    #[test]
    fn overlapping_zones_are_fine_and_need_no_prevention() {
        // The source stack resolves per pixel, so overlap is well defined.
        let dev = strip(1, 6, 0, MapQuality::Mapped);
        let left = zone(
            vec![Clause::Device {
                device: uuid(1),
                leds: Some((0, 4)),
            }],
            vec![],
        );
        let right = zone(
            vec![Clause::Device {
                device: uuid(1),
                leds: Some((2, 6)),
            }],
            vec![],
        );
        let a = left.resolve(&dev);
        let b = right.resolve(&dev);
        assert!(a.contains(2) && b.contains(2));
    }

    #[test]
    fn the_app_can_tell_a_device_apart_from_a_device_that_needs_placing() {
        // "Why is that strip dark" must have a visible answer, because with
        // distributed evaluation nobody holds one centrally.
        let z = zone(
            vec![Clause::Where(Predicate::Compare {
                axis: Axis::Y,
                op: CmpOp::Lt,
                value: m(1),
            })],
            vec![],
        );
        let unplaced = strip(1, 3, 0, MapQuality::Synthetic);
        assert!(z.excluded_for_mapping(&unplaced), "should be flagged");

        // A placed device that simply does not match is not "needs placing".
        let placed_elsewhere = strip(1, 3, 5, MapQuality::Mapped);
        assert!(!z.excluded_for_mapping(&placed_elsewhere));

        // Nor is a device the zone already covers.
        let placed_inside = strip(1, 3, 0, MapQuality::Mapped);
        assert!(!z.excluded_for_mapping(&placed_inside));

        // Nor is a synthetic device the zone would not select anyway.
        let far = DeviceLeds {
            device: uuid(1),
            quality: MapQuality::Synthetic,
            leds: vec![Led {
                index: 0,
                world: [Q16::ZERO, m(9), Q16::ZERO],
                local: [Q16::ZERO; 3],
            }],
        };
        assert!(!z.excluded_for_mapping(&far));
    }

    #[test]
    fn an_empty_zone_has_empty_bounds_and_does_not_panic() {
        let dev = strip(1, 3, 0, MapQuality::Mapped);
        let z = zone(
            vec![Clause::Device {
                device: uuid(7),
                leds: None,
            }],
            vec![],
        );
        let membership = z.resolve(&dev);
        assert!(membership.is_empty());
        assert_eq!(membership.bounds.count, 0);
        assert!(!membership.contains(0));
    }

    // ---- projections -------------------------------------------------------

    fn resolved(dev: &DeviceLeds, projection: Projection) -> (Membership, DeviceLeds) {
        let z = Zone {
            id: uuid(99),
            include: vec![Clause::Device {
                device: dev.device,
                leds: None,
            }],
            exclude: vec![],
            projection,
        };
        (z.resolve(dev), dev.clone())
    }

    #[test]
    fn a_strip_projection_runs_from_zero_to_exactly_one() {
        // Dividing by count instead of count-1 would leave the last LED short
        // of 1, which shows as a dark final pixel on every gradient.
        let dev = strip(1, 5, 0, MapQuality::Synthetic);
        let (mem, dev) = resolved(&dev, Projection::Strip);
        let first = Projection::Strip.project(&dev.leds[0], &mem.bounds, 0);
        let last = Projection::Strip.project(&dev.leds[4], &mem.bounds, 4);
        assert_eq!(first.u, Q16::ZERO);
        assert_eq!(last.u, Q16::ONE);
        assert!(first.uv.is_none());
    }

    #[test]
    fn a_strip_projection_works_on_an_unmapped_device() {
        // The claim that makes mapping a pure upgrade: 1D effects work exactly
        // as intended with nothing mapped.
        let dev = strip(1, 3, 0, MapQuality::Synthetic);
        let (mem, dev) = resolved(&dev, Projection::Strip);
        let mid = Projection::Strip.project(&dev.leds[1], &mem.bounds, 1);
        assert_eq!(mid.u, Q16::HALF);
    }

    #[test]
    fn a_zone_covering_part_of_a_strip_still_runs_u_from_zero_to_one() {
        // `u` is the position among the zone's members, not the raw device
        // index - otherwise a zone over LEDs 40..60 would only ever use the top
        // third of every gradient.
        let dev = strip(1, 10, 0, MapQuality::Mapped);
        let z = Zone {
            id: uuid(99),
            include: vec![Clause::Device {
                device: uuid(1),
                leds: Some((4, 8)),
            }],
            exclude: vec![],
            projection: Projection::Strip,
        };
        let mem = z.resolve(&dev);
        assert_eq!(mem.leds, vec![4, 5, 6, 7]);
        let first = z.projection.project(&dev.leds[4], &mem.bounds, 0);
        let last = z.projection.project(&dev.leds[7], &mem.bounds, 3);
        assert_eq!(first.u, Q16::ZERO);
        assert_eq!(last.u, Q16::ONE);
    }

    #[test]
    fn an_axis_projection_follows_the_declared_direction() {
        // A 1D comet becomes a comet sweeping along whatever axis the zone
        // declares, with no change to the effect.
        let dev = strip(1, 5, 0, MapQuality::Mapped);
        let (mem, dev) = resolved(&dev, Projection::Axis([Q16::ONE, Q16::ZERO, Q16::ZERO]));
        let p = Projection::Axis([Q16::ONE, Q16::ZERO, Q16::ZERO]);
        assert_eq!(p.project(&dev.leds[0], &mem.bounds, 0).u, Q16::ZERO);
        assert_eq!(p.project(&dev.leds[4], &mem.bounds, 4).u, Q16::ONE);
        assert_eq!(p.project(&dev.leds[2], &mem.bounds, 2).u, Q16::HALF);

        // Along an axis the zone has no extent in, everything is at zero rather
        // than at an arbitrary value.
        let flat = Projection::Axis([Q16::ZERO, Q16::ONE, Q16::ZERO]);
        assert_eq!(flat.project(&dev.leds[3], &mem.bounds, 3).u, Q16::ZERO);
    }

    #[test]
    fn a_radial_projection_grows_with_distance() {
        let dev = strip(1, 5, 0, MapQuality::Mapped);
        let origin = [Q16::ZERO; 3];
        let (mem, dev) = resolved(&dev, Projection::Radial(origin));
        let p = Projection::Radial(origin);
        let near = p.project(&dev.leds[0], &mem.bounds, 0).u;
        let far = p.project(&dev.leds[4], &mem.bounds, 4).u;
        assert_eq!(near, Q16::ZERO);
        assert!(far > near);
        assert!(far <= Q16::ONE);
    }

    #[test]
    fn an_angle_projection_wraps_once_around() {
        // A ring of four LEDs around the origin.
        let dev = DeviceLeds {
            device: uuid(1),
            quality: MapQuality::Mapped,
            leds: vec![
                Led {
                    index: 0,
                    world: [m(1), Q16::ZERO, Q16::ZERO],
                    local: [Q16::ZERO; 3],
                },
                Led {
                    index: 1,
                    world: [Q16::ZERO, m(1), Q16::ZERO],
                    local: [Q16::ZERO; 3],
                },
                Led {
                    index: 2,
                    world: [m(-1), Q16::ZERO, Q16::ZERO],
                    local: [Q16::ZERO; 3],
                },
                Led {
                    index: 3,
                    world: [Q16::ZERO, m(-1), Q16::ZERO],
                    local: [Q16::ZERO; 3],
                },
            ],
        };
        let p = Projection::Angle(Axis::Z);
        let (mem, dev) = resolved(&dev, p);
        let us: Vec<Q16> = (0..4)
            .map(|i| p.project(&dev.leds[i], &mem.bounds, i as u16).u)
            .collect();
        // Every value in 0..1, all four distinct, and a quarter turn apart.
        for u in &us {
            assert!(*u >= Q16::ZERO && *u < Q16::ONE, "{u:?} outside 0..1");
        }
        assert_ne!(us[0], us[1]);
        assert_ne!(us[1], us[2]);
        assert_ne!(us[2], us[3]);
    }

    #[test]
    fn a_grid_projection_gives_uv_and_still_gives_u() {
        // `u` staying meaningful on a grid is what lets a 1D effect run on a
        // panel, which is the whole argument for projections.
        let dev = DeviceLeds {
            device: uuid(1),
            quality: MapQuality::Mapped,
            leds: (0..8u16)
                .map(|i| Led {
                    index: i,
                    world: [m((i % 4) as i32), m((i / 4) as i32), Q16::ZERO],
                    local: [Q16::ZERO; 3],
                })
                .collect(),
        };
        let p = Projection::Grid {
            width: 4,
            height: 2,
        };
        assert!(p.is_grid());
        let (mem, dev) = resolved(&dev, p);

        let top_left = p.project(&dev.leds[0], &mem.bounds, 0);
        let top_right = p.project(&dev.leds[3], &mem.bounds, 3);
        let bottom_left = p.project(&dev.leds[4], &mem.bounds, 4);
        assert_eq!(top_left.uv, Some((Q16::ZERO, Q16::ZERO)));
        assert_eq!(top_right.uv, Some((Q16::ONE, Q16::ZERO)));
        assert_eq!(bottom_left.uv, Some((Q16::ZERO, Q16::ONE)));
        assert_eq!(top_left.u, Q16::ZERO);
        assert!(top_right.u > Q16::ZERO);
    }

    #[test]
    fn a_grid_of_zero_size_does_not_divide_by_zero() {
        let dev = strip(1, 2, 0, MapQuality::Mapped);
        let p = Projection::Grid {
            width: 0,
            height: 0,
        };
        let (mem, dev) = resolved(&dev, p);
        let out = p.project(&dev.leds[0], &mem.bounds, 0);
        assert_eq!(out.uv, Some((Q16::ZERO, Q16::ZERO)));
    }

    #[test]
    fn a_single_led_zone_projects_to_zero_rather_than_anything_arbitrary() {
        let dev = strip(1, 1, 0, MapQuality::Mapped);
        let (mem, dev) = resolved(&dev, Projection::Strip);
        assert_eq!(
            Projection::Strip.project(&dev.leds[0], &mem.bounds, 0).u,
            Q16::ZERO
        );
        assert_eq!(mem.bounds.count, 1);
        assert_eq!(mem.bounds.span(Axis::X), Q16::ZERO);
    }

    #[test]
    fn map_quality_is_ordered_so_rough_or_better_is_a_comparison() {
        assert!(MapQuality::Synthetic < MapQuality::Rough);
        assert!(MapQuality::Rough < MapQuality::Mapped);
        assert!(!MapQuality::Synthetic.is_geometrically_trustworthy());
        assert!(MapQuality::Rough.is_geometrically_trustworthy());
        assert!(MapQuality::Mapped.is_geometrically_trustworthy());
    }

    #[test]
    fn bounds_describe_the_extent_of_what_was_selected() {
        let dev = strip(1, 5, 2, MapQuality::Mapped);
        let (mem, _) = resolved(&dev, Projection::Strip);
        assert_eq!(mem.bounds.min[0], Q16::ZERO);
        assert_eq!(mem.bounds.max[0], m(4));
        assert_eq!(mem.bounds.span(Axis::X), m(4));
        assert_eq!(mem.bounds.span(Axis::Z), Q16::ZERO);
        assert_eq!(mem.bounds.centre()[0], m(2));
    }

    // ---- Settling ----------------------------------------------------------

    #[test]
    fn a_zone_edit_applies_at_once() {
        // Somebody's deliberate act, and they are watching the lights to see
        // whether it worked.
        let mut s = Settling::new();
        s.touch(1_000, Resettle::ZoneChanged);
        assert!(s.is_due(1_000));
        assert_eq!(s.due_at(), Some(1_000));
    }

    #[test]
    fn a_moved_device_waits_for_its_position_to_stop_changing() {
        let mut s = Settling::new();
        s.touch(0, Resettle::RootMoved);
        assert!(s.is_pending());
        assert!(!s.is_due(SETTLE_US - 1));
        assert!(s.is_due(SETTLE_US));
    }

    #[test]
    fn a_move_in_progress_keeps_pushing_the_work_out() {
        // The debounce, and the reason this type exists. An AR session emits
        // root changes for as long as somebody points a phone at the device;
        // recomputing on each is wasted work and visible as flicker.
        let mut s = Settling::new();
        s.touch(0, Resettle::RootMoved);
        for tick in 1..20 {
            let now = tick * 100_000;
            s.touch(now, Resettle::RootMoved);
            assert!(!s.is_due(now), "recomputed mid-move at {now}");
        }
        let last = 19 * 100_000;
        assert!(s.is_due(last + SETTLE_US));
    }

    #[test]
    fn a_zone_edit_during_a_move_does_not_pull_the_recompute_forward() {
        // Resolving against a position still being refined produces an answer
        // that has to be thrown away, and the move's own deadline is moments
        // later anyway.
        let mut s = Settling::new();
        s.touch(0, Resettle::RootMoved);
        s.touch(1_000, Resettle::ZoneChanged);
        assert!(!s.is_due(1_000));
        assert!(s.is_due(SETTLE_US));
    }

    #[test]
    fn a_mapping_change_settles_like_a_move() {
        // It arrives from the same AR session and for the same reason.
        let mut s = Settling::new();
        s.touch(0, Resettle::MappingChanged);
        assert!(!s.is_due(SETTLE_US - 1));
        assert!(s.is_due(SETTLE_US));
    }

    #[test]
    fn a_settled_device_asks_for_no_timer() {
        let mut s = Settling::new();
        assert_eq!(s.due_at(), None);
        assert!(!s.is_due(u64::MAX));

        s.touch(0, Resettle::RootMoved);
        assert_eq!(s.due_at(), Some(SETTLE_US));
        s.settled();
        assert_eq!(s.due_at(), None);
        assert!(!s.is_pending());
    }

    #[test]
    fn settling_does_not_overflow_at_the_end_of_the_clock() {
        // The show clock is a u64 of microseconds, so this is not reachable in
        // practice - but a deadline that wrapped would be immediately due, and
        // a device would recompute continuously rather than never.
        let mut s = Settling::new();
        s.touch(u64::MAX, Resettle::RootMoved);
        assert_eq!(s.due_at(), Some(u64::MAX));
        assert!(s.is_due(u64::MAX));
    }
}
