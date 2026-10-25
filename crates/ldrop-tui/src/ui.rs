// SPDX-License-Identifier: GPL-3.0-or-later
//! Drawing (SPEC §11.3): a pure function of the app state, so snapshot tests cover every screen.

use std::ops::Range;
use std::path::Path;
use std::time::Duration;

use ldrop_client::{IncomingRequest, TransferInfo};
use ldrop_core::capabilities::{BackendId, Support};
use ldrop_core::time::format_countdown;
use ldrop_core::{Device, Direction, Item, Peer, TransferState, Visibility, describe_items, format_size};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Conn, Dialog, Edit, Field, SendDialog, SendMode, Sending, Setting, Tab};
use crate::complete::tilde;
use crate::theme::Theme;

const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 12;
const MODAL_WIDTH: u16 = 64;
const PROMPT_ITEMS: usize = 5;
const SEND_PATHS: usize = 5;

pub fn render(f: &mut Frame, app: &App, theme: &Theme) {
    let area = f.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        f.render_widget(Paragraph::new("Terminal too small for ldrop-tui."), area);
        return;
    }
    let block = Block::bordered()
        .border_set(theme.border())
        .padding(Padding::horizontal(1))
        .title_top(header(app, theme))
        .title_top(Line::from(backends(app)).right_aligned())
        .title_bottom(footer(app, theme));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [tabs, _, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
    f.render_widget(tab_bar(app, theme), tabs);
    match app.tab {
        Tab::Devices => devices(f, body, app, theme),
        Tab::Transfers => transfers(f, body, app, theme),
        Tab::Settings => settings(f, body, app, theme),
    }

    let mut cursor = app.dialog.as_ref().and_then(|d| dialog(f, area, app, theme, d));
    if app.help {
        help(f, area, theme);
        cursor = None;
    }
    if let Some((request, count)) = app.prompt() {
        prompt(f, area, app, theme, request, count);
        cursor = None;
    }
    if let Some(position) = cursor {
        f.set_cursor_position(position);
    }
}

fn header(app: &App, theme: &Theme) -> Line<'static> {
    let sep = format!(" {} ", theme.symbols().sep);
    let mut spans = vec![Span::raw(" "), Span::styled("ldrop", theme.title())];
    match (&app.conn, &app.status) {
        (Conn::Up, Some(status)) => {
            spans.push(Span::raw(sep.clone()));
            spans.push(Span::raw(fit(&status.device_name, 24, theme)));
            spans.push(Span::raw(sep));
            spans.push(match (status.visibility, app.visibility_left()) {
                (Visibility::Everyone, Some(left)) => {
                    Span::styled(format!("Everyone {}", format_countdown(left)), theme.accent())
                }
                (Visibility::Everyone, None) => Span::styled("Everyone", theme.accent()),
                (Visibility::Hidden, _) => Span::raw("Hidden"),
            });
        }
        (Conn::Down(_), _) => {
            spans.push(Span::raw(sep));
            spans.push(Span::styled("ldropd not running", theme.error()));
        }
        _ => {
            spans.push(Span::raw(sep));
            spans.push(Span::raw(format!("connecting{}", theme.symbols().ellipsis)));
        }
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// ` QS:lan,bt  AirDrop:off `: what each protocol can use on this machine.
fn backends(app: &App) -> String {
    let Some(caps) = app.capabilities.as_ref().filter(|_| app.conn == Conn::Up) else { return String::new() };
    let usable = |id: &BackendId| caps.backend(*id).is_some_and(|b| b.support != Support::Unsupported);
    let quickshare = if app.config.quickshare.enabled {
        let ids: Vec<&str> = [BackendId::Lan, BackendId::Bt, BackendId::Wfd, BackendId::Hotspot]
            .iter()
            .filter(|id| usable(id))
            .map(|id| id.as_str())
            .collect();
        if ids.is_empty() { "none".to_owned() } else { ids.join(",") }
    } else {
        "off".to_owned()
    };
    let airdrop = if app.config.airdrop.enabled {
        let exclusive = app.config.airdrop.allow_exclusive_radio;
        [BackendId::AwdlShared, BackendId::AwdlExclusive, BackendId::AwdlActivemon, BackendId::Nan]
            .iter()
            .filter(|id| **id != BackendId::AwdlExclusive || exclusive)
            .find(|id| usable(id))
            .map_or("none", |id| id.as_str())
    } else {
        "off"
    };
    format!(" QS:{quickshare}  AirDrop:{airdrop} ")
}

fn footer(app: &App, theme: &Theme) -> Line<'static> {
    if let Some(message) = app.message() {
        let style = if message.error { theme.error() } else { theme.accent() };
        return Line::from(vec![Span::raw(" "), Span::styled(message.text.clone(), style), Span::raw(" ")]);
    }
    let sym = theme.symbols();
    let keys: &[&str] = match app.tab {
        Tab::Devices => &["/jk move", "Enter send", "t text", "v visibility", "? help", "q quit"],
        Tab::Transfers => &[" move", "c cancel", "o open", "y copy", "x clear", "? help", "q quit"],
        Tab::Settings => &[" move", "Enter change", "v visibility", "? help", "q quit"],
    };
    let sep = format!(" {} ", sym.sep);
    Line::from(format!(" {}{} ", sym.up_down, keys.join(&sep))).style(theme.dim())
}

