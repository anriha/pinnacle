use std::{cell::RefCell, collections::HashMap};

use smithay::{
    backend::{
        allocator::{Fourcc as DrmFourcc, Modifier as DrmModifier},
        egl::EGLDevice,
        renderer::{
            Bind, Color32F, ExportMem, Frame as RendererFrame, ImportMem, Offscreen, Renderer,
            buffer_dimensions,
            damage::OutputDamageTracker,
            element::RenderElement,
            gles::{GlesRenderbuffer, GlesRenderer, GlesTarget, GlesTexProgram},
        },
    },
    output::Output,
    reexports::wayland_server::protocol::{wl_buffer::WlBuffer, wl_shm},
    utils::{Buffer, Physical, Point, Rectangle, Size, Transform},
    wayland::{
        compositor,
        dmabuf::get_dmabuf,
        fractional_scale::with_fractional_scale,
        image_capture_source::ImageCaptureSource,
        image_copy_capture::{
            BufferConstraints, CaptureFailureReason, CursorSession, CursorSessionRef,
            DmabufConstraints, Frame, FrameRef, ImageCopyCaptureHandler, ImageCopyCaptureState,
            Session, SessionRef,
        },
        seat::WaylandFocus,
        shm::{shm_format_to_fourcc, with_buffer_contents},
    },
};
use tracing::error;

use crate::{
    handlers::image_capture_source::ImageCaptureSourceKind,
    render::{
        output_render_elements,
        pointer::pointer_render_elements,
        util::{DynElement, damage::BufferDamageElement},
    },
    state::{Pinnacle, State, WithState},
    window::WindowElement,
};

const SUPPORTED_SHM_FORMATS: &[wl_shm::Format] = &[wl_shm::Format::Argb8888];

impl ImageCopyCaptureHandler for State {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.pinnacle.image_copy_capture_state
    }

    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
        let (size, _scale) = self.pinnacle.buffer_size_and_scale_for_source(source)?;

        Some(self.buffer_constraints(size))
    }

    fn new_session(&mut self, session: Session) {
        let Some((size, scale)) = self
            .pinnacle
            .buffer_size_and_scale_for_source(&session.source())
        else {
            return;
        };

        session
            .user_data()
            .insert_if_missing(|| RefCell::new(SessionDamageTrackers::new(size, scale)));

        self.pinnacle.capture_sessions.push(session);
    }

    fn frame(&mut self, session: &SessionRef, frame: Frame) {
        let f = session.user_data().get_or_insert(|| RefCell::new(None));
        *f.borrow_mut() = Some(frame);
    }

    fn cursor_capture_constraints(
        &mut self,
        source: &ImageCaptureSource,
        _pointer: &smithay::reexports::wayland_server::protocol::wl_pointer::WlPointer,
    ) -> Option<BufferConstraints> {
        let (size, _scale) = self
            .pinnacle
            .buffer_size_and_scale_for_cursor_source(source)?;

        Some(self.buffer_constraints(size))
    }

    fn new_cursor_session(&mut self, session: CursorSession) {
        let Some((size, scale)) = self
            .pinnacle
            .buffer_size_and_scale_for_cursor_source(&session.source())
        else {
            return;
        };

        session
            .user_data()
            .insert_if_missing(|| RefCell::new(SessionDamageTrackers::new(size, scale)));

        self.pinnacle.cursor_capture_sessions.push(session);
    }

    fn cursor_frame(&mut self, session: &CursorSessionRef, frame: Frame) {
        let f = session.user_data().get_or_insert(|| RefCell::new(None));
        *f.borrow_mut() = Some(frame);
    }

    fn frame_aborted(&mut self, frame: FrameRef) {
        let _span = tracy_client::span!();

        for session in self.pinnacle.capture_sessions.iter() {
            session
                .user_data()
                .get_or_insert(|| RefCell::new(None::<Frame>))
                .borrow_mut()
                .take_if(|f| **f == frame);
        }

        for session in self.pinnacle.cursor_capture_sessions.iter() {
            session
                .user_data()
                .get_or_insert(|| RefCell::new(None::<Frame>))
                .borrow_mut()
                .take_if(|f| **f == frame);
        }
    }

    fn session_destroyed(&mut self, session: SessionRef) {
        self.pinnacle
            .capture_sessions
            .retain(|sess| *sess != session);
    }

    fn cursor_session_destroyed(&mut self, session: CursorSessionRef) {
        self.pinnacle
            .cursor_capture_sessions
            .retain(|sess| *sess != session);
    }
}

