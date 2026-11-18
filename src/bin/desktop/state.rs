use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use qrcode::{Color, QrCode};
use rune::{Image, ImageCache, TextEdit, Tx, Waker};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use unbind::{
    forwarding::{Binding, Service, Services},
    links, management,
    management::Action,
    network, parse_listen, parse_target, profiles,
    session::{InviteOptions, Share},
    signaling::{DEFAULT_SERVER, Invite},
    split_route,
    transport::{DEFAULT_STUN, parse_stun},
};

use super::worker::{Listener, Network, Request};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Page {
    #[default]
    Share,
    Connect,
    Files,
    Sessions,
    Settings,
}

pub struct ShareForm {
    pub target: TextEdit,
    pub tcp: TextEdit,
    pub udp: TextEdit,
    pub expires: TextEdit,
    pub profile: TextEdit,
    pub lan: bool,
    pub single_use: bool,
    pub approve: bool,
    pub advanced: bool,
}

impl Default for ShareForm {
    fn default() -> Self {
        ShareProfile::default().into_form("")
    }
}

pub struct ConnectForm {
    pub link: TextEdit,
    pub listen: TextEdit,
    pub tcp: TextEdit,
    pub udp: TextEdit,
    pub identify: bool,
    pub retry: bool,
    pub advanced: bool,
    pub origin: Option<String>,
}

impl Default for ConnectForm {
    fn default() -> Self {
        Self {
            link: TextEdit::new("").secure(),
            listen: TextEdit::new("8080"),
            tcp: TextEdit::new(""),
            udp: TextEdit::new(""),
            identify: false,
            retry: true,
            advanced: false,
            origin: None,
        }
    }
}

pub struct FilesForm {
    pub send_path: TextEdit,
    pub receive_path: TextEdit,
    pub link: TextEdit,
    chosen_send: Option<PathBuf>,
    chosen_receive: Option<PathBuf>,
}

impl Default for FilesForm {
    fn default() -> Self {
        Self {
            send_path: TextEdit::new(""),
            receive_path: TextEdit::new(""),
            link: TextEdit::new("").secure(),
            chosen_send: None,
            chosen_receive: None,
        }
    }
}

pub struct SettingsForm {
    pub server: TextEdit,
    pub stun: TextEdit,
    pub udp_bind: TextEdit,
    pub public_endpoint: TextEdit,
    pub pcp_gateway: TextEdit,
}

impl Default for SettingsForm {
    fn default() -> Self {
        Preferences::default().into_form()
    }
}

pub struct Notice {
    pub text: String,
    pub error: bool,
}

pub struct InviteDisplay {
    pub origin: String,
    pub qr: Option<Image>,
    pub show_qr: bool,
    link: String,
}

#[derive(Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Session {
    pub id: String,
    pub kind: String,
    pub state: String,
    pub targets: Vec<String>,
    pub listeners: Vec<String>,
    pub path: Option<String>,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub invites: Vec<SessionInvite>,
}

#[derive(Deserialize, PartialEq)]
pub struct SessionInvite {
    pub id: String,
    pub state: String,
    pub peers: Vec<SessionPeer>,
}

#[derive(Deserialize, PartialEq)]
pub struct SessionPeer {
    pub id: String,
    pub state: String,
    pub identity: Option<String>,
}

#[derive(Default)]
pub struct State {
    pub page: Page,
    pub share: ShareForm,
    pub connect: ConnectForm,
    pub files: FilesForm,
    pub settings: SettingsForm,
    pub sessions: Vec<Session>,
    pub invite: Option<InviteDisplay>,
    pub notice: Option<Notice>,
    pub busy: bool,
    pub diagnostics: Vec<String>,
    pub profiles: Vec<String>,
    pub logo: Option<Image>,
    images: Option<ImageCache>,
    commands: Option<mpsc::UnboundedSender<Request>>,
    configuration: Configuration,
    config_path: Option<PathBuf>,
}

impl State {
    pub fn live(commands: mpsc::UnboundedSender<Request>, incoming: Option<String>) -> Self {
        let mut state = Self {
            commands: Some(commands),
            ..Self::default()
        };
        match state.load_configuration() {
            Ok(()) => {}
            Err(error) => state.set_error(format!("{error:#}")),
        }
        if let Some(link) = incoming {
            state.open_invite(&link);
        }
        state
    }

