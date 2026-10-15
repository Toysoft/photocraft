//! Android tablet gestures that need a little more detail than winit currently exposes.

use egui::{Event, PointerButton, Pos2, RawInput};
use photocraft_engine::prefs::StylusButtonAction;

/// An Android-host gesture expressed independently from Java/JNI, so its translation stays
/// testable with normal Rust unit tests.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum TabletAction {
    /// A two-finger tap should behave exactly like a desktop secondary click.
    SecondaryTap { x: f32, y: f32 },
    /// The S Pen barrel button was held or released while its tip touched the display.
    Barrel { x: f32, y: f32, pressed: bool },
}

/// Rewrites the primary click emitted for a barrel-button touch and appends an atomic secondary
/// click for two fingers. Coordinates are normalized in Java so display density never leaks into
/// the editor's point-based input space.
#[derive(Default)]
pub(crate) struct TabletGestureRouter {
    barrel_down: bool,
    barrel_released: bool,
    touches: Vec<(egui::TouchDeviceId, egui::TouchId, Pos2)>,
    tap_start: Option<f64>,
    tap_center: Option<Pos2>,
    tap_invalid: bool,
    max_touches: usize,
    pending_pointer: Vec<Event>,
    pointer_committed: bool,
    multi_touch: bool,
}

impl TabletGestureRouter {
    pub(crate) fn apply(&mut self, raw: &mut RawInput, actions: impl IntoIterator<Item = TabletAction>, mode: StylusButtonAction) {
        let now = raw.time.unwrap_or(0.0);
        let mut touch_tap = None;
        let mut touch_frame = !self.touches.is_empty();
        let mut finished = false;
        for event in &raw.events {
            let Event::Touch { device_id, id, phase, pos, .. } = event else { continue };
            touch_frame = true;
            match phase {
                egui::TouchPhase::Start => {
                    if self.touches.is_empty() {
                        self.tap_start = Some(now);
                        self.tap_center = None;
                        self.tap_invalid = false;
                        self.pointer_committed = false;
                        self.multi_touch = false;
                        self.max_touches = 0;
                    }
                    self.touches.push((*device_id, *id, *pos));
                    self.max_touches = self.max_touches.max(self.touches.len());
                    if self.touches.len() >= 2 {
                        self.multi_touch = true;
                        if self.touches.len() == 2 {
                            let (_, _, first) = self.touches[0];
                            let (_, _, second) = self.touches[1];
                            self.tap_center = Some(first.lerp(second, 0.5));
                        } else if self.touches.len() > 4 {
                            self.tap_invalid = true;
                        }
                    }
                }
                egui::TouchPhase::Move => {
                    if let Some((_, _, start)) = self.touches.iter().find(|(device, touch, _)| device == device_id && touch == id)
                        && start.distance(*pos) > 25.0
                    {
                        self.tap_invalid = true;
                    }
                }
                egui::TouchPhase::End | egui::TouchPhase::Cancel => {
                    if *phase == egui::TouchPhase::Cancel {
                        self.tap_invalid = true;
                    }
                    self.touches.retain(|(device, touch, _)| device != device_id || touch != id);
                    if self.touches.is_empty() {
                        finished = true;
                        if !self.tap_invalid && self.tap_start.is_some_and(|start| now - start <= 0.35) {
                            touch_tap = Some((self.max_touches, self.tap_center));
                        }
                        self.tap_center = None;
                        self.max_touches = 0;
                    }
                }
            }
        }
        // Reserve a short chord window before forwarding the first finger's emulated mouse
        // press. Otherwise a two-finger context click first starts moving the selection.
        if self.multi_touch {
            self.pending_pointer.clear();
            // During multi-touch gestures (pinch-zoom, pan), suppress primary mouse clicks/drags.
            raw.events.retain(|event| !matches!(event, Event::PointerButton { button: PointerButton::Primary, .. }));
            if let Some(center) = self.tap_center
                && !raw.events.iter().any(|e| matches!(e, Event::PointerMoved(_)))
            {
                raw.events.push(Event::PointerMoved(center));
            }
        } else if touch_frame && !self.pointer_committed {
            let mut retained = Vec::new();
            for event in std::mem::take(&mut raw.events) {
                if matches!(event, Event::PointerButton { button: PointerButton::Primary, .. }) {
                    if self.pending_pointer.len() < 256 {
                        self.pending_pointer.push(event);
                    }
                } else {
                    retained.push(event);
                }
            }
            raw.events = retained;

            if finished || self.tap_invalid || self.tap_start.is_some_and(|start| now - start >= 0.08) {
                let mut events = std::mem::take(&mut self.pending_pointer);
                events.append(&mut raw.events);
                raw.events = events;
                self.pointer_committed = true;
            }
        }
        if finished {
            self.tap_start = None;
            self.multi_touch = false;
            self.pointer_committed = false;
        }
        let mut secondary_taps = Vec::new();
        let mut barrel_press = None;
        let mut barrel_release = None;
        for action in actions {
            match action {
                TabletAction::SecondaryTap { x, y } => secondary_taps.push((x, y)),
                TabletAction::Barrel { x, y, pressed: true } => {
                    self.barrel_down = true;
                    self.barrel_released = false;
                    barrel_press = Some((x, y));
                }
                TabletAction::Barrel { x, y, pressed: false } => {
                    self.barrel_released = self.barrel_down;
                    self.barrel_down = false;
                    barrel_release = Some((x, y));
                }
            }
        }

        let rewrite_primary = mode == StylusButtonAction::ContextMenu && (self.barrel_down || self.barrel_released);
        let mut saw_press = false;
        let mut saw_release = false;
        if rewrite_primary {
            for event in &mut raw.events {
                if let Event::PointerButton { button, pressed, .. } = event
                    && *button == PointerButton::Primary
                {
                    *button = PointerButton::Secondary;
                    saw_press |= *pressed;
                    saw_release |= !*pressed;
                }
            }
        }

        if let Some((x, y)) = barrel_press.filter(|_| mode == StylusButtonAction::ContextMenu && !saw_press) {
            push_button(raw, x, y, true);
        }
        if let Some((x, y)) = barrel_release.filter(|_| mode == StylusButtonAction::ContextMenu && !saw_release) {
            push_button(raw, x, y, false);
        }
        if let Some((x, y)) = barrel_release.filter(|_| mode != StylusButtonAction::ContextMenu) {
            push_primary_release(raw, x, y);
        }
        self.barrel_released = false;

        for (x, y) in secondary_taps {
            push_button(raw, x, y, true);
            push_button(raw, x, y, false);
        }
        if let Some((touches, pos)) = touch_tap {
            if touches == 2 {
                if let Some(pos) = pos {
                    raw.events.push(Event::PointerMoved(pos));
                    for pressed in [true, false] {
                        raw.events.push(Event::PointerButton { pos, button: PointerButton::Secondary, pressed, modifiers: Default::default() });
                    }
                }
            } else if touches == 3 {
                raw.events.push(Event::Key {
                    key: egui::Key::Z,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::COMMAND,
                });
                raw.events.push(Event::Key {
                    key: egui::Key::Z,
                    physical_key: None,
                    pressed: false,
                    repeat: false,
                    modifiers: egui::Modifiers::COMMAND,
                });
            } else if touches == 4 {
                raw.events.push(Event::Key {
                    key: egui::Key::Z,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                });
                raw.events.push(Event::Key {
                    key: egui::Key::Z,
                    physical_key: None,
                    pressed: false,
                    repeat: false,
                    modifiers: egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                });
            }
        }
    }

