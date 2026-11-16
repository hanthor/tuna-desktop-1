//! GDM's channels and the order of calls on them.
//!
//! The greeter's own channel (`Manager.OpenSession`) carries the Greeter
//! interface and, for users without a running session, the
//! conversations. As userVerifier.js `_getUserVerifierProxies` does, every
//! begin for a named user first asks GDM for that user's
//! re-authentication channel (`OpenReauthenticationChannel`): GDM grants
//! it only when the user's session runs, and then the conversation runs
//! there and its success makes GDM switch to that session (user
//! switching); `AccessDenied` sends the conversation to the greeter's own
//! channel. Calls go out one at a time, in order, each on the channel of
//! the begin it belongs to.

use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::rc::Rc;

use crate::gdm::{self, Call};
use crate::{trace, Greeter, GREETER};

pub struct Channel {
    pub id: u64,
    pub connection: gio::DBusConnection,
    _signals: gdm::Subscription,
}

#[derive(Default)]
pub struct Channels {
    pub main: RefCell<Option<Rc<Channel>>>,
    /// The channel of the latest begin.
    verifier: RefCell<Option<Rc<Channel>>>,
    queue: RefCell<VecDeque<(Call, Option<u64>)>>,
    pumping: Cell<bool>,
    next_id: Cell<u64>,
    /// Begins that went to a user's re-authentication channel.
    reauth: RefCell<HashSet<u64>>,
}

impl Channels {
    pub fn is_reauth(&self, request: u64) -> bool {
        self.reauth.borrow().contains(&request)
    }
}

impl Greeter {
    /// Subscribe to a new channel's signals and hear when it closes.
    pub(crate) fn attach(self: &Rc<Self>, connection: gio::DBusConnection) -> Rc<Channel> {
        let id = self.channels.next_id.get() + 1;
        self.channels.next_id.set(id);
        let weak = Rc::downgrade(self);
        let signals = gdm::subscribe(&connection, move |signal| {
            if let Some(greeter) = weak.upgrade() {
                greeter.on_signal(signal);
            }
        });
        // `closed` may be emitted, and its handler dropped, off this
        // thread: the handler only hands the news to the main context.
        connection.connect_closed(move |_, _, _| {
            glib::MainContext::default().invoke(move || {
                if let Some(greeter) = GREETER.with(|g| g.borrow().clone()) {
                    greeter.on_channel_closed(id);
                }
            });
        });
        Rc::new(Channel {
            id,
            connection,
            _signals: signals,
        })
    }

    /// Queue one call; calls go out in order.
    pub(crate) fn send(self: &Rc<Self>, call: Call) {
        self.send_for(call, None);
    }

    pub(crate) fn send_for(self: &Rc<Self>, call: Call, request: Option<u64>) {
        match &call {
            Call::AnswerQuery { service, .. } => {
                trace(&format!("greeter answer service={service}"))
            }
            Call::JsonReply { service, .. } => trace(&format!("greeter reply service={service}")),
            other => trace(&format!("greeter call {other:?}")),
        }
        self.channels.queue.borrow_mut().push_back((call, request));
        if self.channels.pumping.replace(true) {
            return;
        }
        let greeter = self.clone();
        glib::spawn_future_local(async move {
            loop {
                let next = greeter.channels.queue.borrow_mut().pop_front();
                let Some((call, request)) = next else {
                    break;
                };
                greeter.run(call, request).await;
            }
            greeter.channels.pumping.set(false);
        });
    }

