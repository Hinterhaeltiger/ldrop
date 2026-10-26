// SPDX-License-Identifier: GPL-3.0-or-later
//! The async side: the daemon connection and its streams, terminal input, a clock, and running
//! the commands the app returns. Everything reaches the app as an [`Event`] on one channel.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{Event as TermEvent, EventStream};
use futures_util::StreamExt;
use futures_util::stream::{BoxStream, select_all};
use ldrop_client::{
    Client, Decision, DiscoveryEvent, Error, PendingRequest, TransferEvent, TransferWatch, transfer_id, transfer_path,
};
use ldrop_core::config;
use ldrop_core::paths::Paths;
use ldrop_core::time::now_us;
use ratatui::DefaultTerminal;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::app::{App, Command, Event};
use crate::theme::Theme;
use crate::ui;

const TICK: Duration = Duration::from_secs(1);
const RECONNECT_DELAY: Duration = Duration::from_secs(3);
/// Events handled per redraw at most, so a burst of progress updates can't starve input.
const MAX_BATCH: usize = 256;

type Events = mpsc::UnboundedSender<Event>;
/// Answer channels of the prompts this TUI got as the Agent, by transfer id.
type Answers = Arc<Mutex<HashMap<u32, oneshot::Sender<Decision>>>>;

pub async fn run(preselected: Vec<PathBuf>) -> anyhow::Result<()> {
    let config_file = Paths::from_env().config_file;
    let config = config::load(&config_file).map(|l| l.config).unwrap_or_default();
    let theme = Theme::from_env(&config.tui);
    let app = App::new(now_us(), config, dirs::home_dir(), preselected);

    let (tx, rx) = mpsc::unbounded_channel();
    let (client_tx, client) = watch::channel(None);
    let answers = Answers::default();
    tokio::spawn(read_terminal(tx.clone()));
    tokio::spawn(tick(tx.clone()));
    tokio::spawn(supervise(tx.clone(), client_tx, answers.clone()));

    let mut terminal = ratatui::try_init()?;
    let mut runtime = Runtime { tx, client, answers, config_file, discovery: None };
    let result = runtime.event_loop(&mut terminal, app, rx, &theme).await;
    ratatui::restore();
    result
}

struct Runtime {
    tx: Events,
    client: watch::Receiver<Option<Client>>,
    answers: Answers,
    config_file: PathBuf,
    discovery: Option<(oneshot::Sender<()>, JoinHandle<()>)>,
}

impl Runtime {
    async fn event_loop(
        &mut self,
        terminal: &mut DefaultTerminal,
        mut app: App,
        mut rx: mpsc::UnboundedReceiver<Event>,
        theme: &Theme,
    ) -> anyhow::Result<()> {
        loop {
            terminal.draw(|f| ui::render(f, &app, theme))?;
            let Some(event) = rx.recv().await else { return Ok(()) };
            let mut commands = Vec::new();
            app.now = now_us();
            commands.extend(app.handle(event));
            for _ in 1..MAX_BATCH {
                let Ok(event) = rx.try_recv() else { break };
                app.now = now_us();
                commands.extend(app.handle(event));
            }
            for command in commands {
                self.exec(command);
            }
            self.sync_discovery(&app);
            if app.quit {
                return Ok(());
            }
        }
    }

    /// Starts or stops the discovery session to match what the app shows.
    fn sync_discovery(&mut self, app: &App) {
        let running = self.discovery.as_ref().is_some_and(|(_, task)| !task.is_finished());
        if app.wants_discovery() && !running {
            let Some(client) = self.client.borrow().clone() else { return };
            let (stop_tx, stop_rx) = oneshot::channel();
            self.discovery = Some((stop_tx, tokio::spawn(discover(client, self.tx.clone(), stop_rx))));
        } else if !app.wants_discovery()
            && let Some((stop, _)) = self.discovery.take()
        {
            let _ = stop.send(());
        }
    }

