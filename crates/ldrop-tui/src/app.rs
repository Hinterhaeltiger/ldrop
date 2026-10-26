// SPDX-License-Identifier: GPL-3.0-or-later
//! TUI state and key handling. Events go in, commands for the async side come out, so all of
//! this runs in tests without a terminal or a daemon.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ldrop_client::{AirDropLink, Decision, IncomingRequest, Status, TransferInfo};
use ldrop_core::capabilities::DoctorReport;
use ldrop_core::config::Config;
use ldrop_core::{Device, Item, TransferState, Visibility};

use crate::complete;
use crate::input::LineInput;

/// How long a footer message stays.
const MESSAGE_US: u64 = 4_000_000;
/// Answer keys count only once a prompt has been on screen this long, so a request that pops
/// up while the user is typing can't be answered by accident.
const PROMPT_ARM_US: u64 = 1_000_000;
/// Minimum spacing of speed samples.
const RATE_SAMPLE_US: u64 = 500_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Devices,
    Transfers,
    Settings,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Devices, Tab::Transfers, Tab::Settings];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Devices => "Devices",
            Tab::Transfers => "Transfers",
            Tab::Settings => "Settings",
        }
    }

    fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Visibility,
    DeviceName,
    DownloadDir,
    QuickShare,
    AirDrop,
    ExclusiveRadio,
}

impl Setting {
    pub const ALL: [Setting; 6] = [
        Setting::Visibility,
        Setting::DeviceName,
        Setting::DownloadDir,
        Setting::QuickShare,
        Setting::AirDrop,
        Setting::ExclusiveRadio,
    ];
}

/// What the async side feeds in.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Key(KeyEvent),
    /// Nothing changed but the clock or the terminal size.
    Tick,
    Connected {
        status: Status,
        capabilities: Option<DoctorReport>,
    },
    Disconnected(String),
    Status(Status),
    AirDropLink(AirDropLink),
    Capabilities(DoctorReport),
    Config(Config),
    DiscoveryStarted,
    DeviceFound(Device),
    DeviceLost(String),
    /// Full list, on (re)connect.
    Transfers(Vec<TransferInfo>),
    TransferChanged(TransferInfo),
    TransferRemoved(u32),
    /// The daemon asks this TUI, as its Agent, to accept or decline.
    Incoming(IncomingRequest),
    /// The prompt for this transfer was answered, expired or decided elsewhere.
    PromptGone(u32),
    SendStarted(u32),
    SendFailed(String),
    Info(String),
    Error(String),
}