    async fn run(self: &Rc<Self>, call: Call, request: Option<u64>) {
        let main = self.channels.main.borrow().clone();
        let channel = match &call {
            Call::BeginVerificationForUser { user, .. } => match self.verifier_for(user).await {
                Ok(channel) => Some(channel),
                Err(error) => {
                    eprintln!(
                        "tuna-greeter: OpenReauthenticationChannel: {}",
                        error.message()
                    );
                    None
                }
            },
            Call::BeginVerification { .. } => main,
            call if call.is_verifier() => self.channels.verifier.borrow().clone().or(main),
            _ => main,
        };
        let begin = matches!(
            call,
            Call::BeginVerificationForUser { .. } | Call::BeginVerification { .. }
        );
        let Some(channel) = channel else {
            if let Some(request) = request.filter(|_| begin) {
                self.fail_begin(request);
            }
            return;
        };
        let on_main = self
            .channels
            .main
            .borrow()
            .as_ref()
            .is_some_and(|m| m.id == channel.id);
        if begin {
            self.use_verifier(channel.clone());
            if let Some(request) = request {
                self.host.set_channel(request, channel.id);
                if !on_main {
                    self.channels.reauth.borrow_mut().insert(request);
                }
            }
        }
        let (interface, method, args) = call.wire();
        let result = channel
            .connection
            .call_future(
                None,
                gdm::SESSION_PATH,
                interface,
                method,
                Some(&args),
                None,
                gio::DBusCallFlags::NONE,
                -1,
            )
            .await;
        match (result, request.filter(|_| begin)) {
            (Ok(_), Some(_)) if on_main => {
                // GDM forgets the selected session on every reset.
                let chosen = self.chosen_session.borrow().clone();
                if let Some(session) = chosen {
                    self.send(Call::SelectSession { session });
                }
            }
            (Ok(_), _) => {}
            (Err(error), Some(request)) => {
                eprintln!("tuna-greeter: {method}: {}", error.message());
                self.fail_begin(request);
            }
            (Err(error), None) => eprintln!("tuna-greeter: {method}: {}", error.message()),
        }
    }

    /// The channel a begin for `user` runs on (`_getUserVerifierProxies`).
    async fn verifier_for(self: &Rc<Self>, user: &str) -> Result<Rc<Channel>, glib::Error> {
        let main = self.channels.main.borrow().clone();
        let system = self.system.borrow().clone();
        let (Some(main), Some(system)) = (main, system) else {
            return Err(glib::Error::new(
                gio::IOErrorEnum::NotConnected,
                "no channel to GDM",
            ));
        };
        let address = match gdm::open_reauthentication_channel(&system, user).await {
            Ok(address) => address,
            Err(error) if gdm::access_denied(&error) => {
                trace(&format!("greeter verifier user={user} channel=greeter"));
                return Ok(main);
            }
            Err(error) => return Err(error),
        };
        let connection = gdm::connect(&address).await?;
        let channel = self.attach(connection);
        // libgdm enables the extensions when it makes the channel's
        // UserVerifier proxy.
        let (interface, method, args) = Call::EnableExtensions.wire();
        if let Err(error) = channel
            .connection
            .call_future(
                None,
                gdm::SESSION_PATH,
                interface,
                method,
                Some(&args),
                None,
                gio::DBusCallFlags::NONE,
                -1,
            )
            .await
        {
            eprintln!("tuna-greeter: EnableExtensions: {}", error.message());
        }
        trace(&format!(
            "greeter verifier user={user} channel=reauthentication"
        ));
        Ok(channel)
    }

    /// The latest begin's channel; a replaced re-authentication channel
    /// is closed (libgdm drops its proxy).
    fn use_verifier(self: &Rc<Self>, channel: Rc<Channel>) {
        let main_id = self.channels.main.borrow().as_ref().map(|m| m.id);
        let old = self.channels.verifier.replace(Some(channel.clone()));
        if let Some(old) = old.filter(|o| o.id != channel.id && Some(o.id) != main_id) {
            old.connection.close(None::<&gio::Cancellable>, |_| {});
        }
    }

    pub(crate) fn fail_begin(self: &Rc<Self>, request: u64) {
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(greeter) = weak.upgrade() {
                let inputs = greeter.host.begin_failed(request);
                greeter.feed(inputs);
            }
        });
    }

    /// A channel closed: its owed begins are denied; the greeter's own
    /// channel is asked for again.
    pub(crate) fn on_channel_closed(self: &Rc<Self>, id: u64) {
        let is_main = self
            .channels
            .main
            .borrow()
            .as_ref()
            .is_some_and(|m| m.id == id);
        if self
            .channels
            .verifier
            .borrow()
            .as_ref()
            .is_some_and(|v| v.id == id)
        {
            self.channels.verifier.borrow_mut().take();
        }
        if !is_main {
            trace("greeter reauthentication-channel-closed");
            let inputs = self.host.channel_closed(id, false);
            self.feed(inputs);
            return;
        }
        trace("greeter gdm-closed");
        self.channels.main.borrow_mut().take();
        let inputs = self.host.channel_closed(id, true);
        self.feed(inputs);
        self.on_gdm_closed();
        self.connect();
    }
}