fn tab_bar(app: &App, theme: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, tab) in Tab::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        let label = format!("[{}] {}", i + 1, tab.title());
        spans.push(if *tab == app.tab { Span::styled(label, theme.selected()) } else { Span::raw(label) });
    }
    let active = app.transfers.values().filter(|t| t.is_active()).count();
    if active > 0 {
        spans.push(Span::styled(format!("  {active} active"), theme.accent()));
    }
    Line::from(spans)
}

fn devices(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let sym = theme.symbols();
    let width = area.width as usize;
    let mut lines = Vec::new();
    if !app.preselected.is_empty() {
        let what = match app.preselected.as_slice() {
            [one] => one.file_name().map_or_else(|| one.display().to_string(), |n| n.to_string_lossy().into_owned()),
            many => format!("{} files", many.len()),
        };
        let text = format!("Sending {what} {} pick a device and press Enter", sym.dash);
        lines.push(Line::styled(fit(&text, width, theme), theme.accent()));
        lines.push(Line::default());
    }
    match &app.conn {
        Conn::Down(reason) => {
            lines.push(Line::styled("Not connected to ldropd.", theme.error()));
            lines.extend(wrap(reason, width).into_iter().map(|l| Line::styled(l, theme.dim())));
            lines.push(Line::styled(format!("Retrying{}", sym.ellipsis), theme.dim()));
        }
        Conn::Connecting => lines.push(Line::raw(format!("Connecting to ldropd{}", sym.ellipsis))),
        Conn::Up if app.devices.is_empty() => {
            lines.push(Line::raw(format!("Looking for nearby devices{}", sym.ellipsis)));
            lines.push(Line::default());
            let hint = "Phone not listed? On Android, open Quick Share and set it to Everyone.";
            lines.extend(wrap(hint, width).into_iter().map(|l| Line::styled(l, theme.dim())));
        }
        Conn::Up => {
            let room = (area.height as usize).saturating_sub(lines.len() + 2).max(1);
            let selected = app.selected_device().map_or(0, |(i, _)| i);
            for i in window(app.devices.len(), selected, room) {
                lines.push(device_row(&app.devices[i], i == selected, width, theme));
            }
            if app.discovering {
                lines.push(Line::default());
                lines.push(Line::styled(format!("Searching{}", sym.ellipsis), theme.dim()));
            }
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn device_row(d: &Device, selected: bool, width: usize, theme: &Theme) -> Line<'static> {
    let pointer = if selected { theme.symbols().pointer } else { " " };
    let name_width = width.saturating_sub(2 + 9 + 9 + 11).clamp(8, 32);
    let text = format!(
        "{pointer} {} {:<8} {:<8} {}",
        pad(&d.name, name_width, theme),
        d.kind.as_str(),
        d.platform.label(),
        d.protocol.label()
    );
    let text = pad(&text, width, theme);
    if selected { Line::styled(text, theme.selected()) } else { Line::raw(text) }
}

fn transfers(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let width = area.width as usize;
    let list = app.transfer_list();
    if list.is_empty() {
        let mut lines = vec![Line::raw("No transfers yet."), Line::default()];
        let hint = "Pick a device in [1] Devices to send, or press v so nearby devices can send to this computer.";
        lines.extend(wrap(hint, width).into_iter().map(|l| Line::styled(l, theme.dim())));
        f.render_widget(Paragraph::new(lines), area);
        return;
    }
    let details_height = 8.min(area.height / 2);
    let [list_area, details_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(details_height)]).areas(area);

    let selected = app.selected_transfer().map_or(0, |(i, _)| i);
    let rows: Vec<Line> = window(list.len(), selected, list_area.height as usize)
        .map(|i| transfer_row(app, list[i], i == selected, width, theme))
        .collect();
    f.render_widget(Paragraph::new(rows), list_area);

    if let Some((_, t)) = app.selected_transfer() {
        let block = Block::new().borders(Borders::TOP).border_set(theme.border()).border_style(theme.dim());
        let inner = block.inner(details_area);
        f.render_widget(block, details_area);
        let lines = transfer_details(app, t, inner.width as usize, inner.height as usize, theme);
        f.render_widget(Paragraph::new(lines), inner);
    }
}

fn transfer_row(app: &App, t: &TransferInfo, selected: bool, width: usize, theme: &Theme) -> Line<'static> {
    let sym = theme.symbols();
    let pointer = if selected { sym.pointer } else { " " };
    let arrow = match t.direction {
        Direction::Incoming => sym.incoming,
        Direction::Outgoing => sym.outgoing,
    };
    let what_width = width.saturating_sub(46).clamp(10, 40);
    let lead = format!(
        "{pointer} {arrow} {} {} ",
        pad(peer_name(&t.peer), 20, theme),
        pad(&what(&t.items, t.bytes_total), what_width, theme)
    );
    let (state, style) = state(app, t, theme);
    let state = pad(&state, width.saturating_sub(lead.width()), theme);
    if selected {
        Line::styled(format!("{lead}{state}"), theme.selected())
    } else {
        Line::from(vec![Span::raw(lead), Span::styled(state, style)])
    }
}

