//! The render loop.
//!
//! Where the source stack, zones and the VM meet: for each LED, find the source
//! that renders it, run that source's program with the inputs its zone's
//! projection defines, and cross-fade anything on its way out.
//!
//! # Resolution is per pixel, every frame
//!
//! Zones overlap, so a pixel can be covered by several sources at once. The
//! highest-priority admitted source wins it. That is the whole reason the stack
//! is a stack: an alert over a show over an ambient scene resolves with no
//! special case, and it resolves independently on every device.
//!
//! # A device is never dark because of software
//!
//! Every failure here has a defined visual outcome. A program that faults leaves
//! the pixel showing what was underneath rather than black; a pixel no source
//! covers keeps its ambient value; a source still fading contributes until it
//! has finished. Nothing takes a pixel to zero by accident.
//!
//! # Splitting the work across cores
//!
//! The pixels of a frame are independent, so a device with more than one core
//! can render them on all of them. [`Shard`] is how: each core takes a
//! [`Renderer`], a shard, and the slice of the output its shard covers.
//!
//! No thread appears here, and none may. This crate performs no I/O and owns no
//! clock, which is what buys deterministic replay and conformance tests that
//! need no hardware; spending that on a speed-up available to a minority of
//! chips would be a bad trade. What the crate provides is the *seam* - the
//! firmware decides how many cores go through it, and
//! `slice::split_at_mut` hands each one its own output with no sharing, no
//! locking and no copy.
//!
//! Shards must render exactly what one whole render does. That is not a
//! nicety: a two-core device that rendered differently from a one-core device
//! would break the mesh's agreement with itself, which is the property every
//! other decision in this project is arranged to protect.
//! `shards_render_what_one_whole_does` holds it down, over several frames so
//! per-pixel history is covered too.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use lumen_proto::Uuid;
use lumen_vm::program::{Program, Section};
use lumen_vm::q16::Q16;
use lumen_vm::vm::{Machine, PixelInputs, PixelOutput, Uniforms};
use lumen_vm::Fault;

use crate::sources::{Source, SourceStack};
use crate::zones::{DeviceLeds, Membership, Projection};

/// One rendered pixel, in linear RGB.
///
/// Linear throughout. Gamma is applied once by the output stage, never by an
/// effect and never here — blending in linear and encoding once at the end is
/// the difference between fades that look right and fades that look cheap.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Rgb {
    pub r: Q16,
    pub g: Q16,
    pub b: Q16,
}

impl Rgb {
    pub const BLACK: Rgb = Rgb {
        r: Q16::ZERO,
        g: Q16::ZERO,
        b: Q16::ZERO,
    };

    pub fn new(r: Q16, g: Q16, b: Q16) -> Rgb {
        Rgb { r, g, b }
    }

    /// Cross-fade toward `other` by `t`, 0..1.
    pub fn mix(self, other: Rgb, t: Q16) -> Rgb {
        Rgb {
            r: self.r.lerp(other.r, t),
            g: self.g.lerp(other.g, t),
            b: self.b.lerp(other.b, t),
        }
    }

    fn from_output(out: PixelOutput) -> Option<Rgb> {
        match out {
            PixelOutput::Rgb { r, g, b } => Some(Rgb { r, g, b }),
            // The white channel is added by the output stage, which knows the
            // fixture's white point; the compositor works in RGB so that two
            // sources on differently-equipped devices still blend the same.
            PixelOutput::Rgbw { r, g, b, .. } => Some(Rgb { r, g, b }),
            PixelOutput::Cct { .. } => None,
            PixelOutput::None => None,
        }
    }
}

/// What a source needs in order to render.
pub struct Bound<'a> {
    pub source: Source,
    pub program: &'a Program<'a>,
    pub membership: &'a Membership,
    pub projection: Projection,
}

/// Why a source did not render this frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RenderFault {
    /// Its program faulted. The pixel keeps whatever was underneath.
    Program { source: Uuid, fault: Fault },
}

/// The outcome of one frame.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FrameReport {
    /// Sources that actually contributed a pixel.
    pub rendered: Vec<Uuid>,
    /// Sources that faulted, with why.
    ///
    /// Reported rather than silent: a program that faults every frame is a bug
    /// someone needs to hear about, and the alternative is a strip that is
    /// quietly the wrong colour.
    pub faults: Vec<RenderFault>,
    /// Budget units spent across every source.
    pub spent: u32,
}

impl FrameReport {
    /// Fold another core's report into this one.
    ///
    /// `spent` sums, and that total is genuinely higher than a single-core
    /// render of the same frame would report: each shard runs the `frame`
    /// section for itself. That is the price of the split and it is small - the
    /// `frame` section is the part that runs once against the hundreds of pixels
    /// that do not - but it is real, and reporting it as though it were free
    /// would turn the one number a device uses to know whether it is keeping up
    /// into a number that lies.
    pub fn merge(&mut self, other: FrameReport) {
        for id in other.rendered {
            if !self.rendered.contains(&id) {
                self.rendered.push(id);
            }
        }
        self.faults.extend(other.faults);
        self.spent = self.spent.saturating_add(other.spent);
    }
}

/// The run of LEDs one core renders.
///
/// A device with `n` cores builds `n` of these over its LED count, gives each
/// core one along with its own [`Renderer`] and the matching slice of the output
/// buffer, and merges the reports afterwards. Together they render exactly what
/// [`Shard::whole`] renders alone.
///
/// # Each core needs its own `Renderer`
///
/// The VM's register file survives from `frame` into every pixel of that frame -
/// which is the whole reason hoisting pays - so two cores sharing one machine
/// would be two cores writing one register file. The per-LED history is keyed by
/// LED, and shards own disjoint LEDs, so nothing there is shared either.
///
/// # Why every shard runs the `frame` section
///
/// A shard's renderer runs `frame` for itself rather than receiving hoisted
/// registers from a neighbour. The section is a pure function of the program and
/// `t`, so every shard computes the same registers, and buying that with a
/// little duplicated arithmetic is far cheaper than the alternative: handing a
/// live `Machine` between cores means shared mutable state, a barrier between
/// the frame section and the pixels, and a crate that could no longer be tested
/// without threads.
///
/// The exception is a probe build. `Uniforms::probe` is the one method on that
/// trait taking `&mut self`, so a probe would be recorded once per shard. Probe
/// builds render whole.
///
/// # Contiguous, and what that costs
///
/// A shard is a *run* of LED indices rather than every `n`th LED, so the output
/// buffer splits with `split_at_mut` and each core writes its own memory. An
/// interleaved split would balance an uneven effect better - a mask skips work,
/// an `if` takes one arm, and cost is not uniform along a strip - but it would
/// leave two cores writing alternating slots of one buffer, which is a shared
/// mutable buffer however carefully it is described.
///
/// So the imbalance is real and is the accepted cost: an effect that lights only
/// the first half of a strip gets no speed-up from a second core. It is bounded
/// by never being *slower* than one core, and the common case - an effect
/// covering the whole strip - splits evenly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Shard {
    first: u16,
    len: u16,
}

impl Shard {
    /// One core doing all of it, over a device with `len` LEDs. What
    /// [`Renderer::render`] uses, and what a single-core device renders.
    pub const fn whole(len: u16) -> Shard {
        Shard { first: 0, len }
    }

