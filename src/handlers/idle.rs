use smithay::{
    delegate_idle_inhibit, delegate_idle_notify,
    desktop::utils::surface_primary_scanout_output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::IsAlive,
    wayland::{
        compositor,
        idle_inhibit::IdleInhibitHandler,
        idle_notify::{IdleNotifierHandler, IdleNotifierState},
    },
};
use smithay::reexports::wayland_server::Resource;

use tracing::{error, info};
use crate::state::{Pinnacle, State};

impl IdleNotifierHandler for State {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.pinnacle.idle_notifier_state
    }
}
delegate_idle_notify!(State);

impl IdleInhibitHandler for State {
    fn inhibit(&mut self, surface: WlSurface) {
        info!(
            "[IDLE-INHIBIT] New inhibitor CREATED by client for surface {:?}.",
            surface.id()
        );
        self.pinnacle.idle_inhibiting_surfaces.insert(surface);
        self.pinnacle.idle_notifier_state.set_is_inhibited(true);
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        info!(
            "[IDLE-INHIBIT] Inhibitor DESTROYED by client for surface {:?}.",
            surface.id()
        );
        self.pinnacle.idle_inhibiting_surfaces.remove(&surface);
        self.pinnacle.refresh_idle_inhibit();
    }
}
delegate_idle_inhibit!(State);

impl Pinnacle {
    pub fn refresh_idle_inhibit(&mut self) {
        let _span = tracy_client::span!("Pinnacle::refresh_idle_inhibit");

        self.idle_inhibiting_surfaces.retain(|s| s.alive());

        let is_inhibited = self.idle_inhibiting_surfaces.iter().any(|surface| {
            let is_scanned_out = compositor::with_states(surface, |states| {
                // Check if the surface is being scanned out.
                let scanout_output = surface_primary_scanout_output(surface, states);
                
                // --- ADD THIS LOG ---
                // For each surface, log whether it passed the scanout check.
                if let Some(output) = scanout_output {
                    info!(
                        "[IDLE-INHIBIT-REFRESH]   -> Surface {:?} PASSED scanout check on output '{}'.",
                        surface.id(),
                        output.name()
                    );
                    true
                } else {
                    info!(
                        "[IDLE-INHIBIT-REFRESH]   -> Surface {:?} FAILED scanout check.",
                        surface.id()
                    );
                    false
                }
            });
            is_scanned_out
        });

        self.idle_notifier_state.set_is_inhibited(is_inhibited);
    }
}
