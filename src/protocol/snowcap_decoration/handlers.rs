use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

use smithay::{
    wayland::{Dispatch2, GlobalDispatch2},
    reexports::wayland_server::{
        Client, Dispatch, GlobalDispatch, Resource, Weak,
        backend::{ClientId, InvalidId},
        protocol::wl_surface::WlSurface,
    },
    utils::{Point, Serial},
    wayland::{
        compositor::{self, BufferAssignment, SurfaceAttributes},
        shell::xdg::{XdgPopupSurfaceData, XdgShellHandler},
    },
};
use snowcap_protocols::snowcap_decoration_v1::server::{
    snowcap_decoration_manager_v1::{self, SnowcapDecorationManagerV1},
    snowcap_decoration_surface_v1::{self, SnowcapDecorationSurfaceV1},
};
use tracing::warn;

use crate::protocol::snowcap_decoration::{
    DECORATION_SURFACE_ROLE, DecorationSurfaceAttributes, DecorationSurfaceCachedState,
    DecorationSurfaceData, SnowcapDecorationGlobalData, SnowcapDecorationHandler,
    SnowcapDecorationState,
};

/// User data for manager objects (must be a type this crate owns).
#[derive(Debug, Clone, Copy)]
pub struct SnowcapDecorationManagerUserData;

impl<D> GlobalDispatch2<SnowcapDecorationManagerV1, D> for SnowcapDecorationGlobalData
where
    D: Dispatch<SnowcapDecorationManagerV1, SnowcapDecorationManagerUserData>
        + Dispatch<SnowcapDecorationSurfaceV1, SnowcapDecorationSurfaceUserData>
        + SnowcapDecorationHandler
        + XdgShellHandler
        + 'static,
{
    fn bind(
        &self,
        _state: &mut D,
        _handle: &smithay::reexports::wayland_server::DisplayHandle,
        _client: &Client,
        resource: smithay::reexports::wayland_server::New<SnowcapDecorationManagerV1>,
        data_init: &mut smithay::reexports::wayland_server::DataInit<'_, D>,
    ) {
        data_init.init(resource, SnowcapDecorationManagerUserData);
    }

    fn can_view(&self, client: &Client) -> bool {
        (self.filter)(client)
    }
}

impl<D> Dispatch2<SnowcapDecorationManagerV1, D> for SnowcapDecorationManagerUserData
where
    D: Dispatch<SnowcapDecorationSurfaceV1, SnowcapDecorationSurfaceUserData>
        + SnowcapDecorationHandler
        + XdgShellHandler
        + 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &SnowcapDecorationManagerV1,
        request: <SnowcapDecorationManagerV1 as smithay::reexports::wayland_server::Resource>::Request,
        _dhandle: &smithay::reexports::wayland_server::DisplayHandle,
        data_init: &mut smithay::reexports::wayland_server::DataInit<'_, D>,
    ) {
        match request {
            snowcap_decoration_manager_v1::Request::GetDecorationSurface {
                id,
                surface,
                toplevel,
            } => {
                if compositor::give_role(&surface, DECORATION_SURFACE_ROLE).is_err() {
                    resource.post_error(
                        snowcap_decoration_manager_v1::Error::Role,
                        "surface already has a role",
                    );
                    return;
                }

                let id: SnowcapDecorationSurfaceV1 = data_init.init(
                    id,
                    SnowcapDecorationSurfaceUserData {
                        decoration_data: state.decoration_state().clone(),
                        wl_surface: surface.downgrade(),
                        alive_tracker: AtomicBool::new(true),
                    },
                );

                let initial = compositor::with_states(&surface, |states| {
                    let inserted = states.data_map.insert_if_missing_threadsafe(|| {
                        Mutex::new(DecorationSurfaceAttributes::new(id.clone()))
                    });

                    if !inserted {
                        let mut attrs = states
                            .data_map
                            .get::<Mutex<DecorationSurfaceAttributes>>()
                            .unwrap()
                            .lock()
                            .unwrap();
                        attrs.surface = id.clone();
                    }

                    inserted
                });

                if initial {
                    compositor::add_pre_commit_hook::<D, _>(&surface, |_state, _dh, surface| {
                        compositor::with_states(surface, |states| {
                            let mut role = states
                                .data_map
                                .get::<DecorationSurfaceData>()
                                .unwrap()
                                .lock()
                                .unwrap();
                            let mut guard_deco =
                                states.cached_state.get::<DecorationSurfaceCachedState>();
                            let pending = guard_deco.pending();

                            let had_buffer_before = pending.last_acked.is_some();

                            let mut guard_surface = states.cached_state.get::<SurfaceAttributes>();

                            let has_buffer = match guard_surface.pending().buffer.as_ref() {
                                Some(BufferAssignment::NewBuffer(_)) => true,
                                Some(BufferAssignment::Removed) => false,
                                None => had_buffer_before,
                            };

                            let got_unmapped = had_buffer_before && !has_buffer;

                            if has_buffer {
                                let Some(last_acked) = role.last_acked.clone() else {
                                    // TODO: post error here or something
                                    warn!(
                                        "decoration attached buffer before acking initial configure"
                                    );
                                    return;
                                };

                                pending.last_acked = Some(last_acked);
                            } else {
                                pending.last_acked = None;
                            }

                            if got_unmapped {
                                let pending_configures =
                                    std::mem::take(&mut role.pending_configures);
                                role.reset();
                                role.pending_configures = pending_configures;
                                *guard_deco.pending() = Default::default();
                            }
                        })
                    });
                }

                let handle = super::DecorationSurface {
                    wl_surface: surface,
                    decoration_surface: id,
                };

                state
                    .decoration_state()
                    .known_decorations
                    .lock()
                    .unwrap()
                    .push(handle.clone());

                state.new_decoration_surface(handle, toplevel);
            }
            snowcap_decoration_manager_v1::Request::Destroy => {
                // Handled by destructor
            }
            _ => unreachable!(),
        }
    }
}