fn state(app: &App, t: &TransferInfo, theme: &Theme) -> (String, Style) {
    let ellipsis = theme.symbols().ellipsis;
    match (t.state, t.direction) {
        (TransferState::Connecting, _) => (format!("connecting{ellipsis}"), theme.dim()),
        (TransferState::AwaitingAcceptance, Direction::Incoming) => {
            let left = app.left(t.deadline).map(|d| format!(" {}", format_countdown(d))).unwrap_or_default();
            (format!("waiting for you{left}"), theme.warn())
        }
        (TransferState::AwaitingAcceptance, Direction::Outgoing) => ("waiting for answer".into(), theme.warn()),
        (TransferState::Transferring, _) => (format!("{}%", percent(t)), theme.accent()),
        (TransferState::Completed, _) => ("done".into(), theme.ok()),
        (TransferState::Rejected, _) => ("declined".into(), theme.warn()),
        (TransferState::Cancelled, _) => ("cancelled".into(), theme.dim()),
        (TransferState::Failed, _) => ("failed".into(), theme.error()),
    }
}

fn transfer_details(app: &App, t: &TransferInfo, width: usize, height: usize, theme: &Theme) -> Vec<Line<'static>> {
    let sym = theme.symbols();
    let mut lines = Vec::new();
    let direction = match t.direction {
        Direction::Incoming => "From",
        Direction::Outgoing => "To",
    };
    let head = format!("{direction} {} {} {}", peer_label(&t.peer, theme), sym.sep, age(app.now, t.created_at));
    lines.push(Line::styled(fit(&head, width, theme), theme.bold()));

    match (t.state, t.direction) {
        (TransferState::Transferring, _) => lines.push(progress(app, t, width, theme)),
        (TransferState::AwaitingAcceptance, Direction::Incoming) => {
            let left = app.left(t.deadline).map(|d| format!(" {} {} left", sym.sep, format_countdown(d)));
            let text = format!(
                "Waiting for your answer{} {} a accept {} d decline",
                left.unwrap_or_default(),
                sym.sep,
                sym.sep
            );
            lines.push(Line::styled(fit(&text, width, theme), theme.warn()));
        }
        (TransferState::AwaitingAcceptance, Direction::Outgoing) => {
            let text = format!("Waiting for {} to accept{}", peer_name(&t.peer), sym.ellipsis);
            lines.push(Line::styled(fit(&text, width, theme), theme.warn()));
        }
        _ => {}
    }
    if !t.pin.is_empty() && t.is_active() {
        lines.push(Line::from(vec![Span::raw("PIN "), Span::styled(t.pin.clone(), theme.title())]));
    }
    if !t.error.is_empty() {
        let (_, style) = state(app, t, theme);
        lines.push(Line::styled(fit(&capitalize(&t.error), width, theme), style));
    }

    let room = height.saturating_sub(lines.len());
    let shown = if t.items.len() > room { room.saturating_sub(1) } else { t.items.len() };
    for item in &t.items[..shown] {
        lines.push(Line::raw(fit(&item_line(item, app, sym.arrow), width, theme)));
    }
    if shown < t.items.len() {
        lines.push(Line::styled(format!("{} and {} more", sym.ellipsis, t.items.len() - shown), theme.dim()));
    }
    lines
}

