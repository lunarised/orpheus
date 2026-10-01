use crate::state::{AppState, UiMode};
use std::time::Instant;

const LONG_PRESS_DURATION: f64 = 0.4; // seconds

/// Button identifiers for the two physical buttons.
#[derive(Clone, Copy, PartialEq)]
pub enum Button {
    Btn1,
    Btn2,
}

/// Tracks the press state of both buttons.
pub struct ButtonHandler {
    btn1_press_time: Option<Instant>,
    btn2_press_time: Option<Instant>,
    both_triggered: bool,
}

impl ButtonHandler {
    pub fn new() -> Self {
        Self {
            btn1_press_time: None,
            btn2_press_time: None,
            both_triggered: false,
        }
    }

    /// Discard any partial gesture, used when a press only wakes the screensaver.
    pub fn cancel_all(&mut self) {
        self.btn1_press_time = None;
        self.btn2_press_time = None;
        self.both_triggered = false;
    }

    /// Call when a button is pressed down.
    pub fn on_press(&mut self, button: Button) {
        self.on_press_at(button, Instant::now());
    }

    fn on_press_at(&mut self, button: Button, now: Instant) {
        match button {
            Button::Btn1 if self.btn1_press_time.is_none() => {
                self.btn1_press_time = Some(now);
            }
            Button::Btn2 if self.btn2_press_time.is_none() => {
                self.btn2_press_time = Some(now);
            }
            _ => {}
        }
    }

    /// Call when a button is released. Returns the action to perform.
    pub fn on_release(&mut self, button: Button) -> ButtonAction {
        self.on_release_at(button, Instant::now())
    }

    fn on_release_at(&mut self, button: Button, now: Instant) -> ButtonAction {
        // If both-buttons combo already fired, consume the release
        if self.both_triggered {
            match button {
                Button::Btn1 => self.btn1_press_time = None,
                Button::Btn2 => self.btn2_press_time = None,
            }
            // Reset flag once both are released
            if self.btn1_press_time.is_none() && self.btn2_press_time.is_none() {
                self.both_triggered = false;
            }
            return ButtonAction::None;
        }

        match button {
            Button::Btn1 => {
                if let Some(press_time) = self.btn1_press_time.take() {
                    let duration = now.duration_since(press_time).as_secs_f64();
                    if duration >= LONG_PRESS_DURATION {
                        return ButtonAction::Button1Long;
                    } else {
                        return ButtonAction::Button1Short;
                    }
                }
            }
            Button::Btn2 => {
                if let Some(press_time) = self.btn2_press_time.take() {
                    let duration = now.duration_since(press_time).as_secs_f64();
                    if duration >= LONG_PRESS_DURATION {
                        return ButtonAction::Button2Long;
                    } else {
                        return ButtonAction::Button2Short;
                    }
                }
            }
        }

        ButtonAction::None
    }

    /// Call each frame to detect both buttons held simultaneously.
    pub fn check_both_held(&mut self) -> ButtonAction {
        self.check_both_held_at(Instant::now())
    }

    fn check_both_held_at(&mut self, now: Instant) -> ButtonAction {
        if self.both_triggered {
            return ButtonAction::None;
        }

        if let (Some(t1), Some(t2)) = (self.btn1_press_time, self.btn2_press_time) {
            let later = t1.max(t2);
            if now.duration_since(later).as_secs_f64() >= LONG_PRESS_DURATION {
                self.both_triggered = true;
                return ButtonAction::BothLong;
            }
        }

        ButtonAction::None
    }
}

/// Actions that result from button interactions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ButtonAction {
    None,
    Button1Short, // toggle play/pause
    Button1Long,  // previous track
    Button2Short, // skip track (next)
    Button2Long,  // (unassigned)
    BothLong,     // (unassigned)
}

/// Apply a button action to the app state.
/// Behavior depends on the current UI mode.
pub fn apply_action(action: ButtonAction, state: &mut AppState) {
    match state.ui_mode {
        UiMode::NowPlaying => apply_action_now_playing(action, state),
        UiMode::PlaylistPicker => apply_action_picker(action, state),
        UiMode::MainMenu => apply_action_menu(action, state),
        UiMode::SettingEditor(_) => apply_action_setting_editor(action, state),
        UiMode::Diagnostics => apply_action_diagnostics(action, state),
        UiMode::GuestQr => {
            if action != ButtonAction::None {
                state.leave_submenu();
            }
        }
        UiMode::Screensaver => {
            if action != ButtonAction::None {
                state.wake_screensaver();
            }
        }
        UiMode::Morning => {
            if action != ButtonAction::None {
                state.dismiss_morning_mode();
            }
        }
    }
}

