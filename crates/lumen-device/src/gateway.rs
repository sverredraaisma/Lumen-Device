//! Gateway policy for unauthenticated integrations.
//!
//! Art-Net, E1.31, plain MQTT and HTTP have no useful authentication. Anyone on
//! the network can speak them. They are still worth supporting, because they are
//! how the rest of the lighting world talks — so they terminate at a device
//! holding `caps=gateway`, under a policy that bounds what they can do.
//!
//! # What a binding grants, and nothing more
//!
//! An integration is **explicitly bound** to a pixel range and a priority
//! ceiling. It cannot reach a pixel outside its range, cannot outrank the
//! ceiling, and **can never push a program**. That last one matters most: a
//! program is signed code that runs on every device, and accepting one over an
//! unauthenticated channel would make the signing pointless.
//!
//! The bound is enforced here rather than trusted from the source, because the
//! source is by definition untrusted. A gateway that clamped nothing would turn
//! every Art-Net node on the network into a way to take over a room.
//!
//! # Why clamp rather than reject
//!
//! A console sending priority 255 is not attacking anything — it is a console
//! doing what consoles do. Rejecting the frame would leave a strip dark and the
//! operator with no idea why; clamping it to the ceiling does what they meant,
//! within what they are allowed. An out-of-range *pixel* is different: there is
//! no sensible reinterpretation, so those pixels are dropped and counted.

use alloc::vec::Vec;

use lumen_proto::Uuid;

use crate::sources::{Source, AMBIENT_FLOOR_MAX};

/// A protocol that arrives without credentials.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Protocol {
    ArtNet,
    E131,
    Ddp,
    /// Plain MQTT, with no per-message authentication.
    Mqtt,
    Http,
}

/// The lease every gateway source gets whether the integration asked for one or
/// not.
///
/// Art-Net and E1.31 have no concept of releasing a claim, so a console that is
/// switched off would otherwise hold a room forever. Five seconds is long enough
/// to survive a dropped frame at any sane rate and short enough that unplugging
/// the console visibly gives the lights back.
pub const DEFAULT_LEASE_US: u64 = 5_000_000;

/// What an integration is allowed to do.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Binding {
    pub id: Uuid,
    pub protocol: Protocol,
    /// The zone its pixels land in.
    pub zone: Uuid,
    /// Half-open range of pixel indices it may address.
    pub pixels: (u16, u16),
    /// The highest priority it may claim.
    ///
    /// Above the ambient floor is normal — a console is meant to override a
    /// scene — but a gateway binding above the status bands would let anything
    /// on the network suppress an alert, which is the one thing that must not be
    /// overridable by an unauthenticated source.
    pub priority_ceiling: u8,
}

/// The highest ceiling a gateway binding may be configured with.
///
/// Below the system-health band at 192. A gateway that could outrank a smoke
/// alarm indicator is a gateway that can hide it.
pub const MAX_GATEWAY_PRIORITY: u8 = 191;

/// Why a binding is not usable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BindingError {
    /// A ceiling above what a gateway may ever be given.
    CeilingTooHigh { asked: u8, max: u8 },
    /// An empty or inverted pixel range.
    EmptyRange,
}

impl Binding {
    /// Check a binding before it is ever used.
    ///
    /// Configuration is checked once here rather than on every frame, so a
    /// nonsensical binding is refused when someone writes it rather than
    /// silently doing nothing for a week.
    pub fn validate(&self) -> Result<(), BindingError> {
        if self.priority_ceiling > MAX_GATEWAY_PRIORITY {
            return Err(BindingError::CeilingTooHigh {
                asked: self.priority_ceiling,
                max: MAX_GATEWAY_PRIORITY,
            });
        }
        if self.pixels.0 >= self.pixels.1 {
            return Err(BindingError::EmptyRange);
        }
        Ok(())
    }

    pub fn covers(&self, pixel: u16) -> bool {
        pixel >= self.pixels.0 && pixel < self.pixels.1
    }