fn item_line(item: &Item, app: &App, arrow: &str) -> String {
    if let Some(text) = item.text.as_deref().filter(|t| !t.is_empty()) {
        return text.lines().next().unwrap_or_default().to_owned();
    }
    let mut line = item.name.clone();
    if item.text.is_none() {
        line = format!("{line}  {}", format_size(item.size));
    }
    if let Some(path) = &item.path {
        line = format!("{line}  {arrow} {}", tilde(Path::new(path), app.home.as_deref()));
    }
    line
}

/// `████░░░░  45%  12 MB of 27 MB · 6.1 MB/s · 0:02 left`
fn progress(app: &App, t: &TransferInfo, width: usize, theme: &Theme) -> Line<'static> {
    let sym = theme.symbols();
    let mut stats = format!(" {:>3}%  {} of {}", percent(t), format_size(t.bytes_done), format_size(t.bytes_total));
    if let Some(rate) = app.rate(t.id) {
        stats.push_str(&format!(" {} {}/s", sym.sep, format_size(rate as u64)));
        let left = t.bytes_total.saturating_sub(t.bytes_done) as f64 / rate;
        stats.push_str(&format!(" {} {} left", sym.sep, format_countdown(Duration::from_secs_f64(left))));
    }
    let bar_width = width.saturating_sub(stats.width());
    if bar_width < 10 {
        return Line::raw(fit(stats.trim_start(), width, theme));
    }
    let full = (bar_width as u64 * t.bytes_done.min(t.bytes_total) / t.bytes_total.max(1)) as usize;
    Line::from(vec![
        Span::styled(sym.bar_full.repeat(full), theme.accent()),
        Span::styled(sym.bar_empty.repeat(bar_width - full), theme.dim()),
        Span::raw(stats),
    ])
}

