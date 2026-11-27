//! The Servers page's "Friends over the internet" panel: Tailscale on this computer, the address friends
//! type, inviting a friend to this one computer, and keeping shared friends to the server's port
//! (`sunrise_launcher::tailscale`). Signing in is always the player's, in Tailscale's own app or page;
//! the launcher reads `tailscale status` and, with the player's own access token, calls the API.
use super::servers_page::Field;
use super::*;
use serde_json::Value;
use sunrise_launcher::tailscale::{self, Api, Invite, Reach, State};

mod hosting_share;

/// How often Tailscale's state is read while the panel shows.
const POLL: Duration = Duration::from_secs(10);
/// How often the invitations and access controls are read again while the panel shows, so an
/// accepted invitation appears without a click.
const RELOAD: Duration = Duration::from_secs(30);

pub(super) enum Event {
    State(State),
    Token(Option<String>),
    Invites(Result<Vec<Invite>, String>),
    Policy(Result<(Value, Option<String>), String>),
    /// An action finished: what to tell the player, and whether to read invites and policy again.
    Done(Result<String, String>),
    Invited(Result<Invite, String>),
}

pub(super) struct Internet {
    state: Option<State>,
    reading: bool,
    read: Option<Instant>,
    /// The access token, once read from the keychain (None: not read yet; Some(None): none).
    token: Option<Option<String>>,
    asking_token: bool,
    invites: Option<Result<Vec<Invite>, String>>,
    policy: Option<Result<(Value, Option<String>), String>>,
    loaded: Option<Instant>,
    busy: bool,
    message: Option<Result<String, String>>,
    /// The invitation just made, its link shown to copy.
    fresh: Option<Invite>,
    confirm_limit: bool,
    copied: Option<(String, Instant)>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
}

impl Internet {
    pub(super) fn location_busy(&self) -> bool {
        self.busy
    }

    pub(super) fn new() -> Self {
        let (tx, rx) = crate::wake::channel();
        Self {
            state: None,
            reading: false,
            read: None,
            token: None,
            asking_token: false,
            invites: None,
            policy: None,
            loaded: None,
            busy: false,
            message: None,
            fresh: None,
            confirm_limit: false,
            copied: None,
            tx,
            rx,
        }
    }

    fn node(&self) -> Option<&tailscale::Node> {
        match &self.state {
            Some(State::Running(n)) => Some(n),
            _ => None,
        }
    }

    fn token(&self) -> Option<String> {
        self.token.clone().flatten()
    }
}

impl Launcher {
    /// What the background sent, and the next look at Tailscale while the panel shows.
    pub(super) fn internet_tick(&mut self, showing: bool) -> bool {
        let mut changed = false;
        while let Ok(event) = self.internet.rx.try_recv() {
            changed = true;
            self.internet_event(event);
        }
        let i = &mut self.internet;
        if showing && !i.reading && i.read.is_none_or(|at| at.elapsed() >= POLL) {
            self.internet_refresh();
        }
        changed
    }

    fn internet_refresh(&mut self) {
        let i = &mut self.internet;
        i.reading = true;
        i.read = Some(Instant::now());
        let tx = i.tx.clone();
        let want_token = i.token.is_none();
        std::thread::spawn(move || {
            let _ = tx.send(Event::State(tailscale::status()));
            if want_token {
                let token = account::keychain().get(tailscale::TOKEN_KEY).ok().flatten();
                let _ = tx.send(Event::Token(token));
            }
        });
    }

