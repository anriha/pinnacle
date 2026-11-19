// SPDX-License-Identifier: GPL-3.0-or-later

mod drm;
mod frame;
mod gamma;

use assert_matches::assert_matches;
pub use drm::drm_mode_from_modeinfo;
use frame::FrameClock;
use indexmap::IndexSet;
use wayland_backend::server::GlobalId;

use std::{collections::HashMap, mem, path::Path, time::Duration};

use anyhow::{Context, anyhow, bail, ensure};
use drm::{create_drm_mode, refresh_interval};
use smithay::{
    backend::{
        drm::color::{Colorspace, ConnectorColorState, HdrOutputMetadata},
        renderer::gles::GlesTexProgram,
        SwapBuffersError,
        allocator::{
            Buffer, Fourcc,
            gbm::{GbmAllocator, GbmBuffer, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmNode, DrmSurface, NodeType,
            compositor::{FrameFlags, PrimaryPlaneElement, RenderFrameResult},
            exporter::gbm::GbmFramebufferExporter,
            gbm::GbmFramebuffer,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{EGLDevice, EGLDisplay, context::ContextPriority},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            self, Bind, Blit, BufferType, ExportMem, Frame, ImportDma, ImportMem, ImportMemWl,
            Offscreen, Renderer, TextureFilter,
            damage::OutputDamageTracker,
            element::{self, Element, Id, surface::render_elements_from_surface_tree},
            gles::{GlesRenderbuffer, GlesRenderer},
            multigpu::{GpuManager, MultiRenderer, gbm::GbmGlesBackend},
            sync::SyncPoint,
            utils::{CommitCounter, DamageSet},
        },
        session::{self, Session, libseat::LibSeatSession},
        udev::{self, UdevBackend, UdevEvent},
    },
    desktop::utils::{OutputPresentationFeedback, surface_primary_scanout_output},
    output::{Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            self, Dispatcher, Interest, LoopHandle, PostAction, RegistrationToken,
            generic::Generic,
            timer::{TimeoutAction, Timer},
        },
        drm::control::{ModeTypeFlags, connector, crtc},
        input::Libinput,
        rustix::fs::OFlags,
        wayland_protocols::wp::{
            content_type::v1::server::wp_content_type_v1::Type as ContentType,
            linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1,
            presentation_time::server::wp_presentation_feedback,
        },
        wayland_server::{
            DisplayHandle,
            protocol::{wl_shm, wl_surface::WlSurface},
        },
    },
    utils::{DeviceFd, Rectangle, Transform},
    wayland::{
        compositor,
        content_type::ContentTypeSurfaceCachedState,
        dmabuf::{self, DmabufFeedback, DmabufFeedbackBuilder, DmabufGlobal},
        presentation::Refresh,
        shm::shm_format_to_fourcc,
    },
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use tracing::{debug, error, info, trace, warn};

use crate::{
    render::AsGlesRenderer,
    api::signal::Signal,
    backend::Backend,
    config::ConnectorSavedState,
    input::libinput::DeviceState,
    output::{BlankingState, OutputMode, OutputName},
    render::{
        CLEAR_COLOR, CLEAR_COLOR_LOCKED, OutputRenderElement, pointer::pointer_render_elements,
        take_presentation_feedback,
    },
    state::{FrameCallbackSequence, Pinnacle, State, WithState},
};

use super::{BackendData, UninitBackend};

const SUPPORTED_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];
const SUPPORTED_FORMATS_8BIT_ONLY: &[Fourcc] = &[Fourcc::Abgr8888, Fourcc::Argb8888];

/// A [`MultiRenderer`] that uses the [`GbmGlesBackend`].
pub type UdevRenderer<'a> = MultiRenderer<
    'a,
    'a,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
>;

type UdevRenderFrameResult<'a> =
    RenderFrameResult<'a, GbmBuffer, GbmFramebuffer, OutputRenderElement<UdevRenderer<'a>>>;

/// Udev state attached to each [`Output`].
#[derive(Debug, PartialEq)]
struct UdevOutputData {
    /// The GPU node
    device_id: DrmNode,
    /// The [Crtc][crtc::Handle] the output is pushing to
    crtc: crtc::Handle,
}

// TODO: document desperately
pub struct Udev {
    pub session: LibSeatSession,
    udev_dispatcher: Dispatcher<'static, UdevBackend, State>,
    display_handle: DisplayHandle,
    pub primary_gpu: DrmNode,
    pub(super) gpu_manager: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    devices: HashMap<DrmNode, Device>,
    /// The global corresponding to the primary gpu
    dmabuf_global: Option<DmabufGlobal>,
    drm_global: Option<GlobalId>,

    pub(super) upscale_filter: TextureFilter,
    pub(super) downscale_filter: TextureFilter,
}

impl Backend {
    #[allow(dead_code)]
    fn udev(&self) -> &Udev {
        let Backend::Udev(udev) = self else { unreachable!() };
        udev
    }

    fn udev_mut(&mut self) -> &mut Udev {
        let Backend::Udev(udev) = self else { unreachable!() };
        udev
    }
}