    /// Shard `index` of `of`, over a device with `len` LEDs.
    ///
    /// `None` if that is not a share of anything: zero shards, or an index
    /// outside them. Checked rather than clamped, because a firmware that
    /// computes a shard wrongly has a bug in how it counts its cores, and
    /// clamping would turn that into a strip where some LEDs render twice and
    /// others never - which looks like a broken effect and sends the next person
    /// to read the compiler.
    ///
    /// The remainder goes to the earlier shards, so with 301 LEDs across two
    /// cores one takes 151 and the other 150. Every LED belongs to exactly one
    /// shard, which is the property that matters; a shard may be empty when
    /// there are fewer LEDs than cores.
    pub fn new(index: u16, of: u16, len: u16) -> Option<Shard> {
        if of == 0 || index >= of {
            return None;
        }
        let (base, extra) = (len / of, len % of);
        let count = base + u16::from(index < extra);
        let first = index * base + index.min(extra);
        Some(Shard { first, len: count })
    }

    /// The first LED index this shard owns.
    pub fn first(self) -> u16 {
        self.first
    }

    /// How many LEDs it owns, and so how long its output slice must be.
    pub fn len(self) -> u16 {
        self.len
    }

    pub fn is_empty(self) -> bool {
        self.len == 0
    }

    /// Where LED `index` lands in this shard's output slice, if it owns it.
    fn slot(self, index: u16) -> Option<usize> {
        let offset = index.checked_sub(self.first)?;
        (offset < self.len).then_some(offset as usize)
    }
}

/// The LED with this index, without walking the whole strip to find it.
///
/// A device's LEDs are its own list in its own order, so in principle this is a
/// search. In practice a strip is `0..n` in order, and the index *is* the
/// position - so try that first and fall back to the search when a device is
/// laid out some other way.
///
/// The fallback was the only path until Spike S4 measured it: a linear scan per
/// pixel is quadratic in the strip, and at 300 LEDs it cost about as much per
/// frame as running the effect did. That is the shape of thing that never shows
/// up on a four-LED test and decides what a real device can drive.
fn find_led(leds: &DeviceLeds, index: u16) -> Option<&crate::zones::Led> {
    match leds.leds.get(index as usize) {
        Some(led) if led.index == index => Some(led),
        _ => leds.leds.iter().find(|l| l.index == index),
    }
}

/// Renders one device's LEDs.
///
/// Holds a machine per source, because the VM's register file survives from
/// `frame` into every pixel of that frame — which is the whole reason hoisting
/// pays, and sharing one machine between two sources would destroy it.
#[derive(Default)]
pub struct Renderer {
    machines: BTreeMap<Uuid, Machine>,
    /// `t` at the previous frame, so `dt` can be handed to the VM.
    ///
    /// Derived here rather than asked for, because a caller that has to supply
    /// it is a caller that can supply zero - and zero `dt` does not fail, it
    /// makes every trail permanent. That was the bug: the strip fills with stuck
    /// pixels, nothing is reported, and the effect is simply wrong.
    last_t: Option<Q16>,
    /// Per-LED history, fed back as `prev` next frame.
    ///
    /// Keyed by source as well as LED: two sources rendering the same pixel each
    /// have their own trail, and sharing one buffer would make an alert erase a
    /// show's history for as long as it was up.
    history: BTreeMap<(Uuid, u16), [Q16; 3]>,
}

impl Renderer {
    pub fn new() -> Renderer {
        Renderer::default()
    }

    /// Forget a source's state.
    ///
    /// Called when a source leaves the stack, or its history grows without
    /// bound on a device that has been running for a month.
    pub fn forget(&mut self, source: Uuid) {
        self.machines.remove(&source);
        self.history.retain(|(s, _), _| *s != source);
    }

    /// How many sources have live state.
    pub fn tracked(&self) -> usize {
        self.machines.len()
    }