    pub fn setup(&mut self, waker: Waker) {
        let images = ImageCache::new(waker);
        self.logo = Some(images.load(&include_bytes!("../../../site/apple-touch-icon.png")[..]));
        self.images = Some(images);
    }

    pub fn open_invite(&mut self, input: &str) {
        self.page = Page::Connect;
        match invite_link(input)
            .and_then(|link| Invite::from_link(&link).map(|invite| (link, invite.origin)))
        {
            Ok((link, origin)) => {
                self.connect.link.replace_text(link.clone());
                self.connect.origin = Some(origin.to_string());
                self.files.link.replace_text(link);
                self.set_notice("Review the invite and choose Connect to join.");
            }
            Err(_) => self.set_error("The invite link is incomplete or invalid."),
        }
    }

    pub fn submit_share(&mut self) {
        self.start(|state| {
            let network = state.network()?;
            let draft = ShareProfile::from_form(&state.share);
            let name = state.share.profile.text.trim();
            let name = state
                .configuration
                .profiles
                .get(name)
                .map(|_| name.to_owned());
            Ok(Request::Share {
                share: draft.services(&network.server, name)?,
                lan: draft.lan,
                network,
            })
        });
    }

    pub fn submit_connect(&mut self) {
        self.start(|state| {
            let link = invite_link(&state.connect.link.text)?;
            let listener = listener(&state.connect)?;
            let network = state.network()?;
            Ok(Request::Connect {
                link,
                listener,
                identify: state.connect.identify,
                retry: state.connect.retry,
                network,
            })
        });
    }

    pub fn send_file(&mut self) {
        self.start(|state| {
            let network = state.network()?;
            let input = file_path(&state.files.send_path, state.files.chosen_send.as_deref());
            let file = fs::canonicalize(input).context("Find the file to send")?;
            ensure!(file.is_file(), "Choose a single file to send");
            let label = file
                .file_name()
                .context("Missing file name")?
                .to_string_lossy()
                .into_owned();
            Ok(Request::Share {
                share: Share {
                    server: network.server.clone(),
                    target_names: vec![label],
                    compatibility_target: None,
                    services: Services::new(vec![Service::File { path: file }])?,
                    invite: InviteOptions {
                        expires_in: expires_in(&state.share.expires.text)?,
                        single_use: true,
                    },
                    approve: state.share.approve,
                    profile: None,
                    one_file: true,
                    qr: false,
                },
                lan: state.share.lan,
                network,
            })
        });
    }

    pub fn receive_file(&mut self) {
        self.start(|state| {
            let link = invite_link(&state.files.link.text)?;
            let destination = file_path(
                &state.files.receive_path,
                state.files.chosen_receive.as_deref(),
            );
            ensure!(
                !destination.as_os_str().is_empty(),
                "Choose where to save the file"
            );
            Ok(Request::Connect {
                link,
                listener: Listener::File(destination),
                identify: state.connect.identify,
                retry: false,
                network: state.network()?,
            })
        });
    }

    pub fn choose_send_path(&mut self, path: PathBuf) {
        self.files.send_path.replace_text(path.to_string_lossy());
        self.files.chosen_send = Some(path);
    }

    pub fn choose_receive_path(&mut self, path: PathBuf) {
        self.files.receive_path.replace_text(path.to_string_lossy());
        self.files.chosen_receive = Some(path);
    }

    pub fn refresh_sessions(&mut self) {
        self.dispatch(Request::Refresh);
    }

    pub fn stop(&mut self, session: &str) {
        self.manage(session, Action::Stop);
    }

    pub fn new_invite(&mut self, session: &str) {
        match expires_in(&self.share.expires.text) {
            Ok(expires_in) => self.manage(
                session,
                Action::NewInvite {
                    expires_in,
                    single_use: self.share.single_use,
                },
            ),
            Err(error) => self.set_error(format!("{error:#}")),
        }
    }

    pub fn revoke(&mut self, session: &str, invite: &str) {
        self.manage(
            session,
            Action::Revoke {
                invite: invite.to_owned(),
            },
        );
    }

    pub fn approve(&mut self, session: &str, peer: &str, remember: bool) {
        self.manage(
            session,
            Action::Approve {
                peer: peer.to_owned(),
                remember,
            },
        );
    }

    pub fn deny(&mut self, session: &str, peer: &str) {
        self.manage(
            session,
            Action::Deny {
                peer: peer.to_owned(),
            },
        );
    }