impl Udev {
    pub(crate) fn try_new(display_handle: DisplayHandle) -> anyhow::Result<UninitBackend<Udev>> {
        // Initialize session
        let (session, notifier) = LibSeatSession::new()?;

        // Get the primary gpu
        let primary_gpu = std::env::var("PINNACLE_DRM_DEVICES")
            .ok()
            .and_then(|var| var.split(":").next().map(String::from))
            .and_then(|device| {
                info!("Attempting to set primary device as {device}");

                match DrmNode::from_path(device) {
                    Ok(drm_node) => drm_node
                        .node_with_type(NodeType::Render)
                        .and_then(|node| node.ok()),
                    Err(err) => {
                        warn!("Unable to use specified device {err}, falling back");
                        None
                    }
                }
            });

        let primary_gpu = match primary_gpu {
            Some(primary) => primary,
            None => udev::primary_gpu(session.seat())
                .context("unable to get primary gpu path")?
                .and_then(|x| {
                    DrmNode::from_path(x)
                        .ok()?
                        .node_with_type(NodeType::Render)?
                        .ok()
                })
                .unwrap_or_else(|| {
                    udev::all_gpus(session.seat())
                        .expect("failed to get gpu paths")
                        .into_iter()
                        .find_map(|x| DrmNode::from_path(x).ok())
                        .expect("No GPU!")
                }),
        };

        let gpu_manager =
            GpuManager::new(GbmGlesBackend::with_context_priority(ContextPriority::High))?;

        // Initialize the udev backend
        let udev_backend = UdevBackend::new(session.seat())?;

        let udev_dispatcher = Dispatcher::new(udev_backend, move |event, _, state: &mut State| {
            let udev = state.backend.udev_mut();
            let pinnacle = &mut state.pinnacle;

            if !udev.session.is_active() {
                debug!(?event, "Skipping udev event as session is inactive");
                return;
            }

            match event {
                // GPU connected
                UdevEvent::Added { device_id, path } => {
                    if let Err(err) = DrmNode::from_dev_id(device_id)
                        .context("failed to access drm node")
                        .and_then(|node| udev.device_added(pinnacle, node, &path))
                    {
                        error!("Skipping device {device_id}: {err}");
                    }
                }
                UdevEvent::Changed { device_id } => {
                    if let Ok(node) = DrmNode::from_dev_id(device_id) {
                        udev.device_changed(pinnacle, node)
                    }
                }
                // GPU disconnected
                UdevEvent::Removed { device_id } => {
                    if let Ok(node) = DrmNode::from_dev_id(device_id) {
                        udev.device_removed(pinnacle, node)
                    }
                }
            }
        });

        let mut udev = Udev {
            display_handle,
            udev_dispatcher,
            session,
            primary_gpu,
            gpu_manager,
            devices: HashMap::new(),
            dmabuf_global: None,
            drm_global: None,

            upscale_filter: TextureFilter::Linear,
            downscale_filter: TextureFilter::Linear,
        };

        Ok(UninitBackend {
            seat_name: udev.seat_name(),
            init: Box::new(move |pinnacle| {
                pinnacle
                    .loop_handle
                    .register_dispatcher(udev.udev_dispatcher.clone())?;

                let things = udev
                    .udev_dispatcher
                    .as_source_ref()
                    .device_list()
                    .map(|(id, path)| (id, path.to_path_buf()))
                    .collect::<Vec<_>>();

                // Create DrmNodes from already connected GPUs
                for (device_id, path) in things {
                    if let Err(err) = DrmNode::from_dev_id(device_id)
                        .context("failed to access drm node")
                        .and_then(|node| udev.device_added(pinnacle, node, &path))
                    {
                        error!("Skipping device {device_id}: {err}");
                    }
                }

                // Initialize libinput backend
                let mut libinput_context = Libinput::new_with_udev::<
                    LibinputSessionInterface<LibSeatSession>,
                >(udev.session.clone().into());
                libinput_context
                    .udev_assign_seat(pinnacle.seat.name())
                    .expect("failed to assign seat to libinput");
                let libinput_backend = LibinputInputBackend::new(libinput_context.clone());

                // Bind all our objects that get driven by the event loop

                let insert_ret =
                    pinnacle
                        .loop_handle
                        .insert_source(libinput_backend, move |event, _, state| {
                            match &event {
                                smithay::backend::input::InputEvent::DeviceAdded { device } => {
                                    state
                                        .pinnacle
                                        .input_state
                                        .libinput_state
                                        .devices
                                        .insert(device.clone(), DeviceState::default());
                                    state
                                        .pinnacle
                                        .signal_state
                                        .input_device_added
                                        .signal(device);
                                }
                                smithay::backend::input::InputEvent::DeviceRemoved { device } => {
                                    state
                                        .pinnacle
                                        .input_state
                                        .libinput_state
                                        .devices
                                        .shift_remove(device);
                                }
                                _ => (),
                            }
                            state.process_input_event(event);
                        });

                if let Err(err) = insert_ret {
                    anyhow::bail!("Failed to insert libinput_backend into event loop: {err}");
                }

                pinnacle
                    .loop_handle
                    .insert_source(notifier, move |event, _, state| {
                        match event {
                            session::Event::PauseSession => {
                                let udev = state.backend.udev_mut();
                                libinput_context.suspend();
                                info!("pausing session");

                                for device in udev.devices.values_mut() {
                                    device.drm_output_manager.pause();
                                }
                            }
                            session::Event::ActivateSession => {
                                info!("resuming session");

                                if libinput_context.resume().is_err() {
                                    error!("Failed to resume libinput context");
                                }

                                let udev = state.backend.udev_mut();
                                let pinnacle = &mut state.pinnacle;

                                let (mut device_list, connected_devices, disconnected_devices) = {
                                    let device_list = udev
                                        .udev_dispatcher
                                        .as_source_ref()
                                        .device_list()
                                        .flat_map(|(id, path)| {
                                            if should_use_drm_device(path) {
                                                Some((
                                                    DrmNode::from_dev_id(id).ok()?,
                                                    path.to_path_buf(),
                                                ))
                                            } else {
                                                None
                                            }
                                        })
                                        .collect::<HashMap<_, _>>();

                                    let (connected_devices, disconnected_devices) =
                                        udev.devices.keys().copied().partition::<Vec<_>, _>(
                                            |node| device_list.contains_key(node),
                                        );

                                    (device_list, connected_devices, disconnected_devices)
                                };

                                for node in disconnected_devices {
                                    device_list.remove(&node);
                                    udev.device_removed(pinnacle, node);
                                }

                                for node in connected_devices {
                                    device_list.remove(&node);

                                    // INFO: see if this can be moved below udev.device_changed
                                    {
                                        let Some(device) = udev.devices.get_mut(&node) else {
                                            unreachable!();
                                        };

                                        if let Err(err) =
                                            device.drm_output_manager.lock().activate(true)
                                        {
                                            error!("Error activating DRM device: {err}");
                                        }
                                    }

                                    udev.device_changed(pinnacle, node);

                                    let Some(device) = udev.devices.get_mut(&node) else {
                                        unreachable!();
                                    };

                                    // Apply pending gammas
                                    //
                                    // Also welcome to some really doodoo code

                                    for (crtc, surface) in device.surfaces.iter_mut() {
                                        match mem::take(&mut surface.pending_gamma_change) {
                                            PendingGammaChange::Idle => {
                                                debug!("Restoring from previous gamma");
                                                if let Err(err) = Udev::set_gamma_internal(
                                                    device.drm_output_manager.device(),
                                                    crtc,
                                                    surface.previous_gamma.clone(),
                                                ) {
                                                    warn!("Failed to reset gamma: {err}");
                                                    surface.previous_gamma = None;
                                                }
                                            }
                                            PendingGammaChange::Restore => {
                                                debug!("Restoring to original gamma");
                                                if let Err(err) = Udev::set_gamma_internal(
                                                    device.drm_output_manager.device(),
                                                    crtc,
                                                    None::<[&[u16]; 3]>,
                                                ) {
                                                    warn!("Failed to reset gamma: {err}");
                                                }
                                                surface.previous_gamma = None;
                                            }
                                            PendingGammaChange::Change(gamma) => {
                                                debug!("Changing to pending gamma");
                                                match Udev::set_gamma_internal(
                                                    device.drm_output_manager.device(),
                                                    crtc,
                                                    Some([&gamma[0], &gamma[1], &gamma[2]]),
                                                ) {
                                                    Ok(()) => {
                                                        surface.previous_gamma = Some(gamma);
                                                    }
                                                    Err(err) => {
                                                        warn!("Failed to set pending gamma: {err}");
                                                        surface.previous_gamma = None;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }

                                // Newly connected devices
                                for (node, path) in device_list.into_iter() {
                                    if let Err(err) = state.backend.udev_mut().device_added(
                                        &mut state.pinnacle,
                                        node,
                                        &path,
                                    ) {
                                        error!("Error adding device: {err}");
                                    }
                                }

                                for output in
                                    state.pinnacle.space.outputs().cloned().collect::<Vec<_>>()
                                {
                                    state.schedule_render(&output);
                                }
                            }
                        }
                    })
                    .expect("failed to insert libinput notifier into event loop");

                pinnacle.shm_state.update_formats(
                    udev.gpu_manager
                        .single_renderer(&primary_gpu)?
                        .shm_formats(),
                );

                Ok(udev)
            }),
        })
    }

    /// Schedule a new render that will cause the compositor to redraw everything.
    pub fn schedule_render(&mut self, output: &Output) {
        let _span = tracy_client::span!("Udev::schedule_render");

        let Some(surface) = render_surface_for_output(output, &mut self.devices) else {
            debug!("no render surface on output {}", output.name());
            return;
        };

        let old_state = mem::take(&mut surface.render_state);

        surface.render_state = match old_state {
            RenderState::Idle => RenderState::Scheduled,

            value @ (RenderState::Scheduled
            | RenderState::WaitingForEstimatedVblankAndScheduled(_)) => value,

            RenderState::WaitingForVblank { .. } => RenderState::WaitingForVblank {
                render_needed: true,
            },

            RenderState::WaitingForEstimatedVblank(token) => {
                RenderState::WaitingForEstimatedVblankAndScheduled(token)
            }
        };
    }

    pub(super) fn set_output_powered(
        &mut self,
        output: &Output,
        loop_handle: &LoopHandle<'static, State>,
        powered: bool,
    ) {
        let _span = tracy_client::span!("Udev::set_output_powered");

        let UdevOutputData { device_id, crtc } =
            output.user_data().get::<UdevOutputData>().unwrap();

        let Some(device) = self.devices.get_mut(device_id) else {
            return;
        };

        if powered {
            output.with_state_mut(|state| state.powered = true);
        } else {
            output.with_state_mut(|state| state.powered = false);

            if let Err(err) = device
                .surfaces
                .get_mut(crtc)
                .unwrap()
                .drm_output
                .with_compositor(|compositor| compositor.clear())
            {
                warn!("Failed to clear compositor state on crtc {crtc:?}: {err}");
            }

            if let Some(surface) = render_surface_for_output(output, &mut self.devices)
                && let RenderState::WaitingForEstimatedVblankAndScheduled(token)
                | RenderState::WaitingForEstimatedVblank(token) =
                    mem::take(&mut surface.render_state)
            {
                loop_handle.remove(token);
            }
        }
    }
}

impl State {
    /// Switch the tty.
    ///
    /// Does nothing when called on the winit backend.
    pub fn switch_vt(&mut self, vt: i32) {
        if let Backend::Udev(udev) = &mut self.backend {
            info!("Switching to vt {vt}");
            if let Err(err) = udev.session.change_vt(vt) {
                error!("Failed to switch to vt {vt}: {err}");
            }
        }
    }
}

impl BackendData for Udev {
    fn seat_name(&self) -> String {
        self.session.seat()
    }

    fn reset_buffers(&mut self, output: &Output) {
        let _span = tracy_client::span!("Udev: BackendData::reset_buffers");

        if let Some(id) = output.user_data().get::<UdevOutputData>()
            && let Some(gpu) = self.devices.get_mut(&id.device_id)
            && let Some(surface) = gpu.surfaces.get_mut(&id.crtc)
        {
            surface.drm_output.reset_buffers();
        }
    }

    fn early_import(&mut self, surface: &WlSurface) {
        let _span = tracy_client::span!("Udev: BackendData::early_import");

        if let Err(err) = self.gpu_manager.early_import(self.primary_gpu, surface) {
            warn!("early buffer import failed: {}", err);
        }
    }

    fn set_output_mode(&mut self, output: &Output, mode: OutputMode) {
        let _span = tracy_client::span!("Udev: BackendData::set_output_mode");

        let drm_mode = self
            .devices
            .iter()
            .find_map(|(_, device)| {
                device
                    .drm_scanner
                    .crtcs()
                    .find(|(_, handle)| {
                        output
                            .user_data()
                            .get::<UdevOutputData>()
                            .is_some_and(|data| &data.crtc == handle)
                    })
                    .and_then(|(info, _)| {
                        info.modes()
                            .iter()
                            .find(|m| smithay::output::Mode::from(**m) == mode.into())
                    })
                    .copied()
            })
            .unwrap_or_else(|| {
                info!("Unknown mode for {}, creating new one", output.name());
                match mode {
                    OutputMode::Smithay(mode) => {
                        create_drm_mode(mode.size.w, mode.size.h, Some(mode.refresh as u32))
                    }
                    OutputMode::Drm(mode) => mode,
                }
            });

        if let Some(render_surface) = render_surface_for_output(output, &mut self.devices)
            && let Ok(mut renderer) = self.gpu_manager.single_renderer(&self.primary_gpu)
        {
            match render_surface.drm_output.use_mode(
                drm_mode,
                &mut renderer,
                &DrmOutputRenderElements::<_, OutputRenderElement<_>>::default(),
            ) {
                Ok(()) => {
                    let mode = smithay::output::Mode::from(mode);
                    info!(
                        "Set {}'s mode to {}x{}@{:.3}Hz",
                        output.name(),
                        mode.size.w,
                        mode.size.h,
                        mode.refresh as f64 / 1000.0
                    );
                    output.change_current_state(Some(mode), None, None, None);
                    output.with_state_mut(|state| {
                        // TODO: push or no?
                        if !state.modes.contains(&mode) {
                            state.modes.push(mode);
                        }
                    });
                }
                Err(err) => warn!("Failed to set output mode for {}: {err}", output.name()),
            }
        }
    }
}

// TODO: document desperately
struct Device {
    surfaces: HashMap<crtc::Handle, RenderSurface>,
    drm_output_manager: DrmOutputManager<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        Option<OutputPresentationFeedback>,
        DrmDeviceFd,
    >,
    drm_scanner: DrmScanner,
    render_node: DrmNode,
    registration_token: RegistrationToken,
}

fn get_surface_dmabuf_feedback(
    primary_gpu: DrmNode,
    render_node: DrmNode,
    gpu_manager: &mut GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    surface: &DrmSurface,
) -> Option<SurfaceDmabufFeedback> {
    let _span = tracy_client::span!("get_surface_dmabuf_feedback");

    let primary_formats = gpu_manager
        .single_renderer(&primary_gpu)
        .ok()?
        .dmabuf_formats();

    let render_formats = gpu_manager
        .single_renderer(&render_node)
        .ok()?
        .dmabuf_formats();

    let all_render_formats = primary_formats
        .iter()
        .chain(render_formats.iter())
        .copied()
        .collect::<IndexSet<_>>();

    let planes = surface.planes().clone();

    // We limit the scan-out trache to formats we can also render from
    // so that there is always a fallback render path available in case
    // the supplied buffer can not be scanned out directly
    let planes_formats = planes
        .primary
        .into_iter()
        .flat_map(|p| p.formats)
        .chain(planes.overlay.into_iter().flat_map(|p| p.formats))
        .collect::<IndexSet<_>>()
        .intersection(&all_render_formats)
        .copied()
        .collect::<Vec<_>>();

    let builder = DmabufFeedbackBuilder::new(primary_gpu.dev_id(), primary_formats);
    let render_feedback = builder
        .clone()
        .add_preference_tranche(
            render_node.dev_id(),
            zwp_linux_dmabuf_feedback_v1::TrancheFlags::empty(),
            render_formats.clone(),
            1..=4,
        )
        .build()
        .ok()?; // INFO: this is an unwrap in Anvil, does it matter?

    let scanout_feedback = builder
        .add_preference_tranche(
            surface.device_fd().dev_id().ok()?, // INFO: this is an unwrap in Anvil, does it matter?
            zwp_linux_dmabuf_feedback_v1::TrancheFlags::Scanout,
            planes_formats,
            1..=4,
        )
        .add_preference_tranche(
            render_node.dev_id(),
            zwp_linux_dmabuf_feedback_v1::TrancheFlags::empty(),
            render_formats,
            1..=4,
        )
        .build()
        .ok()?; // INFO: this is an unwrap in Anvil, does it matter?

    Some(SurfaceDmabufFeedback {
        render_feedback,
        scanout_feedback,
    })
}

fn get_surface_content_type(surface: &WlSurface) -> ContentType {
    compositor::with_states(surface, |states| {
        let mut guard = states.cached_state.get::<ContentTypeSurfaceCachedState>();
        let current = guard.current();
        *current.content_type()
    })
}

#[derive(Debug)]
pub struct SurfaceDmabufFeedback {
    pub render_feedback: DmabufFeedback,
    pub scanout_feedback: DmabufFeedback,
}

/// The state of a [`RenderSurface`].
#[derive(Debug, Default)]
enum RenderState {
    /// No render is scheduled.
    #[default]
    Idle,
    /// A render is scheduled to happen at the end of the current event loop cycle.
    Scheduled,
    /// A frame was rendered and we are waiting for vblank.
    WaitingForVblank {
        /// A render was scheduled while waiting for vblank.
        /// In this case, another render will be scheduled once vblank happens.
        render_needed: bool,
    },
    /// A frame caused no damage, but we'll still wait as if it did to prevent busy loops.
    WaitingForEstimatedVblank(RegistrationToken),
    /// A render was scheduled during a wait for estimated vblank.
    WaitingForEstimatedVblankAndScheduled(RegistrationToken),
}

/// Render surface for an output.
struct RenderSurface {
    /// The node from `connector_connected`.
    device_id: DrmNode,
    /// The node rendering to the screen? idk
    ///
    /// If this is equal to the primary gpu node then it does the rendering operations.
    /// If it's not it is the node the composited buffer ends up on.
    render_node: DrmNode,
    drm_output: DrmOutput<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        Option<OutputPresentationFeedback>,
        DrmDeviceFd,
    >,
    dmabuf_feedback: Option<SurfaceDmabufFeedback>,
    render_state: RenderState,
    screencopy_commit_state: ScreencopyCommitState,

    /// Whether this output is driven in BT.2020/PQ HDR mode.
    hdr: crate::hdr::HdrConfig,
    /// Luminance 1.0 sRGB maps to on this output, in nits.
    /// Lazily compiled shader that converts sRGB content into the HDR signal.
    hdr_program: Option<GlesTexProgram>,
    /// One-shot / rate-limited pre-scanout frame dumps (`PINNACLE_HDR_DUMP`).
    hdr_dump: Option<crate::hdr::FrameDump>,
    /// Whether to log what the primary plane did (`PINNACLE_HDR_LOG`).
    hdr_log: bool,
    /// Last scanout decision logged, so the log shows transitions only.
    hdr_scanout: Option<String>,
    /// Last value of the passthrough gate, so clients are told on transitions only.
    hdr_gate: Option<bool>,
    /// Last set of surfaces marked as already encoded, for the diagnostics.
    hdr_encoded_signature: Option<String>,
    previous_gamma: Option<[Box<[u16]>; 3]>,
    pending_gamma_change: PendingGammaChange,

    frame_clock: FrameClock,
    frame_callback_sequence: FrameCallbackSequence,
}

#[derive(Debug, Clone, Default)]
enum PendingGammaChange {
    /// No pending gamma
    #[default]
    Idle,
    /// Restore the original gamma
    Restore,
    /// Change the gamma
    Change([Box<[u16]>; 3]),
}

#[derive(Default, Debug, Clone, Copy)]
struct ScreencopyCommitState {
    primary_plane_swapchain: CommitCounter,
    primary_plane_element: CommitCounter,
    cursor: CommitCounter,
}

impl Udev {
    pub fn renderer(&mut self) -> anyhow::Result<UdevRenderer<'_>> {
        Ok(self.gpu_manager.single_renderer(&self.primary_gpu)?)
    }

    /// A GPU was plugged in.
    fn device_added(
        &mut self,
        pinnacle: &mut Pinnacle,
        node: DrmNode,
        path: &Path,
    ) -> anyhow::Result<()> {
        debug!(?node, ?path, "Udev::device_added");

        if !should_use_drm_device(path) {
            debug!("Skipping adding device {}", path.display());
            return Ok(());
        }

        // Try to open the device
        let fd = self
            .session
            .open(
                path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .context("failed to open device with libseat")?;

        let fd = DrmDeviceFd::new(DeviceFd::from(fd));

        let (drm, notifier) =
            DrmDevice::new(fd.clone(), true).context("failed to init drm device")?;
        let gbm = GbmDevice::new(fd).context("failed to init gbm device")?;

        let registration_token = pinnacle
            .loop_handle
            .insert_source(notifier, move |event, metadata, state| {
                let metadata = metadata.expect("vblank events must have metadata");
                match event {
                    DrmEvent::VBlank(crtc) => {
                        state.backend.udev_mut().on_vblank(
                            &mut state.pinnacle,
                            node,
                            crtc,
                            metadata,
                        );
                    }
                    DrmEvent::Error(error) => {
                        error!("{:?}", error);
                    }
                }
            })
            .expect("failed to insert drm notifier into event loop");

        // INFO: Anvil changes this as of c21ff35, figure that out
        // SAFETY: no clue lol just copied this from anvil
        let render_node = EGLDevice::device_for_display(&unsafe {
            EGLDisplay::new(gbm.clone()).expect("failed to create EGLDisplay")
        })
        .ok()
        .and_then(|x| x.try_get_render_node().ok().flatten())
        .unwrap_or(node);

        self.gpu_manager
            .as_mut()
            .add_node(render_node, gbm.clone())
            .context("failed to add device to GpuManager")?;

        if render_node == self.primary_gpu {
            let renderer = self.gpu_manager.single_renderer(&render_node)?;

            let (dmabuf_global, drm_global_id) = pinnacle
                .init_hardware_accel(render_node, renderer.dmabuf_formats())
                .inspect_err(|err| {
                    error!("Failed to initialize EGL hardware acceleration: {err}");
                })?;

            assert!(self.dmabuf_global.replace(dmabuf_global).is_none());
            assert!(self.drm_global.replace(drm_global_id).is_none());

            // Update the per drm surface dmabuf feedback
            for device in self.devices.values_mut() {
                for surface in device.surfaces.values_mut() {
                    let dmabuf_feedback = surface.drm_output.with_compositor(|compositor| {
                        get_surface_dmabuf_feedback(
                            render_node,
                            surface.render_node,
                            &mut self.gpu_manager,
                            compositor.surface(),
                        )
                    });

                    if let Some(dmabuf_feedback) = dmabuf_feedback {
                        surface.dmabuf_feedback.replace(dmabuf_feedback);
                    }
                }
            }
        }

        let allocator = GbmAllocator::new(
            gbm.clone(),
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );
        let color_formats = if std::env::var("PINNACLE_DISABLE_10BIT").is_ok() {
            SUPPORTED_FORMATS_8BIT_ONLY
        } else {
            SUPPORTED_FORMATS
        };

        let mut renderer = self.gpu_manager.single_renderer(&render_node).unwrap();
        let render_formats = renderer
            .as_mut()
            .egl_context()
            .dmabuf_render_formats()
            .clone();

        let drm_output_manager = DrmOutputManager::new(
            drm,
            allocator,
            GbmFramebufferExporter::new(gbm.clone(), render_node.into()),
            Some(gbm),
            color_formats.iter().copied(),
            render_formats,
        );

        self.devices.insert(
            node,
            Device {
                registration_token,
                drm_output_manager,
                drm_scanner: DrmScanner::new(),
                render_node,
                surfaces: HashMap::new(),
            },
        );

        self.device_changed(pinnacle, node);

        Ok(())
    }

    /// A display was plugged in.
    fn connector_connected(
        &mut self,
        pinnacle: &mut Pinnacle,
        node: DrmNode,
        connector: connector::Info,
        crtc: crtc::Handle,
    ) {
        debug!(?node, ?connector, ?crtc, "Udev::connector_connected");

        let Some(device) = self.devices.get_mut(&node) else {
            warn!(?node, "Device disappeared");
            return;
        };

        let mut renderer = self
            .gpu_manager
            .single_renderer(&device.render_node)
            .expect("failed to get primary gpu MultiRenderer");

        info!(
            ?crtc,
            "Trying to setup connector {:?}-{}",
            connector.interface(),
            connector.interface_id(),
        );

        let mode_id = connector
            .modes()
            .iter()
            .position(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
            .unwrap_or(0);

        let drm_mode = connector.modes()[mode_id];
        let smithay_mode = smithay::output::Mode::from(drm_mode);

        let drm_device = device.drm_output_manager.device_mut();

        let surface = match drm_device.create_surface(crtc, drm_mode, &[connector.handle()]) {
            Ok(surface) => surface,
            Err(err) => {
                warn!("Failed to create drm surface: {}", err);
                return;
            }
        };

        let output_name = format!(
            "{}-{}",
            connector.interface().as_str(),
            connector.interface_id()
        );

        let display_info =
            smithay_drm_extras::display_info::for_connector(drm_device, connector.handle());

        let edid_hdr = display_info.as_ref().map(|info| info.hdr_static_metadata());

        let (make, model, serial) = display_info
            .map(|info| {
                (
                    info.make().unwrap_or("Unknown".into()),
                    info.model().unwrap_or("Unknown".into()),
                    info.serial().unwrap_or("Unknown".into()),
                )
            })
            .unwrap_or_else(|| ("Unknown".into(), "Unknown".into(), "Unknown".into()));

        let (phys_w, phys_h) = connector.size().unwrap_or_default();

        let output = Output::new(
            output_name,
            PhysicalProperties {
                size: (phys_w as i32, phys_h as i32).into(),
                subpixel: Subpixel::from(connector.subpixel()),
                make,
                model,
                serial_number: serial,
            },
        );
        let global = output.create_global::<State>(&self.display_handle);
        output.with_state_mut(|state| state.enabled_global_id = Some(global));

        pinnacle.outputs.push(output.clone());
        pinnacle.output_focus_stack.add_to_end(output.clone());

        output.with_state_mut(|state| {
            state.debug_damage_tracker = OutputDamageTracker::from_output(&output);
        });

        output.set_preferred(smithay_mode);

        let modes = connector
            .modes()
            .iter()
            .cloned()
            .map(smithay::output::Mode::from)
            .collect::<Vec<_>>();
        output.with_state_mut(|state| state.modes = modes);

        pinnacle
            .output_management_manager_state
            .add_head::<State>(&output);

        let x = pinnacle.space.outputs().fold(0, |acc, o| {
            let Some(geo) = pinnacle.space.output_geometry(o) else {
                unreachable!()
            };
            acc + geo.size.w
        });
        let position = (x, 0).into();

        output.change_current_state(Some(smithay_mode), None, None, Some(position));

        output.user_data().insert_if_missing(|| UdevOutputData {
            crtc,
            device_id: node,
        });

        let mut hdr = crate::hdr::HdrConfig::from_env(&output);
        if let Some(metadata) = edid_hdr {
            hdr.apply_edid(
                metadata.desired_content_max_luminance,
                metadata.desired_content_max_frame_avg_luminance,
            );
        }

        let drm_output = {
            let planes = surface.planes().clone();

            match device.drm_output_manager.lock().initialize_output(
                crtc,
                drm_mode,
                &[connector.handle()],
                &output,
                Some(planes),
                &mut renderer,
                &DrmOutputRenderElements::<_, OutputRenderElement<_>>::default(),
            ) {
                Ok(drm_output) => drm_output,
                Err(err) => {
                    warn!("Failed to create drm compositor: {}", err);
                    return;
                }
            }
        };

        if hdr.enabled {
            let color_state = ConnectorColorState {
                colorspace: Colorspace::Bt2020Rgb,
                hdr_metadata: Some(HdrOutputMetadata::pq_bt2020(
                    hdr.max_luminance,
                    50,
                    hdr.max_luminance,
                    hdr.max_fall,
                )),
                max_bpc: Some(10),
            };
            let staged = drm_output.with_compositor(|compositor| {
                compositor.use_color_state(color_state)
            });
            match staged {
                Ok(()) => info!(
                    "Driving {} in BT.2020/PQ HDR mode: {} nits SDR white, {} nit peak",
                    output.name(),
                    hdr.reference_white,
                    hdr.max_luminance
                ),
                Err(err) => {
                    // Signalling SDR while the compositor converts content to PQ would
                    // look catastrophically wrong, so fall back to the SDR path.
                    warn!(
                        "Connector {} does not accept HDR state ({err}), staying SDR",
                        output.name()
                    );
                    hdr.enabled = false;
                }
            }
        }

        // `RenderSurface::hdr` is what drives the connector, but the output's state
        // is what the output API, the colour-management handlers and the preferred
        // descriptions read. Seed it from the same value, fallback included: without
        // this a PINNACLE_HDR output drives the panel in PQ while telling every
        // client it is SDR, and they tone map their HDR content away.
        output.with_state_mut(|state| state.hdr = hdr);

        let dmabuf_feedback = drm_output.with_compositor(|compositor| {
            get_surface_dmabuf_feedback(
                self.primary_gpu,
                device.render_node,
                &mut self.gpu_manager,
                compositor.surface(),
            )
        });

        let surface = RenderSurface {
            device_id: node,
            render_node: device.render_node,
            drm_output,
            dmabuf_feedback,
            render_state: RenderState::Idle,
            screencopy_commit_state: ScreencopyCommitState::default(),
            hdr,
            hdr_program: None,
            hdr_dump: crate::hdr::FrameDump::from_env(),
            hdr_log: crate::hdr::verbose(),
            hdr_scanout: None,
            hdr_gate: None,
            hdr_encoded_signature: None,
            previous_gamma: None,
            pending_gamma_change: PendingGammaChange::Idle,
            frame_clock: FrameClock::new(Some(refresh_interval(drm_mode))),
            frame_callback_sequence: FrameCallbackSequence::default(),
        };

        device.surfaces.insert(crtc, surface);

        pinnacle.change_output_state(
            self,
            &output,
            Some(OutputMode::Smithay(smithay_mode)),
            None,
            None,
            Some(position),
        );

        // If there is saved connector state, the connector was previously plugged in.
        // In this case, restore its tags and location.
        // TODO: instead of checking the connector, check the monitor's edid info instead
        if let Some(saved_state) = pinnacle
            .config
            .connector_saved_states
            .get(&OutputName(output.name()))
        {
            let ConnectorSavedState {
                loc,
                tags,
                scale,
                powered,
                hdr,
            } = saved_state.clone();

            output.with_state_mut(|state| state.tags.clone_from(&tags));
            pinnacle.change_output_state(self, &output, None, None, scale, Some(loc));
            if let Some(powered) = powered {
                self.set_output_powered(&output, &pinnacle.loop_handle, powered);
            }
            // The colour state staged at creation came from the environment; a
            // saved configuration from before the unplug wins. The output's state
            // then follows what the connector accepted, not what was asked for: a
            // rejected configuration leaves the surface in SDR signalling, and
            // telling clients otherwise would make them commit PQ into it.
            if let Some(hdr) = hdr {
                let applied = match self.set_output_hdr(&output, hdr) {
                    Ok(()) => hdr,
                    Err(err) => {
                        warn!("Failed to restore HDR on {}: {err}", output.name());
                        crate::hdr::HdrConfig {
                            enabled: false,
                            ..hdr
                        }
                    }
                };
                output.with_state_mut(|state| state.hdr = applied);
                pinnacle.update_preferred_descriptions(&output);
            }
        } else {
            pinnacle.signal_state.output_connect.signal(&output);
        }

        pinnacle.output_management_manager_state.update::<State>();
    }

    /// A display was unplugged.
    fn connector_disconnected(
        &mut self,
        pinnacle: &mut Pinnacle,
        node: DrmNode,
        crtc: crtc::Handle,
    ) {
        debug!(?node, ?crtc, "Udev::connector_disconnected");

        let Some(device) = self.devices.get_mut(&node) else {
            warn!(?node, "Device disappeared");
            return;
        };

        device.surfaces.remove(&crtc);

        let output = pinnacle
            .outputs
            .iter()
            .find(|o| {
                o.user_data()
                    .get::<UdevOutputData>()
                    .map(|id| id.device_id == node && id.crtc == crtc)
                    .unwrap_or(false)
            })
            .cloned();

        if let Some(output) = output {
            pinnacle.remove_output(&output);
        }
    }

    fn device_changed(&mut self, pinnacle: &mut Pinnacle, node: DrmNode) {
        debug!(?node, "Udev::device_changed");

        let Some(device) = self.devices.get_mut(&node) else {
            warn!(?node, "Device disappeared");
            return;
        };

        let drm_scan_result = match device
            .drm_scanner
            .scan_connectors(device.drm_output_manager.device())
        {
            Ok(scan_result) => scan_result,
            Err(err) => {
                error!("Failed to scan drm connectors: {err}");
                return;
            }
        };

        for event in drm_scan_result {
            match event {
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } => {
                    self.connector_connected(pinnacle, node, connector, crtc);
                }
                DrmScanEvent::Disconnected {
                    connector: _,
                    crtc: Some(crtc),
                } => {
                    self.connector_disconnected(pinnacle, node, crtc);
                }
                _ => {}
            }
        }
    }

    /// A GPU was unplugged.
    fn device_removed(&mut self, pinnacle: &mut Pinnacle, node: DrmNode) {
        debug!(?node, "Udev::device_removed");

        let Some(device) = self.devices.get(&node) else {
            warn!(?node, "Device disappeared");
            return;
        };

        let crtcs = device
            .drm_scanner
            .crtcs()
            .map(|(_info, crtc)| crtc)
            .collect::<Vec<_>>();

        for crtc in crtcs {
            self.connector_disconnected(pinnacle, node, crtc);
        }

        debug!("Surfaces dropped");

        let Some(device) = self.devices.remove(&node) else {
            unreachable!()
        };

        self.gpu_manager.as_mut().remove_node(&device.render_node);

        pinnacle.loop_handle.remove(device.registration_token);

        if node == self.primary_gpu {
            if let Some(dmabuf_global) = self.dmabuf_global.take() {
                pinnacle
                    .dmabuf_state
                    .disable_global::<State>(&pinnacle.display_handle, &dmabuf_global);
                // Niri waits 10 seconds to destroy the global
                pinnacle
                    .loop_handle
                    .insert_source(
                        Timer::from_duration(Duration::from_secs(10)),
                        move |_, _, state| {
                            state.pinnacle.dmabuf_state.destroy_global::<State>(
                                &state.pinnacle.display_handle,
                                dmabuf_global,
                            );
                            TimeoutAction::Drop
                        },
                    )
                    .unwrap();

                for device in self.devices.values_mut() {
                    for surface in device.surfaces.values_mut() {
                        surface.dmabuf_feedback = None;
                    }
                }
            } else {
                error!("Could not remove dmabuf global because it was missing");
            }

            if let Some(drm_global_id) = self.drm_global.take() {
                pinnacle
                    .display_handle
                    .remove_global::<State>(drm_global_id);
            } else {
                error!("Could not remove drm global because it was missing");
            }
        }
    }

    fn on_vblank(
        &mut self,
        pinnacle: &mut Pinnacle,
        dev_id: DrmNode,
        crtc: crtc::Handle,
        metadata: DrmEventMetadata,
    ) {
        let span = tracy_client::span!("Udev::on_vblank");

        let Some(surface) = self
            .devices
            .get_mut(&dev_id)
            .and_then(|device| device.surfaces.get_mut(&crtc))
        else {
            return;
        };

        let output = if let Some(output) = pinnacle.outputs.iter().find(|o| {
            let udev_op_data = o.user_data().get::<UdevOutputData>();
            udev_op_data
                .is_some_and(|data| data.device_id == surface.device_id && data.crtc == crtc)
        }) {
            output.clone()
        } else {
            // somehow we got called with an invalid output
            return;
        };

        span.emit_text(&output.name());

        let presentation_time = match metadata.time {
            smithay::backend::drm::DrmEventTime::Monotonic(tp) => tp,
            smithay::backend::drm::DrmEventTime::Realtime(_) => {
                // Not supported

                // This value will be ignored in the frame clock code
                Duration::ZERO
            }
        };

        match surface
            .drm_output
            .frame_submitted()
            .map_err(SwapBuffersError::from)
        {
            Ok(user_data) => {
                if let Some(mut feedback) = user_data.flatten() {
                    let seq = metadata.sequence as u64;

                    let mut flags = wp_presentation_feedback::Kind::Vsync
                        | wp_presentation_feedback::Kind::HwCompletion;

                    let time: Duration = if presentation_time.is_zero() {
                        pinnacle.clock.now().into()
                    } else {
                        flags.insert(wp_presentation_feedback::Kind::HwClock);
                        presentation_time
                    };

                    let refresh = surface
                        .frame_clock
                        .refresh_interval()
                        .map(match surface.frame_clock.vrr() {
                            true => Refresh::Variable,
                            false => Refresh::Fixed,
                        })
                        .unwrap_or(Refresh::Unknown);

                    feedback.presented::<_, smithay::utils::Monotonic>(time, refresh, seq, flags);
                }

                output.with_state_mut(|state| {
                    if let BlankingState::Blanking = state.blanking_state {
                        debug!("Output {} blanked", output.name());
                        state.blanking_state = BlankingState::Blanked;
                    }
                })
            }
            Err(err) => {
                warn!("Error during rendering: {err:?}");
                if let SwapBuffersError::ContextLost(err) = err {
                    panic!("Rendering loop lost: {err}")
                }
            }
        };

        surface.frame_clock.presented(presentation_time);

        let render_needed = match mem::take(&mut surface.render_state) {
            RenderState::WaitingForVblank { render_needed } => render_needed,
            state => {
                debug!("vblank happened but render state was {state:?}",);
                return;
            }
        };

        if render_needed || pinnacle.cursor_state.is_current_cursor_animated() {
            self.schedule_render(&output);
        } else {
            pinnacle.send_frame_callbacks(&output, Some(surface.frame_callback_sequence));
        }
    }

    pub(super) fn render_if_scheduled(&mut self, pinnacle: &mut Pinnacle, output: &Output) {
        let span = tracy_client::span!("Udev::render_if_scheduled");
        span.emit_text(&output.name());

        let Some(surface) = render_surface_for_output(output, &mut self.devices) else {
            return;
        };

        if matches!(
            surface.render_state,
            RenderState::Scheduled | RenderState::WaitingForEstimatedVblankAndScheduled(_)
        ) {
            self.render_surface(pinnacle, output);
        }
    }

    /// Render to the [`RenderSurface`] associated with the given `output`.
    fn render_surface(&mut self, pinnacle: &mut Pinnacle, output: &Output) {
        let span = tracy_client::span!("Udev::render_surface");
        span.emit_text(&output.name());

        let UdevOutputData { device_id, .. } = output.user_data().get().unwrap();
        let is_active = self
            .devices
            .get(device_id)
            .map(|device| device.drm_output_manager.device().is_active())
            .unwrap_or_default();

        let Some(surface) = render_surface_for_output(output, &mut self.devices) else {
            return;
        };

        let make_idle = |render_state: &mut RenderState,
                         loop_handle: &LoopHandle<'static, State>| {
            if let RenderState::WaitingForEstimatedVblankAndScheduled(token)
            | RenderState::WaitingForEstimatedVblank(token) = mem::take(render_state)
            {
                loop_handle.remove(token);
            }
        };

        if !is_active {
            warn!("Device is inactive");
            make_idle(&mut surface.render_state, &pinnacle.loop_handle);
            return;
        }

        if !pinnacle.outputs.contains(output) {
            make_idle(&mut surface.render_state, &pinnacle.loop_handle);
            return;
        }

        // TODO: possibly lift this out and make it so that scheduling a render
        // does nothing on powered off outputs
        if output.with_state(|state| !state.powered) {
            make_idle(&mut surface.render_state, &pinnacle.loop_handle);
            return;
        }

        let Some(output_geo) = pinnacle.space.output_geometry(output) else {
            make_idle(&mut surface.render_state, &pinnacle.loop_handle);
            return;
        };

        assert_matches!(
            surface.render_state,
            RenderState::Scheduled | RenderState::WaitingForEstimatedVblankAndScheduled(_)
        );

        let time_to_next_presentation = surface
            .frame_clock
            .time_to_next_presentation(&pinnacle.clock);

        let render_node = surface.render_node;
        let primary_gpu = self.primary_gpu;
        let mut renderer = if primary_gpu == render_node {
            self.gpu_manager.single_renderer(&render_node)
        } else {
            let format = surface.drm_output.format();
            self.gpu_manager
                .renderer(&primary_gpu, &render_node, format)
        }
        .expect("failed to create MultiRenderer");

        let _ = renderer.upscale_filter(self.upscale_filter);
        let _ = renderer.downscale_filter(self.downscale_filter);

        let pointer_location = pinnacle
            .seat
            .get_pointer()
            .map(|ptr| ptr.current_location())
            .unwrap_or((0.0, 0.0).into());

        let mut output_render_elements = Vec::new();

        let should_blank = pinnacle.lock_state.is_locking()
            || (pinnacle.lock_state.is_locked()
                && output.with_state(|state| state.lock_surface.is_none()));

        let scale = output.current_scale().fractional_scale();

        // The shader bakes in the paper white, so it is compiled per configuration
        // and only here: the renderer exists inside a frame, and a failure has to
        // be dealt with before anything below reads the HDR state. Doing it first
        // means the frame that discovers the failure is already rendered and
        // signalled as SDR, instead of the cursor being placed for a passthrough
        // frame that no longer happens.
        if surface.hdr.enabled && surface.hdr_program.is_none() {
            match renderer
                .as_gles_renderer()
                .compile_custom_texture_shader(surface.hdr.shader(), &[])
            {
                Ok(program) => surface.hdr_program = Some(program),
                Err(err) => {
                    // Nothing can be composited without the program, so the whole
                    // output goes back to SDR: the connector stops signalling PQ,
                    // the output's state says SDR, and every client is told to send
                    // sRGB again. Leaving `state.hdr` at PQ while the panel gets
                    // sRGB is what would turn a shader failure into a washed-out
                    // desktop that the config client cannot even see.
                    warn!(
                        "Failed to compile the HDR shader on {} ({err}), falling back to SDR",
                        output.name()
                    );
                    surface.hdr.enabled = false;
                    surface.hdr_gate = None;
                    surface.hdr_encoded_signature = None;
                    surface.hdr_scanout = None;

                    let color_state = ConnectorColorState {
                        colorspace: Colorspace::Default,
                        hdr_metadata: None,
                        max_bpc: None,
                    };
                    if let Err(err) = surface
                        .drm_output
                        .with_compositor(|compositor| compositor.use_color_state(color_state))
                    {
                        warn!(
                            "Failed to return {} to SDR signalling: {err}",
                            output.name()
                        );
                    }

                    let applied = surface.hdr;
                    output.with_state_mut(|state| state.hdr = applied);
                    pinnacle.update_preferred_descriptions(output);
                    pinnacle
                        .color_management_state
                        .output_description_changed(output);
                }
            }
        }

        // Whether a fullscreen HDR window is on this output. On its own that says
        // nothing about scanout: a frame only goes out untouched when nothing has
        // to be drawn on top of that window either, which is decided below once
        // the element list exists.
        let hdr_passthrough = surface.hdr.enabled && fullscreen_content_is_native(pinnacle, output);

        // Element geometry is physical and relative to this output's top-left
        // corner, so this is what tells a cursor on this output from one on
        // another output: the pointer elements exist for every output, just
        // located off-screen on the rest.
        let output_physical =
            smithay::utils::Rectangle::from_size(output_geo.size.to_physical_precise_round(scale));
        let (pointer_render_elements, cursor_ids) = pointer_render_elements(
            pointer_location - output_geo.loc.to_f64(),
            scale,
            &mut renderer,
            &mut pinnacle.cursor_state,
            &pinnacle.clock,
        );
        let cursor_on_output = pointer_render_elements.iter().any(|element| {
            element
                .geometry(smithay::utils::Scale::from(scale))
                .overlaps(output_physical)
        });

        if should_blank {
            output.with_state_mut(|state| {
                if let BlankingState::NotBlanked = state.blanking_state {
                    debug!("Blanking output {} for session lock", output.name());
                    state.blanking_state = BlankingState::Blanking;
                }
            });
        } else if pinnacle.lock_state.is_locked() {
            if let Some(lock_surface) = output.with_state(|state| state.lock_surface.clone()) {
                let elems = render_elements_from_surface_tree(
                    &mut renderer,
                    lock_surface.wl_surface(),
                    (0, 0),
                    output.current_scale().fractional_scale(),
                    1.0,
                    element::Kind::Unspecified,
                );

                output_render_elements.extend(elems);
            }
        } else {
            output_render_elements.extend(crate::render::output_render_elements(
                output,
                &mut renderer,
                &pinnacle.space,
                &pinnacle.z_index_stack,
            ));
        }

        if pinnacle.config.debug.visualize_opaque_regions {
            crate::render::util::render_opaque_regions(
                &mut output_render_elements,
                smithay::utils::Scale::from(output.current_scale().fractional_scale()),
            );
        }

        if pinnacle.config.debug.visualize_damage {
            let damage_elements = output.with_state_mut(|state| {
                crate::render::util::render_damage_from_elements(
                    &mut state.debug_damage_tracker,
                    &output_render_elements,
                    [0.3, 0.0, 0.0, 0.3].into(),
                )
            });
            output_render_elements = damage_elements
                .into_iter()
                .map(From::from)
                .chain(output_render_elements)
                .collect();
        }

        let clear_color = if pinnacle.lock_state.is_unlocked() {
            CLEAR_COLOR
        } else {
            CLEAR_COLOR_LOCKED
        };

        // Elements whose client already outputs this output's encoding. They are
        // drawn as a copy instead of through the conversion shader, which is what
        // makes a windowed or tiled HDR client look right: without this the whole
        // frame is one encoding and an HDR client inside it gets encoded twice.
        //
        // `EncodedSurfaces` looks the description up per surface, not per window,
        // and inherits it from the nearest ancestor, which is what catches Wine's
        // swapchain surface. Membership is needed before rendering, because the
        // passthrough decision below asks whether the top element is one of these.
        let encoded_surfaces = if surface.hdr.enabled {
            Some(crate::hdr::EncodedSurfaces::for_windows(
                pinnacle.space.elements_for_output(output),
            ))
        } else {
            None
        };
        let already_encoded = encoded_surfaces
            .as_ref()
            .map(|encoded| encoded.ids.clone())
            .unwrap_or_default();

        if surface.hdr_log
            && let Some(encoded) = &encoded_surfaces
        {
            let signature = encoded.signature();
            if surface.hdr_encoded_signature.as_deref() != Some(&signature) {
                info!(
                    "{}: already encoded [{}] other descriptions [{}] no description [{}]",
                    output.name(),
                    encoded.marked.join(","),
                    encoded.rejected.join(","),
                    encoded.undescribed.join(","),
                );
                surface.hdr_encoded_signature = Some(signature);
            }
        }

        // A frame whose topmost element is the HDR client's own buffer covering
        // the whole output is already in the output's encoding, so a copy is the
        // correct conversion and running the SDR shader over it would encode it
        // twice. This is what makes fullscreen HDR content look right even when
        // the primary plane cannot scan the client's buffer out (an fp16
        // swapchain, for instance).
        //
        // The top element has to *be* native, not merely cover the output: an SDR
        // layer surface above a fullscreen HDR window is exactly as wide, and
        // passing the frame through would send its sRGB pixels to the panel as PQ.
        // Membership in `already_encoded` is what settles that.
        //
        // Elements *below* it do not matter: an opaque element covering the output
        // culls everything underneath, and real frames are never a single element
        // (a tiling layout keeps every window on the output, plus layer surfaces).
        //
        // The cursor is part of the decision. In HDR mode it cannot use its plane
        // (the hardware would blend sRGB into the PQ signal) and any element above
        // the primary plane forces composition, so a cursor over the output means
        // the frame is composited - which is converted, and therefore correct.
        // Losing scanout for it is the price of a visible pointer; a client that
        // hides its cursor keeps it.
        let first_element_geometry = output_render_elements
            .first()
            .map(|element| element.geometry(smithay::utils::Scale::from(scale)));
        let top_is_native_full_output_surface =
            output_render_elements.first().is_some_and(|element| {
                matches!(element, OutputRenderElement::Surface(_))
                    && already_encoded.iter().any(|known| element.id() == known)
            }) && first_element_geometry == Some(output_physical);
        let hdr_passthrough_frame = PassthroughFrame {
            hdr_passthrough,
            cursor_on_output,
            top_is_native_full_output_surface,
        }
        .can_passthrough();

        // The cursor belongs on top of a composited frame. It is only absent on the
        // passthrough path, where there is nothing to draw it over: the client's
        // buffer fills the output and the pointer is hidden or on another output.
        if !hdr_passthrough_frame {
            output_render_elements.splice(
                0..0,
                pointer_render_elements
                    .into_iter()
                    .map(OutputRenderElement::from),
            );
        }

        let already_encoded_count = already_encoded.len();
        // The geometry of each matching element, so the log says whether the marked
        // element is the client's picture or a small overlay that happens to carry
        // the description.
        let matching_geometry = output_render_elements
            .iter()
            .filter(|element| matches!(element, OutputRenderElement::Surface(_)))
            .filter(|element| already_encoded.iter().any(|known| element.id() == known))
            .map(|element| {
                let geometry = element.geometry(smithay::utils::Scale::from(scale));
                format!(
                    "{}x{}+{}+{}",
                    geometry.size.w, geometry.size.h, geometry.loc.x, geometry.loc.y
                )
            })
            .collect::<Vec<_>>();
        let matching_elements = matching_geometry.len();

        {
            // Set on every output, empty or not, so a list from another output's
            // frame can never leak into this one.
            renderer
                .as_gles_renderer()
                .set_already_encoded_elements(already_encoded);
        }

        if surface.hdr.enabled {
            // The program was compiled at the top of this function; a failure
            // there has already put the output back into SDR.
            let ref_white = surface.hdr.reference_white;
            let program = surface.hdr_program.clone();
            let gles = renderer.as_gles_renderer();
            if let Some(program) = program
                && !hdr_passthrough_frame
            {
                gles.set_default_tex_program_override(Some((program, Vec::new())));
            }
            // Covers `Frame::clear` and solid elements. The clear colour is handed
            // over unconverted on purpose: Smithay applies the transform to it, so
            // converting it here as well would encode it twice.
            gles.set_solid_color_transform(Some(Box::new(move |color| {
                crate::hdr::premultiplied_srgb_to_pq(color, ref_white)
            })));
        }

        // No overlay planes cuz they wonk
        let mut frame_flags =
            FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT_ANY | FrameFlags::ALLOW_CURSOR_PLANE_SCANOUT;

        if surface.hdr.enabled {
            // Cursor and overlay planes bypass the convert shader entirely, so their
            // pixels would be read as PQ. The primary plane may only scan out when
            // the frame is the client's native-encoding buffer and nothing else.
            frame_flags.remove(FrameFlags::ALLOW_CURSOR_PLANE_SCANOUT);
            if !hdr_passthrough_frame {
                frame_flags.remove(FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT_ANY);
            }
        }

        if pinnacle.config.debug.disable_cursor_plane_scanout {
            frame_flags.remove(FrameFlags::ALLOW_CURSOR_PLANE_SCANOUT);
        }

        if surface.frame_clock.vrr()
            && let Some(time_since_last_presentation) = surface
                .frame_clock
                .time_since_last_presentation(&pinnacle.clock)
        {
            // We want to skip cursor-only updates so moving the mouse doesn't jump the refresh
            // rate to the max, which would cause stuttering in games. We're doing this only
            // when vrr is on and the cursor is above a fullscreen window.
            //
            // However, this would cause the cursor to freeze if the window doesn't refresh.
            // Therefore we're forcing the cursor to refresh at at least 24 fps.

            const _24_FPS: Duration = Duration::from_nanos(1_000_000_000 / 24);

            let too_long_since_last_present =
                time_to_next_presentation + time_since_last_presentation > _24_FPS;
            let window_under = pinnacle.space.element_under(
                pinnacle
                    .seat
                    .get_pointer()
                    .unwrap()
                    .current_location()
                    .to_i32_round(),
            );

            let cursor_over_fs_window = window_under
                .is_some_and(|(win, _)| win.with_state(|state| state.layout_mode.is_fullscreen()));

            let is_xwayland = window_under.is_some_and(|(win, _)| win.is_x11());

            let content_type = window_under
                .and_then(|(win, _)| {
                    win.toplevel()
                        .map(|toplevel| get_surface_content_type(toplevel.wl_surface()))
                })
                .unwrap_or(ContentType::None);

            // Only skip cursor updates for games.
            //
            // We don't do this for `Photo`s because they're static and don't stutter when
            // the refresh rate jumps, nor for `Video`s because we don't expect the mouse
            // to be moved much while video is playing.
            //
            // We assume xwayland windows may be a game for this purpose.
            let should_skip = content_type == ContentType::Game || is_xwayland;

            if !too_long_since_last_present && cursor_over_fs_window && should_skip {
                // FIXME: With a non-1 scale, the cursor no longer resides on the cursor plane,
                // making this useless

                frame_flags |= FrameFlags::SKIP_CURSOR_ONLY_UPDATES;
            }
        }

        let render_frame_result = surface.drm_output.render_frame(
            &mut renderer,
            &output_render_elements,
            clear_color,
            frame_flags,
        );

        // Dump before the conversion hooks are cleared below: the dump re-renders
        // the elements, and with the hooks already gone it would show pixels the
        // panel never received.
        if surface.hdr.enabled
            && let Ok(res) = &render_frame_result
            && surface.hdr_dump.as_mut().is_some_and(|dump| dump.poll())
            && let Some(path) = surface.hdr_dump.as_ref().map(|d| d.path_for(&output.name()))
        {
            let scale = output.current_scale().fractional_scale();
            let transform = output.current_transform();
            let physical = output_geo.size.to_physical_precise_round(scale);
            let size = transform.transform_size(physical);
            match dump_hdr_frame(&mut renderer, res, &path, size, transform, scale) {
                Ok(()) => info!("wrote HDR framebuffer dump {}", path.display()),
                Err(err) => warn!("HDR framebuffer dump failed: {err}"),
            }
        }

        if surface.hdr.enabled {
            let gles = renderer.as_gles_renderer();
            gles.set_default_tex_program_override(None);
            gles.set_solid_color_transform(None);
            gles.set_already_encoded_elements(Vec::new());
        }

        let hdr_gate = hdr_passthrough;

        if surface.hdr.enabled && surface.hdr_gate != Some(hdr_gate) {
            // The answer to "what does this output prefer" changed, so tell the
            // clients on it: a player that asked before it was mapped gets one
            // chance to switch to PQ instead of tone mapping to SDR.
            surface.hdr_gate = Some(hdr_gate);
            if surface.hdr_log {
                info!("{}: passthrough gate is now {hdr_gate}", output.name());
            }
            pinnacle.update_preferred_descriptions(output);
        }

        let failed = match render_frame_result {
            Ok(res) => {
                if surface.hdr.enabled && surface.hdr_log {
                    // The only honest answer to "is the passthrough happening": a
                    // `Swapchain` primary plane means the frame went through the
                    // convert shader, an `Element` means the client's own buffer
                    // was scanned out. The element list decides which of the two is
                    // possible at all, so it is logged with the decision, on change.
                    let kind = match &res.primary_element {
                        PrimaryPlaneElement::Element(_) => "direct scanout of the client buffer",
                        PrimaryPlaneElement::Swapchain(_) => "composited through the HDR shader",
                    };
                    let elements = output_render_elements
                        .iter()
                        .map(|element| match element {
                            OutputRenderElement::Surface(_) => "surface",
                            OutputRenderElement::Pointer(_) => "pointer",
                            OutputRenderElement::Snapshot(_) => "snapshot",
                            OutputRenderElement::SolidColor(_) => "solid",
                        })
                        .collect::<Vec<_>>()
                        .join(",");
                    let count = output_render_elements.len();
                    let framebuffer = format!("{:?}", surface.drm_output.format());
                    let signature = format!(
                        "{kind}|{elements}|{hdr_gate}|{hdr_passthrough_frame}"
                    );
                    if surface.hdr_scanout.as_deref() != Some(&signature) {
                        info!(
                            "{}: primary plane {kind} ([{elements}] {count} elements, \
                             top {first_element_geometry:?} of {output_physical:?}, gate {hdr_gate}, \
                             client buffer passed through {hdr_passthrough_frame}, \
                             {already_encoded_count} surfaces marked, {matching_elements} elements in frame, \
                             matched [{}],                              framebuffer {framebuffer})",
                            output.name(),
                            matching_geometry.join(","),
                        );
                        surface.hdr_scanout = Some(signature);
                    }
                }

                if res.needs_sync()
                    && let PrimaryPlaneElement::Swapchain(element) = &res.primary_element
                    && let Err(err) = element.sync.wait()
                {
                    warn!("Failed to wait for sync point: {err}");
                }

                if pinnacle.lock_state.is_unlocked() {
                    handle_pending_screencopy(
                        &mut renderer,
                        output,
                        surface,
                        &res,
                        &pinnacle.loop_handle,
                        cursor_ids,
                    );
                }

                pinnacle.update_primary_scanout_output(output, &res.states);

                if let Some(dmabuf_feedback) = surface.dmabuf_feedback.as_ref() {
                    pinnacle.send_dmabuf_feedback(output, dmabuf_feedback, &res.states);
                }

                let rendered = !res.is_empty;

                if rendered {
                    let output_presentation_feedback =
                        take_presentation_feedback(output, &pinnacle.space, &res.states);

                    match surface
                        .drm_output
                        .queue_frame(Some(output_presentation_feedback))
                    {
                        Ok(()) => {
                            let new_state = RenderState::WaitingForVblank {
                                render_needed: false,
                            };

                            match mem::replace(&mut surface.render_state, new_state) {
                                RenderState::Idle => unreachable!(),
                                RenderState::Scheduled => (),
                                RenderState::WaitingForVblank { .. } => unreachable!(),
                                RenderState::WaitingForEstimatedVblank(_) => unreachable!(),
                                RenderState::WaitingForEstimatedVblankAndScheduled(token) => {
                                    pinnacle.loop_handle.remove(token);
                                }
                            };

                            // From niri: We queued this frame successfully, so the current client buffers were
                            // latched. We can send frame callbacks now, since a new client commit
                            // will no longer overwrite this frame and will wait for a VBlank.
                            surface.frame_callback_sequence.increment();

                            pinnacle.send_frame_callbacks(
                                output,
                                Some(surface.frame_callback_sequence),
                            );

                            self.update_output_vrr(pinnacle, output);

                            // Return here to not queue the estimated vblank timer on a submitted frame
                            return;
                        }
                        Err(err) => {
                            warn!("Error queueing frame: {err}");
                            true
                        }
                    }
                } else {
                    false
                }
            }
            Err(err) => {
                // Can fail if we switched to a different TTY
                warn!("Render failed for surface: {err}");
                true
            }
        };

        Self::queue_estimated_vblank_timer(surface, pinnacle, output, time_to_next_presentation);

        if failed {
            surface.render_state = if let RenderState::WaitingForEstimatedVblank(token)
            | RenderState::WaitingForEstimatedVblankAndScheduled(
                token,
            ) = surface.render_state
            {
                RenderState::WaitingForEstimatedVblank(token)
            } else {
                RenderState::Idle
            };
        }

        pinnacle.send_frame_callbacks(output, Some(surface.frame_callback_sequence));
    }

    fn queue_estimated_vblank_timer(
        surface: &mut RenderSurface,
        pinnacle: &mut Pinnacle,
        output: &Output,
        mut time_to_next_presentation: Duration,
    ) {
        let span = tracy_client::span!("Udev::queue_estimated_vblank_timer");
        span.emit_text(&output.name());

        match mem::take(&mut surface.render_state) {
            RenderState::Idle => unreachable!(),
            RenderState::Scheduled => (),
            RenderState::WaitingForVblank { .. } => unreachable!(),
            RenderState::WaitingForEstimatedVblank(token)
            | RenderState::WaitingForEstimatedVblankAndScheduled(token) => {
                surface.render_state = RenderState::WaitingForEstimatedVblank(token);
                return;
            }
        }

        // No use setting a zero timer, since we'll send frame callbacks anyway right after the call to
        // render(). This can happen for example with unknown presentation time from DRM.
        if time_to_next_presentation.is_zero() {
            time_to_next_presentation += surface
                .frame_clock
                .refresh_interval()
                // Unknown refresh interval, i.e. winit backend. Would be good to estimate it somehow
                // but it's not that important for this code path.
                .unwrap_or(Duration::from_micros(16_667));
        }

        let timer = Timer::from_duration(time_to_next_presentation);

        let output = output.clone();
        let token = pinnacle
            .loop_handle
            .insert_source(timer, move |_, _, state| {
                state
                    .backend
                    .udev_mut()
                    .on_estimated_vblank_timer(&mut state.pinnacle, &output);
                TimeoutAction::Drop
            })
            .unwrap();

        surface.render_state = RenderState::WaitingForEstimatedVblank(token);
    }

    fn on_estimated_vblank_timer(&mut self, pinnacle: &mut Pinnacle, output: &Output) {
        let span = tracy_client::span!("Udev::on_estimated_vblank_timer");
        span.emit_text(&output.name());

        let Some(surface) = render_surface_for_output(output, &mut self.devices) else {
            return;
        };

        surface.frame_callback_sequence.increment();

        match mem::take(&mut surface.render_state) {
            RenderState::Idle => {
                // FIXME: this is still reachable after sleep
            }
            RenderState::Scheduled => unreachable!(),
            RenderState::WaitingForVblank { .. } => unreachable!(),
            RenderState::WaitingForEstimatedVblank(_) => (),
            RenderState::WaitingForEstimatedVblankAndScheduled(_) => {
                surface.render_state = RenderState::Scheduled;
                return;
            }
        }

        if pinnacle.cursor_state.is_current_cursor_animated() {
            self.schedule_render(output);
        } else {
            pinnacle.send_frame_callbacks(output, Some(surface.frame_callback_sequence));
        }
    }

    fn update_output_vrr(&mut self, pinnacle: &mut Pinnacle, output: &Output) {
        if output.with_state(|state| !state.is_vrr_on_demand) {
            return;
        }

        let vrr = pinnacle.space.elements_for_output(output).any(|win| {
            let Some(demand) = win.with_state(|state| state.vrr_demand) else {
                return false;
            };

            let mut visible = false;
            win.with_surfaces(|surface, states| {
                if surface_primary_scanout_output(surface, states).as_ref() == Some(output) {
                    visible = true;
                }
            });

            visible
                // FIXME: We probably want to check the actual fullscreen state, not the layout mode,
                // but this isn't a *super* huge deal
                && (!demand.fullscreen || win.with_state(|state| state.layout_mode.is_fullscreen()))
        });

        self.set_output_vrr(output, vrr);
    }

    /// Whether this output's connector can be driven in BT.2020/PQ.
    pub(super) fn output_hdr_supported(&mut self, output: &Output) -> bool {
        let Some(surface) = render_surface_for_output(output, &mut self.devices) else {
            return false;
        };

        surface
            .drm_output
            .with_compositor(|compositor| {
                let drm_surface = compositor.surface();
                let Some(connector) = drm_surface.current_connectors().into_iter().next() else {
                    return false;
                };
                let hdr_metadata = drm_surface
                    .hdr_metadata_supported(connector)
                    .unwrap_or(false);
                let bt2020 = drm_surface
                    .supported_colorspaces(connector)
                    .map(|spaces| spaces.contains(&Colorspace::Bt2020Rgb))
                    .unwrap_or(false);
                hdr_metadata && bt2020
            })
    }

    /// Drive an output in BT.2020/PQ, or return it to SDR signalling.
    ///
    /// The colour properties are staged into the next atomic commit; nothing
    /// takes effect until the next frame is rendered.
    pub(super) fn set_output_hdr(
        &mut self,
        output: &Output,
        config: crate::hdr::HdrConfig,
    ) -> Result<(), String> {
        let Some(surface) = render_surface_for_output(output, &mut self.devices) else {
            return Err(format!("no render surface for output {}", output.name()));
        };

        let color_state = if config.enabled {
            ConnectorColorState {
                colorspace: Colorspace::Bt2020Rgb,
                hdr_metadata: Some(HdrOutputMetadata::pq_bt2020(
                    config.max_luminance,
                    50,
                    config.max_luminance,
                    config.max_fall,
                )),
                max_bpc: Some(10),
            }
        } else {
            ConnectorColorState {
                colorspace: Colorspace::Default,
                hdr_metadata: None,
                max_bpc: None,
            }
        };

        surface
            .drm_output
            .with_compositor(|compositor| compositor.use_color_state(color_state))
            .map_err(|err| format!("{err}"))?;

        surface.hdr = config;
        // The program bakes in the paper white, so it is rebuilt with the new
        // configuration. The gate and signatures describe the previous state.
        surface.hdr_program = None;
        surface.hdr_gate = None;
        surface.hdr_encoded_signature = None;
        surface.hdr_scanout = None;

        info!(
            "{} HDR on {}: {} nits SDR white, {} nit peak",
            if config.enabled { "Enabling" } else { "Disabling" },
            output.name(),
            config.reference_white,
            config.max_luminance
        );

        Ok(())
    }

    pub(super) fn set_output_vrr(&mut self, output: &Output, vrr: bool) {
        let Some(surface) = render_surface_for_output(output, &mut self.devices) else {
            return;
        };

        if surface.frame_clock.vrr() == vrr {
            return;
        }

        info!(
            "{} vrr on output {}",
            if vrr { "Enabling" } else { "Disabling" },
            output.name()
        );

        if let Err(err) = surface.drm_output.with_compositor(|comp| comp.use_vrr(vrr)) {
            warn!("Failed to set vrr on output {}: {err}", output.name());
        }
        surface.frame_clock.set_vrr(
            surface
                .drm_output
                .with_compositor(|comp| comp.vrr_enabled()),
        );
        output.with_state_mut(|state| state.is_vrr_on = surface.frame_clock.vrr());
    }
}

fn render_surface_for_output<'a>(
    output: &Output,
    devices: &'a mut HashMap<DrmNode, Device>,
) -> Option<&'a mut RenderSurface> {
    let UdevOutputData { device_id, crtc } = output.user_data().get()?;

    devices
        .get_mut(device_id)
        .and_then(|device| device.surfaces.get_mut(crtc))
}

// FIXME: damage is completely wrong lol, totally didn't test that
// Use an OutputDamageTracker or something
/// Whether the output shows a fullscreen window whose client content is HDR.
///
/// Whether a fullscreen window on this output is itself in the output's encoding.
///
/// Direct primary-plane scanout skips the convert shader, so it is only correct
/// for BT.2020/PQ content; SDR, HLG and scRGB content all have to be composited
/// through the shader.
fn fullscreen_content_is_native(pinnacle: &Pinnacle, output: &Output) -> bool {
    // Must be a fullscreen window *on this output*: taking the first fullscreen
    // window in the space would let a fullscreen window on another monitor veto
    // passthrough here.
    let Some(window) = pinnacle.space.elements().find(|window| {
        window.with_state(|state| state.layout_mode.is_fullscreen())
            && pinnacle
                .space
                .outputs_for_element(window)
                .iter()
                .any(|o| o == output)
    }) else {
        return false;
    };

    let Some(toplevel) = window.toplevel() else {
        return false;
    };

    // `is_hdr` is not enough: it is also true for HLG, for scRGB and for BT.2020
    // primaries with an SDR transfer function, and none of those are what the
    // connector is being told to expect. Only PQ with BT.2020 primaries is.
    crate::hdr::surface_description_in_tree(toplevel.wl_surface())
        .is_some_and(|description| crate::hdr::description_matches_output_encoding(&description))
}

/// What the frame's element list says about handing it to the panel untouched.
///
/// A frame can only skip the SDR-to-PQ conversion when every pixel in it is
/// already in the output's encoding. That is the case for a fullscreen client
/// whose own buffer covers the whole output, which has nothing composited above
/// it, and whose committed description is BT.2020/PQ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PassthroughFrame {
    /// A fullscreen window on this output declares BT.2020/PQ itself.
    hdr_passthrough: bool,
    /// A cursor element - the pointer or a drag icon - lands on this output.
    cursor_on_output: bool,
    /// The topmost element is a native-encoding client surface covering the whole
    /// output, i.e. one of the `already_encoded` elements.
    top_is_native_full_output_surface: bool,
}

impl PassthroughFrame {
    /// Whether the frame is already in the output's encoding, top to bottom.
    ///
    /// The cursor is deliberately part of this: in HDR mode it cannot be scanned
    /// out on its own plane, because the hardware would blend sRGB into the PQ
    /// signal, so a visible cursor means the frame is composited. That path is the
    /// correct one - the cursor goes through the convert shader and the client's
    /// buffer is copied rather than converted again - and it is what keeps the
    /// pointer visible over fullscreen HDR content.
    fn can_passthrough(self) -> bool {
        self.hdr_passthrough && !self.cursor_on_output && self.top_is_native_full_output_surface
    }
}

/// Write the exact pixels this frame is about to send to KMS as a PPM.
///
/// Debug aid for the on-glass checkpoints: the dump is the compositor's output
/// before the connector's own processing, so it isolates "what we signalled"
/// from "what the panel did with it". Enabled with `PINNACLE_HDR_DUMP=<dir>`.
fn dump_hdr_frame<'a>(
    renderer: &mut UdevRenderer<'a>,
    res: &UdevRenderFrameResult<'a>,
    path: &std::path::Path,
    size: smithay::utils::Size<i32, smithay::utils::Physical>,
    transform: smithay::utils::Transform,
    scale: f64,
) -> anyhow::Result<()> {
    use smithay::backend::allocator::Fourcc;
    use smithay::backend::renderer::gles::GlesRenderbuffer;
    use smithay::utils::Buffer as BufferCoords;

    let buffer_size = size.to_logical(1).to_buffer(1, transform);
    let mut offscreen: GlesRenderbuffer = renderer.create_buffer(Fourcc::Abgr8888, buffer_size)?;
    let mut framebuffer = renderer.bind(&mut offscreen)?;

    let _ = res.blit_frame_result(
        size,
        transform,
        scale,
        renderer,
        &mut framebuffer,
        [smithay::utils::Rectangle::from_size(size)],
        Vec::new(),
    )?;

    let region = smithay::utils::Rectangle::<i32, BufferCoords>::from_size(buffer_size);
    let mapping = renderer.copy_framebuffer(&framebuffer, region, Fourcc::Abgr8888)?;
    let data = renderer.map_texture(&mapping)?;

    let width = buffer_size.w as usize;
    let height = buffer_size.h as usize;
    let stride = width * 4;
    if data.len() < stride * height {
        anyhow::bail!("short readback: {} bytes for {width}x{height}", data.len());
    }

    write_ppm(path, width, height, data)?;
    Ok(())
}