    /// Render one frame into `out`.
    ///
    /// `out` must be as long as the device has LEDs. Anything no source covers
    /// keeps the value already there, which is what makes an ambient floor an
    /// ambient floor rather than a special case.
    ///
    /// Eight parameters, and each one is a distinct input the loop genuinely
    /// needs. Bundling them into a context struct would move the same list one
    /// level away and make the borrow checker's job harder for no reader's
    /// benefit.
    #[allow(clippy::too_many_arguments)]
    pub fn render<U: Uniforms>(
        &mut self,
        now_us: u64,
        t: Q16,
        leds: &DeviceLeds,
        stack: &SourceStack,
        bound: &[Bound<'_>],
        uniforms: &mut U,
        out: &mut [Rgb],
    ) -> FrameReport {
        let shard = Shard::whole(out.len().min(u16::MAX as usize) as u16);
        self.render_shard(now_us, t, leds, stack, bound, uniforms, out, shard)
    }

    /// Render this core's run of LEDs into `out`.
    ///
    /// For a device rendering on more than one core. `out` is this shard's own
    /// slice - `Shard::len` entries, starting at `Shard::first` - so two cores
    /// hold the two halves of one buffer from `split_at_mut` and neither can
    /// reach the other's pixels.
    ///
    /// Fold the reports together with [`FrameReport::merge`] once the cores have
    /// joined.
    ///
    /// A slice shorter than the shard renders what fits rather than panicking.
    /// A frame is a soft real-time thing produced sixty times a second; a
    /// mis-sized buffer should cost some pixels, not the device.
    #[allow(clippy::too_many_arguments)]
    pub fn render_shard<U: Uniforms>(
        &mut self,
        now_us: u64,
        t: Q16,
        leds: &DeviceLeds,
        stack: &SourceStack,
        bound: &[Bound<'_>],
        uniforms: &mut U,
        out: &mut [Rgb],
        shard: Shard,
    ) -> FrameReport {
        let mut report = FrameReport {
            rendered: Vec::new(),
            faults: Vec::new(),
            spent: 0,
        };

        // Seconds since the last frame, which is what makes a feedback effect
        // rate-independent: a trail written as `pow(decay, dt * 60)` is the same
        // length at 30 fps as at 60, and a mesh of mixed-rate devices shows one
        // effect rather than two.
        //
        // Zero on the first frame, and zero across a wrap of the show clock -
        // `t` wraps at 32 768 seconds and a negative `dt` would be worse than
        // none. One frame that does not decay is invisible; a negative one is
        // an effect running backwards.
        let dt = match self.last_t {
            Some(last) if t.0 >= last.0 => Q16(t.0 - last.0),
            _ => Q16::ZERO,
        };
        self.last_t = Some(t);

        // Bottom to top, so a higher-priority source overwrites a lower one and
        // the last write wins. Iterating top-down instead would need a per-pixel
        // "already claimed" set, which costs more than the overdraw it saves at
        // the two or three sources a device actually carries.
        let mut order: Vec<&Bound<'_>> = bound
            .iter()
            .filter(|b| stack.is_admitted(b.source.id))
            .collect();
        order.sort_by(|a, b| {
            a.source
                .priority
                .cmp(&b.source.priority)
                .then(a.source.pushed_at_us.cmp(&b.source.pushed_at_us))
        });

        for b in &order {
            // Weighted by how far through its fade in it is, which is `Q16::ONE`
            // for the overwhelming majority of sources - `fade_in_ms` defaults
            // to zero and most things simply appear.
            let alpha = b.source.fade_in_alpha(now_us);
            if alpha <= Q16::ZERO {
                // Nothing of it is showing yet. Skipping is not only cheaper:
                // rendering at zero would still charge the source's budget
                // against the frame for a contribution nobody can see.
                continue;
            }
            if self.render_source(
                now_us,
                t,
                dt,
                leds,
                b,
                uniforms,
                out,
                &mut report,
                alpha,
                shard,
            ) {
                report.rendered.push(b.source.id);
            }
        }

        // Then anything still fading, blended over the result by how far it has
        // left to go. A source fading out is still on top of what replaced it
        // until the fade finishes, which is what makes a hand-back look like a
        // fade rather than a cut.
        for fading in stack.fading() {
            let Some(b) = bound.iter().find(|b| b.source.id == fading.source.id) else {
                continue;
            };
            let remaining = Q16::ONE.sub(Q16(fading.progress(now_us) as i32));
            if remaining <= Q16::ZERO {
                continue;
            }
            self.render_source(
                now_us,
                t,
                dt,
                leds,
                b,
                uniforms,
                out,
                &mut report,
                remaining,
                shard,
            );
        }

        report
    }

    /// Render one source over `out`, weighted by `alpha`.
    #[allow(clippy::too_many_arguments)]
    fn render_source<U: Uniforms>(
        &mut self,
        _now_us: u64,
        t: Q16,
        dt: Q16,
        leds: &DeviceLeds,
        b: &Bound<'_>,
        uniforms: &mut U,
        out: &mut [Rgb],
        report: &mut FrameReport,
        alpha: Q16,
        shard: Shard,
    ) -> bool {
        if b.membership.is_empty() {
            return false;
        }
        // Nothing of this source falls in this shard's run, so not even the
        // frame section is worth running: no pixel of it would read the
        // registers the section hoists.
        if !b.membership.leds.iter().any(|i| shard.slot(*i).is_some()) {
            return false;
        }
        let machine = self.machines.entry(b.source.id).or_default();

        // The `frame` section gets its own allowance, not the per-pixel one.
        //
        // The header's `budget` is the cost of the *pixel* section - it is what
        // a device multiplies by its LED count to decide whether it can afford a
        // source. Spending it on the frame section instead charges the one part
        // of a program designed to be expensive against the allowance for the
        // part designed to be cheap, and faults exactly the effects that hoist
        // most: which is to say, the well-written ones. Spike S4 caught
        // `07-alert` failing this way on hardware, every frame, rendering
        // nothing at all.
        machine.set_budget(b.program.section_cost(Section::Frame).max(1));
        if let Err(fault) = machine.run_frame_at(b.program, t, dt, uniforms) {
            report.faults.push(RenderFault::Program {
                source: b.source.id,
                fault,
            });
            // The frame section failed, so every pixel would fail the same way.
            // Bailing here rather than per pixel keeps one bug from producing
            // three hundred identical reports.
            return false;
        }

        let frame_spent = machine.spent();
        // And the pixels are charged what the program promised per pixel, which
        // is the number the budget check at publish time was computed against.
        machine.set_budget(b.program.budget.max(1));

        let count = b.membership.bounds.count;
        let mut contributed = false;
        // Accumulated per pixel, because `Machine::spent` reports the *last*
        // invocation rather than a running total. Reading it once after the loop
        // charges the frame for one pixel and reports a device rendering three
        // hundred as though it had rendered one - which is a budget figure that
        // says a device is fine right up until it visibly is not.
        let mut pixel_spent = 0u32;
        for (ordinal, index) in b.membership.leds.iter().enumerate() {
            // `ordinal` still counts every LED of the membership, shard or no
            // shard: it is what the projection maps along, so skipping one must
            // not renumber the rest. A shard renders fewer pixels, never
            // different ones - and that is the whole equivalence.
            let Some(offset) = shard.slot(*index) else {
                continue;
            };
            let Some(led) = find_led(leds, *index) else {
                continue;
            };
            let Some(slot) = out.get_mut(offset) else {
                continue;
            };
            let projected = b
                .projection
                .project(led, &b.membership.bounds, ordinal as u16);
            let prev = self
                .history
                .get(&(b.source.id, *index))
                .copied()
                .unwrap_or([Q16::ZERO; 3]);

            let inputs = PixelInputs {
                x: led.world[0],
                y: led.world[1],
                z: led.world[2],
                lx: led.local[0],
                ly: led.local[1],
                lz: led.local[2],
                index: Q16::from_int(*index as i16),
                count: Q16::from_int(count as i16),
                u: projected.u,
                uv_x: projected.uv.map(|(u, _)| u).unwrap_or(Q16::ZERO),
                uv_y: projected.uv.map(|(_, v)| v).unwrap_or(Q16::ZERO),
                prev,
            };

            let outcome = machine.run_pixel(b.program, &inputs, uniforms);
            // Charged whether it succeeded or faulted: a pixel that ran out of
            // budget spent the budget it ran out of.
            pixel_spent = pixel_spent.saturating_add(machine.spent());
            match outcome {
                Ok(output) => {
                    self.history
                        .insert((b.source.id, *index), machine.prev_out());
                    if let Some(colour) = Rgb::from_output(output) {
                        *slot = if alpha >= Q16::ONE {
                            colour
                        } else {
                            slot.mix(colour, alpha)
                        };
                        contributed = true;
                    }
                }
                Err(fault) => {
                    // The pixel keeps what was underneath. Black would be a
                    // decision nobody made, and a dark strip is exactly the
                    // failure the "never dark because of software" rule names.
                    report.faults.push(RenderFault::Program {
                        source: b.source.id,
                        fault,
                    });
                    break;
                }
            }
        }
        // `spent` is per invocation, so the frame section's share has to be
        // carried across the pixel loop rather than read back at the end.
        report.spent = report
            .spent
            .saturating_add(frame_spent)
            .saturating_add(pixel_spent);
        contributed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use lumen_vm::isa::{Instruction, OpCode};
    use lumen_vm::program::builder::ProgramBuilder;
    use lumen_vm::vm::NoUniforms;

    use crate::sources::Change;
    use crate::zones::{Clause, Led, MapQuality, Zone};

    fn uuid(n: u8) -> Uuid {
        Uuid([n; 16])
    }

    /// A program emitting a constant colour.
    fn solid(r: f64, g: f64, b: f64) -> Vec<u8> {
        let mut p = ProgramBuilder::new();
        let q = |v: f64| Q16((v * 65536.0) as i32);
        let kr = p.constant(q(r));
        let kg = p.constant(q(g));
        let kb = p.constant(q(b));
        p.push(Section::Pixel, Instruction::with_imm(OpCode::LoadK, 20, kr));
        p.push(Section::Pixel, Instruction::with_imm(OpCode::LoadK, 21, kg));
        p.push(Section::Pixel, Instruction::with_imm(OpCode::LoadK, 22, kb));
        p.push(
            Section::Pixel,
            Instruction::new(OpCode::EmitRgb, 20, 21, 22),
        );
        p.build()
    }

    /// A program emitting `u` in every channel — a ramp along the zone.
    fn ramp() -> Vec<u8> {
        let mut p = ProgramBuilder::new();
        p.push(Section::Pixel, Instruction::new(OpCode::EmitRgb, 8, 8, 8));
        p.build()
    }

    /// A program that divides by zero on every pixel.
    fn faulty() -> Vec<u8> {
        let mut p = ProgramBuilder::new();
        let one = p.constant(Q16::ONE);
        p.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::LoadK, 20, one),
        );
        p.push(Section::Pixel, Instruction::new(OpCode::Div, 22, 20, 21));
        p.push(
            Section::Pixel,
            Instruction::new(OpCode::EmitRgb, 22, 22, 22),
        );
        p.build()
    }

    fn device(n: u16) -> DeviceLeds {
        DeviceLeds {
            device: uuid(1),
            quality: MapQuality::Mapped,
            leds: (0..n)
                .map(|i| Led {
                    index: i,
                    world: [Q16::from_int(i as i16), Q16::ZERO, Q16::ZERO],
                    local: [Q16::from_int(i as i16), Q16::ZERO, Q16::ZERO],
                })
                .collect(),
        }
    }

    fn whole_device(dev: &DeviceLeds) -> (Zone, Membership) {
        let z = Zone {
            id: uuid(50),
            include: vec![Clause::Device {
                device: dev.device,
                leds: None,
            }],
            exclude: vec![],
            projection: Projection::Strip,
        };
        let m = z.resolve(dev);
        (z, m)
    }

    fn source(id: u8, priority: u8, expires: Option<u64>) -> Source {
        Source {
            id: uuid(id),
            zone: uuid(50),
            scene: uuid(id),
            priority,
            expires_at_us: expires,
            fade_in_ms: 0,
            fade_out_ms: 0,
            pushed_at_us: 0,
            cost: 10,
        }
    }

    /// A program whose colour varies along the strip *and* feeds back its own
    /// history, so a comparison cannot pass by rendering one flat colour and
    /// cannot pass while feeding a shard the wrong pixel's `prev` either.
    fn ramp_with_history() -> Vec<u8> {
        use lumen_vm::vm::{R_PREV, R_T, R_U};
        let mut p = ProgramBuilder::new();
        let half = p.constant(Q16::HALF);
        // A frame section that actually does something, so every shard running
        // it for itself is exercised rather than assumed - and so the cost of
        // that duplication is a number a test can see.
        p.push(Section::Frame, Instruction::new(OpCode::Sqrt, 25, R_T, 0));
        p.push(Section::Pixel, Instruction::new(OpCode::Mov, 20, R_U, 0));
        // blue = (prev.r + u) / 2, which converges to a different value on
        // every LED and only after several frames.
        p.push(
            Section::Pixel,
            Instruction::new(OpCode::Add, 21, 20, R_PREV),
        );
        p.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::LoadK, 22, half),
        );
        p.push(Section::Pixel, Instruction::new(OpCode::Mul, 21, 21, 22));
        p.push(
            Section::Pixel,
            Instruction::new(OpCode::EmitRgb, 20, 20, 21),
        );
        p.build()
    }

    /// One admitted source covering a whole device of `n` LEDs.
    fn one_source_over(n: u16) -> (DeviceLeds, Zone, Membership, SourceStack, Source, Vec<u8>) {
        let dev = device(n);
        let (zone, mem) = whole_device(&dev);
        let mut stack = SourceStack::new(1_000, 4);
        let src = source(1, 10, None);
        stack.push(0, src, &mut Vec::new()).unwrap();
        (dev, zone, mem, stack, src, ramp_with_history())
    }

    #[test]
    fn a_shard_of_one_is_the_whole_device() {
        assert_eq!(Shard::whole(300), Shard::new(0, 1, 300).unwrap());
        let one = Shard::whole(300);
        assert_eq!((one.first(), one.len()), (0, 300));
        assert!(!one.is_empty());
    }

    #[test]
    fn shards_tile_the_strip_with_no_gap_and_no_overlap() {
        // The property that actually matters: every LED belongs to exactly one
        // shard. A gap is a dark run on the strip; an overlap is two cores
        // writing one slot, which is the bug this whole design exists to avoid.
        for len in [0u16, 1, 2, 5, 60, 299, 300, 301] {
            for of in [1u16, 2, 3, 4, 8] {
                let mut covered = vec![0u8; len as usize];
                let mut next = 0u16;
                for k in 0..of {
                    let shard = Shard::new(k, of, len).expect("a real share");
                    assert_eq!(shard.first(), next, "shards must abut: len {len}, {k}/{of}");
                    next += shard.len();
                    for i in shard.first()..shard.first() + shard.len() {
                        covered[i as usize] += 1;
                    }
                }
                assert_eq!(next, len, "shards must cover the strip: len {len}, of {of}");
                assert!(
                    covered.iter().all(|c| *c == 1),
                    "len {len}, of {of}: {covered:?}"
                );
            }
        }
    }

    #[test]
    fn the_remainder_goes_to_the_earliest_shards() {
        // 301 across two cores is 151 and 150, not 150 and 150 with one LED
        // nobody renders.
        assert_eq!(Shard::new(0, 2, 301).unwrap().len(), 151);
        assert_eq!(Shard::new(1, 2, 301).unwrap().len(), 150);
        assert_eq!(Shard::new(1, 2, 301).unwrap().first(), 151);
    }

    #[test]
    fn more_cores_than_leds_leaves_some_with_nothing() {
        let last = Shard::new(3, 4, 2).unwrap();
        assert!(last.is_empty());
        assert_eq!(last.first(), 2);
    }

    #[test]
    fn a_shard_that_is_not_a_share_of_anything_is_refused() {
        // Clamping would give a strip where some LEDs render twice and others
        // never, which looks like a broken effect rather than a miscounted core.
        assert!(Shard::new(0, 0, 300).is_none());
        assert!(Shard::new(2, 2, 300).is_none());
        assert!(Shard::new(9, 4, 300).is_none());
    }

    #[test]
    fn shards_render_what_one_whole_does() {
        // The claim the whole multicore design rests on, checked rather than
        // asserted. If this ever differs, a two-core device renders a different
        // show from a one-core device and the mesh stops agreeing with itself -
        // which is worse than being slow, and would stay invisible until two
        // kinds of device shared one room.
        //
        // Four frames, because one would not exercise the per-LED history: a
        // shard feeding back the wrong pixel's `prev` passes a single-frame
        // comparison and drifts apart from the second frame on.
        const FRAMES: u32 = 4;
        let (dev, zone, mem, stack, src, bytes) = one_source_over(24);
        let program = Program::parse(&bytes).unwrap();
        let bound = [Bound {
            source: src,
            program: &program,
            membership: &mem,
            projection: zone.projection,
        }];

        let mut whole_r = Renderer::new();
        let mut whole = vec![Rgb::BLACK; 24];
        for f in 0..FRAMES {
            let t = Q16::from_ratio(f as i32, 60);
            whole_r.render(
                f as u64 * 16_667,
                t,
                &dev,
                &stack,
                &bound,
                &mut NoUniforms,
                &mut whole,
            );
        }

        for of in [2u16, 3, 4, 5] {
            let mut renderers: Vec<Renderer> = (0..of).map(|_| Renderer::new()).collect();
            let mut out = vec![Rgb::BLACK; 24];

            for f in 0..FRAMES {
                let t = Q16::from_ratio(f as i32, 60);
                // Exactly what a firmware does: hand each core its own run of
                // the buffer. No sharing, no locking, no copy.
                let mut rest: &mut [Rgb] = &mut out;
                for (k, r) in renderers.iter_mut().enumerate() {
                    let shard = Shard::new(k as u16, of, 24).unwrap();
                    let (mine, tail) = rest.split_at_mut(shard.len() as usize);
                    rest = tail;
                    r.render_shard(
                        f as u64 * 16_667,
                        t,
                        &dev,
                        &stack,
                        &bound,
                        &mut NoUniforms,
                        mine,
                        shard,
                    );
                }
                assert!(rest.is_empty(), "the shards did not consume the buffer");
            }

            assert_eq!(out, whole, "{of} shards rendered a different frame");
        }

        // And the comparison is not vacuous: the strip is neither flat nor
        // black, so an all-zero render could not have passed it.
        assert_ne!(whole[0], whole[23]);
        assert_ne!(whole[23], Rgb::BLACK);
    }

    #[test]
    fn merged_reports_name_every_source_once() {
        let (dev, zone, mem, stack, src, bytes) = one_source_over(8);
        let program = Program::parse(&bytes).unwrap();
        let bound = [Bound {
            source: src,
            program: &program,
            membership: &mem,
            projection: zone.projection,
        }];

        let mut whole_r = Renderer::new();
        let mut whole_out = vec![Rgb::BLACK; 8];
        let single = whole_r.render(
            0,
            Q16::ZERO,
            &dev,
            &stack,
            &bound,
            &mut NoUniforms,
            &mut whole_out,
        );

        let mut merged = FrameReport {
            rendered: Vec::new(),
            faults: Vec::new(),
            spent: 0,
        };
        let mut out = vec![Rgb::BLACK; 8];
        let mut rest: &mut [Rgb] = &mut out;
        for k in 0..2u16 {
            let shard = Shard::new(k, 2, 8).unwrap();
            let (mine, tail) = rest.split_at_mut(shard.len() as usize);
            rest = tail;
            let mut r = Renderer::new();
            merged.merge(r.render_shard(
                0,
                Q16::ZERO,
                &dev,
                &stack,
                &bound,
                &mut NoUniforms,
                mine,
                shard,
            ));
        }

        // The source rendered on both cores, and is named once.
        assert_eq!(merged.rendered, single.rendered);
        assert!(merged.faults.is_empty());
        // Spend is higher, not equal: each shard ran the frame section. A
        // device sizing its frame needs the true figure, not the flattering
        // one - see the doc comment on `merge`.
        assert!(
            merged.spent > single.spent,
            "merged {} vs whole {}",
            merged.spent,
            single.spent
        );
    }

    #[test]
    fn a_shard_owning_none_of_a_source_does_not_run_its_frame_section() {
        // A source covering only the front of the strip must cost the back core
        // nothing at all - not even the frame section, whose hoisted registers
        // no pixel of that core would read.
        let (dev, zone, mut mem, stack, src, bytes) = one_source_over(8);
        mem.leds.retain(|i| *i < 4);
        let program = Program::parse(&bytes).unwrap();
        let bound = [Bound {
            source: src,
            program: &program,
            membership: &mem,
            projection: zone.projection,
        }];

        let far = Shard::new(1, 2, 8).unwrap();
        let mut r = Renderer::new();
        let mut out = vec![Rgb::BLACK; far.len() as usize];
        let report = r.render_shard(
            0,
            Q16::ZERO,
            &dev,
            &stack,
            &bound,
            &mut NoUniforms,
            &mut out,
            far,
        );

        assert!(report.rendered.is_empty());
        assert_eq!(report.spent, 0, "the frame section ran for no pixels");
        assert_eq!(r.tracked(), 0, "and no machine was built for it either");
        assert!(out.iter().all(|p| *p == Rgb::BLACK));
    }

    #[test]
    fn an_output_slice_shorter_than_the_shard_renders_what_fits() {
        // A mis-sized buffer costs pixels, not the device. Sixty times a second
        // is the wrong cadence for a panic.
        let (dev, zone, mem, stack, src, bytes) = one_source_over(8);
        let program = Program::parse(&bytes).unwrap();
        let bound = [Bound {
            source: src,
            program: &program,
            membership: &mem,
            projection: zone.projection,
        }];
        let mut r = Renderer::new();
        let mut out = vec![Rgb::BLACK; 3];
        let report = r.render_shard(
            0,
            Q16::ZERO,
            &dev,
            &stack,
            &bound,
            &mut NoUniforms,
            &mut out,
            Shard::whole(8),
        );
        assert_eq!(report.rendered, vec![uuid(1)]);
    }

    #[test]
    fn a_hoisted_frame_section_is_not_charged_the_per_pixel_budget() {
        // Spike S4 found `07-alert` faulting every frame on real hardware and
        // rendering nothing: its `frame` section costs more than its per-pixel
        // budget, and the render loop was spending the one on the other.
        //
        // That penalises exactly the programs the VM's whole performance story
        // asks authors to write. Hoisting work out of the pixel section is the
        // point; an effect should not fault for doing it well.
        let mut p = ProgramBuilder::new();
        let one = p.constant(Q16::ONE);
        // A frame section far dearer than the single instruction per pixel.
        for r in 20..31u8 {
            p.push(Section::Frame, Instruction::with_imm(OpCode::LoadK, r, one));
            p.push(Section::Frame, Instruction::new(OpCode::Sqrt, r, r, 0));
        }
        p.push(
            Section::Pixel,
            Instruction::new(OpCode::EmitRgb, 20, 20, 20),
        );
        let bytes = p.build();

        let program = Program::parse(&bytes).unwrap();
        assert!(
            program.section_cost(Section::Frame) > program.budget,
            "the test program does not have the shape the bug needs"
        );

        let dev = device(4);
        let (zone, mem) = whole_device(&dev);
        let mut stack = SourceStack::new(100_000, 4);
        let src = source(1, 10, None);
        stack.push(0, src, &mut Vec::new()).unwrap();

        let mut out = vec![Rgb::BLACK; 4];
        let mut r = Renderer::new();
        let report = r.render(
            0,
            Q16::ZERO,
            &dev,
            &stack,
            &[Bound {
                source: src,
                program: &program,
                membership: &mem,
                projection: zone.projection,
            }],
            &mut NoUniforms,
            &mut out,
        );

        assert!(report.faults.is_empty(), "{:?}", report.faults);
        assert_eq!(report.rendered, vec![uuid(1)]);
        assert!(out.iter().all(|p| p.r == Q16::ONE));
        // And the frame section's cost is still reported, not lost when the
        // limit was reset for the pixels.
        assert!(report.spent > program.budget * 4);
    }

    #[test]
    fn a_frame_reports_what_every_pixel_cost_not_what_the_last_one_did() {
        // Spike S5 caught this on hardware: a device rendering thirty LEDs
        // reported 391 units a frame, which is one pixel's worth. A device sizes
        // its frame from this number, so under-reporting by the length of the
        // strip is a budget that says everything is fine right until the light
        // visibly stutters.
        let dev = device(8);
        let (zone, mem) = whole_device(&dev);
        let bytes = solid(1.0, 0.5, 0.0);
        let program = Program::parse(&bytes).unwrap();

        let mut stack = SourceStack::new(100_000, 4);
        let src = source(1, 10, None);
        stack.push(0, src, &mut Vec::new()).unwrap();

        let bound = [Bound {
            source: src,
            program: &program,
            membership: &mem,
            projection: zone.projection,
        }];

        let mut one = Renderer::new();
        let mut out1 = vec![Rgb::BLACK; 8];
        let eight = one
            .render(
                0,
                Q16::ZERO,
                &dev,
                &stack,
                &bound,
                &mut NoUniforms,
                &mut out1,
            )
            .spent;

        // The same program over half as many LEDs costs about half as much.
        // Exactly half is not asserted: the frame section is paid once either
        // way, and that is the part that does not scale.
        let dev4 = device(4);
        let (zone4, mem4) = whole_device(&dev4);
        let bound4 = [Bound {
            source: src,
            program: &program,
            membership: &mem4,
            projection: zone4.projection,
        }];
        let mut other = Renderer::new();
        let mut out2 = vec![Rgb::BLACK; 4];
        let four = other
            .render(
                0,
                Q16::ZERO,
                &dev4,
                &stack,
                &bound4,
                &mut NoUniforms,
                &mut out2,
            )
            .spent;

        assert!(
            eight > four,
            "eight LEDs reported {eight}, four reported {four}"
        );
        assert!(
            eight >= program.budget * 8,
            "eight pixels at {} units each reported only {eight}",
            program.budget
        );
    }

    #[test]
    fn dt_is_the_gap_between_frames_and_not_the_clock() {
        // `dt` used to compile to the same register as `t`, so it was the
        // absolute show time. Nothing failed: `pow(decay, dt * 60)` saturated,
        // every trail became permanent, and a real strip filled with stuck white
        // pixels over about a minute. The language documents rate-independent
        // decay as *the* way to write a feedback effect, so this was wrong in
        // the construct people are told to reach for.
        use lumen_vm::vm::R_DT;

        let mut p = ProgramBuilder::new();
        // Declared, because the VM supplies `dt` only to a program whose header
        // says it reads one - that is what lets every other program keep the
        // register. A hand-built program that forgets this reads zero, which
        // looks exactly like the bug this test exists to catch.
        p.reads_dt = true;
        // Emit `dt` straight out, so the rendered colour *is* the value the VM
        // held for it.
        p.push(
            Section::Pixel,
            Instruction::new(OpCode::EmitRgb, R_DT, R_DT, R_DT),
        );
        let bytes = p.build();
        let program = Program::parse(&bytes).unwrap();

        let dev = device(2);
        let (zone, mem) = whole_device(&dev);
        let mut stack = SourceStack::new(100_000, 4);
        let src = source(1, 10, None);
        stack.push(0, src, &mut Vec::new()).unwrap();
        let bound = [Bound {
            source: src,
            program: &program,
            membership: &mem,
            projection: zone.projection,
        }];

        let mut r = Renderer::new();
        let mut out = vec![Rgb::BLACK; 2];

        // First frame: nothing to measure from, so zero. One frame that does
        // not decay is invisible; a guess would not be.
        r.render(
            0,
            Q16::from_int(100),
            &dev,
            &stack,
            &bound,
            &mut NoUniforms,
            &mut out,
        );
        assert_eq!(out[0].r, Q16::ZERO);

        // A frame later. `t` is 100.5 s into the show and `dt` is half a second:
        // the gap, not the clock.
        let half_later = Q16(Q16::from_int(100).0 + Q16::HALF.0);
        r.render(
            0,
            half_later,
            &dev,
            &stack,
            &bound,
            &mut NoUniforms,
            &mut out,
        );
        assert_eq!(out[0].r, Q16::HALF);

        // And across a wrap of the show clock, zero rather than an enormous
        // negative - an effect that ran backwards for one frame would be a
        // visible glitch every nine hours.
        r.render(
            0,
            Q16::from_int(1),
            &dev,
            &stack,
            &bound,
            &mut NoUniforms,
            &mut out,
        );
        assert_eq!(out[0].r, Q16::ZERO);
    }

    #[test]
    fn one_source_fills_its_zone() {
        let dev = device(4);
        let (zone, mem) = whole_device(&dev);
        let bytes = solid(1.0, 0.5, 0.0);
        let program = Program::parse(&bytes).unwrap();

        let mut stack = SourceStack::new(1_000, 4);
        let mut changes = Vec::new();
        let src = source(1, 10, None);
        stack.push(0, src, &mut changes).unwrap();

        let mut out = vec![Rgb::BLACK; 4];
        let mut r = Renderer::new();
        let report = r.render(
            0,
            Q16::ZERO,
            &dev,
            &stack,
            &[Bound {
                source: src,
                program: &program,
                membership: &mem,
                projection: zone.projection,
            }],
            &mut NoUniforms,
            &mut out,
        );

        assert_eq!(report.rendered, vec![uuid(1)]);
        assert!(report.faults.is_empty());
        assert!(report.spent > 0);
        for pixel in &out {
            assert_eq!(pixel.r, Q16::ONE);
            assert_eq!(pixel.g, Q16::HALF);
            assert_eq!(pixel.b, Q16::ZERO);
        }
    }

    #[test]
    fn the_zone_projection_reaches_the_program() {
        // `u` is what makes a 1D effect work on any zone, so it has to be the
        // position within the zone rather than anything the device knows.
        let dev = device(5);
        let (zone, mem) = whole_device(&dev);
        let bytes = ramp();
        let program = Program::parse(&bytes).unwrap();

        let mut stack = SourceStack::new(1_000, 4);
        let mut changes = Vec::new();
        let src = source(1, 10, None);
        stack.push(0, src, &mut changes).unwrap();

        let mut out = vec![Rgb::BLACK; 5];
        let mut r = Renderer::new();
        r.render(
            0,
            Q16::ZERO,
            &dev,
            &stack,
            &[Bound {
                source: src,
                program: &program,
                membership: &mem,
                projection: zone.projection,
            }],
            &mut NoUniforms,
            &mut out,
        );
        assert_eq!(out[0].r, Q16::ZERO);
        assert_eq!(out[4].r, Q16::ONE);
        assert!(out[2].r > out[1].r);
    }

    #[test]
    fn the_higher_priority_source_wins_the_pixel() {
        // The whole argument for the stack, at the pixel level.
        let dev = device(3);
        let (zone, mem) = whole_device(&dev);
        let ambient_bytes = solid(0.25, 0.0, 0.0);
        let alert_bytes = solid(0.0, 0.0, 1.0);
        let ambient_p = Program::parse(&ambient_bytes).unwrap();
        let alert_p = Program::parse(&alert_bytes).unwrap();

        let mut stack = SourceStack::new(1_000, 4);
        let mut changes = Vec::new();
        let ambient = source(1, 10, None);
        let alert = source(2, 240, Some(9_000));
        stack.push(0, ambient, &mut changes).unwrap();
        stack.push(0, alert, &mut changes).unwrap();

        let bound = [
            Bound {
                source: ambient,
                program: &ambient_p,
                membership: &mem,
                projection: zone.projection,
            },
            Bound {
                source: alert,
                program: &alert_p,
                membership: &mem,
                projection: zone.projection,
            },
        ];
        let mut out = vec![Rgb::BLACK; 3];
        let mut r = Renderer::new();
        r.render(
            0,
            Q16::ZERO,
            &dev,
            &stack,
            &bound,
            &mut NoUniforms,
            &mut out,
        );
        assert_eq!(out[0].b, Q16::ONE, "the alert should own the pixel");
        assert_eq!(out[0].r, Q16::ZERO);

        // When the alert expires the ambient scene is underneath, unchanged.
        stack.advance(9_000, &mut changes);
        let mut out2 = vec![Rgb::BLACK; 3];
        r.render(
            9_000,
            Q16::ZERO,
            &dev,
            &stack,
            &bound,
            &mut NoUniforms,
            &mut out2,
        );
        assert_eq!(out2[0].r, Q16::from_ratio(1, 4));
        assert_eq!(out2[0].b, Q16::ZERO);
    }

    #[test]
    fn a_pixel_no_source_covers_keeps_what_was_there() {
        // What makes an ambient floor a floor rather than a special case.
        let dev = device(6);
        let z = Zone {
            id: uuid(50),
            include: vec![Clause::Device {
                device: dev.device,
                leds: Some((0, 3)),
            }],
            exclude: vec![],
            projection: Projection::Strip,
        };
        let mem = z.resolve(&dev);
        let bytes = solid(1.0, 1.0, 1.0);
        let program = Program::parse(&bytes).unwrap();

        let mut stack = SourceStack::new(1_000, 4);
        let mut changes = Vec::new();
        let src = source(1, 10, None);
        stack.push(0, src, &mut changes).unwrap();

        let held = Rgb::new(Q16::HALF, Q16::ZERO, Q16::ZERO);
        let mut out = vec![held; 6];
        let mut r = Renderer::new();
        r.render(
            0,
            Q16::ZERO,
            &dev,
            &stack,
            &[Bound {
                source: src,
                program: &program,
                membership: &mem,
                projection: z.projection,
            }],
            &mut NoUniforms,
            &mut out,
        );
        assert_eq!(out[0].r, Q16::ONE, "inside the zone");
        assert_eq!(out[4], held, "outside the zone, untouched");
    }

    #[test]
    fn a_faulting_program_leaves_the_pixel_showing_what_was_underneath() {
        // Black would be a decision nobody made, and a dark strip is exactly the
        // failure the "never dark because of software" rule names.
        let dev = device(3);
        let (zone, mem) = whole_device(&dev);
        let bytes = faulty();
        let program = Program::parse(&bytes).unwrap();

        let mut stack = SourceStack::new(1_000, 4);
        let mut changes = Vec::new();
        let src = source(1, 10, None);
        stack.push(0, src, &mut changes).unwrap();

        let held = Rgb::new(Q16::HALF, Q16::HALF, Q16::HALF);
        let mut out = vec![held; 3];
        let mut r = Renderer::new();
        let report = r.render(
            0,
            Q16::ZERO,
            &dev,
            &stack,
            &[Bound {
                source: src,
                program: &program,
                membership: &mem,
                projection: zone.projection,
            }],
            &mut NoUniforms,
            &mut out,
        );
        assert_eq!(out[0], held, "the pixel went dark on a fault");
        assert_eq!(report.faults.len(), 1, "the fault must be reported");
        assert!(matches!(
            report.faults[0],
            RenderFault::Program {
                fault: Fault::DivideByZero,
                ..
            }
        ));
    }

    #[test]
    fn a_rejected_source_does_not_render() {
        // Admission said no; rendering it anyway would blow the budget the
        // admission decision existed to protect.
        let dev = device(2);
        let (zone, mem) = whole_device(&dev);
        let bytes = solid(1.0, 0.0, 0.0);
        let program = Program::parse(&bytes).unwrap();

        let mut stack = SourceStack::new(100, 8);
        let mut changes = Vec::new();
        let mut big = source(1, 240, Some(9_000));
        big.cost = 80;
        let mut small = source(2, 100, Some(9_000));
        small.cost = 80;
        stack.push(0, big, &mut changes).unwrap();
        stack.push(0, small, &mut changes).unwrap();
        assert!(!stack.is_admitted(uuid(2)));

        let mut out = vec![Rgb::BLACK; 2];
        let mut r = Renderer::new();
        let report = r.render(
            0,
            Q16::ZERO,
            &dev,
            &stack,
            &[
                Bound {
                    source: big,
                    program: &program,
                    membership: &mem,
                    projection: zone.projection,
                },
                Bound {
                    source: small,
                    program: &program,
                    membership: &mem,
                    projection: zone.projection,
                },
            ],
            &mut NoUniforms,
            &mut out,
        );
        assert_eq!(report.rendered, vec![uuid(1)]);
    }

    #[test]
    fn a_fading_source_still_contributes_until_it_finishes() {
        // A hand-back should look like a fade, not a cut.
        let dev = device(2);
        let (zone, mem) = whole_device(&dev);
        let old_bytes = solid(1.0, 0.0, 0.0);
        let new_bytes = solid(0.0, 0.0, 1.0);
        let old_p = Program::parse(&old_bytes).unwrap();
        let new_p = Program::parse(&new_bytes).unwrap();

        let mut stack = SourceStack::new(1_000, 4);
        let mut changes = Vec::new();
        let mut going = source(1, 200, Some(1_000));
        going.fade_out_ms = 100;
        let staying = source(2, 10, None);
        stack.push(0, going, &mut changes).unwrap();
        stack.push(0, staying, &mut changes).unwrap();

        let bound = [
            Bound {
                source: going,
                program: &old_p,
                membership: &mem,
                projection: zone.projection,
            },
            Bound {
                source: staying,
                program: &new_p,
                membership: &mem,
                projection: zone.projection,
            },
        ];

        // It expires and starts fading.
        stack.advance(1_000, &mut changes);
        assert_eq!(stack.fading().len(), 1);

        let mut r = Renderer::new();
        let mut early = vec![Rgb::BLACK; 2];
        r.render(
            1_000,
            Q16::ZERO,
            &dev,
            &stack,
            &bound,
            &mut NoUniforms,
            &mut early,
        );
        let mut late = vec![Rgb::BLACK; 2];
        r.render(
            1_000 + 90_000,
            Q16::ZERO,
            &dev,
            &stack,
            &bound,
            &mut NoUniforms,
            &mut late,
        );

        assert!(early[0].r > late[0].r, "the outgoing source did not fade");
        assert!(late[0].b > early[0].b, "the incoming source did not arrive");
    }

    #[test]
    fn each_source_keeps_its_own_history() {
        // Two sources rendering the same pixel each have their own trail.
        // Sharing one buffer would make an alert erase a show's history for as
        // long as it was up.
        let dev = device(1);
        let (zone, mem) = whole_device(&dev);
        let a_bytes = solid(1.0, 0.0, 0.0);
        let b_bytes = solid(0.0, 1.0, 0.0);
        let a_p = Program::parse(&a_bytes).unwrap();
        let b_p = Program::parse(&b_bytes).unwrap();

        let mut stack = SourceStack::new(1_000, 4);
        let mut changes = Vec::new();
        let a = source(1, 10, None);
        let b = source(2, 20, Some(9_000));
        stack.push(0, a, &mut changes).unwrap();
        stack.push(0, b, &mut changes).unwrap();

        let mut out = vec![Rgb::BLACK; 1];
        let mut r = Renderer::new();
        r.render(
            0,
            Q16::ZERO,
            &dev,
            &stack,
            &[
                Bound {
                    source: a,
                    program: &a_p,
                    membership: &mem,
                    projection: zone.projection,
                },
                Bound {
                    source: b,
                    program: &b_p,
                    membership: &mem,
                    projection: zone.projection,
                },
            ],
            &mut NoUniforms,
            &mut out,
        );
        assert_eq!(r.tracked(), 2, "each source needs its own machine");

        r.forget(uuid(2));
        assert_eq!(r.tracked(), 1);
    }

    #[test]
    fn an_empty_membership_renders_nothing_and_does_not_panic() {
        let dev = device(2);
        let z = Zone {
            id: uuid(50),
            include: vec![Clause::Device {
                device: uuid(99),
                leds: None,
            }],
            exclude: vec![],
            projection: Projection::Strip,
        };
        let mem = z.resolve(&dev);
        assert!(mem.is_empty());
        let bytes = solid(1.0, 1.0, 1.0);
        let program = Program::parse(&bytes).unwrap();

        let mut stack = SourceStack::new(1_000, 4);
        let mut changes = Vec::new();
        let src = source(1, 10, None);
        stack.push(0, src, &mut changes).unwrap();

        let mut out = vec![Rgb::BLACK; 2];
        let mut r = Renderer::new();
        let report = r.render(
            0,
            Q16::ZERO,
            &dev,
            &stack,
            &[Bound {
                source: src,
                program: &program,
                membership: &mem,
                projection: z.projection,
            }],
            &mut NoUniforms,
            &mut out,
        );
        assert!(report.rendered.is_empty());
        assert_eq!(out[0], Rgb::BLACK);
    }

    #[test]
    fn an_empty_stack_leaves_every_pixel_alone() {
        let dev = device(3);
        let stack = SourceStack::new(1_000, 4);
        let held = Rgb::new(Q16::HALF, Q16::HALF, Q16::ZERO);
        let mut out = vec![held; 3];
        let mut r = Renderer::new();
        let report = r.render(0, Q16::ZERO, &dev, &stack, &[], &mut NoUniforms, &mut out);
        assert!(report.rendered.is_empty());
        assert_eq!(out[0], held);
        assert_eq!(report.spent, 0);
    }

    #[test]
    fn mixing_hits_both_ends_exactly() {
        let a = Rgb::new(Q16::ZERO, Q16::ZERO, Q16::ZERO);
        let b = Rgb::new(Q16::ONE, Q16::ONE, Q16::ONE);
        assert_eq!(a.mix(b, Q16::ZERO), a);
        assert_eq!(a.mix(b, Q16::ONE), b);
        assert_eq!(a.mix(b, Q16::HALF).r, Q16::HALF);
    }

    #[test]
    fn changes_from_the_stack_are_consumed_so_the_test_is_honest() {
        // Not an assertion about rendering - it just keeps `changes` used, so a
        // reader is not left wondering whether the stack reported something the
        // renderer should have acted on.
        let mut stack = SourceStack::new(1_000, 4);
        let mut changes = Vec::new();
        stack.push(0, source(1, 10, None), &mut changes).unwrap();
        assert!(changes.iter().any(|c| matches!(c, Change::Admitted(_))));
    }

    #[test]
    fn a_source_fading_in_arrives_gradually() {
        // `fade_in_ms` was decoded from the wire and never used: a source asking
        // to arrive over a second appeared instantly. This is the render-loop
        // half of fixing that.
        let dev = device(4);
        let (zone, mem) = whole_device(&dev);
        let bytes = solid(1.0, 1.0, 1.0);
        let program = Program::parse(&bytes).unwrap();

        let mut src = source(1, 10, None);
        src.fade_in_ms = 1_000;
        src.pushed_at_us = 2_000_000;

        let mut stack = SourceStack::new(1_000, 4);
        let mut changes = Vec::new();
        stack.push(0, src, &mut changes).unwrap();

        let render_at = |now_us: u64| {
            let mut out = vec![Rgb::BLACK; 4];
            let mut r = Renderer::new();
            let report = r.render(
                now_us,
                Q16::ZERO,
                &dev,
                &stack,
                &[Bound {
                    source: src,
                    program: &program,
                    membership: &mem,
                    projection: zone.projection,
                }],
                &mut NoUniforms,
                &mut out,
            );
            (out[0], report)
        };

        // Before it starts: nothing showing, and nothing charged for it either.
        let (pixel, report) = render_at(2_000_000);
        assert_eq!(pixel, Rgb::BLACK);
        assert!(report.rendered.is_empty(), "rendered before it had arrived");

        // Halfway: over black, half brightness.
        let (pixel, _) = render_at(2_500_000);
        assert_eq!(pixel.r, Q16::HALF);

        // Arrived.
        let (pixel, report) = render_at(3_000_000);
        assert_eq!(pixel.r, Q16::ONE);
        assert_eq!(report.rendered, vec![uuid(1)]);
    }
}