fn settings(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let width = area.width as usize;
    let (selected, current) = app.selected_setting();
    let on_off = |on: bool| if on { "on" } else { "off" }.to_owned();
    let mut lines = Vec::new();
    for (i, setting) in Setting::ALL.iter().enumerate() {
        let (label, value) = match setting {
            Setting::Visibility => (
                "Visibility",
                match (app.status.as_ref().map(|s| s.visibility), app.visibility_left()) {
                    (Some(Visibility::Everyone), Some(left)) => {
                        format!("Everyone {} {} left", theme.symbols().sep, format_countdown(left))
                    }
                    (Some(Visibility::Everyone), None) => "Everyone".into(),
                    (Some(Visibility::Hidden), _) => "Hidden".into(),
                    (None, _) => theme.symbols().dash.into(),
                },
            ),
            Setting::DeviceName => (
                "Device name",
                app.status.as_ref().map_or_else(|| theme.symbols().dash.into(), |s| s.device_name.clone()),
            ),
            Setting::DownloadDir => (
                "Download folder",
                app.status.as_ref().map_or_else(
                    || theme.symbols().dash.into(),
                    |s| tilde(Path::new(&s.download_dir), app.home.as_deref()),
                ),
            ),
            Setting::QuickShare => ("Quick Share", on_off(app.config.quickshare.enabled)),
            Setting::AirDrop => ("AirDrop", on_off(app.config.airdrop.enabled)),
            Setting::ExclusiveRadio => ("Exclusive radio", on_off(app.config.airdrop.allow_exclusive_radio)),
        };
        let pointer = if i == selected { theme.symbols().pointer } else { " " };
        let text = pad(&format!("{pointer} {label:<17} {value}"), width, theme);
        lines.push(if i == selected { Line::styled(text, theme.selected()) } else { Line::raw(text) });
    }
    lines.push(Line::default());
    let minutes = app.config.visibility_timeout_secs.div_ceil(60);
    let help = match current {
        Setting::Visibility => {
            format!(
                "Everyone lets all nearby devices see this computer for {minutes} minutes. Press v on any tab to switch."
            )
        }
        Setting::DeviceName => "The name nearby devices show for this computer.".into(),
        Setting::DownloadDir => "Where received files are saved.".into(),
        Setting::QuickShare => "Sharing with Android phones, tablets and Chromebooks. Turning it off applies to new \
                                connections; turning it on needs an ldropd restart."
            .into(),
        Setting::AirDrop => "Sharing with iPhone, iPad and Mac. Not available yet in this version.".into(),
        Setting::ExclusiveRadio => "Lets AirDrop use Wi-Fi cards that can't run it next to a normal connection. Wi-Fi \
                                    disconnects while Apple devices can see this computer."
            .into(),
    };
    lines.extend(wrap(&help, width).into_iter().map(|l| Line::styled(l, theme.dim())));
    f.render_widget(Paragraph::new(lines), area);
}

/// Inner width of a modal of `width` columns on `area`.
fn modal_inner(area: Rect, width: u16) -> usize {
    width.min(area.width.saturating_sub(4)).saturating_sub(4) as usize
}

/// Draws a centered box with `lines` and returns its inner area.
fn modal(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    title: &str,
    footer: &str,
    lines: Vec<Line<'static>>,
    width: u16,
) -> Rect {
    let w = width.min(area.width.saturating_sub(4));
    let h = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let rect = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, rect);
    let title = fit(title, w.saturating_sub(4) as usize, theme);
    let block = Block::bordered()
        .border_set(theme.border())
        .padding(Padding::horizontal(1))
        .title_top(Line::from(vec![Span::raw(" "), Span::styled(title, theme.title()), Span::raw(" ")]))
        .title_bottom(Line::from(format!(" {footer} ")).style(theme.dim()));
    let inner = block.inner(rect);
    f.render_widget(Paragraph::new(lines).block(block), rect);
    inner
}

fn prompt(f: &mut Frame, area: Rect, app: &App, theme: &Theme, request: &IncomingRequest, count: usize) {
    let sym = theme.symbols();
    let width = modal_inner(area, MODAL_WIDTH);
    let mut lines = vec![
        Line::from(vec![
            Span::styled(fit(peer_name(&request.peer), width.saturating_sub(24), theme), theme.bold()),
            Span::styled(
                format!(" ({} {} {})", request.peer.platform.label(), sym.sep, request.peer.protocol.label()),
                theme.dim(),
            ),
        ]),
        Line::raw(fit(&format!("wants to share {}", what(&request.items, request.bytes_total)), width, theme)),
        Line::default(),
    ];
    let shown = if request.items.len() > PROMPT_ITEMS { PROMPT_ITEMS - 1 } else { request.items.len() };
    for item in &request.items[..shown] {
        let size = if item.text.is_some() { String::new() } else { format_size(item.size) };
        let name_width = width.saturating_sub(size.width() + 3);
        lines.push(Line::raw(format!("  {} {size}", pad(&item.name, name_width, theme))));
    }
    if shown < request.items.len() {
        lines.push(Line::styled(format!("  {} and {} more", sym.ellipsis, request.items.len() - shown), theme.dim()));
    }
    lines.push(Line::default());
    if !request.pin.is_empty() {
        lines.push(Line::from(vec![
            Span::raw("PIN "),
            Span::styled(request.pin.clone(), theme.title()),
            Span::raw(format!(" {} check it matches the sender's screen", sym.dash)),
        ]));
    }
    if let Some(left) = app.left(request.deadline) {
        lines.push(Line::styled(format!("{} left to answer", format_countdown(left)), theme.warn()));
    }
    let title = if count > 1 { format!("Incoming (1 of {count})") } else { "Incoming".to_owned() };
    modal(f, area, theme, &title, &format!("a accept {} d decline", sym.sep), lines, MODAL_WIDTH);
}