/// Write `width` x `height` pixels of RGBA (R in the first byte) as a binary PPM.
fn write_ppm(path: &std::path::Path, width: usize, height: usize, rgba: &[u8]) -> std::io::Result<()> {
    let stride = width * 4;
    let mut out = Vec::with_capacity(width * height * 3 + 32);
    out.extend_from_slice(format!("P6\n{width} {height}\n255\n").as_bytes());
    for row in rgba.chunks_exact(stride).take(height) {
        for px in row.chunks_exact(4) {
            out.extend_from_slice(&px[0..3]);
        }
    }
    std::fs::write(path, out)
}

fn handle_pending_screencopy<'a>(
    renderer: &mut UdevRenderer<'a>,
    output: &Output,
    surface: &mut RenderSurface,
    render_frame_result: &UdevRenderFrameResult<'a>,
    loop_handle: &LoopHandle<'static, State>,
    cursor_ids: Vec<Id>,
) {
    let span = tracy_client::span!("udev::handle_pending_screencopy");
    span.emit_text(&output.name());

    let screencopies =
        output.with_state_mut(|state| state.screencopies.drain(..).collect::<Vec<_>>());

    for mut screencopy in screencopies {
        assert_eq!(screencopy.output(), output);

        let untransformed_output_size = output.current_mode().expect("output no mode").size;

        let scale = smithay::utils::Scale::from(output.current_scale().fractional_scale());

        if screencopy.with_damage() {
            if render_frame_result.is_empty {
                output.with_state_mut(|state| state.screencopies.push(screencopy));
                continue;
            }

            // Compute damage
            //
            // I have no idea if the damage event is supposed to send rects local to the output or to the
            // region. Sway does the former, Hyprland the latter. Also, no one actually seems to be using the
            // received damage. wf-recorder and wl-mirror have no-op handlers for the damage event.

            let mut damage = match &render_frame_result.primary_element {
                PrimaryPlaneElement::Swapchain(element) => {
                    let swapchain_commit =
                        &mut surface.screencopy_commit_state.primary_plane_swapchain;
                    let damage = element.damage.damage_since(Some(*swapchain_commit));
                    *swapchain_commit = element.damage.current_commit();
                    damage.map(|dmg| {
                        dmg.into_iter()
                            .map(|rect| {
                                rect.to_logical(1, Transform::Normal, &rect.size)
                                    .to_physical(1)
                            })
                            .collect()
                    })
                }
                PrimaryPlaneElement::Element(element) => {
                    // INFO: Is this element guaranteed to be the same size as the
                    // |     output? If not this becomes a
                    // FIXME: offset the damage by the element's location
                    //
                    // also is this even ever reachable?
                    let element_commit = &mut surface.screencopy_commit_state.primary_plane_element;
                    let damage = element.damage_since(scale, Some(*element_commit));
                    *element_commit = element.current_commit();
                    Some(damage)
                }
            }
            .unwrap_or_else(|| {
                // Returning `None` means the previous CommitCounter is too old or damage
                // was reset, so damage the whole output
                DamageSet::from_slice(&[Rectangle::from_size(untransformed_output_size)])
            });

            let cursor_damage = render_frame_result
                .cursor_element
                .map(|cursor| {
                    let damage =
                        cursor.damage_since(scale, Some(surface.screencopy_commit_state.cursor));
                    surface.screencopy_commit_state.cursor = cursor.current_commit();
                    damage
                })
                .unwrap_or_default();

            damage = damage.into_iter().chain(cursor_damage).collect();

            // The primary plane and cursor had no damage but something got rendered,
            // so it must be the cursor moving.
            //
            // We currently have overlay planes disabled, so we don't have to worry about that.
            if damage.is_empty()
                && !render_frame_result.is_empty
                && let Some(cursor_elem) = render_frame_result.cursor_element
            {
                damage = damage
                    .into_iter()
                    .chain([cursor_elem.geometry(scale)])
                    .collect();
            }

            // INFO: Protocol states that `copy_with_damage` should wait until there is
            // |     damage to be copied.
            // |.
            // |     Now, for region screencopies this currently submits the frame if there is
            // |     *any* damage on the output, not just in the region. I've found that
            // |     wf-recorder blocks until the last frame is submitted, and if I don't
            // |     send a submission because its region isn't damaged it will hang.
            // |     I'm fairly certain Sway is doing a similar thing.
            if damage.is_empty() {
                output.with_state_mut(|state| state.screencopies.push(screencopy));
                continue;
            }

            screencopy.damage(&damage);
        }

        let untransformed_size = untransformed_output_size;
        let output_buffer_size = untransformed_size
            .to_logical(1)
            .to_buffer(1, Transform::Normal);

        // A screencopy of an HDR output leaves the compositor as PQ, so every
        // screenshot would be colour-shifted; it takes a detour through an
        // offscreen buffer where the frame can be decoded back to sRGB. An SDR
        // output is already the frame the client asked for: reading it back only
        // to copy it again would cost a frame of memory traffic for nothing.
        let sync_point = if surface.hdr.enabled {
            (|| -> anyhow::Result<Option<SyncPoint>> {
                let mut offscreen: GlesRenderbuffer = renderer.create_buffer(
                    smithay::backend::allocator::Fourcc::Abgr8888,
                    output_buffer_size,
                )?;

                let mut offscreen_fb = renderer.bind(&mut offscreen)?;

                // The blit and the readback below run on one GL context, so they
                // are already ordered; a sync point is for handing a texture to
                // another GPU, and this frame is read back right here.
                let _sync = render_frame_result.blit_frame_result(
                    untransformed_size,
                    Transform::Normal,
                    output.current_scale().fractional_scale(),
                    renderer,
                    &mut offscreen_fb,
                    [Rectangle::from_size(untransformed_size)],
                    if !screencopy.overlay_cursor() {
                        cursor_ids.clone()
                    } else {
                        Vec::new()
                    },
                )?;

                // The frame is PQ/BT.2020 and the client wants sRGB. This is the
                // CPU twin of the capture path's inverse program; a screencopy
                // pays the readback because this renderer cannot sample its own
                // offscreen as a render element.
                let mapping = renderer.copy_framebuffer(
                    &offscreen_fb,
                    Rectangle::<i32, smithay::utils::Buffer>::from_size(output_buffer_size),
                    smithay::backend::allocator::Fourcc::Argb8888,
                )?;
                let mut converted = renderer.map_texture(&mapping)?.to_vec();
                crate::hdr::pq_bgra_to_srgb_in_place(&mut converted, surface.hdr.reference_white);

                let dst_region = screencopy.physical_region();

                if let Ok(mut dmabuf) = dmabuf::get_dmabuf(screencopy.buffer()).cloned() {
                    trace!("Dmabuf screencopy");

                    let format_correct = Some(dmabuf.format().code)
                        == shm_format_to_fourcc(wl_shm::Format::Argb8888);
                    let width_correct = dmabuf.width() == dst_region.size.w as u32;
                    let height_correct = dmabuf.height() == dst_region.size.h as u32;

                    ensure!(
                        format_correct && width_correct && height_correct,
                        "dmabuf screencopy has the wrong format or size"
                    );

                    let texture = renderer.import_memory(
                        &converted,
                        smithay::backend::allocator::Fourcc::Argb8888,
                        output_buffer_size,
                        false,
                    )?;

                    let mut dmabuf_fb = renderer.bind(&mut dmabuf)?;
                    let mut frame =
                        renderer.render(&mut dmabuf_fb, dst_region.size, Transform::Normal)?;
                    let src = Rectangle::<f64, smithay::utils::Buffer>::new(
                        (dst_region.loc.x as f64, dst_region.loc.y as f64).into(),
                        (dst_region.size.w as f64, dst_region.size.h as f64).into(),
                    );
                    let dst =
                        Rectangle::<i32, smithay::utils::Physical>::from_size(dst_region.size);
                    frame.render_texture_from_to(
                        &texture,
                        src,
                        dst,
                        &[dst],
                        &[],
                        Transform::Normal,
                        1.0,
                    )?;
                    let sync_point = frame.finish()?;

                    return Ok(Some(sync_point));
                }

                if !matches!(
                    renderer::buffer_type(screencopy.buffer()),
                    Some(BufferType::Shm)
                ) {
                    bail!("not a shm buffer");
                }

                trace!("Shm screencopy");

                smithay::wayland::shm::with_buffer_contents_mut(
                    &screencopy.buffer().clone(),
                    |shm_ptr, shm_len, buffer_data| {
                        ensure!(
                            buffer_data.format == wl_shm::Format::Argb8888
                                && buffer_data.stride == dst_region.size.w * 4
                                && buffer_data.height == dst_region.size.h
                                && shm_len as i32 == buffer_data.stride * buffer_data.height,
                            "invalid buffer format or size"
                        );

                        let region_w = dst_region.size.w as usize;
                        let region_h = dst_region.size.h as usize;
                        let row_bytes = region_w * 4;
                        let full_stride = output_buffer_size.w as usize * 4;
                        let src_x = dst_region.loc.x as usize * 4;
                        let src_y = dst_region.loc.y as usize;
                        ensure!(
                            converted.len() >= full_stride * (src_y + region_h),
                            "converted frame too short for the capture region"
                        );

                        // SAFETY: `converted` covers the rows and columns the
                        // region touches (checked above), and the destination is
                        // a writable shm mapping of `shm_len == region` bytes.
                        unsafe {
                            for row in 0..region_h {
                                let src =
                                    converted.as_ptr().add((src_y + row) * full_stride + src_x);
                                let dst = shm_ptr.add(row * row_bytes);
                                std::ptr::copy_nonoverlapping(src, dst, row_bytes);
                            }
                        }

                        Ok(None)
                    },
                )
                .unwrap_or_else(|_| unreachable!())
            })()
        } else if let Ok(mut dmabuf) = dmabuf::get_dmabuf(screencopy.buffer()).cloned() {
            trace!("Dmabuf screencopy");

            let format_correct =
                Some(dmabuf.format().code) == shm_format_to_fourcc(wl_shm::Format::Argb8888);
            let width_correct = dmabuf.width() == screencopy.physical_region().size.w as u32;
            let height_correct = dmabuf.height() == screencopy.physical_region().size.h as u32;

            if !(format_correct && width_correct && height_correct) {
                continue;
            }

            (|| -> anyhow::Result<Option<SyncPoint>> {
                if screencopy.physical_region() == Rectangle::from_size(untransformed_output_size) {
                    // Optimization to not have to do an extra blit;
                    // just blit the whole output
                    let mut framebuffer = renderer.bind(&mut dmabuf)?;

                    Ok(Some(render_frame_result.blit_frame_result(
                        screencopy.physical_region().size,
                        Transform::Normal,
                        output.current_scale().fractional_scale(),
                        renderer,
                        &mut framebuffer,
                        [screencopy.physical_region()],
                        if !screencopy.overlay_cursor() {
                            cursor_ids.clone()
                        } else {
                            Vec::new()
                        },
                    )?))
                } else {
                    // `RenderFrameResult::blit_frame_result` doesn't expose a way to
                    // blit from a source rectangle, so blit into another buffer
                    // then blit from that into the dmabuf.

                    let mut offscreen: GlesRenderbuffer = renderer.create_buffer(
                        smithay::backend::allocator::Fourcc::Abgr8888,
                        output_buffer_size,
                    )?;

                    let mut offscreen_fb = renderer.bind(&mut offscreen)?;

                    // TODO: Figure out if this sync point needs waiting
                    let _ = render_frame_result.blit_frame_result(
                        untransformed_output_size,
                        Transform::Normal,
                        output.current_scale().fractional_scale(),
                        renderer,
                        &mut offscreen_fb,
                        [Rectangle::from_size(untransformed_output_size)],
                        if !screencopy.overlay_cursor() {
                            cursor_ids.clone()
                        } else {
                            Vec::new()
                        },
                    )?;

                    let mut dmabuf_fb = renderer.bind(&mut dmabuf)?;

                    let sync_point = renderer.blit(
                        &offscreen_fb,
                        &mut dmabuf_fb,
                        screencopy.physical_region(),
                        Rectangle::from_size(screencopy.physical_region().size),
                        TextureFilter::Linear,
                    )?;

                    Ok(Some(sync_point))
                }
            })()
        } else if !matches!(
            renderer::buffer_type(screencopy.buffer()),
            Some(BufferType::Shm)
        ) {
            Err(anyhow!("not a shm buffer"))
        } else {
            trace!("Shm screencopy");

            let res = smithay::wayland::shm::with_buffer_contents_mut(
                &screencopy.buffer().clone(),
                |shm_ptr, shm_len, buffer_data| {
                    // yoinked from Niri (thanks yall)
                    ensure!(
                        // The buffer prefers pixels in little endian ...
                        buffer_data.format == wl_shm::Format::Argb8888
                            && buffer_data.stride == screencopy.physical_region().size.w * 4
                            && buffer_data.height == screencopy.physical_region().size.h
                            && shm_len as i32 == buffer_data.stride * buffer_data.height,
                        "invalid buffer format or size"
                    );

                    let src_buffer_rect = screencopy.physical_region().to_logical(1).to_buffer(
                        1,
                        Transform::Normal,
                        &screencopy.physical_region().size.to_logical(1),
                    );

                    let mut offscreen: GlesRenderbuffer = renderer.create_buffer(
                        smithay::backend::allocator::Fourcc::Abgr8888,
                        output_buffer_size,
                    )?;

                    let mut framebuffer = renderer.bind(&mut offscreen)?;

                    // Blit the entire output to `offscreen`.
                    // Only the needed region will be copied below
                    let sync_point = render_frame_result.blit_frame_result(
                        untransformed_output_size,
                        Transform::Normal,
                        output.current_scale().fractional_scale(),
                        renderer,
                        &mut framebuffer,
                        [Rectangle::from_size(untransformed_output_size)],
                        if !screencopy.overlay_cursor() {
                            cursor_ids.clone()
                        } else {
                            Vec::new()
                        },
                    )?;

                    // Can someone explain to me why it feels like some things are
                    // arbitrarily `Physical` or `Buffer`
                    let mapping = renderer.copy_framebuffer(
                        &framebuffer,
                        src_buffer_rect,
                        smithay::backend::allocator::Fourcc::Argb8888,
                    )?;

                    let bytes = renderer.map_texture(&mapping)?;

                    ensure!(bytes.len() == shm_len, "mapped buffer has wrong length");

                    // SAFETY: TODO: safety docs
                    unsafe {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), shm_ptr, shm_len);
                    }

                    Ok(Some(sync_point))
                },
            );

            let Ok(res) = res else {
                unreachable!();
            };

            res
        };
        match sync_point {
            Ok(Some(sync_point)) if !sync_point.is_reached() => {
                let Some(sync_fd) = sync_point.export() else {
                    screencopy.submit(false);
                    continue;
                };
                let mut screencopy = Some(screencopy);
                let source = Generic::new(sync_fd, Interest::READ, calloop::Mode::OneShot);
                let res = loop_handle.insert_source(source, move |_, _, _| {
                    let Some(screencopy) = screencopy.take() else {
                        unreachable!("This source is removed after one run");
                    };
                    screencopy.submit(false);
                    trace!("Submitted screencopy");
                    Ok(PostAction::Remove)
                });
                if res.is_err() {
                    error!("Failed to schedule screencopy submission");
                }
            }
            Ok(_) => screencopy.submit(false),
            Err(err) => error!("Failed to submit screencopy: {err}"),
        }
    }
}