    pub fn pixel_count(&self) -> u16 {
        self.pixels.1.saturating_sub(self.pixels.0)
    }
}

/// What an integration asked for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Ingress {
    /// The first pixel the data addresses.
    pub offset: u16,
    /// How many pixels follow.
    pub count: u16,
    /// The priority claimed, if the protocol has one.
    pub priority: Option<u8>,
}

/// What the gateway will actually do.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Admitted {
    /// The source to push, already clamped.
    pub source: Source,
    /// The pixel range that survived the bound, half-open.
    pub pixels: (u16, u16),
    /// Pixels dropped for falling outside the binding.
    ///
    /// Counted rather than silent, so an app can say "this Art-Net universe is
    /// addressing 170 pixels and only 60 of them are yours" instead of leaving
    /// someone to wonder why two thirds of their console does nothing.
    pub clipped: u16,
    /// Whether the claimed priority had to be lowered.
    pub priority_clamped: bool,
}

/// Why an ingress was refused outright.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// Nothing it addressed is inside the binding.
    OutsideBinding,
    /// The binding itself is not usable.
    BadBinding(BindingError),
}

/// Apply a binding to an incoming frame.
///
/// `now_us` and `source_id` come from the caller because this is sans-IO: the
/// shell knows the clock and mints the id, and keeping both out of here is what
/// lets a conformance vector drive it.
pub fn admit(
    binding: &Binding,
    ingress: &Ingress,
    source_id: Uuid,
    now_us: u64,
) -> Result<Admitted, Refusal> {
    binding.validate().map_err(Refusal::BadBinding)?;

    let asked_from = ingress.offset;
    let asked_to = ingress.offset.saturating_add(ingress.count);
    let from = asked_from.max(binding.pixels.0);
    let to = asked_to.min(binding.pixels.1);
    if from >= to {
        return Err(Refusal::OutsideBinding);
    }
    let clipped = ingress.count.saturating_sub(to - from);

    // A console sending 255 is a console doing what consoles do. Clamping does
    // what they meant, within what they are allowed; rejecting would leave a
    // dark strip and no explanation.
    let asked_priority = ingress.priority.unwrap_or(AMBIENT_FLOOR_MAX);
    let priority = asked_priority.min(binding.priority_ceiling);

    Ok(Admitted {
        source: Source {
            id: source_id,
            zone: binding.zone,
            // A gateway pushes pixels, never a program. `scene` is nil because
            // there is no scene: the data *is* the content.
            scene: Uuid::NIL,
            priority,
            // Always leased, whatever the protocol thinks. Art-Net has no
            // concept of releasing, so a console switched off mid-show would
            // otherwise hold the room until someone power-cycled a light.
            expires_at_us: Some(now_us.saturating_add(DEFAULT_LEASE_US)),
            fade_in_ms: 0,
            fade_out_ms: 0,
            pushed_at_us: now_us,
            cost: 0,
        },
        pixels: (from, to),
        clipped,
        priority_clamped: priority < asked_priority,
    })
}

/// Every binding this gateway holds.
#[derive(Clone, Default, Debug)]
pub struct Gateway {
    bindings: Vec<Binding>,
}

impl Gateway {
    pub fn new() -> Gateway {
        Gateway::default()
    }

    /// Add a binding, checking it first.
    pub fn bind(&mut self, binding: Binding) -> Result<(), BindingError> {
        binding.validate()?;
        self.bindings.retain(|b| b.id != binding.id);
        self.bindings.push(binding);
        Ok(())
    }

    pub fn unbind(&mut self, id: Uuid) -> bool {
        let before = self.bindings.len();
        self.bindings.retain(|b| b.id != id);
        self.bindings.len() != before
    }

    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    pub fn get(&self, id: Uuid) -> Option<&Binding> {
        self.bindings.iter().find(|b| b.id == id)
    }

