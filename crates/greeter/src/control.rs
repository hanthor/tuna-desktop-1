//! The login screen's side of the compositor's control socket
//! (`TUNA_CONTROL_SOCKET`, only this supervised process may hold it).
//!
//! GNOME Shell's login screen is part of Mutter's process, so it switches
//! the screen reader, the keymap and the input aids directly; here those
//! are the control protocol's commands, as the shell sends them for a
//! user: `SetScreenReader` (the compositor runs the trusted Orca, as for
//! the lock screen), `SetInputSettings` (the greeter user's keymap,
//! keyboard and pointer aids and bell) and `SwitchInputSource`. Nothing
//! the compositor sends is needed: frames are read and dropped so the
//! socket never backs up.

use std::cell::{Cell, RefCell};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::rc::Rc;

use gtk4::glib;
use tuna_shell_control::{CommandKind, InputSettings, Message, CURRENT_VERSION};

pub struct Control {
    stream: RefCell<Option<UnixStream>>,
    next_id: Cell<u64>,
}

impl Control {
    /// Connect to the compositor's control socket, when there is one.
    pub fn connect() -> Rc<Self> {
        let control = Rc::new(Self {
            stream: RefCell::new(None),
            next_id: Cell::new(1),
        });
        control.dial();
        control
    }

    fn dial(self: &Rc<Self>) {
        let Some(path) = std::env::var_os("TUNA_CONTROL_SOCKET") else {
            return;
        };
        let stream = match UnixStream::connect(&path) {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("tuna-greeter: compositor control unavailable: {error}");
                return;
            }
        };
        let hello = tuna_shell_control::encode_frame(&Message::Hello {
            version: CURRENT_VERSION,
        });
        if (&stream).write_all(&hello).is_err() || stream.set_nonblocking(true).is_err() {
            return;
        }
        let socket = match stream
            .try_clone()
            .map_err(|e| e.to_string())
            .and_then(|dup| gio::Socket::from_fd(dup.into()).map_err(|e| e.to_string()))
        {
            Ok(socket) => socket,
            Err(error) => {
                eprintln!("tuna-greeter: compositor control: {error}");
                return;
            }
        };
        let weak = Rc::downgrade(self);
        let source = gio::prelude::SocketExtManual::create_source(
            &socket,
            glib::IOCondition::IN | glib::IOCondition::HUP | glib::IOCondition::ERR,
            None::<&gio::Cancellable>,
            Some("tuna-greeter control"),
            glib::Priority::DEFAULT,
            move |_, _| {
                let Some(control) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                if control.drain() {
                    glib::ControlFlow::Continue
                } else {
                    control.stream.borrow_mut().take();
                    eprintln!("tuna-greeter: compositor control closed");
                    glib::ControlFlow::Break
                }
            },
        );
        // The attached source keeps the socket until it returns Break.
        source.attach(None);
        *self.stream.borrow_mut() = Some(stream);
        crate::trace("greeter control-connected");
    }

    /// Read what the compositor sent; `false` once the socket is gone.
    fn drain(&self) -> bool {
        let mut stream = self.stream.borrow_mut();
        let Some(stream) = stream.as_mut() else {
            return false;
        };
        let mut chunk = [0u8; 16384];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => return false,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
    }

    fn send(&self, kind: CommandKind) {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let frame = tuna_shell_control::encode_frame(&Message::Command { id, kind });
        if let Some(stream) = self.stream.borrow_mut().as_mut() {
            if let Err(error) = write_frame(stream, &frame) {
                eprintln!("tuna-greeter: compositor control: {error}");
            }
        }
    }

    /// `screen-reader-enabled`: the compositor runs Orca for the login
    pub fn set_screen_reader(&self, enabled: bool) {
        self.send(CommandKind::SetScreenReader { enabled });
    }

    /// The greeter user's input settings.
    pub fn set_input_settings(&self, settings: InputSettings) {
        self.send(CommandKind::SetInputSettings(Box::new(settings)));
    }

    /// One step to the next (or previous) input source.
    pub fn switch_input_source(&self, backward: bool) {
        self.send(CommandKind::SwitchInputSource { backward });
    }
}

/// Write a whole frame on the nonblocking socket, waiting out a full
/// buffer (a frame is small; the compositor reads every turn).
fn write_frame(stream: &mut UnixStream, mut frame: &[u8]) -> std::io::Result<()> {
    while !frame.is_empty() {
        match stream.write(frame) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => frame = &frame[n..],
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
