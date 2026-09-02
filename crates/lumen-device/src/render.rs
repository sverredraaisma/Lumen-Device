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

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use lumen_proto::Uuid;
use lumen_vm::program::Program;
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

/// Renders one device's LEDs.
///
/// Holds a machine per source, because the VM's register file survives from
/// `frame` into every pixel of that frame — which is the whole reason hoisting
/// pays, and sharing one machine between two sources would destroy it.
#[derive(Default)]
pub struct Renderer {
    machines: BTreeMap<Uuid, Machine>,
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
        let mut report = FrameReport {
            rendered: Vec::new(),
            faults: Vec::new(),
            spent: 0,
        };

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
            if self.render_source(now_us, t, leds, b, uniforms, out, &mut report, Q16::ONE) {
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
            self.render_source(now_us, t, leds, b, uniforms, out, &mut report, remaining);
        }

        report
    }

    /// Render one source over `out`, weighted by `alpha`.
    #[allow(clippy::too_many_arguments)]
    fn render_source<U: Uniforms>(
        &mut self,
        _now_us: u64,
        t: Q16,
        leds: &DeviceLeds,
        b: &Bound<'_>,
        uniforms: &mut U,
        out: &mut [Rgb],
        report: &mut FrameReport,
        alpha: Q16,
    ) -> bool {
        if b.membership.is_empty() {
            return false;
        }
        let machine = self.machines.entry(b.source.id).or_default();
        machine.set_budget(b.program.budget.max(1));

        if let Err(fault) = machine.run_frame_at(b.program, t, uniforms) {
            report.faults.push(RenderFault::Program {
                source: b.source.id,
                fault,
            });
            // The frame section failed, so every pixel would fail the same way.
            // Bailing here rather than per pixel keeps one bug from producing
            // three hundred identical reports.
            return false;
        }

        let count = b.membership.bounds.count;
        let mut contributed = false;
        for (ordinal, index) in b.membership.leds.iter().enumerate() {
            let Some(led) = leds.leds.iter().find(|l| l.index == *index) else {
                continue;
            };
            let Some(slot) = out.get_mut(*index as usize) else {
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

            match machine.run_pixel(b.program, &inputs, uniforms) {
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
        report.spent = report.spent.saturating_add(machine.spent());
        contributed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use lumen_vm::isa::{Instruction, OpCode};
    use lumen_vm::program::builder::ProgramBuilder;
    use lumen_vm::program::Section;
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
}