fn dialog(f: &mut Frame, area: Rect, app: &App, theme: &Theme, dialog: &Dialog) -> Option<Position> {
    match dialog {
        Dialog::Send(d) => Some(send_dialog(f, area, app, theme, d)),
        Dialog::Sending(s) => {
            sending(f, area, app, theme, s);
            None
        }
        Dialog::Edit(e) => Some(edit(f, area, theme, e)),
        Dialog::ConfirmExclusive => {
            let width = modal_inner(area, MODAL_WIDTH);
            let mut lines: Vec<Line> = wrap("Wi-Fi disconnects while Apple devices can see this computer.", width)
                .into_iter()
                .map(|l| Line::styled(l, theme.warn()))
                .collect();
            lines.push(Line::default());
            lines.push(Line::raw("Allow AirDrop to take over the Wi-Fi card?"));
            let footer = format!("y allow {} n cancel", theme.symbols().sep);
            modal(f, area, theme, "Exclusive radio", &footer, lines, MODAL_WIDTH);
            None
        }
    }
}

fn input_line(input: &crate::input::LineInput, width: usize) -> (Line<'static>, u16) {
    let (shown, col) = input.view(width.saturating_sub(2));
    (Line::raw(format!("> {shown}")), col + 2)
}

fn send_dialog(f: &mut Frame, area: Rect, app: &App, theme: &Theme, d: &SendDialog) -> Position {
    let sym = theme.symbols();
    let width = modal_inner(area, MODAL_WIDTH);
    let mut modes = Vec::new();
    for mode in SendMode::ALL {
        let label = format!(" {} ", mode.label());
        modes.push(if mode == d.mode { Span::styled(label, theme.selected()) } else { Span::raw(label) });
        modes.push(Span::raw(" "));
    }
    modes.push(Span::styled("^T switch", theme.dim()));
    let (input, col) = input_line(&d.input, width);
    let mut lines = vec![Line::from(modes), Line::default(), input];
    let input_row = 2;
    match d.mode {
        SendMode::Files => {
            let candidates = pack(&d.candidates, width.saturating_sub(2), theme);
            lines.extend(candidates.into_iter().take(3).map(|l| Line::styled(format!("  {l}"), theme.dim())));
            lines.push(Line::default());
            lines.push(Line::styled("To send:", theme.dim()));
            if d.paths.is_empty() {
                lines.push(Line::styled("  nothing yet", theme.dim()));
            }
            let skip = d.paths.len().saturating_sub(SEND_PATHS);
            if skip > 0 {
                lines.push(Line::styled(format!("  {} and {skip} more", sym.ellipsis), theme.dim()));
            }
            for path in &d.paths[skip..] {
                lines.push(Line::raw(fit(&format!("  {}", tilde(path, app.home.as_deref())), width, theme)));
            }
        }
        SendMode::Text => lines.push(Line::styled("Enter sends the text.", theme.dim())),
        SendMode::Link => lines.push(Line::styled("Enter sends the link.", theme.dim())),
    }
    if let Some(error) = &d.error {
        lines.push(Line::styled(fit(error, width, theme), theme.error()));
    }
    let footer = match d.mode {
        SendMode::Files => format!("Tab complete {0} Enter add, again to send {0} Esc", sym.sep),
        SendMode::Text | SendMode::Link => format!("Enter send {} Esc cancel", sym.sep),
    };
    let inner = modal(f, area, theme, &format!("Send to {}", d.device.name), &footer, lines, MODAL_WIDTH);
    Position::new(inner.x + col, inner.y + input_row)
}

