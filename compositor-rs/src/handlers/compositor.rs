use crate::{State, grabs::resize_grab, state::ClientState};
use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    reexports::wayland_server::{
        Client,
        protocol::{wl_buffer, wl_surface::WlSurface},
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes,
            get_parent, is_sync_subsurface, with_states,
        },
        shm::{ShmHandler, ShmState},
    },
};

use super::xdg_shell;

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self
                .space
                .elements()
                .find(|w| w.toplevel().map(|t| t.wl_surface() == &root).unwrap_or(false))
            {
                window.on_commit();
            }
        };

        // Drag icon: clients position the icon relative to the cursor by
        // committing buffer offsets (wl_surface.attach dx/dy). Accumulate them,
        // as anvil does. Without this the icon sits at the cursor's top-right
        // instead of where the client placed it.
        if matches!(&self.dnd_icon, Some(icon) if &icon.surface == surface) {
            let dnd_icon = self.dnd_icon.as_mut().unwrap();
            with_states(&dnd_icon.surface, |states| {
                let buffer_delta = states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .current()
                    .buffer_delta
                    .take()
                    .unwrap_or_default();
                // Client-controlled delta: saturate rather than wrap/panic on
                // an adversarial accumulation (only mis-positions the ghost).
                dnd_icon.offset = smithay::utils::Point::from((
                    dnd_icon.offset.x.saturating_add(buffer_delta.x),
                    dnd_icon.offset.y.saturating_add(buffer_delta.y),
                ));
            });
        }

        xdg_shell::handle_commit(&mut self.popups, &self.space, surface);
        resize_grab::handle_commit(&mut self.space, surface);
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}
