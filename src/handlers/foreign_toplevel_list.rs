use smithay::{
    wayland::foreign_toplevel_list::{ForeignToplevelListHandler, ForeignToplevelListState},
};

use crate::state::State;

impl ForeignToplevelListHandler for State {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.pinnacle.foreign_toplevel_list_state
    }
}