impl State {
    pub fn process_capture_sessions(&mut self) {
        let _span = tracy_client::span!();

        self.update_cursor_capture_positions();

        for session in self
            .pinnacle
            .capture_sessions
            .iter()
            .map(|session| session.as_ref())
            .collect::<Vec<_>>()
        {
            self.process_capture_session(session);
        }

        for session in self
            .pinnacle
            .cursor_capture_sessions
            .iter()
            .map(|session| session.as_ref())
            .collect::<Vec<_>>()
        {
            self.process_cursor_capture_session(session);
        }
    }

    fn process_capture_session(&mut self, session: SessionRef) {
        let _span = tracy_client::span!();

        let maybe_frame = session
            .user_data()
            .get_or_insert(|| RefCell::new(None::<Frame>));

        let Some(frame) = maybe_frame.borrow_mut().take() else {
            return;
        };

        let Some((size, scale)) = self
            .pinnacle
            .buffer_size_and_scale_for_source(&session.source())
        else {
            return;
        };

        let trackers = session
            .user_data()
            .get_or_insert(|| RefCell::new(SessionDamageTrackers::new(size, scale)));

        let mut trackers = trackers.borrow_mut();

        if (size, scale) != (trackers.size(), trackers.scale()) {
            session.update_constraints(self.buffer_constraints(size));
            frame.fail(CaptureFailureReason::BufferConstraints);
            *trackers = SessionDamageTrackers::new(size, scale);
            return;
        }

        let elements_and_hdr = match session
            .source()
            .user_data()
            .get::<ImageCaptureSourceKind>()
            .unwrap()
        {
            ImageCaptureSourceKind::Output(output) => {
                let Some(output) = output.upgrade() else {
                    frame.fail(CaptureFailureReason::Stopped);
                    return;
                };

                let elements = if session.draw_cursor() {
                    let Some(output_geo) = self.pinnacle.space.output_geometry(&output) else {
                        frame.fail(CaptureFailureReason::Stopped);
                        return;
                    };

                    let pointer_loc = self.pinnacle.seat.get_pointer().unwrap().current_location()
                        - output_geo.loc.to_f64();
                    let scale = output.current_scale().fractional_scale();

                    self.backend
                        .with_renderer(|renderer| {
                            let (pointer_elements, _) = pointer_render_elements(
                                pointer_loc,
                                scale,
                                renderer,
                                &mut self.pinnacle.cursor_state,
                                &self.pinnacle.clock,
                            );
                            let elements = output_render_elements(
                                &output,
                                renderer,
                                &self.pinnacle.space,
                                &self.pinnacle.z_index_stack,
                            );
                            pointer_elements
                                .into_iter()
                                .map(DynElement::owned)
                                .chain(elements.into_iter().map(DynElement::owned))
                                .collect::<Vec<_>>()
                        })
                        .unwrap()
                } else {
                    self.backend
                        .with_renderer(|renderer| {
                            output_render_elements(
                                &output,
                                renderer,
                                &self.pinnacle.space,
                                &self.pinnacle.z_index_stack,
                            )
                            .into_iter()
                            .map(DynElement::owned)
                            .collect::<Vec<_>>()
                        })
                        .unwrap()
                };

                (elements, self.capture_hdr_for_output(&output))
            }
            ImageCaptureSourceKind::Toplevel(foreign_toplevel) => {
                let Some(foreign_toplevel) = foreign_toplevel.upgrade() else {
                    frame.fail(CaptureFailureReason::Stopped);
                    return;
                };

                let Some(win) = self
                    .pinnacle
                    .window_for_foreign_toplevel_handle(&foreign_toplevel)
                    .cloned()
                else {
                    frame.fail(CaptureFailureReason::Stopped);
                    return;
                };

                let elements = if session.draw_cursor() {
                    self.backend
                        .with_renderer(|renderer| {
                            let win_loc = self.pinnacle.space.element_location(&win);
                            let pointer_elements = if let Some(win_loc) = win_loc {
                                let pointer_loc =
                                    self.pinnacle.seat.get_pointer().unwrap().current_location()
                                        - win_loc.to_f64()
                                        - win.total_decoration_offset().to_f64();

                                let (pointer_elements, _) = pointer_render_elements(
                                    pointer_loc,
                                    scale,
                                    renderer,
                                    &mut self.pinnacle.cursor_state,
                                    &self.pinnacle.clock,
                                );
                                pointer_elements
                            } else {
                                Vec::new()
                            };
                            let elements = win.render_elements(
                                renderer,
                                (0, 0).into(),
                                scale.into(),
                                1.0,
                                false,
                            );
                            let elements = pointer_elements
                                .into_iter()
                                .map(DynElement::owned)
                                .chain(
                                    elements
                                        .popup_elements
                                        .into_iter()
                                        .chain(elements.surface_elements)
                                        .map(DynElement::owned),
                                )
                                .collect::<Vec<_>>();
                            elements
                        })
                        .unwrap()
                } else {
                    self.backend
                        .with_renderer(|renderer| {
                            let elements = win.render_elements(
                                renderer,
                                (0, 0).into(),
                                scale.into(),
                                1.0,
                                false,
                            );

                            elements
                                .popup_elements
                                .into_iter()
                                .chain(elements.surface_elements)
                                .map(DynElement::owned)
                                .collect::<Vec<_>>()
                        })
                        .unwrap()
                };

                (elements, self.capture_hdr_for_window(&win))
            }
        };
        let (elements, hdr) = elements_and_hdr;

        if let Err(frame) = self.handle_frame(frame, &elements, &mut trackers, hdr) {
            *maybe_frame.borrow_mut() = Some(frame);
        }
    }