/// What the app asks the async side to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    SetVisibility(Visibility),
    SendFiles {
        device: String,
        paths: Vec<PathBuf>,
    },
    SendText {
        device: String,
        text: String,
        is_url: bool,
    },
    /// Answers a prompt this TUI got as the Agent.
    Answer(u32, Decision),
    /// Accepts or declines a waiting transfer without a prompt here (another agent has it).
    Decide(u32, Decision),
    Cancel(u32),
    ClearHistory,
    SetDeviceName(String),
    SetDownloadDir(String),
    SetConfig {
        key: &'static str,
        value: String,
    },
    /// `xdg-open` a path or URL.
    Open(String),
    Copy(String),
    Bell,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conn {
    Connecting,
    Up,
    Down(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub text: String,
    pub error: bool,
    until: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Prompt {
    pub request: IncomingRequest,
    shown_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendMode {
    Files,
    Text,
    Link,
}

impl SendMode {
    pub const ALL: [SendMode; 3] = [SendMode::Files, SendMode::Text, SendMode::Link];

    pub fn label(self) -> &'static str {
        match self {
            SendMode::Files => "Files",
            SendMode::Text => "Text",
            SendMode::Link => "Link",
        }
    }

    fn next(self) -> Self {
        match self {
            SendMode::Files => SendMode::Text,
            SendMode::Text => SendMode::Link,
            SendMode::Link => SendMode::Files,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SendDialog {
    pub device: Device,
    pub mode: SendMode,
    pub input: LineInput,
    pub paths: Vec<PathBuf>,
    /// Completion candidates from the last Tab.
    pub candidates: Vec<String>,
    pub error: Option<String>,
}

/// Follows one outgoing transfer, from the send call until the user closes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Sending {
    pub device: String,
    pub what: String,
    pub transfer: Option<u32>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    DeviceName,
    DownloadDir,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Edit {
    pub field: Field,
    pub input: LineInput,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Dialog {
    Send(SendDialog),
    Sending(Sending),
    Edit(Edit),
    /// The exclusive-radio opt-in and its warning.
    ConfirmExclusive,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Rate {
    bytes: u64,
    at: u64,
    bytes_per_sec: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct App {
    /// Unix µs; set by the caller before each event so rendering is deterministic.
    pub now: u64,
    pub tab: Tab,
    pub conn: Conn,
    pub status: Option<Status>,
    pub airdrop_link: AirDropLink,
    pub capabilities: Option<DoctorReport>,
    pub config: Config,
    pub home: Option<PathBuf>,
    pub devices: Vec<Device>,
    pub discovering: bool,
    device_sel: usize,
    pub transfers: BTreeMap<u32, TransferInfo>,
    transfer_sel: Option<u32>,
    rates: HashMap<u32, Rate>,
    settings_sel: usize,
    prompts: Vec<Prompt>,
    pub dialog: Option<Dialog>,
    /// From `ldrop-tui send FILE…`: picking a device sends these.
    pub preselected: Vec<PathBuf>,
    message: Option<Message>,
    pub help: bool,
    pub quit: bool,
}

impl App {
    pub fn new(now: u64, config: Config, home: Option<PathBuf>, preselected: Vec<PathBuf>) -> Self {
        App {
            now,
            tab: Tab::Devices,
            conn: Conn::Connecting,
            status: None,
            airdrop_link: AirDropLink::default(),
            capabilities: None,
            config,
            home,
            devices: Vec::new(),
            discovering: false,
            device_sel: 0,
            transfers: BTreeMap::new(),
            transfer_sel: None,
            rates: HashMap::new(),
            settings_sel: 0,
            prompts: Vec::new(),
            dialog: None,
            preselected,
            message: None,
            help: false,
            quit: false,
        }
    }

    /// Discovery runs while the device list is on screen, like Android's share sheet.
    pub fn wants_discovery(&self) -> bool {
        self.conn == Conn::Up && self.tab == Tab::Devices
    }

    pub fn selected_device(&self) -> Option<(usize, &Device)> {
        let i = self.device_sel.min(self.devices.len().checked_sub(1)?);
        Some((i, &self.devices[i]))
    }

    /// Newest first.
    pub fn transfer_list(&self) -> Vec<&TransferInfo> {
        self.transfers.values().rev().collect()
    }

    pub fn selected_transfer(&self) -> Option<(usize, &TransferInfo)> {
        let list = self.transfer_list();
        let i = self.transfer_sel.and_then(|id| list.iter().position(|t| t.id == id)).unwrap_or(0);
        list.get(i).map(|t| (i, *t))
    }

    pub fn selected_setting(&self) -> (usize, Setting) {
        let i = self.settings_sel.min(Setting::ALL.len() - 1);
        (i, Setting::ALL[i])
    }

    /// Bytes per second, once there are two samples.
    pub fn rate(&self, id: u32) -> Option<f64> {
        self.rates.get(&id).map(|r| r.bytes_per_sec).filter(|r| *r > 0.0)
    }

    pub fn left(&self, deadline_us: u64) -> Option<Duration> {
        (deadline_us != 0).then(|| Duration::from_micros(deadline_us.saturating_sub(self.now)))
    }

    pub fn visibility_left(&self) -> Option<Duration> {
        self.status
            .as_ref()
            .filter(|s| s.visibility == Visibility::Everyone)
            .and_then(|s| self.left(s.visibility_expires_at))
    }

    pub fn message(&self) -> Option<&Message> {
        self.message.as_ref().filter(|m| self.now < m.until)
    }

    /// The prompt on screen, and how many wait in total.
    pub fn prompt(&self) -> Option<(&IncomingRequest, usize)> {
        self.prompts.first().map(|p| (&p.request, self.prompts.len()))
    }

    fn info(&mut self, text: impl Into<String>) {
        self.message = Some(Message { text: text.into(), error: false, until: self.now + MESSAGE_US });
    }

    fn error(&mut self, text: impl Into<String>) {
        self.message = Some(Message { text: text.into(), error: true, until: self.now + MESSAGE_US });
    }

    pub fn handle(&mut self, event: Event) -> Vec<Command> {
        let commands = match event {
            Event::Key(key) => self.key(key),
            Event::Tick => Vec::new(),
            Event::Connected { status, capabilities } => {
                self.conn = Conn::Up;
                self.status = Some(status);
                self.capabilities = capabilities;
                Vec::new()
            }
            Event::Disconnected(reason) => {
                self.conn = Conn::Down(reason);
                // The agent and discovery session died with the daemon connection.
                self.prompts.clear();
                Vec::new()
            }
            Event::Status(status) => {
                self.status = Some(status);
                Vec::new()
            }
            Event::AirDropLink(link) => {
                self.airdrop_link = link;
                Vec::new()
            }
            Event::Capabilities(report) => {
                self.capabilities = Some(report);
                Vec::new()
            }
            Event::Config(config) => {
                self.config = config;
                Vec::new()
            }
            Event::DiscoveryStarted => {
                self.devices.clear();
                self.device_sel = 0;
                self.discovering = true;
                Vec::new()
            }
            Event::DeviceFound(device) if self.discovering => {
                match self.devices.iter_mut().find(|d| d.id == device.id) {
                    Some(d) => *d = device,
                    None => self.devices.push(device),
                }
                Vec::new()
            }
            Event::DeviceFound(_) => Vec::new(),
            Event::DeviceLost(id) => {
                if let Some(i) = self.devices.iter().position(|d| d.id == id) {
                    self.devices.remove(i);
                    if i < self.device_sel {
                        self.device_sel -= 1;
                    }
                }
                Vec::new()
            }
            Event::Transfers(list) => {
                self.transfers = list.into_iter().map(|t| (t.id, t)).collect();
                self.rates.retain(|id, _| self.transfers.contains_key(id));
                Vec::new()
            }
            Event::TransferChanged(t) => {
                self.transfer_changed(t);
                Vec::new()
            }
            Event::TransferRemoved(id) => {
                self.transfers.remove(&id);
                self.rates.remove(&id);
                Vec::new()
            }
            Event::Incoming(request) => {
                if self.prompts.iter().any(|p| p.request.id == request.id) {
                    return Vec::new();
                }
                self.prompts.push(Prompt { request, shown_at: self.now });
                vec![Command::Bell]
            }
            Event::PromptGone(id) => {
                self.remove_prompt(id);
                Vec::new()
            }
            Event::SendStarted(id) => {
                if let Some(Dialog::Sending(s)) = &mut self.dialog
                    && s.transfer.is_none()
                    && s.error.is_none()
                {
                    s.transfer = Some(id);
                }
                self.transfer_sel = Some(id);
                Vec::new()
            }
            Event::SendFailed(reason) => {
                match &mut self.dialog {
                    Some(Dialog::Sending(s)) if s.transfer.is_none() => s.error = Some(reason),
                    _ => self.error(reason),
                }
                Vec::new()
            }
            Event::Info(text) => {
                self.info(text);
                Vec::new()
            }
            Event::Error(text) => {
                self.error(text);
                Vec::new()
            }
        };
        if !self.wants_discovery() {
            self.discovering = false;
            self.devices.clear();
        }
        commands
    }

    fn transfer_changed(&mut self, t: TransferInfo) {
        if t.state == TransferState::Transferring {
            let now = self.now;
            let rate = self.rates.entry(t.id).or_insert(Rate { bytes: t.bytes_done, at: now, bytes_per_sec: 0.0 });
            let dt = now.saturating_sub(rate.at);
            if dt >= RATE_SAMPLE_US {
                let sample = t.bytes_done.saturating_sub(rate.bytes) as f64 / (dt as f64 / 1e6);
                rate.bytes_per_sec =
                    if rate.bytes_per_sec == 0.0 { sample } else { 0.7 * rate.bytes_per_sec + 0.3 * sample };
                rate.bytes = t.bytes_done;
                rate.at = now;
            }
        } else {
            self.rates.remove(&t.id);
        }
        // The request may arrive before the transfer reaches AwaitingAcceptance, so only
        // states past it close a prompt.
        if t.state.is_final() || t.state == TransferState::Transferring {
            self.remove_prompt(t.id);
        }
        self.transfers.insert(t.id, t);
    }

    fn remove_prompt(&mut self, id: u32) {
        let Some(i) = self.prompts.iter().position(|p| p.request.id == id) else { return };
        self.prompts.remove(i);
        if i == 0
            && let Some(next) = self.prompts.first_mut()
        {
            next.shown_at = self.now;
        }
    }

    fn key(&mut self, key: KeyEvent) -> Vec<Command> {
        if key.kind == KeyEventKind::Release {
            return Vec::new();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return Vec::new();
        }
        if !self.prompts.is_empty() {
            return self.prompt_key(key);
        }
        if self.help {
            self.help = false;
            return Vec::new();
        }
        if self.dialog.is_some() {
            return self.dialog_key(key);
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Tab => self.tab = Tab::ALL[(self.tab.index() + 1) % Tab::ALL.len()],
            KeyCode::BackTab => self.tab = Tab::ALL[(self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len()],
            KeyCode::Char(c @ '1'..='3') => self.tab = Tab::ALL[c as usize - '1' as usize],
            KeyCode::Char('v') => return self.toggle_visibility(),
            _ => {
                return match self.tab {
                    Tab::Devices => self.devices_key(key),
                    Tab::Transfers => self.transfers_key(key),
                    Tab::Settings => self.settings_key(key),
                };
            }
        }
        Vec::new()
    }

    fn prompt_key(&mut self, key: KeyEvent) -> Vec<Command> {
        let Some(prompt) = self.prompts.first() else { return Vec::new() };
        if self.now < prompt.shown_at + PROMPT_ARM_US {
            return Vec::new();
        }
        let decision = match key.code {
            KeyCode::Char('a') => Decision::Accept,
            KeyCode::Char('d') => Decision::Decline,
            _ => return Vec::new(),
        };
        let id = prompt.request.id;
        self.remove_prompt(id);
        self.answered(id, decision);
        vec![Command::Answer(id, decision)]
    }

    fn answered(&mut self, id: u32, decision: Decision) {
        self.transfer_sel = Some(id);
        match decision {
            Decision::Accept if self.tab != Tab::Transfers => self.info("Accepted. Progress is in [2] Transfers."),
            Decision::Accept => self.info("Accepted."),
            Decision::Decline => self.info("Declined."),
        }
    }

    fn toggle_visibility(&mut self) -> Vec<Command> {
        match &self.status {
            Some(s) if self.conn == Conn::Up => vec![Command::SetVisibility(match s.visibility {
                Visibility::Hidden => Visibility::Everyone,
                Visibility::Everyone => Visibility::Hidden,
            })],
            _ => {
                self.error("Not connected to ldropd.");
                Vec::new()
            }
        }
    }

    fn devices_key(&mut self, key: KeyEvent) -> Vec<Command> {
        let last = self.devices.len().saturating_sub(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.device_sel = self.device_sel.min(last).saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.device_sel = (self.device_sel + 1).min(last),
            KeyCode::Home | KeyCode::Char('g') => self.device_sel = 0,
            KeyCode::End | KeyCode::Char('G') => self.device_sel = last,
            KeyCode::Enter | KeyCode::Char('t') => {
                let Some((_, device)) = self.selected_device() else { return Vec::new() };
                let device = device.clone();
                if key.code == KeyCode::Enter && !self.preselected.is_empty() {
                    let paths = self.preselected.clone();
                    return self.start_send(
                        &device,
                        describe_paths(&paths),
                        Command::SendFiles { device: device.id.clone(), paths },
                    );
                }
                let mode = if key.code == KeyCode::Enter { SendMode::Files } else { SendMode::Text };
                self.dialog = Some(Dialog::Send(SendDialog {
                    device,
                    mode,
                    input: LineInput::default(),
                    paths: Vec::new(),
                    candidates: Vec::new(),
                    error: None,
                }));
            }
            _ => {}
        }
        Vec::new()
    }

    fn start_send(&mut self, device: &Device, what: String, command: Command) -> Vec<Command> {
        self.dialog = Some(Dialog::Sending(Sending { device: device.name.clone(), what, transfer: None, error: None }));
        vec![command]
    }

    fn move_transfer(&mut self, delta: isize) {
        let list = self.transfer_list();
        let Some((i, _)) = self.selected_transfer() else { return };
        let j = i.saturating_add_signed(delta).min(list.len() - 1);
        self.transfer_sel = Some(list[j].id);
    }

    fn transfers_key(&mut self, key: KeyEvent) -> Vec<Command> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_transfer(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_transfer(1),
            KeyCode::Home | KeyCode::Char('g') => self.move_transfer(isize::MIN),
            KeyCode::End | KeyCode::Char('G') => self.move_transfer(isize::MAX),
            KeyCode::Char('x') => return vec![Command::ClearHistory],
            KeyCode::Char(c @ ('c' | 'a' | 'd' | 'o' | 'y')) => {
                let Some((_, t)) = self.selected_transfer() else { return Vec::new() };
                let t = t.clone();
                return self.transfer_action(c, &t);
            }
            _ => {}
        }
        Vec::new()
    }

    fn transfer_action(&mut self, key: char, t: &TransferInfo) -> Vec<Command> {
        match key {
            'c' if t.is_active() => vec![Command::Cancel(t.id)],
            'c' => {
                self.error("This transfer is over.");
                Vec::new()
            }
            'a' | 'd' if t.needs_answer() => {
                let decision = if key == 'a' { Decision::Accept } else { Decision::Decline };
                let command = if self.prompts.iter().any(|p| p.request.id == t.id) {
                    Command::Answer(t.id, decision)
                } else {
                    Command::Decide(t.id, decision)
                };
                self.remove_prompt(t.id);
                self.answered(t.id, decision);
                vec![command]
            }
            'a' | 'd' => Vec::new(),
            'o' => match open_target(t) {
                Some(target) => vec![Command::Open(target)],
                None => {
                    self.error("Nothing to open yet.");
                    Vec::new()
                }
            },
            _ => match copy_text(t) {
                Some(text) => vec![Command::Copy(text)],
                None => {
                    self.error("Nothing to copy.");
                    Vec::new()
                }
            },
        }
    }

    fn settings_key(&mut self, key: KeyEvent) -> Vec<Command> {
        let last = Setting::ALL.len() - 1;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.settings_sel = self.settings_sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.settings_sel = (self.settings_sel + 1).min(last),
            KeyCode::Enter | KeyCode::Char(' ') => return self.change_setting(),
            _ => {}
        }
        Vec::new()
    }

    fn change_setting(&mut self) -> Vec<Command> {
        let toggle = |key: &'static str, on: bool| vec![Command::SetConfig { key, value: (!on).to_string() }];
        match self.selected_setting().1 {
            Setting::Visibility => self.toggle_visibility(),
            Setting::DeviceName | Setting::DownloadDir if self.conn != Conn::Up => {
                self.error("Not connected to ldropd.");
                Vec::new()
            }
            Setting::DeviceName => {
                let name = self.status.as_ref().map(|s| s.device_name.clone()).unwrap_or_default();
                self.dialog =
                    Some(Dialog::Edit(Edit { field: Field::DeviceName, input: LineInput::new(name), error: None }));
                Vec::new()
            }
            Setting::DownloadDir => {
                let dir = self.status.as_ref().map(|s| s.download_dir.clone()).unwrap_or_default();
                let shown = complete::tilde(Path::new(&dir), self.home.as_deref());
                self.dialog =
                    Some(Dialog::Edit(Edit { field: Field::DownloadDir, input: LineInput::new(shown), error: None }));
                Vec::new()
            }
            Setting::QuickShare => toggle("quickshare.enabled", self.config.quickshare.enabled),
            Setting::AirDrop => toggle("airdrop.enabled", self.config.airdrop.enabled),
            Setting::ExclusiveRadio if !self.config.airdrop.allow_exclusive_radio => {
                self.dialog = Some(Dialog::ConfirmExclusive);
                Vec::new()
            }
            Setting::ExclusiveRadio => toggle("airdrop.allow_exclusive_radio", true),
        }
    }

    fn dialog_key(&mut self, key: KeyEvent) -> Vec<Command> {
        let Some(dialog) = self.dialog.take() else { return Vec::new() };
        let (dialog, commands) = match dialog {
            Dialog::Send(d) => self.send_key(d, key),
            Dialog::Sending(s) => self.sending_key(s, key),
            Dialog::Edit(e) => self.edit_key(e, key),
            Dialog::ConfirmExclusive => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    (None, vec![Command::SetConfig { key: "airdrop.allow_exclusive_radio", value: "true".into() }])
                }
                KeyCode::Char('n') | KeyCode::Esc => (None, Vec::new()),
                _ => (Some(Dialog::ConfirmExclusive), Vec::new()),
            },
        };
        // Keep a dialog opened by the handler itself (e.g. Send → Sending).
        if self.dialog.is_none() {
            self.dialog = dialog;
        }
        commands
    }

    fn send_key(&mut self, mut d: SendDialog, key: KeyEvent) -> (Option<Dialog>, Vec<Command>) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return (None, Vec::new()),
            KeyCode::Char('t') if ctrl => {
                d.mode = d.mode.next();
                d.candidates.clear();
                d.error = None;
            }
            KeyCode::Tab if d.mode == SendMode::Files => {
                let completion = complete::complete(d.input.text(), self.home.as_deref());
                d.input.set(completion.text);
                d.candidates = completion.candidates;
            }
            KeyCode::Enter => return self.submit_send(d),
            KeyCode::Backspace if d.mode == SendMode::Files && d.input.is_empty() => {
                d.paths.pop();
            }
            _ => {
                if d.input.handle(key) {
                    d.candidates.clear();
                    d.error = None;
                }
            }
        }
        (Some(Dialog::Send(d)), Vec::new())
    }

    fn submit_send(&mut self, mut d: SendDialog) -> (Option<Dialog>, Vec<Command>) {
        let text = d.input.text().trim().to_owned();
        match d.mode {
            SendMode::Files if !text.is_empty() => {
                let path = complete::expand(&text, self.home.as_deref());
                if path.exists() {
                    d.paths.push(path);
                    d.input.clear();
                    d.candidates.clear();
                    d.error = None;
                } else {
                    d.error = Some(format!("Not found: {text}"));
                }
            }
            SendMode::Files if d.paths.is_empty() => d.error = Some("Type a path and press Enter to add it.".into()),
            SendMode::Files => {
                let what = describe_paths(&d.paths);
                let command = Command::SendFiles { device: d.device.id.clone(), paths: d.paths };
                return (None, self.start_send(&d.device, what, command));
            }
            SendMode::Text | SendMode::Link if text.is_empty() => {
                d.error = Some(format!("Type the {} to send.", if d.mode == SendMode::Link { "link" } else { "text" }));
            }
            SendMode::Text | SendMode::Link => {
                let is_url = d.mode == SendMode::Link;
                let what = if is_url { text.clone() } else { "some text".into() };
                let command = Command::SendText { device: d.device.id.clone(), text, is_url };
                return (None, self.start_send(&d.device, what, command));
            }
        }
        (Some(Dialog::Send(d)), Vec::new())
    }

    fn sending_key(&mut self, s: Sending, key: KeyEvent) -> (Option<Dialog>, Vec<Command>) {
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => (None, Vec::new()),
            KeyCode::Char('c') => {
                let active = s.transfer.and_then(|id| self.transfers.get(&id)).filter(|t| t.is_active());
                let commands = active.map(|t| vec![Command::Cancel(t.id)]).unwrap_or_default();
                (Some(Dialog::Sending(s)), commands)
            }
            _ => (Some(Dialog::Sending(s)), Vec::new()),
        }
    }

    fn edit_key(&mut self, mut e: Edit, key: KeyEvent) -> (Option<Dialog>, Vec<Command>) {
        match key.code {
            KeyCode::Esc => return (None, Vec::new()),
            KeyCode::Tab if e.field == Field::DownloadDir => {
                let completion = complete::complete(e.input.text(), self.home.as_deref());
                e.input.set(completion.text);
            }
            KeyCode::Enter => {
                let text = e.input.text().trim();
                match e.field {
                    _ if text.is_empty() => e.error = Some("This can't be empty.".into()),
                    Field::DeviceName => return (None, vec![Command::SetDeviceName(text.to_owned())]),
                    Field::DownloadDir => {
                        let path = complete::expand(text.trim_end_matches('/'), self.home.as_deref());
                        if path.is_absolute() {
                            return (None, vec![Command::SetDownloadDir(path.display().to_string())]);
                        }
                        e.error = Some("Use a full path, like ~/Downloads.".into());
                    }
                }
            }
            _ => {
                if e.input.handle(key) {
                    e.error = None;
                }
            }
        }
        (Some(Dialog::Edit(e)), Vec::new())
    }
}

/// `photo.jpg` or `3 files`.
fn describe_paths(paths: &[PathBuf]) -> String {
    match paths {
        [one] => one.file_name().map_or_else(|| one.display().to_string(), |n| n.to_string_lossy().into_owned()),
        many => format!("{} files", many.len()),
    }
}

fn received_paths(t: &TransferInfo) -> Vec<&str> {
    t.items.iter().filter(|i| i.text.is_none()).filter_map(|i| i.path.as_deref()).collect()
}

fn first_text(t: &TransferInfo) -> Option<&Item> {
    t.items.iter().find(|i| i.text.as_deref().is_some_and(|s| !s.is_empty()))
}

/// One file opens itself; several open their folder; a link opens in the browser.
fn open_target(t: &TransferInfo) -> Option<String> {
    if let Some(item) = first_text(t).filter(|i| i.is_url()) {
        return item.text.clone();
    }
    match received_paths(t).as_slice() {
        [] => None,
        [one] => Some((*one).to_owned()),
        [first, ..] => Path::new(first).parent().map(|p| p.display().to_string()),
    }
}

/// Text or link if there is one, else the file paths.
fn copy_text(t: &TransferInfo) -> Option<String> {
    if let Some(text) = first_text(t).and_then(|i| i.text.clone()) {
        return Some(text);
    }
    let paths = received_paths(t);
    (!paths.is_empty()).then(|| paths.join("\n"))
}

#[cfg(test)]
pub mod fixtures {
    //! Fixed sample state for unit and snapshot tests.

    use ldrop_core::capabilities::{BackendId, BackendStatus, Support};
    use ldrop_core::{DeviceKind, Direction, Medium, Peer, Platform, Protocol};

    use super::*;

    /// 2026-10-25 12:00:00 UTC.
    pub const NOW: u64 = 1_792_929_600_000_000;
    pub const SEC: u64 = 1_000_000;

    fn path(id: u32) -> ldrop_client::zbus::OwnedObjectPath {
        ldrop_client::transfer_path(&id.to_string()).unwrap()
    }

    pub fn device(id: &str, name: &str, kind: DeviceKind, platform: Platform) -> Device {
        Device {
            id: id.into(),
            name: name.into(),
            kind,
            platform,
            protocol: if platform == Platform::Apple { Protocol::AirDrop } else { Protocol::QuickShare },
            mediums: vec![Medium::Lan],
            vendor: None,
        }
    }

    pub fn peer(name: &str, platform: Platform) -> Peer {
        Peer {
            name: name.into(),
            platform,
            kind: DeviceKind::Phone,
            protocol: if platform == Platform::Apple { Protocol::AirDrop } else { Protocol::QuickShare },
            vendor: None,
        }
    }

    pub fn file(name: &str, size: u64, path: Option<&str>) -> Item {
        Item {
            name: name.into(),
            mime: "application/octet-stream".into(),
            size,
            path: path.map(Into::into),
            text: None,
        }
    }

    pub fn transfer(id: u32, direction: Direction, state: TransferState, peer: Peer, items: Vec<Item>) -> TransferInfo {
        let bytes_total = items.iter().map(|i| i.size).sum();
        TransferInfo {
            path: path(id),
            id,
            direction,
            state,
            peer,
            pin: String::new(),
            items,
            bytes_total,
            bytes_done: if state == TransferState::Completed { bytes_total } else { 0 },
            error: String::new(),
            deadline: 0,
            created_at: NOW - 60 * SEC * u64::from(10 - id.min(10)),
            preview: Vec::new(),
            preview_mime: String::new(),
        }
    }

    pub fn request(id: u32) -> IncomingRequest {
        IncomingRequest {
            transfer: path(id),
            id,
            peer: peer("Pixel 9 Pro", Platform::Android),
            pin: "4821".into(),
            items: vec![
                file("IMG_20261025_101530.jpg", 2_400_000, None),
                file("VID_20261025_101602.mp4", 24_000_000, None),
                file("notes.txt", 12_000, None),
            ],
            bytes_total: 26_412_000,
            deadline: NOW + 42 * SEC,
        }
    }

    pub fn status() -> Status {
        Status {
            device_name: "thinkpad".into(),
            visibility: Visibility::Everyone,
            visibility_expires_at: NOW + 462 * SEC,
            download_dir: "/home/me/Downloads".into(),
        }
    }

    pub fn capabilities() -> DoctorReport {
        let backend = |id, support| BackendStatus { id, support, reason: String::new() };
        DoctorReport {
            backends: vec![
                backend(BackendId::Lan, Support::Supported),
                backend(BackendId::Bt, Support::Supported),
                backend(BackendId::Wfd, Support::Unsupported),
                backend(BackendId::AwdlShared, Support::Unsupported),
                backend(BackendId::AwdlExclusive, Support::Degraded),
            ],
            ..DoctorReport::default()
        }
    }

    /// Connected, discovering, three devices, a mix of transfers.
    pub fn app() -> App {
        let mut app = App::new(NOW, Config::default(), Some("/home/me".into()), Vec::new());
        app.handle(Event::Connected { status: status(), capabilities: Some(capabilities()) });
        app.handle(Event::DiscoveryStarted);
        for d in [
            device("qs-1", "Pixel 9 Pro", DeviceKind::Phone, Platform::Android),
            device("qs-2", "Galaxy Tab S9", DeviceKind::Tablet, Platform::Android),
            device("ad-1", "Anna's iPhone", DeviceKind::Phone, Platform::Apple),
        ] {
            app.handle(Event::DeviceFound(d));
        }

        let mut done = transfer(
            1,
            Direction::Incoming,
            TransferState::Completed,
            peer("Pixel 9 Pro", Platform::Android),
            vec![file("IMG_20261024_183012.jpg", 3_100_000, Some("/home/me/Downloads/IMG_20261024_183012.jpg"))],
        );
        done.created_at = NOW - 3 * 3600 * SEC;
        let mut declined = transfer(
            2,
            Direction::Outgoing,
            TransferState::Rejected,
            peer("Galaxy S24", Platform::Android),
            vec![file("report.pdf", 812_000, Some("/home/me/Documents/report.pdf"))],
        );
        declined.error = "declined by the receiver".into();
        let mut link = transfer(
            3,
            Direction::Incoming,
            TransferState::Completed,
            peer("Pixel 9 Pro", Platform::Android),
            vec![Item {
                name: "https://example.org/recipe".into(),
                mime: ldrop_core::MIME_URL.into(),
                size: 0,
                path: None,
                text: Some("https://example.org/recipe".into()),
            }],
        );
        link.bytes_total = 0;
        let mut active = transfer(
            4,
            Direction::Outgoing,
            TransferState::Transferring,
            peer("Pixel 9 Pro", Platform::Android),
            vec![
                file("holiday.mp4", 27_000_000, Some("/home/me/Videos/holiday.mp4")),
                file("holiday.srt", 40_000, Some("/home/me/Videos/holiday.srt")),
            ],
        );
        active.pin = "4821".into();
        app.handle(Event::Transfers(vec![done, declined, link]));
        app.now = NOW - 2 * SEC;
        app.handle(Event::TransferChanged(active.clone()));
        active.bytes_done = 12_160_000;
        app.now = NOW;
        app.handle(Event::TransferChanged(active));
        app
    }
}

#[cfg(test)]
mod tests {
    use ldrop_core::Direction;

    use super::fixtures::*;
    use super::*;

    fn press(app: &mut App, code: KeyCode) -> Vec<Command> {
        app.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn typed(app: &mut App, s: &str) {
        for c in s.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    #[test]
    fn tabs_and_discovery() {
        let mut app = app();
        assert!(app.wants_discovery());
        assert_eq!(app.devices.len(), 3);
        press(&mut app, KeyCode::Char('2'));
        assert_eq!(app.tab, Tab::Transfers);
        assert!(!app.wants_discovery());
        assert!(app.devices.is_empty(), "leaving the list ends the session");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Settings);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Devices);
        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.tab, Tab::Settings);
        // Found events from a stopped session are dropped.
        app.handle(Event::DeviceFound(device(
            "x",
            "Late",
            ldrop_core::DeviceKind::Phone,
            ldrop_core::Platform::Android,
        )));
        assert!(app.devices.is_empty());
        press(&mut app, KeyCode::Char('q'));
        assert!(app.quit);
    }

    #[test]
    fn device_selection_follows_removals() {
        let mut app = app();
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.selected_device().unwrap().1.name, "Anna's iPhone");
        app.handle(Event::DeviceLost("qs-1".into()));
        assert_eq!(app.selected_device().unwrap().1.name, "Anna's iPhone");
        app.handle(Event::DeviceLost("ad-1".into()));
        assert_eq!(app.selected_device().unwrap().1.name, "Galaxy Tab S9");
    }

    #[test]
    fn send_files_dialog() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        let mut app = app();
        app.home = Some(dir.path().to_owned());
        press(&mut app, KeyCode::Enter);
        assert!(matches!(&app.dialog, Some(Dialog::Send(d)) if d.device.id == "qs-1" && d.mode == SendMode::Files));

        assert!(press(&mut app, KeyCode::Enter).is_empty(), "nothing to send yet");
        typed(&mut app, "~/a");
        press(&mut app, KeyCode::Tab);
        typed(&mut app, "x");
        press(&mut app, KeyCode::Enter);
        let Some(Dialog::Send(d)) = &app.dialog else { panic!() };
        assert_eq!(d.error.as_deref(), Some("Not found: ~/a.txtx"));
        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Enter);
        let Some(Dialog::Send(d)) = &app.dialog else { panic!() };
        assert_eq!(d.paths, [dir.path().join("a.txt")]);
        assert!(d.input.is_empty());

        let commands = press(&mut app, KeyCode::Enter);
        assert_eq!(commands, [Command::SendFiles { device: "qs-1".into(), paths: vec![dir.path().join("a.txt")] }]);
        assert!(matches!(&app.dialog, Some(Dialog::Sending(s)) if s.what == "a.txt" && s.transfer.is_none()));
        app.handle(Event::SendStarted(9));
        assert!(matches!(&app.dialog, Some(Dialog::Sending(s)) if s.transfer == Some(9)));
        assert!(press(&mut app, KeyCode::Char('c')).is_empty(), "unknown transfer: nothing to cancel");
        app.handle(Event::TransferChanged(transfer(
            9,
            Direction::Outgoing,
            TransferState::AwaitingAcceptance,
            peer("Pixel 9 Pro", ldrop_core::Platform::Android),
            vec![],
        )));
        assert_eq!(press(&mut app, KeyCode::Char('c')), [Command::Cancel(9)]);
        press(&mut app, KeyCode::Esc);
        assert!(app.dialog.is_none());
    }

    #[test]
    fn send_text_and_link() {
        let mut app = app();
        press(&mut app, KeyCode::Char('t'));
        typed(&mut app, "hello");
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            [Command::SendText { device: "qs-1".into(), text: "hello".into(), is_url: false }]
        );
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Enter);
        app.handle(Event::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)));
        app.handle(Event::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)));
        typed(&mut app, "https://example.org");
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            [Command::SendText { device: "qs-1".into(), text: "https://example.org".into(), is_url: true }]
        );
    }

    #[test]
    fn preselected_files_send_on_enter() {
        let mut app = app();
        app.preselected = vec!["/tmp/a.jpg".into(), "/tmp/b.jpg".into()];
        press(&mut app, KeyCode::Char('j'));
        let commands = press(&mut app, KeyCode::Enter);
        assert_eq!(commands, [Command::SendFiles { device: "qs-2".into(), paths: app.preselected.clone() }]);
        assert!(matches!(&app.dialog, Some(Dialog::Sending(s)) if s.what == "2 files"));
        app.handle(Event::SendFailed("device went away".into()));
        assert!(matches!(&app.dialog, Some(Dialog::Sending(s)) if s.error.as_deref() == Some("device went away")));
    }

    #[test]
    fn incoming_prompt() {
        let mut app = app();
        assert_eq!(app.handle(Event::Incoming(request(7))), [Command::Bell]);
        assert!(app.handle(Event::Incoming(request(7))).is_empty(), "duplicates ignored");
        // Keys pressed right as the prompt appears don't answer it.
        assert!(press(&mut app, KeyCode::Char('a')).is_empty());
        assert!(app.prompt().is_some());
        app.now += 2 * SEC;
        // Other keys are swallowed while the prompt is up.
        press(&mut app, KeyCode::Char('2'));
        assert_eq!(app.tab, Tab::Devices);
        assert_eq!(press(&mut app, KeyCode::Char('a')), [Command::Answer(7, Decision::Accept)]);
        assert!(app.prompt().is_none());
        assert!(app.message().unwrap().text.contains("Transfers"));

        app.handle(Event::Incoming(request(8)));
        app.handle(Event::PromptGone(8));
        assert!(app.prompt().is_none());
        app.handle(Event::Incoming(request(9)));
        app.handle(Event::Disconnected("ldropd stopped".into()));
        assert!(app.prompt().is_none());
    }

    #[test]
    fn transfer_actions() {
        let mut app = app();
        press(&mut app, KeyCode::Char('2'));
        assert_eq!(app.selected_transfer().unwrap().1.id, 4, "newest first");
        assert_eq!(press(&mut app, KeyCode::Char('c')), [Command::Cancel(4)]);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(press(&mut app, KeyCode::Char('o')), [Command::Open("https://example.org/recipe".into())]);
        assert_eq!(press(&mut app, KeyCode::Char('y')), [Command::Copy("https://example.org/recipe".into())]);
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(app.selected_transfer().unwrap().1.id, 1);
        assert_eq!(
            press(&mut app, KeyCode::Char('o')),
            [Command::Open("/home/me/Downloads/IMG_20261024_183012.jpg".into())]
        );
        assert!(press(&mut app, KeyCode::Char('c')).is_empty());
        assert!(app.message().unwrap().error);
        assert_eq!(press(&mut app, KeyCode::Char('x')), [Command::ClearHistory]);

        // A waiting transfer without a prompt here is decided through the manager.
        let mut waiting = transfer(
            5,
            Direction::Incoming,
            TransferState::AwaitingAcceptance,
            peer("Pixel 9 Pro", ldrop_core::Platform::Android),
            vec![],
        );
        waiting.deadline = NOW + 30 * SEC;
        app.handle(Event::TransferChanged(waiting));
        press(&mut app, KeyCode::Char('g'));
        assert_eq!(press(&mut app, KeyCode::Char('d')), [Command::Decide(5, Decision::Decline)]);
    }

    #[test]
    fn speed_is_smoothed() {
        let app = app();
        // 12.16 MB in 2 s.
        assert_eq!(app.rate(4), Some(6_080_000.0));
        assert_eq!(app.rate(1), None);
    }

    #[test]
    fn settings() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app();
        press(&mut app, KeyCode::Char('3'));
        assert_eq!(press(&mut app, KeyCode::Enter), [Command::SetVisibility(Visibility::Hidden)]);

        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Enter);
        app.handle(Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)));
        assert!(press(&mut app, KeyCode::Enter).is_empty());
        typed(&mut app, "  laptop ");
        assert_eq!(press(&mut app, KeyCode::Enter), [Command::SetDeviceName("laptop".into())]);
        assert!(app.dialog.is_none());

        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Enter);
        assert!(matches!(&app.dialog, Some(Dialog::Edit(e)) if e.input.text() == "~/Downloads"));
        app.handle(Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)));
        typed(&mut app, "relative");
        assert!(press(&mut app, KeyCode::Enter).is_empty());
        app.handle(Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)));
        typed(&mut app, &format!("{}/", dir.path().display()));
        assert_eq!(press(&mut app, KeyCode::Enter), [Command::SetDownloadDir(dir.path().display().to_string())]);

        press(&mut app, KeyCode::Char('j'));
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            [Command::SetConfig { key: "quickshare.enabled", value: "false".into() }]
        );
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('j'));
        assert!(press(&mut app, KeyCode::Enter).is_empty());
        assert_eq!(app.dialog, Some(Dialog::ConfirmExclusive));
        assert!(press(&mut app, KeyCode::Char('x')).is_empty());
        assert_eq!(
            press(&mut app, KeyCode::Char('y')),
            [Command::SetConfig { key: "airdrop.allow_exclusive_radio", value: "true".into() }]
        );
        let mut config = app.config.clone();
        config.airdrop.allow_exclusive_radio = true;
        app.handle(Event::Config(config));
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            [Command::SetConfig { key: "airdrop.allow_exclusive_radio", value: "false".into() }]
        );
    }

    #[test]
    fn visibility_needs_a_connection() {
        let mut app = App::new(NOW, Config::default(), None, Vec::new());
        assert!(press(&mut app, KeyCode::Char('v')).is_empty());
        assert!(app.message().unwrap().error);
        app.now += 5 * SEC;
        assert!(app.message().is_none(), "messages expire");
    }
}