fn sending(f: &mut Frame, area: Rect, app: &App, theme: &Theme, s: &Sending) {
    let sym = theme.symbols();
    let width = modal_inner(area, MODAL_WIDTH);
    let mut lines = vec![Line::styled(fit(&s.what, width, theme), theme.bold()), Line::default()];
    let transfer = s.transfer.and_then(|id| app.transfers.get(&id));
    let mut active = false;
    match (transfer, &s.error) {
        (_, Some(error)) => lines.push(Line::styled(fit(&format!("Failed: {error}"), width, theme), theme.error())),
        (None, None) => lines.push(Line::raw(format!("Connecting{}", sym.ellipsis))),
        (Some(t), None) => {
            active = t.is_active();
            if !t.pin.is_empty() && active {
                lines.push(Line::from(vec![
                    Span::raw("PIN "),
                    Span::styled(t.pin.clone(), theme.title()),
                    Span::raw(fit(
                        &format!(" {} check it matches {}", sym.dash, s.device),
                        width.saturating_sub(8),
                        theme,
                    )),
                ]));
            }
            let reason = |prefix: &str| {
                if t.error.is_empty() { format!("{prefix}.") } else { format!("{prefix}: {}", t.error) }
            };
            match t.state {
                TransferState::Connecting => lines.push(Line::raw(format!("Connecting{}", sym.ellipsis))),
                TransferState::AwaitingAcceptance => lines.push(Line::raw(fit(
                    &format!("Waiting for {} to accept{}", s.device, sym.ellipsis),
                    width,
                    theme,
                ))),
                TransferState::Transferring => lines.push(progress(app, t, width, theme)),
                TransferState::Completed => lines.push(Line::styled("Sent.", theme.ok())),
                TransferState::Rejected => {
                    lines.push(Line::styled(fit(&reason("Declined"), width, theme), theme.warn()))
                }
                TransferState::Cancelled => {
                    lines.push(Line::styled(fit(&reason("Cancelled"), width, theme), theme.dim()))
                }
                TransferState::Failed => lines.push(Line::styled(fit(&reason("Failed"), width, theme), theme.error())),
            }
        }
    }
    let footer = if active { format!("c cancel {} Esc close", sym.sep) } else { "Esc close".to_owned() };
    modal(f, area, theme, &format!("Send to {}", s.device), &footer, lines, MODAL_WIDTH);
}

fn edit(f: &mut Frame, area: Rect, theme: &Theme, e: &Edit) -> Position {
    let sym = theme.symbols();
    let width = modal_inner(area, MODAL_WIDTH);
    let (title, about, footer) = match e.field {
        Field::DeviceName => {
            ("Device name", "The name nearby devices show.", format!("Enter save {} Esc cancel", sym.sep))
        }
        Field::DownloadDir => (
            "Download folder",
            "Where received files are saved.",
            format!("Tab complete {0} Enter save {0} Esc cancel", sym.sep),
        ),
    };
    let (input, col) = input_line(&e.input, width);
    let mut lines = vec![Line::styled(about, theme.dim()), Line::default(), input];
    if let Some(error) = &e.error {
        lines.push(Line::styled(fit(error, width, theme), theme.error()));
    }
    let inner = modal(f, area, theme, title, &footer, lines, MODAL_WIDTH);
    Position::new(inner.x + col, inner.y + 2)
}

fn help(f: &mut Frame, area: Rect, theme: &Theme) {
    let sym = theme.symbols();
    let up_down = format!("{} or j k", sym.up_down);
    let rows: [(&str, &str); 12] = [
        ("1 2 3, Tab", "switch tabs"),
        (&up_down, "move"),
        ("Enter", "send to device, change setting"),
        ("t", "send text or a link"),
        ("v", "visible to everyone / hidden"),
        ("a, d", "accept, decline a request"),
        ("c", "cancel the transfer"),
        ("o", "open what was received"),
        ("y", "copy text, link or paths"),
        ("x", "clear the history"),
        ("Esc", "close a dialog"),
        ("q, Ctrl-C", "quit"),
    ];
    let lines = rows
        .iter()
        .map(|(k, v)| Line::from(vec![Span::styled(format!("{k:<14}"), theme.accent()), Span::raw(*v)]))
        .collect();
    modal(f, area, theme, "Keys", "any key closes", lines, 52);
}