    pub(crate) fn waiting_for_chord(&self) -> bool {
        self.tap_start.is_some() && !self.pointer_committed && !self.multi_touch
    }
}

fn push_button(raw: &mut RawInput, x: f32, y: f32, pressed: bool) {
    let Some(pos) = normalized_pos(raw, x, y) else { return };
    raw.events.push(Event::PointerMoved(pos));
    raw.events.push(Event::PointerButton { pos, button: PointerButton::Secondary, pressed, modifiers: Default::default() });
}

fn push_primary_release(raw: &mut RawInput, x: f32, y: f32) {
    let Some(pos) = normalized_pos(raw, x, y) else { return };
    raw.events.push(Event::PointerMoved(pos));
    raw.events.push(Event::PointerButton { pos, button: PointerButton::Primary, pressed: false, modifiers: Default::default() });
}

fn normalized_pos(raw: &RawInput, x: f32, y: f32) -> Option<Pos2> {
    let rect = raw.screen_rect?;
    if !(x.is_finite() && y.is_finite()) {
        return None;
    }
    Some(Pos2::new(rect.left() + x.clamp(0.0, 1.0) * rect.width(), rect.top() + y.clamp(0.0, 1.0) * rect.height()))
}

#[cfg(test)]
mod tests {
    use egui::{Rect, pos2, vec2};