    fn internet_event(&mut self, event: Event) {
        let i = &mut self.internet;
        match event {
            Event::State(state) => {
                i.reading = false;
                let was_running = i.node().map(|n| n.id.clone());
                i.state = Some(state);
                let now = i.node().map(|n| n.id.clone());
                let stale = i.loaded.is_none_or(|at| at.elapsed() >= RELOAD);
                if now.is_some() && (now != was_running || (stale && !i.busy)) {
                    self.internet_load();
                }
            }
            Event::Token(token) => {
                i.token = Some(token);
                self.internet_load();
            }
            Event::Invites(r) => i.invites = Some(r),
            Event::Policy(r) => i.policy = Some(r),
            Event::Done(r) => {
                i.busy = false;
                i.message = Some(r);
                self.internet_load();
            }
            Event::Invited(r) => {
                i.busy = false;
                match r {
                    Ok(invite) => {
                        i.message = None;
                        i.fresh = Some(invite);
                        self.servers.text(Field::Email).set(String::new());
                    }
                    Err(e) => i.message = Some(Err(e)),
                }
                self.internet_load();
            }
        }
    }

    /// Reads the invitations and the access controls (needs the token and a running Tailscale).
    fn internet_load(&mut self) {
        let i = &mut self.internet;
        let (Some(token), Some(node)) = (i.token(), i.node().cloned()) else {
            return;
        };
        i.loaded = Some(Instant::now());
        let tx = i.tx.clone();
        std::thread::spawn(move || {
            let api = Api::new(&token);
            let _ = tx.send(Event::Invites(api.invites(&node.id)));
            let _ = tx.send(Event::Policy(api.policy()));
        });
    }

    pub(super) fn save_token(&mut self) {
        let token = self.servers.value(Field::Token).trim().to_string();
        if !token.starts_with("tskey-") {
            self.internet.message = Some(Err(crate::i18n::tr(
                "Paste the access token Tailscale showed (it begins with tskey-).",
            )
            .into()));
            return;
        }
        self.servers.text(Field::Token).set(String::new());
        self.internet.asking_token = false;
        self.internet.message = match account::keychain().set(tailscale::TOKEN_KEY, &token) {
            Ok(()) if account::keychain().persistent() => None,
            Ok(()) => Some(Ok(crate::i18n::tr(
                "No keychain here: the token is kept until the launcher closes.",
            )
            .into())),
            Err(e) => Some(Err(e.into())),
        };
        self.internet.token = Some(Some(token));
        self.internet_load();
    }

    fn forget_token(&mut self) {
        let _ = account::keychain().delete(tailscale::TOKEN_KEY);
        let i = &mut self.internet;
        i.token = Some(None);
        i.invites = None;
        i.policy = None;
        i.fresh = None;
        i.message = Some(Ok(
            "The access token is forgotten. Delete it in Tailscale's admin \
                             console too (Settings, Keys) if you won't use it again."
                .into(),
        ));
    }

    pub(super) fn invite_friend(&mut self) {
        let i = &mut self.internet;
        let (Some(token), Some(node)) = (i.token(), i.node().cloned()) else {
            return;
        };
        if i.busy {
            return;
        }
        let email = self.servers.value(Field::Email).trim().to_string();
        if !email.is_empty() && !email.contains('@') {
            i.message = Some(Err("That isn't an email address.".into()));
            return;
        }
        i.busy = true;
        i.fresh = None;
        let tx = i.tx.clone();
        std::thread::spawn(move || {
            let email = Some(email.as_str()).filter(|e| !e.is_empty());
            let _ = tx.send(Event::Invited(Api::new(&token).invite(&node.id, email)));
        });
    }

    fn internet_action(
        &mut self,
        job: impl FnOnce(&Api) -> Result<String, String> + Send + 'static,
    ) {
        let i = &mut self.internet;
        let Some(token) = i.token() else {
            return;
        };
        if i.busy {
            return;
        }
        i.busy = true;
        i.message = None;
        let tx = i.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Event::Done(job(&Api::new(&token))));
        });
    }

    /// Applies `change` to the access controls read at `etag`, checked by Tailscale first.
    fn change_policy(&mut self, new: Value, etag: Option<String>, said: &'static str) {
        self.internet.confirm_limit = false;
        self.internet_action(move |api| {
            api.validate(&new)?;
            api.set_policy(&new, etag.as_deref())?;
            Ok(said.to_string())
        });
    }
}