fn peer_name(peer: &Peer) -> &str {
    if peer.name.is_empty() { "Unknown device" } else { &peer.name }
}

/// `Pixel 9 Pro (Android · Quick Share)`
fn peer_label(peer: &Peer, theme: &Theme) -> String {
    format!("{} ({} {} {})", peer_name(peer), peer.platform.label(), theme.symbols().sep, peer.protocol.label())
}

/// `photo.jpg (2.4 MB)`, `3 files (12 MB)`, a link itself, or `some text`.
fn what(items: &[Item], bytes_total: u64) -> String {
    match items {
        [one] if one.is_url() && one.text.as_deref().is_some_and(|t| !t.is_empty()) => {
            one.text.clone().unwrap_or_default()
        }
        [one] if one.text.is_none() => format!("{} ({})", one.name, format_size(bytes_total)),
        _ if items.iter().all(|i| i.text.is_some()) => describe_items(items),
        _ => format!("{} ({})", describe_items(items), format_size(bytes_total)),
    }
}

fn percent(t: &TransferInfo) -> u64 {
    match t.bytes_total {
        0 => 0,
        total => t.bytes_done.min(total) * 100 / total,
    }
}

fn age(now: u64, then: u64) -> String {
    let secs = now.saturating_sub(then) / 1_000_000;
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86_400 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86_400),
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// Rows of a list of `len` that fit into `room`, scrolled to keep `selected` visible.
fn window(len: usize, selected: usize, room: usize) -> Range<usize> {
    let room = room.max(1);
    if len <= room {
        return 0..len;
    }
    let start = selected.saturating_sub(room - 1).min(len - room);
    start..start + room
}

/// Cuts `s` to `width` columns, marking the cut.
fn fit(s: &str, width: usize, theme: &Theme) -> String {
    if s.width() <= width {
        return s.to_owned();
    }
    let ellipsis = theme.symbols().ellipsis;
    let room = width.saturating_sub(ellipsis.width());
    let mut out = String::new();
    for c in s.chars() {
        if out.width() + c.to_string().width() > room {
            break;
        }
        out.push(c);
    }
    if width >= ellipsis.width() {
        out.push_str(ellipsis);
    }
    out
}

/// [`fit`], then padded with spaces to exactly `width` columns.
fn pad(s: &str, width: usize, theme: &Theme) -> String {
    let mut out = fit(s, width, theme);
    let w = out.width();
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(w)));
    out
}

/// Names side by side, two spaces apart, never split across lines.
fn pack(names: &[String], width: usize, theme: &Theme) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for name in names {
        let name = fit(name, width, theme);
        match lines.last_mut() {
            Some(line) if line.width() + 2 + name.width() <= width => {
                line.push_str("  ");
                line.push_str(&name);
            }
            _ => lines.push(name),
        }
    }
    lines
}

/// Greedy word wrap to `width` columns.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.width() + 1 + word.width() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_helpers() {
        let theme = Theme::new(&Default::default(), true, false);
        assert_eq!(fit("Galaxy Tab S9", 8, &theme), "Galaxy …");
        assert_eq!(pad("ab", 4, &theme), "ab  ");
        assert_eq!(wrap("one two three four", 9), ["one two", "three", "four"]);
        let names = ["my file.txt".to_owned(), "b".to_owned(), "c".to_owned()];
        assert_eq!(pack(&names, 14, &theme), ["my file.txt  b", "c"]);
        assert_eq!(window(10, 0, 4), 0..4);
        assert_eq!(window(10, 6, 4), 3..7);
        assert_eq!(window(10, 9, 4), 6..10);
        assert_eq!(window(3, 2, 4), 0..3);
        assert_eq!(age(1_000_000_000, 1_000_000_000 - 90_000_000), "1 min ago");
        assert_eq!(capitalize("declined by the receiver"), "Declined by the receiver");
    }
}