    use super::*;

    fn input(events: Vec<Event>) -> RawInput {
        RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 500.0))), events, ..Default::default() }
    }

    #[test]
    fn staggered_two_finger_tap_never_starts_primary_selection_drag() {
        let mut router = TabletGestureRouter::default();
        let ctx = egui::Context::default();
        let touch = |id, phase, x| Event::Touch { device_id: egui::TouchDeviceId(1), id: egui::TouchId(id), phase, pos: pos2(x, 100.0), force: None };
        let button = |pressed| Event::PointerButton { pos: pos2(100.0, 100.0), button: PointerButton::Primary, pressed, modifiers: Default::default() };
        let frames = [
            vec![touch(1, egui::TouchPhase::Start, 100.0), Event::PointerMoved(pos2(100.0, 100.0)), button(true)],
            vec![touch(2, egui::TouchPhase::Start, 200.0)],
            vec![touch(2, egui::TouchPhase::End, 200.0), touch(1, egui::TouchPhase::End, 100.0), button(false), Event::PointerGone],
        ];
        for (frame, events) in frames.into_iter().enumerate() {
            let mut raw = input(events);
            raw.time = Some(1.0 + frame as f64 * 0.02);
            router.apply(&mut raw, [], StylusButtonAction::ContextMenu);
            let has_secondary = raw.events.iter().any(|e| matches!(e, Event::PointerButton { button: PointerButton::Secondary, pressed: true, .. }));
            ctx.begin_pass(raw);
            ctx.input(|i| {
                assert!(!i.pointer.primary_down());
                assert!(!i.pointer.button_pressed(PointerButton::Primary));
            });
            if frame == 2 {
                assert!(has_secondary);
            }
            let mut output = ctx.end_pass();
            output.textures_delta.clear();
        }
    }

    #[test]
    fn single_touch_press_is_forwarded_after_chord_window() {
        let mut router = TabletGestureRouter::default();
        let mut raw = input(vec![
            Event::Touch { device_id: egui::TouchDeviceId(1), id: egui::TouchId(1), phase: egui::TouchPhase::Start, pos: pos2(100.0, 100.0), force: None },
            Event::PointerButton { pos: pos2(100.0, 100.0), button: PointerButton::Primary, pressed: true, modifiers: Default::default() },
        ]);
        raw.time = Some(1.0);
        router.apply(&mut raw, [], StylusButtonAction::ContextMenu);
        assert!(router.waiting_for_chord());
        let mut raw = input(vec![]);
        raw.time = Some(1.09);
        router.apply(&mut raw, [], StylusButtonAction::ContextMenu);
        assert!(raw.events.iter().any(|event| matches!(event, Event::PointerButton { button: PointerButton::Primary, pressed: true, .. })));
        assert!(!router.waiting_for_chord());
    }

    #[test]
    fn native_two_finger_tap_clicks_but_pinch_does_not() {
        let touch = |id, phase, x| Event::Touch { device_id: egui::TouchDeviceId(1), id: egui::TouchId(id), phase, pos: pos2(x, 100.0), force: None };
        for moved in [false, true] {
            let mut router = TabletGestureRouter::default();
            let mut raw = input(vec![touch(1, egui::TouchPhase::Start, 100.0), touch(2, egui::TouchPhase::Start, 200.0)]);
            raw.time = Some(1.0);
            router.apply(&mut raw, [], StylusButtonAction::ContextMenu);
            let mut events = Vec::new();
            if moved {
                events.push(touch(2, egui::TouchPhase::Move, 250.0));
            }
            events.extend([touch(2, egui::TouchPhase::End, 200.0), touch(1, egui::TouchPhase::End, 100.0)]);
            let mut raw = input(events);
            raw.time = Some(1.1);
            router.apply(&mut raw, [], StylusButtonAction::ContextMenu);
            let secondaries = raw.events.iter().filter(|event| matches!(event, Event::PointerButton { button: PointerButton::Secondary, pressed: true, .. })).count();
            assert_eq!(secondaries, if moved { 0 } else { 1 });
        }
    }

    #[test]
    fn native_three_finger_tap_emits_undo() {
        let touch = |id, phase, x| Event::Touch { device_id: egui::TouchDeviceId(1), id: egui::TouchId(id), phase, pos: pos2(x, 100.0), force: None };
        let mut router = TabletGestureRouter::default();
        let mut raw = input(vec![touch(1, egui::TouchPhase::Start, 100.0), touch(2, egui::TouchPhase::Start, 200.0), touch(3, egui::TouchPhase::Start, 300.0)]);
        raw.time = Some(1.0);
        router.apply(&mut raw, [], StylusButtonAction::ContextMenu);
        let mut raw = input(vec![touch(3, egui::TouchPhase::End, 300.0), touch(2, egui::TouchPhase::End, 200.0), touch(1, egui::TouchPhase::End, 100.0)]);
        raw.time = Some(1.1);
        router.apply(&mut raw, [], StylusButtonAction::ContextMenu);
        let undos = raw.events.iter().filter(|event| matches!(event, Event::Key { key: egui::Key::Z, modifiers, .. } if modifiers.command && !modifiers.shift)).count();
        assert_eq!(undos, 2);
    }

    #[test]
    fn native_four_finger_tap_emits_redo() {
        let touch = |id, phase, x| Event::Touch { device_id: egui::TouchDeviceId(1), id: egui::TouchId(id), phase, pos: pos2(x, 100.0), force: None };
        let mut router = TabletGestureRouter::default();
        let mut raw = input(vec![
            touch(1, egui::TouchPhase::Start, 100.0),
            touch(2, egui::TouchPhase::Start, 200.0),
            touch(3, egui::TouchPhase::Start, 300.0),
            touch(4, egui::TouchPhase::Start, 400.0),
        ]);
        raw.time = Some(1.0);
        router.apply(&mut raw, [], StylusButtonAction::ContextMenu);
        let mut raw = input(vec![
            touch(4, egui::TouchPhase::End, 400.0),
            touch(3, egui::TouchPhase::End, 300.0),
            touch(2, egui::TouchPhase::End, 200.0),
            touch(1, egui::TouchPhase::End, 100.0),
        ]);
        raw.time = Some(1.1);
        router.apply(&mut raw, [], StylusButtonAction::ContextMenu);
        let redos = raw.events.iter().filter(|event| matches!(event, Event::Key { key: egui::Key::Z, modifiers, .. } if modifiers.command && modifiers.shift)).count();
        assert_eq!(redos, 2);
    }

    #[test]
    fn barrel_touch_rewrites_primary_down_and_up_to_secondary() {
        let mut router = TabletGestureRouter::default();
        let down = Event::PointerButton { pos: pos2(20.0, 30.0), button: PointerButton::Primary, pressed: true, modifiers: Default::default() };
        let mut raw = input(vec![down]);
        router.apply(&mut raw, [TabletAction::Barrel { x: 0.02, y: 0.06, pressed: true }], StylusButtonAction::ContextMenu);
        assert!(matches!(raw.events.as_slice(), [Event::PointerButton { button: PointerButton::Secondary, pressed: true, .. }]));

        let up = Event::PointerButton { pos: pos2(20.0, 30.0), button: PointerButton::Primary, pressed: false, modifiers: Default::default() };
        let mut raw = input(vec![up]);
        router.apply(&mut raw, [TabletAction::Barrel { x: 0.02, y: 0.06, pressed: false }], StylusButtonAction::ContextMenu);
        assert!(matches!(raw.events.as_slice(), [Event::PointerButton { button: PointerButton::Secondary, pressed: false, .. }]));
    }

    #[test]
    fn temporary_barrel_tool_keeps_primary_input_and_ends_when_released() {
        let mut router = TabletGestureRouter::default();
        let down = Event::PointerButton { pos: pos2(20.0, 30.0), button: PointerButton::Primary, pressed: true, modifiers: Default::default() };
        let mut raw = input(vec![down]);
        router.apply(&mut raw, [TabletAction::Barrel { x: 0.02, y: 0.06, pressed: true }], StylusButtonAction::Eraser);
        assert!(matches!(raw.events.as_slice(), [Event::PointerButton { button: PointerButton::Primary, pressed: true, .. }]));

        let mut raw = input(Vec::new());
        router.apply(&mut raw, [TabletAction::Barrel { x: 0.02, y: 0.06, pressed: false }], StylusButtonAction::Eraser);
        assert!(matches!(raw.events.as_slice(), [Event::PointerMoved(_), Event::PointerButton { button: PointerButton::Primary, pressed: false, .. }]));
    }

    #[test]
    fn two_finger_tap_adds_a_complete_secondary_click_at_the_normalized_point() {
        let mut raw = input(Vec::new());
        TabletGestureRouter::default().apply(&mut raw, [TabletAction::SecondaryTap { x: 0.25, y: 0.8 }], StylusButtonAction::ContextMenu);
        assert!(
            matches!(raw.events.as_slice(), [Event::PointerMoved(pos), Event::PointerButton { pos: down, button: PointerButton::Secondary, pressed: true, .. }, Event::PointerMoved(release_pos), Event::PointerButton { pos: up, button: PointerButton::Secondary, pressed: false, .. }] if *pos == pos2(250.0, 400.0) && *down == *pos && *release_pos == *pos && *up == *pos)
        );
    }

    #[test]
    fn test_pinch_zoom_multi_touch() {
        let mut router = TabletGestureRouter::default();
        let ctx = egui::Context::default();
        let touch = |id, phase, x| Event::Touch { device_id: egui::TouchDeviceId(1), id: egui::TouchId(id), phase, pos: pos2(x, 100.0), force: None };
        let mut raw1 = input(vec![touch(1, egui::TouchPhase::Start, 100.0), Event::PointerMoved(pos2(100.0, 100.0)), Event::PointerButton { pos: pos2(100.0, 100.0), button: PointerButton::Primary, pressed: true, modifiers: Default::default() }]);
        raw1.time = Some(1.0);
        router.apply(&mut raw1, [], StylusButtonAction::ContextMenu);
        ctx.begin_pass(raw1);
        let mut output = ctx.end_pass();
        output.textures_delta.clear();

        let mut raw2 = input(vec![touch(2, egui::TouchPhase::Start, 200.0)]);
        raw2.time = Some(1.02);
        router.apply(&mut raw2, [], StylusButtonAction::ContextMenu);
        ctx.begin_pass(raw2);
        let _multi2 = ctx.input(|i| i.multi_touch());
        let mut output = ctx.end_pass();
        output.textures_delta.clear();

        let mut raw3 = input(vec![
            touch(1, egui::TouchPhase::Move, 80.0),
            touch(2, egui::TouchPhase::Move, 220.0),
            Event::PointerMoved(pos2(80.0, 100.0)),
        ]);
        raw3.time = Some(1.04);
        router.apply(&mut raw3, [], StylusButtonAction::ContextMenu);
        ctx.begin_pass(raw3);
        let multi3 = ctx.input(|i| i.multi_touch());
        let mut output = ctx.end_pass();
        output.textures_delta.clear();

        assert!(multi3.is_some());
        assert!(multi3.unwrap().zoom_delta > 1.0);
    }

    #[test]
    fn simultaneous_two_finger_pinch_zooms() {
        let mut router = TabletGestureRouter::default();
        let ctx = egui::Context::default();
        let touch = |id, phase, x| Event::Touch { device_id: egui::TouchDeviceId(1), id: egui::TouchId(id), phase, pos: pos2(x, 100.0), force: None };
        // Both fingers touch down in frame 1
        let mut raw1 = input(vec![touch(1, egui::TouchPhase::Start, 100.0), touch(2, egui::TouchPhase::Start, 200.0)]);
        raw1.time = Some(1.0);
        router.apply(&mut raw1, [], StylusButtonAction::ContextMenu);
        ctx.begin_pass(raw1);
        let mut output = ctx.end_pass();
        output.textures_delta.clear();

        // Frame 2: fingers pinch outward
        let mut raw2 = input(vec![
            touch(1, egui::TouchPhase::Move, 80.0),
            touch(2, egui::TouchPhase::Move, 220.0),
        ]);
        raw2.time = Some(1.02);
        router.apply(&mut raw2, [], StylusButtonAction::ContextMenu);
        ctx.begin_pass(raw2);
        let _multi2 = ctx.input(|i| i.multi_touch());
        let mut output = ctx.end_pass();
        output.textures_delta.clear();

        // Frame 3: fingers pinch more
        let mut raw3 = input(vec![
            touch(1, egui::TouchPhase::Move, 60.0),
            touch(2, egui::TouchPhase::Move, 240.0),
        ]);
        raw3.time = Some(1.04);
        router.apply(&mut raw3, [], StylusButtonAction::ContextMenu);
        ctx.begin_pass(raw3);
        let multi3 = ctx.input(|i| i.multi_touch());
        let mut output = ctx.end_pass();
        output.textures_delta.clear();

        assert!(multi3.is_some());
        let info = multi3.unwrap();
        assert!(info.zoom_delta > 1.0);
    }

    #[test]
    fn two_finger_pan_navigates() {
        let mut router = TabletGestureRouter::default();
        let ctx = egui::Context::default();
        let touch = |id, phase, x, y| Event::Touch { device_id: egui::TouchDeviceId(1), id: egui::TouchId(id), phase, pos: pos2(x, y), force: None };
        let mut raw1 = input(vec![touch(1, egui::TouchPhase::Start, 100.0, 100.0), touch(2, egui::TouchPhase::Start, 200.0, 100.0)]);
        raw1.time = Some(1.0);
        router.apply(&mut raw1, [], StylusButtonAction::ContextMenu);
        ctx.begin_pass(raw1);
        let mut output = ctx.end_pass();
        output.textures_delta.clear();

        let mut raw2 = input(vec![
            touch(1, egui::TouchPhase::Move, 130.0, 100.0),
            touch(2, egui::TouchPhase::Move, 230.0, 100.0),
        ]);
        raw2.time = Some(1.02);
        router.apply(&mut raw2, [], StylusButtonAction::ContextMenu);
        ctx.begin_pass(raw2);
        let _ = ctx.input(|i| i.multi_touch());
        let mut output = ctx.end_pass();
        output.textures_delta.clear();

        let mut raw3 = input(vec![
            touch(1, egui::TouchPhase::Move, 160.0, 100.0),
            touch(2, egui::TouchPhase::Move, 260.0, 100.0),
        ]);
        raw3.time = Some(1.04);
        router.apply(&mut raw3, [], StylusButtonAction::ContextMenu);
        ctx.begin_pass(raw3);
        let multi3 = ctx.input(|i| i.multi_touch());
        let mut output = ctx.end_pass();
        output.textures_delta.clear();

        assert!(multi3.is_some());
        let info = multi3.unwrap();
        assert_eq!(info.translation_delta.x, 30.0);
        assert_eq!(info.translation_delta.y, 0.0);
    }
}