    pub fn diagnose(&mut self) {
        self.start(|state| Ok(Request::Diagnose(state.network()?)));
    }

    pub fn enable_links(&mut self) {
        self.start(|_| Ok(Request::EnableLinks));
    }

    pub fn save_settings(&mut self) {
        let result = (|| {
            let preferences = Preferences::from_form(&self.settings);
            preferences.network()?;
            self.persist(|config| config.preferences = preferences)
        })();
        self.saved(result, "Network settings saved.");
    }

    pub fn save_profile(&mut self) {
        let result = (|| {
            let name = self.share.profile.text.trim().to_owned();
            ensure!(
                profiles::valid_name(&name),
                "Enter a profile name using letters, digits, hyphens, or underscores"
            );
            let draft = ShareProfile::from_form(&self.share);
            draft.services(DEFAULT_SERVER, None)?;
            self.persist(|config| {
                config.profiles.insert(name, draft);
            })
        })();
        self.saved(result, "Share profile saved.");
    }

    pub fn load_profile(&mut self, name: &str) {
        if let Some(profile) = self.configuration.profiles.get(name) {
            self.share = profile.clone().into_form(name);
            self.set_notice(format!("Loaded {name}."));
        }
    }

    pub fn copy_invite(&mut self, tx: &mut Tx) {
        if let Some(invite) = &self.invite {
            tx.copy_secret(invite.link.clone(), Duration::from_secs(120));
            self.set_notice("Invite copied. The clipboard clears after two minutes.");
        }
    }

    pub fn set_notice(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice {
            text: text.into(),
            error: false,
        });
    }

    pub fn set_error(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice {
            text: text.into(),
            error: true,
        });
    }

    pub fn set_sessions(&mut self, sessions: Vec<Session>) {
        self.sessions = sessions;
    }

    pub fn show_invite(&mut self, link: String) {
        match Invite::from_link(&link) {
            Ok(invite) => {
                self.invite = Some(InviteDisplay {
                    origin: invite.origin.to_string(),
                    qr: self
                        .images
                        .as_ref()
                        .and_then(|images| qr_image(&link, images)),
                    show_qr: false,
                    link,
                });
                self.set_notice("Invite ready to share.");
            }
            Err(_) => self.set_error("Could not display the new invite."),
        }
    }

    fn network(&self) -> Result<Network> {
        Preferences::from_form(&self.settings).network()
    }

    fn start(&mut self, prepare: impl FnOnce(&Self) -> Result<Request>) {
        if self.busy {
            return;
        }
        match prepare(self) {
            Ok(request) => {
                if self.dispatch(request) {
                    self.busy = true;
                    self.notice = None;
                }
            }
            Err(error) => self.set_error(format!("{error:#}")),
        }
    }

    fn dispatch(&mut self, request: Request) -> bool {
        if self
            .commands
            .as_ref()
            .is_some_and(|commands| commands.send(request).is_ok())
        {
            true
        } else {
            self.set_error("The networking service is unavailable.");
            false
        }
    }

    fn manage(&mut self, session: &str, action: Action) {
        self.dispatch(Request::Manage {
            session: session.to_owned(),
            action,
        });
    }

    fn load_configuration(&mut self) -> Result<()> {
        let path = management::state_dir()?.join("desktop.toml");
        self.configuration = read_configuration(&path)?;
        self.settings = self.configuration.preferences.clone().into_form();
        self.profiles = self.configuration.profiles.keys().cloned().collect();
        self.config_path = Some(path);
        Ok(())
    }

    fn persist(&mut self, update: impl FnOnce(&mut Configuration)) -> Result<()> {
        let path = self
            .config_path
            .as_deref()
            .context("The desktop settings file is unavailable")?;
        let parent = path
            .parent()
            .context("Find the desktop settings directory")?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(parent.join("desktop.lock"))?;
        lock.lock().context("Lock the desktop settings")?;
        let mut configuration = read_configuration(path)?;
        update(&mut configuration);
        let mut staged = tempfile::NamedTempFile::new_in(parent)?;
        staged.write_all(toml::to_string(&configuration)?.as_bytes())?;
        staged.as_file().sync_all()?;
        staged
            .persist(path)
            .with_context(|| format!("Save {}", path.display()))?;
        self.profiles = configuration.profiles.keys().cloned().collect();
        self.configuration = configuration;
        Ok(())
    }

    fn saved(&mut self, result: Result<()>, message: &str) {
        match result {
            Ok(()) => self.set_notice(message),
            Err(error) => self.set_error(format!("{error:#}")),
        }
    }
}