    fn process_cursor_capture_session(&mut self, session: CursorSessionRef) {
        let _span = tracy_client::span!();

        let maybe_frame = session
            .user_data()
            .get_or_insert(|| RefCell::new(None::<Frame>));

        let Some(frame) = maybe_frame.borrow_mut().take() else {
            return;
        };

        let Some((size, scale)) = self
            .pinnacle
            .buffer_size_and_scale_for_cursor_source(&session.source())
        else {
            return;
        };

        let trackers = session
            .user_data()
            .get_or_insert(|| RefCell::new(SessionDamageTrackers::new(size, scale)));

        let mut trackers = trackers.borrow_mut();

        if (size, scale) != (trackers.size(), trackers.scale()) {
            session.update_constraints(self.buffer_constraints(size));
            frame.fail(CaptureFailureReason::BufferConstraints);
            *trackers = SessionDamageTrackers::new(size, scale);
            return;
        }

        let cursor_offset = self
            .pinnacle
            .cursor_state
            .cursor_geometry(self.pinnacle.clock.now(), scale)
            .unwrap_or_default()
            .loc;

        session.set_cursor_hotspot(Point::default() - cursor_offset);

        let elements = self
            .backend
            .with_renderer(|renderer| {
                let (pointer_elements, _) = pointer_render_elements(
                    (0.0, 0.0).into(),
                    scale,
                    renderer,
                    &mut self.pinnacle.cursor_state,
                    &self.pinnacle.clock,
                );
                let pointer_elements =
                    crate::render::util::to_local_coord_space(pointer_elements, scale);
                pointer_elements
                    .into_iter()
                    .map(DynElement::owned)
                    .collect::<Vec<_>>()
            })
            .unwrap();

        if let Err(frame) = self.handle_frame(frame, &elements, &mut trackers, None) {
            *maybe_frame.borrow_mut() = Some(frame);
        }
    }