fn should_use_drm_device<P: AsRef<Path>>(device_path: P) -> bool {
    if let Ok(var) = std::env::var("PINNACLE_DRM_DEVICES") {
        let device_path = device_path.as_ref();

        return var.split(":").map(Path::new).any(|p| p == device_path);
    }

    true
}

#[cfg(test)]
mod hdr_dump_tests {
    use super::write_ppm;

    #[test]
    fn ppm_header_and_pixels() {
        let dir = std::env::temp_dir();
        let path = dir.join("pinnacle-hdr-dump-test.ppm");
        // 2x1: red, green
        let rgba = [255u8, 0, 0, 255, 0, 255, 0, 255];
        write_ppm(&path, 2, 1, &rgba).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..11], b"P6\n2 1\n255\n");
        assert_eq!(&bytes[11..], &[255, 0, 0, 0, 255, 0]);
        let _ = std::fs::remove_file(path);
    }
}

/// The passthrough decision is where the cursor used to disappear: gating the
/// cursor on "a fullscreen HDR window exists" instead of "this frame is that
/// window alone" dropped the pointer even on frames that were composited anyway.
#[cfg(test)]
mod passthrough_tests {
    use super::PassthroughFrame;

    fn frame(cursor_on_output: bool, top_is_native_full_output_surface: bool) -> PassthroughFrame {
        PassthroughFrame {
            hdr_passthrough: true,
            cursor_on_output,
            top_is_native_full_output_surface,
        }
    }

    /// A hidden cursor, or one on another output, must not cost the direct scanout.
    #[test]
    fn fullscreen_hdr_without_a_cursor_passes_through() {
        assert!(frame(false, true).can_passthrough());
    }

    /// The regression: a visible cursor over fullscreen HDR content has to compose,
    /// and composition is what draws it.
    #[test]
    fn a_cursor_over_the_output_forces_composition() {
        assert!(!frame(true, true).can_passthrough());
    }

    /// Anything that does not fill the output is composited whether or not the
    /// cursor is around.
    #[test]
    fn content_that_does_not_cover_the_output_is_composited() {
        assert!(!frame(false, false).can_passthrough());
    }

    /// An SDR output always goes through the shader; only HDR frames can be
    /// passed through.
    #[test]
    fn sdr_is_never_passthrough() {
        let mut frame = frame(false, true);
        frame.hdr_passthrough = false;
        assert!(!frame.can_passthrough());
    }
}