fn file_path(edit: &TextEdit, chosen: Option<&Path>) -> PathBuf {
    chosen
        .filter(|path| path.to_string_lossy() == edit.text)
        .map_or_else(|| PathBuf::from(&edit.text), Path::to_path_buf)
}

fn read_configuration(path: &Path) -> Result<Configuration> {
    match fs::read_to_string(path) {
        Ok(contents) => toml::from_str(&contents)
            .with_context(|| format!("Read desktop settings at {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Configuration::default()),
        Err(error) => Err(error).with_context(|| format!("Read {}", path.display())),
    }
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct Configuration {
    preferences: Preferences,
    profiles: BTreeMap<String, ShareProfile>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
struct Preferences {
    server: String,
    stun: String,
    udp_bind: String,
    public_endpoint: String,
    pcp_gateway: String,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            server: DEFAULT_SERVER.to_owned(),
            stun: DEFAULT_STUN.to_owned(),
            udp_bind: String::new(),
            public_endpoint: String::new(),
            pcp_gateway: String::new(),
        }
    }
}

impl Preferences {
    fn from_form(form: &SettingsForm) -> Self {
        Self {
            server: form.server.text.trim().to_owned(),
            stun: form.stun.text.trim().to_owned(),
            udp_bind: form.udp_bind.text.trim().to_owned(),
            public_endpoint: form.public_endpoint.text.trim().to_owned(),
            pcp_gateway: form.pcp_gateway.text.trim().to_owned(),
        }
    }

    fn into_form(self) -> SettingsForm {
        SettingsForm {
            server: TextEdit::new(self.server),
            stun: TextEdit::new(self.stun),
            udp_bind: TextEdit::new(self.udp_bind),
            public_endpoint: TextEdit::new(self.public_endpoint),
            pcp_gateway: TextEdit::new(self.pcp_gateway),
        }
    }

    fn network(&self) -> Result<Network> {
        let server = if self.server.is_empty() {
            DEFAULT_SERVER
        } else {
            &self.server
        };
        Invite::new(server).context("Check the rendezvous URL")?;
        let stun = parse_stun(if self.stun.is_empty() {
            DEFAULT_STUN
        } else {
            &self.stun
        })
        .context("Check the STUN URLs")?;
        let udp_bind = socket_option(&self.udp_bind).context("Check the UDP bind address")?;
        let public_endpoint =
            socket_option(&self.public_endpoint).context("Check the public UDP endpoint")?;
        let pcp_gateway = (!self.pcp_gateway.is_empty())
            .then(|| network::parse_router(&self.pcp_gateway))
            .transpose()
            .context("Check the PCP router")?;
        ensure!(
            udp_bind.is_some() || (public_endpoint.is_none() && pcp_gateway.is_none()),
            "Set a UDP bind address before using a public endpoint or PCP router"
        );
        ensure!(
            public_endpoint.is_none() || pcp_gateway.is_none(),
            "Choose either a public endpoint or a PCP router"
        );
        Ok(Network {
            server: server.to_owned(),
            stun,
            options: network::Options::new(udp_bind, public_endpoint, pcp_gateway),
        })
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
struct ShareProfile {
    target: String,
    tcp: String,
    udp: String,
    expires: String,
    lan: bool,
    single_use: bool,
    approve: bool,
}

impl Default for ShareProfile {
    fn default() -> Self {
        Self {
            target: "3000".to_owned(),
            tcp: String::new(),
            udp: String::new(),
            expires: String::new(),
            lan: false,
            single_use: false,
            approve: false,
        }
    }
}

impl ShareProfile {
    fn from_form(form: &ShareForm) -> Self {
        Self {
            target: form.target.text.trim().to_owned(),
            tcp: form.tcp.text.trim().to_owned(),
            udp: form.udp.text.trim().to_owned(),
            expires: form.expires.text.trim().to_owned(),
            lan: form.lan,
            single_use: form.single_use,
            approve: form.approve,
        }
    }

    fn into_form(self, name: &str) -> ShareForm {
        let advanced = !self.udp.is_empty() || !self.tcp.is_empty() || !self.expires.is_empty();
        ShareForm {
            target: TextEdit::new(self.target),
            tcp: TextEdit::new(self.tcp),
            udp: TextEdit::new(self.udp),
            expires: TextEdit::new(self.expires),
            profile: TextEdit::new(name),
            lan: self.lan,
            single_use: self.single_use,
            approve: self.approve,
            advanced,
        }
    }

    fn services(&self, server: &str, profile: Option<String>) -> Result<Share> {
        let compatibility_target = (!self.target.is_empty())
            .then(|| parse_target(&self.target))
            .transpose()
            .context("Check the primary service address")?;
        let mut services = Vec::new();
        let mut names = Vec::new();
        if let Some(target) = &compatibility_target {
            names.push(target.clone());
            services.push(Service::Tcp {
                name: "default".to_owned(),
                target: target.clone(),
            });
        }
        for (udp, field) in [(false, &self.tcp), (true, &self.udp)] {
            for route in routes(field) {
                let (name, target) = named_route(route)?;
                let target = parse_target(target)
                    .with_context(|| format!("Check {name}'s service address"))?;
                let protocol = if udp { "UDP" } else { "TCP" };
                names.push(format!("{name} {protocol} {target}"));
                services.push(if udp {
                    Service::Udp {
                        name: name.to_owned(),
                        target,
                    }
                } else {
                    Service::Tcp {
                        name: name.to_owned(),
                        target,
                    }
                });
            }
        }
        Ok(Share {
            server: server.to_owned(),
            target_names: names,
            compatibility_target,
            services: Services::new(services)?,
            invite: InviteOptions {
                expires_in: expires_in(&self.expires)?,
                single_use: self.single_use,
            },
            approve: self.approve,
            profile,
            one_file: false,
            qr: false,
        })
    }
}

fn socket_option(input: &str) -> Result<Option<SocketAddr>> {
    if input.is_empty() {
        Ok(None)
    } else {
        Ok(Some(input.parse()?))
    }
}

fn invite_link(input: &str) -> Result<String> {
    let input = input.trim();
    if input.starts_with("unbind:") {
        links::unpack(input)
    } else {
        Invite::from_link(input).map(|invite| invite.link().to_string())
    }
}

fn expires_in(input: &str) -> Result<Option<u64>> {
    if input.trim().is_empty() {
        return Ok(None);
    }
    let minutes: u64 = input
        .trim()
        .parse()
        .context("Enter the invite lifetime in minutes")?;
    ensure!(
        (1..=525_600).contains(&minutes),
        "Choose an invite lifetime between one minute and one year"
    );
    Ok(Some(minutes * 60))
}

fn routes(field: &str) -> impl Iterator<Item = &str> {
    field
        .split([',', '\n'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn named_route(route: &str) -> Result<(&str, &str)> {
    let (name, address) = split_route(route)?;
    Ok((name.trim(), address.trim()))
}

fn listener(form: &ConnectForm) -> Result<Listener> {
    let primary = form.listen.text.trim();
    let mut bindings = Vec::new();
    for (udp, field) in [(false, &form.tcp), (true, &form.udp)] {
        for route in routes(&field.text) {
            let (name, address) = named_route(route)?;
            let listen = parse_listen(address)
                .with_context(|| format!("Check the local address for {name}"))?;
            bindings.push(if udp {
                Binding::Udp {
                    name: name.to_owned(),
                    listen,
                }
            } else {
                Binding::Tcp {
                    name: name.to_owned(),
                    listen,
                }
            });
        }
    }
    if bindings.is_empty() {
        return Ok(Listener::Tcp(parse_listen(if primary.is_empty() {
            "8080"
        } else {
            primary
        })?));
    }
    if !primary.is_empty() {
        bindings.insert(
            0,
            Binding::Tcp {
                name: "default".to_owned(),
                listen: parse_listen(primary)?,
            },
        );
    }
    Ok(Listener::Named(bindings))
}

fn qr_image(link: &str, images: &ImageCache) -> Option<Image> {
    let code = QrCode::new(link).ok()?;
    let scale = 4;
    let margin = 4 * scale;
    let side = code.width() * scale + 2 * margin;
    let mut pixels = vec![255u8; side * side * 4];
    for y in margin..(side - margin) {
        for x in margin..(side - margin) {
            if code[((x - margin) / scale, (y - margin) / scale)] == Color::Dark {
                let offset = (y * side + x) * 4;
                pixels[offset..offset + 3].fill(0);
            }
        }
    }
    Some(images.rgba(side as u32, side as u32, pixels))
}