    /// Sends copy-capture clients updated cursor positions relative to their source.
    fn update_cursor_capture_positions(&mut self) {
        let _span = tracy_client::span!();

        let cursor_loc = self.pinnacle.seat.get_pointer().unwrap().current_location();

        for session in self.pinnacle.cursor_capture_sessions.iter() {
            // PERF: We recompute the location every time regardless if we've already done it for a
            // window or output. We should be able to cache this if needed.

            match session
                .source()
                .user_data()
                .get::<ImageCaptureSourceKind>()
                .unwrap()
            {
                ImageCaptureSourceKind::Output(output) => {
                    let Some(output) = output.upgrade() else {
                        return;
                    };

                    let cursor_loc: Point<i32, Physical> = (cursor_loc
                        - output.current_location().to_f64())
                    .to_physical_precise_round(output.current_scale().fractional_scale());
                    let cursor_loc: Point<i32, Buffer> = (cursor_loc.x, cursor_loc.y).into();

                    let mut cursor_geo = self
                        .pinnacle
                        .cursor_state
                        .cursor_geometry(
                            self.pinnacle.clock.now(),
                            output.current_scale().fractional_scale(),
                        )
                        .unwrap_or_default();

                    cursor_geo.loc += cursor_loc;

                    let mode_size = output
                        .current_mode()
                        .map(|mode| mode.size)
                        .unwrap_or_default();
                    let mode_rect: Rectangle<i32, Buffer> =
                        Rectangle::from_size((mode_size.w, mode_size.h).into());

                    let position = if cursor_geo.overlaps(mode_rect) {
                        Some(cursor_loc)
                    } else {
                        None
                    };

                    session.set_cursor_pos(position);
                }
                ImageCaptureSourceKind::Toplevel(foreign_toplevel) => {
                    let Some(foreign_toplevel) = foreign_toplevel.upgrade() else {
                        return;
                    };

                    let Some(window) = self
                        .pinnacle
                        .window_for_foreign_toplevel_handle(&foreign_toplevel)
                        .cloned()
                    else {
                        return;
                    };

                    let Some(window_loc) = self.pinnacle.space.element_location(&window) else {
                        return;
                    };

                    let Some(surface) = window.wl_surface() else {
                        return;
                    };

                    let fractional_scale = compositor::with_states(&surface, |data| {
                        with_fractional_scale(data, |scale| scale.preferred_scale())
                    })
                    .unwrap_or(1.0);

                    let cursor_loc = cursor_loc
                        - window_loc.to_f64()
                        - window.total_decoration_offset().to_f64();

                    let cursor_loc: Point<i32, Physical> =
                        cursor_loc.to_physical_precise_round(fractional_scale);
                    let cursor_loc: Point<i32, Buffer> = (cursor_loc.x, cursor_loc.y).into();

                    let mut cursor_geo = self
                        .pinnacle
                        .cursor_state
                        .cursor_geometry(self.pinnacle.clock.now(), fractional_scale)
                        .unwrap_or_default();

                    cursor_geo.loc += cursor_loc;

                    let buffer_size: Size<i32, Physical> = window
                        .geometry_without_decorations()
                        .size
                        .to_f64()
                        .to_physical_precise_round(fractional_scale);
                    let buffer_geo: Rectangle<i32, Buffer> =
                        Rectangle::from_size((buffer_size.w, buffer_size.h).into());

                    let position = if cursor_geo.overlaps(buffer_geo) {
                        Some(cursor_loc)
                    } else {
                        None
                    };

                    session.set_cursor_pos(position);
                }
            }
        }
    }

    fn buffer_constraints(&mut self, size: Size<i32, Buffer>) -> BufferConstraints {
        let shm_formats = SUPPORTED_SHM_FORMATS.to_vec();

        // TODO: maybe cache these
        let dmabuf_device = self
            .backend
            .with_renderer(|renderer| {
                EGLDevice::device_for_display(renderer.egl_context().display())
                    .ok()
                    .and_then(|device| device.try_get_render_node().ok().flatten())
            })
            .flatten();

        let dmabuf_constraints = dmabuf_device.map(|device| {
            let dmabuf_formats = self
                .backend
                .with_renderer(|renderer| renderer.egl_context().dmabuf_render_formats().clone())
                .unwrap_or_default();

            let mut formats: HashMap<DrmFourcc, Vec<DrmModifier>> = HashMap::new();

            for format in dmabuf_formats.into_iter() {
                formats
                    .entry(format.code)
                    .or_default()
                    .push(format.modifier);
            }

            DmabufConstraints {
                node: device,
                formats: formats.into_iter().collect(),
            }
        });

        BufferConstraints {
            size,
            shm: shm_formats,
            dma: dmabuf_constraints,
        }
    }

