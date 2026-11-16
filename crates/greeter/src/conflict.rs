//! A login whose user already has a session (gnome-shell 51
//! `loginDialog.js` `_onSessionOpened`, `_findConflictingSession`,
//! `_showConflictingSessionDialog`; `conflictingSessionDialog.js`).
//!
//! When GDM opens the session, logind is asked for another graphical,
//! running session of the same user. None: the session starts. One: the
//! "Session Already Running" dialog offers Cancel (back to the user list)
//! and Force Stop (`Greeter.StopConflictingSession`; once logind reports
//! the old session removed, the new one starts). Unanswered for 60 s,
//! the dialog closes with "Login Attempt Timed Out" and the prompt
//! resets.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk4 as gtk;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;

use crate::gdm::Call;
use crate::logind;
use crate::{trace, Greeter};

/// loginDialog.js `_CONFLICTING_SESSION_DIALOG_TIMEOUT`.
const DIALOG_TIMEOUT_SECS: u32 = 60;

/// conflictingSessionDialog.js's text for the four local/remote cases.
pub fn banner_text(greeter_remote: bool, conflicting_remote: bool, user: &str) -> String {
    match (greeter_remote, conflicting_remote) {
        (true, true) => format!("Remote login is not possible because a remote session is already running for {user}. To login remotely, you must log out from the remote session or force stop it."),
        (false, true) => format!("Login is not possible because a remote session is already running for {user}. To login, you must log out from the remote session or force stop it."),
        (true, false) => format!("Remote login is not possible because a local session is already running for {user}. To login remotely, you must log out from the local session or force stop it."),
        (false, false) => format!("Login is not possible because a session is already running for {user}. To login, you must log out from the session or force stop it."),
    }
}

/// The dialog, over the login screen.
pub struct Conflict {
    pub toasts: adw::ToastOverlay,
    content: gtk::Widget,
    dialog: gtk::Box,
    description: gtk::Label,
    cancel: gtk::Button,
    force: gtk::Button,
    spinner: gtk::Spinner,
    /// The open dialog: the GDM service to start and the conflicting
    /// session's id.
    open: RefCell<Option<(String, String)>>,
    generation: Cell<u64>,
}

impl Conflict {
    /// Wrap the login screen's `content`.
    pub fn new(content: &impl IsA<gtk::Widget>) -> Rc<Self> {
        let title = gtk::Label::new(Some("Session Already Running"));
        title.add_css_class("conflicting-session-dialog-title");
        title.add_css_class("title-2");
        let description = gtk::Label::new(None);
        description.add_css_class("conflicting-session-dialog-desc");
        description.set_wrap(true);
        description.set_justify(gtk::Justification::Center);
        let warning = gtk::Label::new(Some(
            "Force stopping will quit any running apps and processes, and could result in data loss",
        ));
        warning.add_css_class("conflicting-session-dialog-desc-warning");
        warning.set_wrap(true);
        warning.set_justify(gtk::Justification::Center);
        let cancel = gtk::Button::with_label("Cancel");
        cancel.add_css_class("suggested-action");
        let force = gtk::Button::with_label("Force Stop");
        force.add_css_class("destructive-action");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        buttons.set_homogeneous(true);
        buttons.append(&cancel);
        buttons.append(&force);
        let spinner = gtk::Spinner::new();
        spinner.set_visible(false);
        let dialog = gtk::Box::new(gtk::Orientation::Vertical, 12);
        dialog.add_css_class("conflicting-session-dialog");
        dialog.add_css_class("modal-dialog");
        dialog.set_halign(gtk::Align::Center);
        dialog.set_valign(gtk::Align::Center);
        dialog.set_size_request(420, -1);
        dialog.append(&title);
        dialog.append(&description);
        dialog.append(&warning);
        dialog.append(&spinner);
        dialog.append(&buttons);
        dialog.set_visible(false);
        dialog.set_accessible_role(gtk::AccessibleRole::AlertDialog);
        dialog.update_property(&[gtk::accessible::Property::Label("Session Already Running")]);
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(content));
        overlay.add_overlay(&dialog);
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&overlay));
        Rc::new(Self {
            toasts,
            content: content.as_ref().clone().upcast(),
            dialog,
            description,
            cancel,
            force,
            spinner,
            open: RefCell::default(),
            generation: Cell::new(0),
        })
    }

    pub fn is_open(&self) -> bool {
        self.open.borrow().is_some()
    }

    fn close(&self) -> Option<(String, String)> {
        self.generation.set(self.generation.get() + 1);
        self.dialog.set_visible(false);
        self.content.set_sensitive(true);
        self.spinner.set_visible(false);
        self.spinner.set_spinning(false);
        self.open.borrow_mut().take()
    }
}

