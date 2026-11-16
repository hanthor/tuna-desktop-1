//! Everything around the login dialog's prompt (#1164): the panel's
//! keyboard indicator and quick settings (`input.rs`, `quick.rs`), the
//! banner and logo (`branding.rs`), the accessibility menu and its
//! effects (`a11y.rs`), and the prompt's on-screen keyboard, over the
//! window's content.

use std::rc::Rc;

use gtk4 as gtk;
use gtk4::prelude::*;
use tuna_auth_prompt::entry::PromptEntry;
use tuna_osk::prompt::PromptKeyboard;

use crate::branding::Branding;
use crate::control::Control;
use crate::input::InputSources;
use crate::quick::QuickSettings;

pub struct Chrome {
    /// Held for the process: the compositor's control socket and the
    /// prompt's keyboard live as long as the login screen.
    _control: Rc<Control>,
    pub input: Rc<InputSources>,
    pub quick: Rc<QuickSettings>,
    pub branding: Rc<Branding>,
    _keyboard: Rc<PromptKeyboard>,
    /// The accessibility menu button (the bottom-right group).
    pub a11y: gtk::MenuButton,
}

impl Chrome {
    /// `overlay` is the window's root over `content`; the on-screen
    /// keyboard and the end-session dialog sit on it.
    pub fn new(
        window: &gtk::Window,
        overlay: &gtk::Overlay,
        content: &gtk::Widget,
        entry: &Rc<PromptEntry>,
    ) -> Self {
        let control = Control::connect();
        let input = InputSources::new(control.clone());
        let quick = QuickSettings::new(overlay, content);
        let branding = Branding::new();
        // GNOME's keyboard box over the login dialog: it opens when the
        // prompt takes the focus and types into that prompt alone
        // (`_onKeyFocusChanged`), as on the lock screen.
        let prompt = Rc::downgrade(entry);
        let keyboard = PromptKeyboard::new(
            &input.current_id(),
            None,
            Rc::new(move || prompt.upgrade().map(|e| e.editable())),
            false,
        );
        overlay.add_overlay(keyboard.widget());
        for widget in [
            entry.password.upcast_ref::<gtk::Widget>(),
            entry.text.upcast_ref(),
        ] {
            let focus = gtk::EventControllerFocus::new();
            let enter = Rc::downgrade(&keyboard);
            focus.connect_enter(move |_| {
                if let Some(keyboard) = enter.upgrade() {
                    keyboard.prompt_focus(true);
                }
            });
            let leave = Rc::downgrade(&keyboard);
            focus.connect_leave(move |_| {
                if let Some(keyboard) = leave.upgrade() {
                    keyboard.prompt_focus(false);
                }
            });
            widget.add_controller(focus);
        }
        {
            let keyboard = Rc::downgrade(&keyboard);
            input.connect_changed(move |id| {
                if let Some(keyboard) = keyboard.upgrade() {
                    keyboard.set_group(id);
                }
            });
        }
        crate::a11y::follow(window, &control, &keyboard);
        Self {
            _control: control,
            input,
            quick,
            branding,
            _keyboard: keyboard,
            a11y: crate::a11y::menu_button(),
        }
    }

    /// The panel's right side (the `gdm` mode's `keyboard`,
    /// `quickSettings`).
    pub fn panel_end(&self) -> gtk::Box {
        let end = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        end.append(&self.input.button);
        end.append(&self.quick.button);
        end
    }
}