    /// Renders elements to a [`Frame`] if they caused damage, then notifies the client.
    ///
    /// If there was no damage, an `Err` containing the frame is returned.
    fn handle_frame(
        &mut self,
        frame: Frame,
        elements: &[impl RenderElement<GlesRenderer>],
        trackers: &mut SessionDamageTrackers,
        hdr: Option<CaptureHdr>,
    ) -> Result<(), Frame> {
        let _span = tracy_client::span!();

        let (damage, _) = trackers.damage.damage_output(1, elements).unwrap();
        let damage = damage.cloned().unwrap_or_default();
        if damage.is_empty() {
            return Err(frame);
        }

        let buffer = frame.buffer();
        let buffer_size = buffer_dimensions(&buffer).expect("this buffer is handled");

        let client_damage = frame
            .damage()
            .into_iter()
            .map(BufferDamageElement::new)
            .collect::<Vec<_>>();

        let rendered = self.backend.with_renderer(|renderer| {
            render_capture(
                renderer,
                &buffer,
                buffer_size,
                elements,
                &client_damage,
                trackers,
                hdr.as_ref(),
                &mut self.pinnacle.capture_hdr_program,
            )
        });

        match rendered {
            Some(Ok(())) => {
                let damage = damage
                    .iter()
                    .map(|rect| {
                        Rectangle::new(
                            (rect.loc.x, rect.loc.y).into(),
                            (rect.size.w, rect.size.h).into(),
                        )
                    })
                    .collect::<Vec<_>>();

                frame.success(Transform::Normal, damage, self.pinnacle.clock.now());
                Ok(())
            }
            Some(Err(err)) => {
                error!("Failed to capture a frame: {err}");
                frame.fail(CaptureFailureReason::Unknown);
                Ok(())
            }
            None => {
                frame.fail(CaptureFailureReason::Unknown);
                Ok(())
            }
        }
    }
}

/// What a capture of an HDR source has to undo before it can leave as sRGB.
///
/// Neither wlr-screencopy nor image-copy-capture carries a colour description, so
/// a capture client always gets sRGB. The capture therefore composes the source
/// the way the output renders it - SDR elements through the sRGB-to-PQ shader,
/// clients that already commit PQ copied unchanged - and converts the result back
/// at the output's paper white, the same inverse the screencopy path applies.
/// Content above the paper white clips.
struct CaptureHdr {
    /// Where the output maps SDR white, in nits.
    reference_white: f64,
    /// Surfaces that are already in the output's encoding, so they must not be
    /// converted a second time.
    already_encoded: Vec<smithay::backend::renderer::element::Id>,
}

impl State {
    /// The HDR encoding a capture of this output has to convert back from.
    fn capture_hdr_for_output(&self, output: &Output) -> Option<CaptureHdr> {
        let hdr = output.with_state(|state| state.hdr);
        hdr.enabled.then(|| CaptureHdr {
            reference_white: hdr.reference_white,
            already_encoded: crate::hdr::EncodedSurfaces::for_windows(
                self.pinnacle.space.elements_for_output(output),
            )
            .ids,
        })
    }

    /// The same for a capture of one window, which is encoded by the output it is
    /// shown on.
    fn capture_hdr_for_window(&self, window: &WindowElement) -> Option<CaptureHdr> {
        let hdr = window
            .output(&self.pinnacle)
            .map(|output| output.with_state(|state| state.hdr))?;
        hdr.enabled.then(|| CaptureHdr {
            reference_white: hdr.reference_white,
            already_encoded: crate::hdr::EncodedSurfaces::for_windows([window]).ids,
        })
    }
}