/// Button actions in NowPlaying mode.
fn apply_action_now_playing(action: ButtonAction, state: &mut AppState) {
    match action {
        ButtonAction::Button1Short => {
            state.toggle_play_pause();
        }
        ButtonAction::Button1Long => {
            state.previous_track();
        }
        ButtonAction::Button2Short => {
            state.skip_track();
        }
        ButtonAction::Button2Long => {
            // Unused
        }
        ButtonAction::BothLong => {
            state.enter_menu();
        }
        ButtonAction::None => {}
    }
}

/// Button actions in PlaylistPicker mode.
fn apply_action_picker(action: ButtonAction, state: &mut AppState) {
    match action {
        ButtonAction::Button1Short => state.picker_scroll(),
        ButtonAction::Button1Long => state.picker_previous(),
        ButtonAction::Button2Short | ButtonAction::Button2Long => {
            // Select current playlist
            state.picker_select();
        }
        ButtonAction::BothLong => {
            // Exit picker without selecting
            state.exit_picker();
        }
        ButtonAction::None => {}
    }
}

fn apply_action_menu(action: ButtonAction, state: &mut AppState) {
    match action {
        ButtonAction::Button1Short => state.menu_next(),
        ButtonAction::Button1Long => state.menu_previous(),
        ButtonAction::Button2Short | ButtonAction::Button2Long => state.menu_select(),
        ButtonAction::BothLong => state.exit_menu(),
        ButtonAction::None => {}
    }
}

fn apply_action_setting_editor(action: ButtonAction, state: &mut AppState) {
    match action {
        ButtonAction::Button1Short => state.adjust_setting(1),
        ButtonAction::Button1Long => state.adjust_setting(-1),
        ButtonAction::Button2Short | ButtonAction::Button2Long | ButtonAction::BothLong => {
            state.leave_submenu();
        }
        ButtonAction::None => {}
    }
}

fn apply_action_diagnostics(action: ButtonAction, state: &mut AppState) {
    match action {
        ButtonAction::Button2Short | ButtonAction::Button2Long | ButtonAction::BothLong => {
            state.leave_submenu();
        }
        ButtonAction::Button1Short | ButtonAction::Button1Long | ButtonAction::None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn distinguishes_short_and_long_presses_without_sleeping() {
        let start = Instant::now();
        let mut handler = ButtonHandler::new();

        handler.on_press_at(Button::Btn1, start);
        assert_eq!(
            handler.on_release_at(Button::Btn1, start + Duration::from_millis(399)),
            ButtonAction::Button1Short
        );

        handler.on_press_at(Button::Btn2, start);
        assert_eq!(
            handler.on_release_at(Button::Btn2, start + Duration::from_millis(400)),
            ButtonAction::Button2Long
        );
    }

    #[test]
    fn both_button_hold_fires_once_and_consumes_releases() {
        let start = Instant::now();
        let mut handler = ButtonHandler::new();

        handler.on_press_at(Button::Btn1, start);
        handler.on_press_at(Button::Btn2, start + Duration::from_millis(50));

        assert_eq!(
            handler.check_both_held_at(start + Duration::from_millis(449)),
            ButtonAction::None
        );
        assert_eq!(
            handler.check_both_held_at(start + Duration::from_millis(450)),
            ButtonAction::BothLong
        );
        assert_eq!(
            handler.check_both_held_at(start + Duration::from_secs(1)),
            ButtonAction::None
        );
        assert_eq!(
            handler.on_release_at(Button::Btn1, start + Duration::from_secs(1)),
            ButtonAction::None
        );
        assert_eq!(
            handler.on_release_at(Button::Btn2, start + Duration::from_secs(1)),
            ButtonAction::None
        );

        handler.on_press_at(Button::Btn1, start + Duration::from_secs(2));
        assert_eq!(
            handler.on_release_at(Button::Btn1, start + Duration::from_millis(2100)),
            ButtonAction::Button1Short
        );
    }
}