    /// Whether this gateway would ever accept a program from an integration.
    ///
    /// It would not, and this exists so the answer is a test rather than an
    /// absence. A program is signed code that runs on every device in the mesh;
    /// accepting one over a channel anybody on the network can speak would make
    /// signing programs pointless.
    pub const fn accepts_programs(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid(n: u8) -> Uuid {
        Uuid([n; 16])
    }

    fn binding() -> Binding {
        Binding {
            id: uuid(1),
            protocol: Protocol::ArtNet,
            zone: uuid(50),
            pixels: (10, 70),
            priority_ceiling: 100,
        }
    }

    fn ingress(offset: u16, count: u16, priority: Option<u8>) -> Ingress {
        Ingress {
            offset,
            count,
            priority,
        }
    }

    #[test]
    fn an_ingress_inside_the_binding_passes_through() {
        let out = admit(&binding(), &ingress(20, 10, Some(50)), uuid(9), 1_000).unwrap();
        assert_eq!(out.pixels, (20, 30));
        assert_eq!(out.clipped, 0);
        assert!(!out.priority_clamped);
        assert_eq!(out.source.priority, 50);
        assert_eq!(out.source.zone, uuid(50));
    }

    #[test]
    fn pixels_outside_the_binding_are_clipped_and_counted() {
        // "This universe is addressing 170 pixels and only 60 of them are yours"
        // beats leaving someone to wonder why two thirds of their console does
        // nothing.
        let out = admit(&binding(), &ingress(0, 100, None), uuid(9), 0).unwrap();
        assert_eq!(out.pixels, (10, 70), "clamped to the binding");
        assert_eq!(out.clipped, 40);
    }

    #[test]
    fn an_ingress_entirely_outside_the_binding_is_refused() {
        // There is no sensible reinterpretation, unlike a too-high priority.
        assert_eq!(
            admit(&binding(), &ingress(200, 10, None), uuid(9), 0),
            Err(Refusal::OutsideBinding)
        );
        assert_eq!(
            admit(&binding(), &ingress(0, 5, None), uuid(9), 0),
            Err(Refusal::OutsideBinding)
        );
    }

    #[test]
    fn a_priority_above_the_ceiling_is_clamped_rather_than_refused() {
        // A console sending 255 is a console doing what consoles do. Rejecting
        // the frame would leave a dark strip and no explanation.
        let out = admit(&binding(), &ingress(20, 10, Some(255)), uuid(9), 0).unwrap();
        assert_eq!(out.source.priority, 100, "clamped to the ceiling");
        assert!(out.priority_clamped, "the clamp must be visible");
    }

    #[test]
    fn a_protocol_with_no_priority_lands_at_the_ambient_floor() {
        // DDP and plain Art-Net say nothing about priority. Defaulting high
        // would let any node on the network take a room simply by connecting.
        let out = admit(&binding(), &ingress(20, 10, None), uuid(9), 0).unwrap();
        assert_eq!(out.source.priority, AMBIENT_FLOOR_MAX);
        assert!(!out.priority_clamped);
    }

    #[test]
    fn every_gateway_source_is_leased_whatever_the_protocol_thinks() {
        // Art-Net has no concept of releasing a claim, so a console switched off
        // mid-show would otherwise hold the room until someone power-cycled a
        // light.
        let out = admit(&binding(), &ingress(20, 10, Some(90)), uuid(9), 7_000).unwrap();
        assert_eq!(
            out.source.expires_at_us,
            Some(7_000 + DEFAULT_LEASE_US),
            "an unleased gateway source is how a room gets stuck"
        );
    }

    #[test]
    fn a_gateway_source_carries_no_scene_because_the_data_is_the_content() {
        let out = admit(&binding(), &ingress(20, 10, None), uuid(9), 0).unwrap();
        assert_eq!(out.source.scene, Uuid::NIL);
    }

    #[test]
    fn a_ceiling_above_the_status_bands_is_refused() {
        // A gateway that could outrank a smoke alarm indicator is a gateway that
        // can hide it.
        let mut b = binding();
        b.priority_ceiling = 250;
        assert_eq!(
            b.validate(),
            Err(BindingError::CeilingTooHigh {
                asked: 250,
                max: MAX_GATEWAY_PRIORITY
            })
        );
        // The boundary, not just a comfortably high number.
        b.priority_ceiling = MAX_GATEWAY_PRIORITY + 1;
        assert!(b.validate().is_err());
        b.priority_ceiling = MAX_GATEWAY_PRIORITY;
        assert!(b.validate().is_ok());
    }

    #[test]
    fn an_empty_or_inverted_range_is_refused_when_it_is_written() {
        // Checked once at configuration time, so a nonsensical binding is
        // refused when someone writes it rather than silently doing nothing for
        // a week.
        let mut b = binding();
        b.pixels = (10, 10);
        assert_eq!(b.validate(), Err(BindingError::EmptyRange));
        b.pixels = (70, 10);
        assert_eq!(b.validate(), Err(BindingError::EmptyRange));
    }

    #[test]
    fn a_bad_binding_refuses_every_ingress() {
        let mut b = binding();
        b.priority_ceiling = 255;
        assert!(matches!(
            admit(&b, &ingress(20, 10, None), uuid(9), 0),
            Err(Refusal::BadBinding(_))
        ));
    }

    #[test]
    fn a_binding_reports_what_it_covers() {
        let b = binding();
        assert!(!b.covers(9));
        assert!(b.covers(10));
        assert!(b.covers(69));
        assert!(!b.covers(70), "the range is half-open");
        assert_eq!(b.pixel_count(), 60);
    }

    #[test]
    fn a_gateway_never_accepts_a_program() {
        // A program is signed code that runs on every device in the mesh.
        // Accepting one over a channel anybody on the network can speak would
        // make signing programs pointless.
        let g = Gateway::new();
        assert!(!g.accepts_programs());
    }

    #[test]
    fn bindings_can_be_added_replaced_and_removed() {
        let mut g = Gateway::new();
        assert!(g.is_empty());
        g.bind(binding()).unwrap();
        assert_eq!(g.len(), 1);

        // Rebinding the same id replaces rather than duplicating, so a console
        // reconfigured twice does not end up with two conflicting grants.
        let mut wider = binding();
        wider.pixels = (0, 200);
        g.bind(wider).unwrap();
        assert_eq!(g.len(), 1);
        assert_eq!(g.get(uuid(1)).unwrap().pixels, (0, 200));

        assert!(g.unbind(uuid(1)));
        assert!(!g.unbind(uuid(1)));
        assert!(g.is_empty());
    }

    #[test]
    fn an_invalid_binding_is_never_stored() {
        let mut g = Gateway::new();
        let mut bad = binding();
        bad.pixels = (5, 5);
        assert!(g.bind(bad).is_err());
        assert!(g.is_empty(), "an unusable binding must not be held");
    }

    #[test]
    fn two_integrations_get_separate_grants() {
        let mut g = Gateway::new();
        g.bind(binding()).unwrap();
        g.bind(Binding {
            id: uuid(2),
            protocol: Protocol::E131,
            zone: uuid(51),
            pixels: (100, 160),
            priority_ceiling: 80,
        })
        .unwrap();
        assert_eq!(g.len(), 2);

        // Neither can reach the other's pixels.
        let first = g.get(uuid(1)).unwrap();
        let second = g.get(uuid(2)).unwrap();
        assert!(!first.covers(120));
        assert!(!second.covers(20));
    }

    #[test]
    fn a_count_that_would_overflow_the_index_space_does_not_wrap() {
        // A malformed Art-Net packet claiming 65535 pixels from offset 65000
        // must clip, not wrap round to the start of the strip.
        let out = admit(&binding(), &ingress(65_000, u16::MAX, None), uuid(9), 0);
        assert_eq!(out, Err(Refusal::OutsideBinding));
    }
}