/// Renders one capture frame into the client's buffer.
#[allow(clippy::too_many_arguments)]
fn render_capture(
    renderer: &mut GlesRenderer,
    buffer: &WlBuffer,
    buffer_size: Size<i32, Buffer>,
    elements: &[impl RenderElement<GlesRenderer>],
    client_damage: &[BufferDamageElement],
    trackers: &mut SessionDamageTrackers,
    hdr: Option<&CaptureHdr>,
    program_cache: &mut Option<(f64, GlesTexProgram)>,
) -> anyhow::Result<()> {
    let mut dmabuf;
    let mut renderbuffer: GlesRenderbuffer;
    let mut shm_format = None;

    let mut framebuffer = if let Ok(dma) = get_dmabuf(buffer).cloned() {
        dmabuf = dma;
        renderer.bind(&mut dmabuf)?
    } else if let Ok(format) = with_buffer_contents(buffer, |_, _, data| data.format) {
        shm_format = Some(format);
        renderbuffer =
            renderer.create_buffer(shm_format_to_fourcc(format).unwrap(), buffer_size)?;
        renderer.bind(&mut renderbuffer)?
    } else {
        anyhow::bail!("captured frame that doesn't have a shm or dma buffer");
    };

    let elements = client_damage
        .iter()
        .map(DynElement::borrowed)
        .chain(elements.iter().map(DynElement::borrowed))
        .collect::<Vec<_>>();

    if let Some(hdr) = hdr {
        return render_hdr_capture(
            renderer,
            &mut framebuffer,
            buffer,
            buffer_size,
            shm_format.is_some(),
            &elements,
            &mut trackers.render,
            hdr,
            program_cache,
        );
    }

    let rendered_damage = trackers.render.render_output(
        renderer,
        &mut framebuffer,
        1,
        &elements,
        Color32F::TRANSPARENT,
    )?;

    if let Some(shm_format) = shm_format {
        let mapping = renderer.copy_framebuffer(
            &framebuffer,
            Rectangle::<i32, Buffer>::from_size(buffer_size),
            shm_format_to_fourcc(shm_format).unwrap(),
        )?;

        let bytes = renderer.map_texture(&mapping)?;

        for rect in rendered_damage.damage.into_iter().flatten() {
            blit_rect(bytes, buffer_size, *rect, buffer)?;
        }
    }

    Ok(())
}

/// Composes the capture the way the output renders, then converts it back to sRGB.
#[allow(clippy::too_many_arguments)]
fn render_hdr_capture(
    renderer: &mut GlesRenderer,
    framebuffer: &mut GlesTarget<'_>,
    buffer: &WlBuffer,
    buffer_size: Size<i32, Buffer>,
    is_shm: bool,
    elements: &[DynElement<GlesRenderer>],
    damage_tracker: &mut OutputDamageTracker,
    hdr: &CaptureHdr,
    program_cache: &mut Option<(f64, GlesTexProgram)>,
) -> anyhow::Result<()> {
    let program = capture_program(renderer, hdr.reference_white, program_cache)?;
    let reference_white = hdr.reference_white;

    let mut offscreen: GlesRenderbuffer =
        renderer.create_buffer(DrmFourcc::Abgr8888, buffer_size)?;
    let mut offscreen_fb = renderer.bind(&mut offscreen)?;

    renderer.set_default_tex_program_override(Some((program, Vec::new())));
    renderer.set_solid_color_transform(Some(Box::new(move |color| {
        crate::hdr::premultiplied_srgb_to_pq(color, reference_white)
    })));
    renderer.set_already_encoded_elements(hdr.already_encoded.clone());

    let rendered = damage_tracker.render_output(
        renderer,
        &mut offscreen_fb,
        1,
        elements,
        Color32F::TRANSPARENT,
    );

    // The hooks are renderer state, so they are cleared even when the render
    // failed: a leaked override would convert the next frame of every output.
    renderer.set_default_tex_program_override(None);
    renderer.set_solid_color_transform(None);
    renderer.set_already_encoded_elements(Vec::new());

    let rendered = rendered?;

    let mapping = renderer.copy_framebuffer(
        &offscreen_fb,
        Rectangle::<i32, Buffer>::from_size(buffer_size),
        DrmFourcc::Argb8888,
    )?;
    let mut bytes = renderer.map_texture(&mapping)?.to_vec();
    crate::hdr::pq_bgra_to_srgb_in_place(&mut bytes, reference_white);

    if is_shm {
        for rect in rendered.damage.into_iter().flatten() {
            blit_rect(&bytes, buffer_size, *rect, buffer)?;
        }
        return Ok(());
    }

    // A dmabuf capture has no CPU mapping to write, and the capture path has no
    // renderer that can sample its own offscreen as a `DynElement`, so the
    // converted pixels go back to the GPU as a texture and are drawn 1:1.
    let texture = renderer.import_memory(&bytes, DrmFourcc::Argb8888, buffer_size, false)?;
    let size: Size<i32, Physical> = (buffer_size.w, buffer_size.h).into();
    let full = Rectangle::<i32, Physical>::from_size(size);

    let mut gl_frame = renderer.render(framebuffer, size, Transform::Normal)?;
    gl_frame.clear(Color32F::TRANSPARENT, &[full])?;
    gl_frame.render_texture_from_to(
        &texture,
        Rectangle::<f64, Buffer>::from_size(buffer_size.to_f64()),
        full,
        &[full],
        &[],
        Transform::Normal,
        1.0,
        None,
        &[],
    )?;
    let _sync = gl_frame.finish()?;

    Ok(())
}