#[cfg(test)]
mod find_tests {
    use super::*;
    use crate::zones::{Led, MapQuality};
    use alloc::vec;
    use lumen_proto::Uuid;

    fn led(index: u16) -> Led {
        Led {
            index,
            world: [Q16::from_int(index as i16), Q16::ZERO, Q16::ZERO],
            local: [Q16::ZERO; 3],
        }
    }

    fn dev(leds: Vec<Led>) -> DeviceLeds {
        DeviceLeds {
            device: Uuid([1; 16]),
            quality: MapQuality::Mapped,
            leds,
        }
    }

    #[test]
    fn a_strip_in_order_is_found_by_position() {
        let d = dev((0..8).map(led).collect());
        for i in 0..8u16 {
            assert_eq!(find_led(&d, i).unwrap().index, i);
        }
        assert!(find_led(&d, 8).is_none());
    }

    #[test]
    fn a_device_laid_out_some_other_way_is_still_found() {
        // The fast path is an optimisation, not an assumption. A device whose
        // LED list is sparse or out of order must still render every LED it
        // has - getting this wrong would drop pixels on exactly the devices
        // nobody tests with.
        let d = dev(vec![led(9), led(4), led(0), led(7)]);
        for i in [0u16, 4, 7, 9] {
            assert_eq!(find_led(&d, i).unwrap().index, i);
        }
        assert!(find_led(&d, 1).is_none());
        assert!(find_led(&d, 5).is_none());
    }

    #[test]
    fn a_position_holding_a_different_led_falls_back() {
        // Position 1 exists but holds LED 4, so the fast path must miss and the
        // search must find LED 1 further along.
        let d = dev(vec![led(0), led(4), led(1)]);
        assert_eq!(find_led(&d, 1).unwrap().index, 1);
    }
}