#[derive(Debug)]
pub struct SnowcapDecorationSurfaceUserData {
    decoration_data: SnowcapDecorationState,
    wl_surface: Weak<WlSurface>,
    /// `true` if alive, `false` if dead
    pub(super) alive_tracker: AtomicBool,
}

impl<D> Dispatch2<SnowcapDecorationSurfaceV1, D> for SnowcapDecorationSurfaceUserData
where
    D: SnowcapDecorationHandler + XdgShellHandler + 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        deco_surface: &SnowcapDecorationSurfaceV1,
        request: <SnowcapDecorationSurfaceV1 as Resource>::Request,
        _dhandle: &smithay::reexports::wayland_server::DisplayHandle,
        _data_init: &mut smithay::reexports::wayland_server::DataInit<'_, D>,
    ) {
        match request {
            snowcap_decoration_surface_v1::Request::SetLocation { x, y } => {
                let _ = with_surface_pending_state(deco_surface, |data| {
                    data.location = Point::new(x, y);
                });
            }
            snowcap_decoration_surface_v1::Request::SetBounds {
                left,
                right,
                top,
                bottom,
            } => {
                let _ = with_surface_pending_state(deco_surface, |data| {
                    data.bounds.left = left;
                    data.bounds.right = right;
                    data.bounds.top = top;
                    data.bounds.bottom = bottom;
                });

                if let Some(deco) = {
                    state
                        .decoration_state()
                        .known_decorations
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|deco| deco.decoration_surface() == deco_surface)
                        .cloned()
                } {
                    state.bounds_changed(deco);
                }
            }
            snowcap_decoration_surface_v1::Request::SetZIndex { z_index } => {
                let _ = with_surface_pending_state(deco_surface, |data| {
                    data.z_index = z_index;
                });
            }
            snowcap_decoration_surface_v1::Request::GetPopup { popup } => {
                let Ok(parent_surface) = self.wl_surface.upgrade() else {
                    return;
                };

                let Some(popup) = state.xdg_shell_state().get_popup(&popup) else {
                    warn!("client called decoration_surface::get_popup with a dead popup");
                    return;
                };

                compositor::with_states(popup.wl_surface(), move |states| {
                    states
                        .data_map
                        .get::<XdgPopupSurfaceData>()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .parent = Some(parent_surface);
                });

                let deco_surface = state
                    .decoration_state()
                    .known_decorations
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|deco| deco.decoration_surface() == deco_surface)
                    .cloned();

                if let Some(deco_surface) = deco_surface {
                    SnowcapDecorationHandler::new_popup(state, deco_surface, popup);
                }
            }
            snowcap_decoration_surface_v1::Request::AckConfigure { serial } => {
                let Ok(surface) = self.wl_surface.upgrade() else {
                    return;
                };

                let serial = Serial::from(serial);

                let found_configure = compositor::with_states(&surface, |states| {
                    states
                        .data_map
                        .get::<DecorationSurfaceData>()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .ack_configure(serial)
                });

                let _configure = match found_configure {
                    Some(configure) => configure,
                    None => {
                        // TODO: post error
                        return;
                    }
                };

                // TODO: Handler::ack_configure
            }
            snowcap_decoration_surface_v1::Request::Destroy => (),
            _ => panic!(),
        }
    }

    fn destroyed(
        &self,
        state: &mut D,
        _client: ClientId,
        resource: &SnowcapDecorationSurfaceV1,
    ) {
        self.alive_tracker.store(false, Ordering::Release);

        let mut decorations = self.decoration_data.known_decorations.lock().unwrap();
        if let Some(index) = decorations
            .iter()
            .position(|deco| deco.decoration_surface.id() == resource.id())
        {
            let deco = decorations.remove(index);
            drop(decorations);
            let surface = deco.wl_surface().clone();
            state.decoration_destroyed(deco);
            compositor::with_states(&surface, |states| {
                let mut attrs = states
                    .data_map
                    .get::<Mutex<DecorationSurfaceAttributes>>()
                    .unwrap()
                    .lock()
                    .unwrap();
                attrs.reset();

                let mut guard = states.cached_state.get::<DecorationSurfaceCachedState>();
                *guard.pending() = Default::default();
                *guard.current() = Default::default();
            })
        }
    }
}

fn with_surface_pending_state<F, T>(
    decoration_surface: &SnowcapDecorationSurfaceV1,
    f: F,
) -> Result<T, InvalidId>
where
    F: FnOnce(&mut DecorationSurfaceCachedState) -> T,
{
    let data = decoration_surface
        .data::<SnowcapDecorationSurfaceUserData>()
        .unwrap();
    let surface = data.wl_surface.upgrade()?;
    Ok(compositor::with_states(&surface, |states| {
        f(states
            .cached_state
            .get::<DecorationSurfaceCachedState>()
            .pending())
    }))
}