fn blit_rect(
    bytes: &[u8],
    buffer_size: Size<i32, Buffer>,
    rect: Rectangle<i32, Physical>,
    buffer: &WlBuffer,
) -> anyhow::Result<()> {
    crate::render::util::blit(
        bytes,
        buffer_size,
        Rectangle::new(
            (rect.loc.x, rect.loc.y).into(),
            (rect.size.w, rect.size.h).into(),
        ),
        buffer,
    )
}

/// The sRGB-to-PQ program for this paper white, compiled once per value.
fn capture_program(
    renderer: &mut GlesRenderer,
    reference_white: f64,
    cache: &mut Option<(f64, GlesTexProgram)>,
) -> anyhow::Result<GlesTexProgram> {
    if let Some((compiled_for, program)) = cache.as_ref()
        && *compiled_for == reference_white
    {
        return Ok(program.clone());
    }

    let program =
        renderer.compile_custom_texture_shader(crate::hdr::texture_shader(reference_white), &[])?;
    *cache = Some((reference_white, program.clone()));
    Ok(program)
}

impl Pinnacle {
    pub fn stop_capture_sessions_for_output(&mut self, output: &Output) {
        self.capture_sessions.retain(|session| {
            let session_is_for_this_output = matches!(
                session
                    .source()
                    .user_data()
                    .get::<ImageCaptureSourceKind>()
                    .unwrap(),
                ImageCaptureSourceKind::Output(weak) if weak == output
            );
            !session_is_for_this_output
        });

        self.cursor_capture_sessions.retain(|session| {
            let session_is_for_this_output = matches!(
                session
                    .source()
                    .user_data()
                    .get::<ImageCaptureSourceKind>()
                    .unwrap(),
                ImageCaptureSourceKind::Output(weak) if weak == output
            );
            !session_is_for_this_output
        });
    }

    pub fn stop_capture_sessions_for_window(&mut self, window: &WindowElement) {
        self.capture_sessions.retain(|session| {
            let session_is_for_this_window = matches!(
                session
                    .source()
                    .user_data()
                    .get::<ImageCaptureSourceKind>()
                    .unwrap(),
                ImageCaptureSourceKind::Toplevel(weak)
                    if window.with_state(|state| {
                        // TODO: figure out if this upgrade is correct
                        weak.upgrade().map(|handle| handle.identifier())
                            == state
                                .foreign_toplevel_list_handle
                                .as_ref()
                                .map(|handle| handle.identifier())
                    })
            );
            !session_is_for_this_window
        });

        self.cursor_capture_sessions.retain(|session| {
            let session_is_for_this_window = matches!(
                session
                    .source()
                    .user_data()
                    .get::<ImageCaptureSourceKind>()
                    .unwrap(),
                ImageCaptureSourceKind::Toplevel(weak)
                    if window.with_state(|state| {
                        weak.upgrade().map(|handle| handle.identifier())
                            == state
                                .foreign_toplevel_list_handle
                                .as_ref()
                                .map(|handle| handle.identifier())
                    })
            );
            !session_is_for_this_window
        });
    }