    fn exec(&mut self, command: Command) {
        match command {
            Command::Bell => {
                let mut out = std::io::stdout();
                let _ = out.write_all(b"\x07").and_then(|()| out.flush());
            }
            Command::Answer(id, decision) => {
                let pending = self.answers.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                match pending {
                    Some(answer) => {
                        let _ = answer.send(decision);
                    }
                    None => self.call(move |c| async move { decide(&c, id, decision).await.map(|()| None) }),
                }
            }
            Command::Decide(id, decision) => {
                self.call(move |c| async move { decide(&c, id, decision).await.map(|()| None) })
            }
            Command::SetVisibility(mode) => {
                self.call(move |c| async move { c.set_visibility(mode, Duration::ZERO).await.map(|()| None) })
            }
            Command::SendFiles { device, paths } => self.call(move |c| async move {
                Ok(Some(match c.send_files(&device, &paths).await {
                    Ok(path) => Event::SendStarted(transfer_id(path.as_str()).unwrap_or(0)),
                    Err(e) => Event::SendFailed(describe(&e)),
                }))
            }),
            Command::SendText { device, text, is_url } => self.call(move |c| async move {
                Ok(Some(match c.send_text(&device, &text, is_url).await {
                    Ok(path) => Event::SendStarted(transfer_id(path.as_str()).unwrap_or(0)),
                    Err(e) => Event::SendFailed(describe(&e)),
                }))
            }),
            Command::Cancel(id) => self.call(move |c| async move {
                c.cancel(&path(id)?.as_ref()).await.map(|()| Some(Event::Info("Cancelled.".into())))
            }),
            Command::ClearHistory => {
                self.call(
                    |c| async move { c.clear_history().await.map(|()| Some(Event::Info("History cleared.".into()))) },
                )
            }
            Command::SetDeviceName(name) => self.call(move |c| async move {
                c.manager().set_device_name(&name).await.map_err(Error::from)?;
                Ok(Some(Event::Info("Device name saved.".into())))
            }),
            Command::SetDownloadDir(dir) => self.call(move |c| async move {
                c.manager().set_download_dir(&dir).await.map_err(Error::from)?;
                Ok(Some(Event::Info("Download folder saved.".into())))
            }),
            Command::SetConfig { key, value } => {
                let (tx, file) = (self.tx.clone(), self.config_file.clone());
                tokio::spawn(async move {
                    let saved = tokio::task::spawn_blocking(move || config::set(&file, key, &value)).await;
                    let events = match saved {
                        Ok(Ok(config)) => {
                            let note = if key == "quickshare.enabled" && config.quickshare.enabled {
                                "Saved. Restart ldropd to turn Quick Share on."
                            } else {
                                "Saved."
                            };
                            vec![Event::Config(config), Event::Info(note.into())]
                        }
                        Ok(Err(e)) => vec![Event::Error(e.to_string())],
                        Err(e) => vec![Event::Error(e.to_string())],
                    };
                    for event in events {
                        let _ = tx.send(event);
                    }
                });
            }
            Command::Open(target) => {
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = open(&target).await {
                        let _ = tx.send(Event::Error(e));
                    }
                });
            }
            Command::Copy(text) => {
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(match copy(&text).await {
                        Ok(()) => Event::Info("Copied.".into()),
                        Err(e) => Event::Error(e),
                    });
                });
            }
        }
    }

    /// Runs a daemon call in the background; its event, or its error, comes back on the channel.
    fn call<F, Fut>(&self, f: F)
    where
        F: FnOnce(Client) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Option<Event>, Error>> + Send,
    {
        let tx = self.tx.clone();
        let Some(client) = self.client.borrow().clone() else {
            let _ = tx.send(Event::Error("Not connected to ldropd.".into()));
            return;
        };
        tokio::spawn(async move {
            match f(client).await {
                Ok(Some(event)) => {
                    let _ = tx.send(event);
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = tx.send(Event::Error(describe(&e)));
                }
            }
        });
    }
}

fn path(id: u32) -> Result<ldrop_client::zbus::OwnedObjectPath, Error> {
    transfer_path(&id.to_string()).ok_or_else(|| Error::Failed(format!("bad transfer id {id}")))
}

async fn decide(client: &Client, id: u32, decision: Decision) -> Result<(), Error> {
    let path = path(id)?;
    match decision {
        Decision::Accept => client.accept(&path.as_ref()).await,
        Decision::Decline => client.reject(&path.as_ref()).await,
    }
}

fn describe(e: &Error) -> String {
    if e.is_daemon_missing() {
        "ldropd is not running and could not be started (try `systemctl --user start ldropd`).".into()
    } else {
        e.message()
    }
}

async fn read_terminal(tx: Events) {
    let mut events = EventStream::new();
    while let Some(Ok(event)) = events.next().await {
        let event = match event {
            TermEvent::Key(key) => Event::Key(key),
            TermEvent::Resize(..) => Event::Tick,
            _ => continue,
        };
        if tx.send(event).is_err() {
            return;
        }
    }
}

async fn tick(tx: Events) {
    let mut interval = tokio::time::interval(TICK);
    loop {
        interval.tick().await;
        if tx.send(Event::Tick).is_err() {
            return;
        }
    }
}

