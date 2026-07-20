use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
    },
    input::{
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::SERIAL_COUNTER,
};

use crate::state::State;

impl State {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);
                let pressed = event.state() == KeyState::Pressed;

                self.seat.get_keyboard().unwrap().input::<(), _>(
                    self,
                    event.key_code(),
                    event.state(),
                    serial,
                    time,
                    move |state, modifiers, keysym| {
                        // Veracage clipboard shortcuts, user-configurable (defaults
                        // Ctrl+Alt+C = Copy out sandbox->host, Ctrl+Alt+V = Paste in
                        // host->sandbox). NOT plain Ctrl+C/V. Those are the apps'
                        // own copy/paste. Read live from state.binds.
                        if pressed {
                            let sym = keysym.modified_sym();
                            if state.binds.copy_out.as_ref().is_some_and(|b| b.matches(&modifiers, sym)) {
                                crate::clipboard::pull_to_host(state);
                                return FilterResult::Intercept(());
                            }
                            if state.binds.paste_in.as_ref().is_some_and(|b| b.matches(&modifiers, sym)) {
                                crate::clipboard::push_from_host(state);
                                return FilterResult::Intercept(());
                            }
                        }
                        FilterResult::Forward
                    },
                );
            }
            InputEvent::PointerMotion { .. } => {}
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let output = self.space.outputs().next().unwrap();

                let output_geo = self.space.output_geometry(output).unwrap();

                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();

                // Toolbar first, UNLESS a pointer grab is active (an interactive
                // move/resize, or a button held on a sandbox window): during a grab
                // every event must reach smithay so the release ends it, else the
                // grab sticks and the window stays glued to the cursor.
                let grabbed = self.seat.get_pointer().map(|p| p.is_grabbed()).unwrap_or(false);
                if !grabbed {
                    if let Some(tb) = self.toolbar.as_mut() {
                        tb.pointer_moved(pos.x, pos.y);
                        // The strip itself, OR anywhere an open menu dropdown is
                        // capturing the pointer (dropdowns extend below the strip).
                        if tb.contains_y(pos.y) || tb.wants_pointer() {
                            return; // the menu owns this motion
                        }
                    }
                }

                let serial = SERIAL_COUNTER.next_serial();

                let pointer = self.seat.get_pointer().unwrap();

                let under = self.surface_under(pos);

                pointer.motion(
                    self,
                    under,
                    &MotionEvent {
                        location: pos,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerButton { event, .. } => {
                // Toolbar click gating, but NOT while a pointer grab is active
                // (see the motion arm): a release that ends a grab must reach
                // smithay even if the pointer wandered into the strip. Gate on
                // egui's OWN tracked pointer, not smithay's (motion over the strip
                // returns early before updating smithay's pointer, so its location
                // is stale for a strip click).
                let grabbed = self.seat.get_pointer().map(|p| p.is_grabbed()).unwrap_or(false);
                if !grabbed {
                    if let Some(tb) = self.toolbar.as_mut() {
                        tb.pointer_button(event.state() == ButtonState::Pressed);
                        // Strip click, or a click on an open dropdown below it.
                        if tb.over_strip() || tb.wants_pointer() {
                            return; // menu click
                        }
                    }
                }

                let pointer = self.seat.get_pointer().unwrap();
                let keyboard = self.seat.get_keyboard().unwrap();

                let serial = SERIAL_COUNTER.next_serial();

                let button = event.button_code();

                let button_state = event.state();

                if ButtonState::Pressed == button_state && !pointer.is_grabbed() {
                    if let Some((window, _loc)) = self
                        .space
                        .element_under(pointer.current_location())
                        .map(|(w, l)| (w.clone(), l))
                    {
                        self.space.raise_element(&window, true);
                        keyboard.set_focus(
                            self,
                            Some(window.toplevel().unwrap().wl_surface().clone()),
                            serial,
                        );
                        self.space.elements().for_each(|window| {
                            window.toplevel().unwrap().send_pending_configure();
                        });
                    } else {
                        self.space.elements().for_each(|window| {
                            window.set_activated(false);
                            window.toplevel().unwrap().send_pending_configure();
                        });
                        keyboard.set_focus(self, Option::<WlSurface>::None, serial);
                    }
                };

                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: button_state,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerAxis { event, .. } => {
                let source = event.source();

                let horizontal_amount = event
                    .amount(Axis::Horizontal)
                    .unwrap_or_else(|| event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.);
                let vertical_amount = event
                    .amount(Axis::Vertical)
                    .unwrap_or_else(|| event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.);
                let horizontal_amount_discrete = event.amount_v120(Axis::Horizontal);
                let vertical_amount_discrete = event.amount_v120(Axis::Vertical);

                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                if horizontal_amount != 0.0 {
                    frame = frame.value(Axis::Horizontal, horizontal_amount);
                    if let Some(discrete) = horizontal_amount_discrete {
                        frame = frame.v120(Axis::Horizontal, discrete as i32);
                    }
                }
                if vertical_amount != 0.0 {
                    frame = frame.value(Axis::Vertical, vertical_amount);
                    if let Some(discrete) = vertical_amount_discrete {
                        frame = frame.v120(Axis::Vertical, discrete as i32);
                    }
                }

                if source == AxisSource::Finger {
                    if event.amount(Axis::Horizontal) == Some(0.0) {
                        frame = frame.stop(Axis::Horizontal);
                    }
                    if event.amount(Axis::Vertical) == Some(0.0) {
                        frame = frame.stop(Axis::Vertical);
                    }
                }

                let pointer = self.seat.get_pointer().unwrap();
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            _ => {}
        }
    }
}