    /// Returns the target buffer size and scale for an [`ImageCaptureSource`].
    ///
    /// Returns `None` if the source doesn't exist.
    fn buffer_size_and_scale_for_source(
        &mut self,
        source: &ImageCaptureSource,
    ) -> Option<(Size<i32, Buffer>, f64)> {
        let kind = source
            .user_data()
            .get::<ImageCaptureSourceKind>()
            .expect("source should have source here");

        match kind {
            ImageCaptureSourceKind::Output(output) => {
                let output = output.upgrade()?;
                let scale = output.current_scale().fractional_scale();

                let size = output.current_mode()?.size;
                Some(((size.w, size.h).into(), scale))
            }
            ImageCaptureSourceKind::Toplevel(foreign_toplevel) => {
                let foreign_toplevel = foreign_toplevel.upgrade()?;
                let window = self.window_for_foreign_toplevel_handle(&foreign_toplevel)?;

                let surface = window.wl_surface()?;

                let fractional_scale = compositor::with_states(&surface, |data| {
                    with_fractional_scale(data, |scale| scale.preferred_scale())
                })?;

                let size = window
                    .geometry_without_decorations()
                    .size
                    .to_f64()
                    .to_buffer(fractional_scale, Transform::Normal)
                    .to_i32_round();

                Some((size, fractional_scale))
            }
        }
    }

    fn buffer_size_and_scale_for_cursor_source(
        &mut self,
        source: &ImageCaptureSource,
    ) -> Option<(Size<i32, Buffer>, f64)> {
        let kind = source
            .user_data()
            .get::<ImageCaptureSourceKind>()
            .expect("source should have source here");

        let scale = match kind {
            ImageCaptureSourceKind::Output(output) => {
                let output = output.upgrade()?;
                output.current_scale().fractional_scale()
            }
            ImageCaptureSourceKind::Toplevel(foreign_toplevel) => {
                let foreign_toplevel = foreign_toplevel.upgrade()?;
                let window = self.window_for_foreign_toplevel_handle(&foreign_toplevel)?;

                let surface = window.wl_surface()?;

                let fractional_scale = compositor::with_states(&surface, |data| {
                    with_fractional_scale(data, |scale| scale.preferred_scale())
                })?;

                fractional_scale
            }
        };

        let geo = self
            .cursor_state
            .cursor_geometry(self.clock.now(), scale)
            .unwrap_or(Rectangle::from_size((1, 1).into()));
        Some((geo.size, scale))
    }
}

/// Damage trackers for copy-capture sessions.
#[derive(Debug)]
pub struct SessionDamageTrackers {
    /// The "something has changed on-screen" damage tracker.
    ///
    /// This tracks actual screen damage to see if a new frame
    /// should be rendered.
    damage: OutputDamageTracker,
    /// The rendering damage tracker.
    ///
    /// This is used to render to session buffers, and the returned damage
    /// is used to optimize blitting into said buffers.
    render: OutputDamageTracker,
}

impl SessionDamageTrackers {
    /// Creates a new set of damage trackers for handling copy-capture sessions.
    fn new(size: Size<i32, Buffer>, scale: f64) -> Self {
        Self {
            damage: OutputDamageTracker::new((size.w, size.h), scale, Transform::Normal),
            render: OutputDamageTracker::new((size.w, size.h), scale, Transform::Normal),
        }
    }

    /// Returns the current buffer size of these trackers.
    fn size(&self) -> Size<i32, Buffer> {
        let (size, _, _) = self.render.mode().try_into().unwrap();
        (size.w, size.h).into()
    }

    /// Returns the current scale of these trackers.
    fn scale(&self) -> f64 {
        let (_, scale, _) = self.render.mode().try_into().unwrap();
        scale.x
    }
}