impl Greeter {
    /// Hook the dialog's buttons up.
    pub(crate) fn wire_conflict(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.conflict.cancel.connect_clicked(move |_| {
            if let Some(greeter) = weak.upgrade() {
                greeter.cancel_conflict();
            }
        });
        let weak = Rc::downgrade(self);
        self.conflict.force.connect_clicked(move |_| {
            let Some(greeter) = weak.upgrade() else {
                return;
            };
            trace("greeter conflict force-stop");
            let c = &greeter.conflict;
            c.cancel.set_sensitive(false);
            c.force.set_sensitive(false);
            c.spinner.set_visible(true);
            c.spinner.set_spinning(true);
            greeter.send(Call::StopConflictingSession);
        });
    }

    /// The dialog's Cancel (also Escape): back to the user list.
    pub(crate) fn cancel_conflict(self: &Rc<Self>) {
        if self.conflict.close().is_some() {
            trace("greeter conflict cancel");
            self.abandon_opened();
        }
    }

    /// authPrompt.js `reset()` after a session opened: the verifier's
    /// `reset()` cancels at GDM (which closes the opened session), then
    /// the user list.
    fn abandon_opened(self: &Rc<Self>) {
        self.send(Call::Cancel);
        self.show_users();
    }

    /// `_onSessionOpened`.
    pub(crate) fn session_opened(self: &Rc<Self>, service: String, session_id: String) {
        trace(&format!(
            "greeter session-opened service={service} session={session_id}"
        ));
        let system = self.system.borrow().clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let found = match (&system, session_id.is_empty()) {
                (Some(system), false) => logind::conflicting_session(system, &session_id).await,
                _ => Ok(None),
            };
            let Some(greeter) = weak.upgrade() else {
                return;
            };
            match found {
                Ok(None) => greeter.start_session(&service),
                Ok(Some(conflicting)) => {
                    let remote = match &system {
                        Some(system) => match logind::own_session_path(system).await {
                            Some(path) => {
                                logind::session(system, &path).await.is_ok_and(|s| s.remote)
                            }
                            None => false,
                        },
                        None => false,
                    };
                    greeter.show_conflict(service, conflicting, remote);
                }
                Err(error) => {
                    eprintln!(
                        "tuna-greeter: Failed to start session '{session_id}': {}",
                        error.message()
                    );
                    greeter.abandon_opened();
                }
            }
        });
    }

    /// `_startSession`.
    pub(crate) fn start_session(self: &Rc<Self>, service: &str) {
        self.send(Call::StartSessionWhenReady {
            service: service.to_owned(),
        });
    }

    fn show_conflict(self: &Rc<Self>, service: String, session: logind::Session, remote: bool) {
        trace(&format!(
            "greeter conflict user={} session={} remote={}",
            session.name, session.id, session.remote
        ));
        let c = &self.conflict;
        c.description
            .set_label(&banner_text(remote, session.remote, &session.name));
        c.cancel.set_sensitive(true);
        c.force.set_sensitive(true);
        c.content.set_sensitive(false);
        c.dialog.set_visible(true);
        c.cancel.grab_focus();
        *c.open.borrow_mut() = Some((service, session.id));
        let generation = c.generation.get() + 1;
        c.generation.set(generation);
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(
            Duration::from_secs(u64::from(DIALOG_TIMEOUT_SECS)),
            move || {
                let Some(greeter) = weak.upgrade() else {
                    return;
                };
                if greeter.conflict.generation.get() != generation {
                    return;
                }
                greeter.conflict.close();
                trace("greeter conflict timed-out");
                let toast = adw::Toast::new(
                    "Login Attempt Timed Out: login took too long, please try again",
                );
                greeter.conflict.toasts.add_toast(toast);
                greeter.abandon_opened();
            },
        );
    }

    /// logind removed a session: the conflicting one's end starts ours.
    pub(crate) fn session_removed(self: &Rc<Self>, id: &str) {
        let open = self.conflict.open.borrow().clone();
        if let Some((service, conflicting)) = open {
            if conflicting == id {
                self.conflict.close();
                trace(&format!("greeter conflict stopped session={id}"));
                self.start_session(&service);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_banner_names_who_and_where() {
        assert!(banner_text(false, false, "ada")
            .starts_with("Login is not possible because a session is already running for ada."));
        assert!(banner_text(false, true, "ada").contains("a remote session is already running"));
        assert!(banner_text(true, false, "ada")
            .starts_with("Remote login is not possible because a local session"));
        assert!(banner_text(true, true, "ada").contains("log out from the remote session"));
    }
}