/// Keeps a connection to the daemon: connects, follows it until the daemon leaves the bus, and
/// tries again. The daemon is D-Bus activated, so connecting also starts it.
async fn supervise(tx: Events, client_tx: watch::Sender<Option<Client>>, answers: Answers) {
    loop {
        let reason = match Client::connect().await {
            Ok(client) => follow(&client, &tx, &client_tx, &answers).await,
            Err(e) => describe(&e),
        };
        client_tx.send_replace(None);
        if tx.send(Event::Disconnected(reason)).is_err() {
            return;
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// Runs one daemon session; returns why it ended.
async fn follow(client: &Client, tx: &Events, client_tx: &watch::Sender<Option<Client>>, answers: &Answers) -> String {
    let setup = async {
        let status = client.status().await?;
        let capabilities = client.capabilities().await.ok();
        let watch = client.watch_transfers().await?;
        let agent = client.register_agent_channel().await?;
        Ok::<_, Error>((status, capabilities, watch, agent))
    };
    let (status, capabilities, watch, (_registration, requests)) = match setup.await {
        Ok(parts) => parts,
        Err(e) => return describe(&e),
    };
    client_tx.send_replace(Some(client.clone()));
    let _ = tx.send(Event::Connected { status, capabilities });
    if let Ok(link) = client.airdrop_link().await {
        let _ = tx.send(Event::AirDropLink(link));
    }
    let _ = tx.send(Event::Transfers(watch.transfers().cloned().collect()));

    tokio::select! {
        _ = client.daemon_exited() => "ldropd stopped.".into(),
        () = follow_transfers(watch, tx.clone()) => "Lost the connection to ldropd.".into(),
        () = serve_agent(requests, tx.clone(), answers.clone()) => "Lost the connection to ldropd.".into(),
        () = follow_status(client, tx.clone()) => "Lost the connection to ldropd.".into(),
    }
}

async fn follow_transfers(mut watch: TransferWatch, tx: Events) {
    while let Some(change) = watch.next().await {
        let event = match change {
            TransferEvent::Added(t) | TransferEvent::Changed(t) => Event::TransferChanged(t),
            TransferEvent::Removed(id) => Event::TransferRemoved(id),
        };
        if tx.send(event).is_err() {
            return;
        }
    }
}

async fn follow_status(client: &Client, tx: Events) {
    let manager = client.manager();
    let mut status: Vec<BoxStream<'_, ()>> = vec![
        manager.receive_device_name_changed().await.map(|_| ()).boxed(),
        manager.receive_visibility_changed().await.map(|_| ()).boxed(),
        manager.receive_visibility_expires_at_changed().await.map(|_| ()).boxed(),
        manager.receive_download_dir_changed().await.map(|_| ()).boxed(),
    ];
    let mut status = select_all(status.drain(..));
    let mut capabilities = manager.receive_capabilities_changed().await;
    let mut link = manager.receive_air_drop_link_changed().await;
    loop {
        let event = tokio::select! {
            Some(()) = status.next() => client.status().await.map(Event::Status),
            Some(_) = link.next() => client.airdrop_link().await.map(Event::AirDropLink),
            Some(_) = capabilities.next() => client.capabilities().await.map(Event::Capabilities),
            else => return,
        };
        if let Ok(event) = event
            && tx.send(event).is_err()
        {
            return;
        }
    }
}

async fn serve_agent(mut requests: mpsc::Receiver<PendingRequest>, tx: Events, answers: Answers) {
    while let Some(mut pending) = requests.recv().await {
        let id = pending.request.id;
        let (answer_tx, answer_rx) = oneshot::channel();
        answers.lock().unwrap_or_else(|e| e.into_inner()).insert(id, answer_tx);
        let _ = tx.send(Event::Incoming(pending.request.clone()));
        let (tx, answers) = (tx.clone(), answers.clone());
        tokio::spawn(async move {
            let decision = tokio::select! {
                Ok(decision) = answer_rx => Some(decision),
                () = pending.expired() => None,
            };
            if let Some(decision) = decision {
                pending.respond(decision);
            }
            answers.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            let _ = tx.send(Event::PromptGone(id));
        });
    }
}

async fn discover(client: Client, tx: Events, stop: oneshot::Receiver<()>) {
    let mut session = match client.discover().await {
        Ok(session) => session,
        Err(e) => {
            let _ = tx.send(Event::Error(format!("Cannot search for devices: {}", describe(&e))));
            return;
        }
    };
    let _ = tx.send(Event::DiscoveryStarted);
    let mut stop = std::pin::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            event = session.next() => {
                let event = match event {
                    Some(DiscoveryEvent::Found(device)) => Event::DeviceFound(device),
                    Some(DiscoveryEvent::Lost(id)) => Event::DeviceLost(id),
                    None => return,
                };
                if tx.send(event).is_err() {
                    break;
                }
            }
        }
    }
    let _ = session.stop().await;
}

async fn open(target: &str) -> Result<(), String> {
    let status = tokio::process::Command::new("xdg-open")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(|e| format!("Cannot run xdg-open: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("xdg-open could not open {target}")) }
}

/// Copies through `wl-copy` on Wayland, else `xclip`.
async fn copy(text: &str) -> Result<(), String> {
    const WL_COPY: (&str, &[&str]) = ("wl-copy", &[]);
    const XCLIP: (&str, &[&str]) = ("xclip", &["-selection", "clipboard"]);
    let tools = if std::env::var_os("WAYLAND_DISPLAY").is_some() { [WL_COPY, XCLIP] } else { [XCLIP, WL_COPY] };
    for (program, args) in tools {
        let child = tokio::process::Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("Cannot run {program}: {e}")),
        };
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes()).await.map_err(|e| format!("{program}: {e}"))?;
        }
        let status = child.wait().await.map_err(|e| format!("{program}: {e}"))?;
        return if status.success() { Ok(()) } else { Err(format!("{program} failed")) };
    }
    Err("Install wl-clipboard or xclip to copy.".into())
}
