// SPDX-License-Identifier: GPL-3.0-or-later
//! Golden-buffer snapshots of every screen at 80×24 and 120×40, plus `NO_COLOR` and ASCII
//! variants (SPEC §17). `LDROP_UPDATE_SNAPSHOTS=1 cargo test -p ldrop-tui` rewrites the files
//! in `snapshots/`; review the diff before committing.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ldrop_core::config::{Config, TuiConfig};
use ldrop_core::{DeviceKind, Direction, Platform, TransferState};
use ratatui::Terminal;
use ratatui::backend::{Backend, TestBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::style::Color;

use crate::app::fixtures::*;
use crate::app::{App, Dialog, Edit, Event, Field, SendDialog, SendMode, Sending};
use crate::input::LineInput;
use crate::theme::Theme;
use crate::ui;

const SIZES: [(u16, u16); 2] = [(80, 24), (120, 40)];

fn unicode() -> Theme {
    Theme::new(&TuiConfig::default(), true, false)
}

fn draw(app: &App, theme: &Theme, width: u16, height: u16) -> (Buffer, Position) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| ui::render(f, app, theme)).unwrap();
    let cursor = terminal.backend_mut().get_cursor_position().unwrap();
    (terminal.backend().buffer().clone(), cursor)
}

fn text(buffer: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buffer.area.height {
        let line: String = (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect();
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn check_with(name: &str, app: &App, theme: &Theme, sizes: &[(u16, u16)]) {
    for &(width, height) in sizes {
        let file = format!("{}/snapshots/{name}_{width}x{height}.txt", env!("CARGO_MANIFEST_DIR"));
        let actual = text(&draw(app, theme, width, height).0);
        if std::env::var_os("LDROP_UPDATE_SNAPSHOTS").is_some() {
            std::fs::write(&file, &actual).unwrap();
            continue;
        }
        let expected = std::fs::read_to_string(&file)
            .unwrap_or_else(|_| panic!("{file} is missing; run with LDROP_UPDATE_SNAPSHOTS=1"));
        assert!(actual == expected, "{name} at {width}x{height} differs from {file}:\n{actual}");
    }
}

fn check(name: &str, app: &App) {
    check_with(name, app, &unicode(), &SIZES);
}

fn press(app: &mut App, code: KeyCode) {
    app.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

fn send_dialog(mode: SendMode, input: &str) -> Dialog {
    Dialog::Send(SendDialog {
        device: device("qs-1", "Pixel 9 Pro", DeviceKind::Phone, Platform::Android),
        mode,
        input: LineInput::new(input),
        paths: if mode == SendMode::Files { vec!["/home/me/Documents/report.pdf".into()] } else { Vec::new() },
        candidates: if mode == SendMode::Files {
            vec!["IMG_20261025_101530.jpg".into(), "IMG_20261025_101602.jpg".into()]
        } else {
            Vec::new()
        },
        error: None,
    })
}

/// Every screen, by name.
fn screens() -> Vec<(&'static str, App)> {
    let mut screens = vec![("devices", app())];

    let mut searching = app();
    searching.handle(Event::DiscoveryStarted);
    screens.push(("devices_searching", searching));

    let mut preselected = app();
    preselected.preselected = vec!["/home/me/Pictures/beach.jpg".into(), "/home/me/Pictures/sunset.jpg".into()];
    screens.push(("devices_preselected", preselected));

    let mut files = app();
    files.dialog = Some(send_dialog(SendMode::Files, "~/Pictures/IMG_2026"));
    screens.push(("send_files", files));

    let mut text = app();
    text.dialog = Some(send_dialog(SendMode::Text, "Dinner at 8?"));
    screens.push(("send_text", text));

    let mut sending = app();
    sending.dialog = Some(Dialog::Sending(Sending {
        device: "Pixel 9 Pro".into(),
        what: "2 files".into(),
        transfer: Some(4),
        error: None,
    }));
    screens.push(("sending", sending));

    let mut declined = app();
    declined.dialog = Some(Dialog::Sending(Sending {
        device: "Galaxy S24".into(),
        what: "report.pdf".into(),
        transfer: Some(2),
        error: None,
    }));
    screens.push(("sending_declined", declined));

    let mut incoming = app();
    incoming.handle(Event::Incoming(request(7)));
    screens.push(("incoming", incoming));

    let mut many = app();
    let mut big = request(8);
    big.items = (1..=7).map(|n| file(&format!("PXL_20261025_10{n:02}00.jpg"), 3_000_000, None)).collect();
    big.bytes_total = 21_000_000;
    many.handle(Event::Incoming(big));
    many.handle(Event::Incoming(request(9)));
    screens.push(("incoming_queue", many));

    let mut transfers = app();
    press(&mut transfers, KeyCode::Char('2'));
    screens.push(("transfers", transfers));

    let mut waiting = app();
    let mut t = transfer(
        6,
        Direction::Incoming,
        TransferState::AwaitingAcceptance,
        peer("Pixel 7a", Platform::Android),
        vec![file("scan.pdf", 420_000, None)],
    );
    t.deadline = NOW + 51 * SEC;
    t.pin = "0193".into();
    waiting.handle(Event::TransferChanged(t));
    press(&mut waiting, KeyCode::Char('2'));
    screens.push(("transfers_waiting", waiting));

    let mut empty = app();
    empty.handle(Event::Transfers(Vec::new()));
    press(&mut empty, KeyCode::Char('2'));
    screens.push(("transfers_empty", empty));

    let mut settings = app();
    press(&mut settings, KeyCode::Char('3'));
    screens.push(("settings", settings));

    let mut confirm = app();
    press(&mut confirm, KeyCode::Char('3'));
    confirm.dialog = Some(Dialog::ConfirmExclusive);
    screens.push(("confirm_exclusive", confirm));

    let mut edit = app();
    press(&mut edit, KeyCode::Char('3'));
    edit.dialog = Some(Dialog::Edit(Edit { field: Field::DeviceName, input: LineInput::new("thinkpad"), error: None }));
    screens.push(("edit_name", edit));

    let mut help = app();
    press(&mut help, KeyCode::Char('?'));
    screens.push(("help", help));

    screens.push(("connecting", App::new(NOW, Config::default(), None, Vec::new())));
    let mut down = App::new(NOW, Config::default(), None, Vec::new());
    down.handle(Event::Disconnected(
        "ldropd is not running and could not be started (try `systemctl --user start ldropd`).".into(),
    ));
    screens.push(("disconnected", down));
    screens
}

#[test]
fn every_screen() {
    for (name, app) in screens() {
        check(name, &app);
    }
}

#[test]
fn ascii_fallback() {
    let theme = Theme::new(&TuiConfig::default(), true, true);
    for (name, app) in screens() {
        let (buffer, _) = draw(&app, &theme, 80, 24);
        let text = text(&buffer);
        assert!(text.is_ascii(), "{name} has non-ASCII output:\n{text}");
        if ["devices", "incoming", "transfers"].contains(&name) {
            check_with(&format!("{name}_ascii"), &app, &theme, &[(80, 24)]);
        }
    }
}

#[test]
fn no_color_uses_only_attributes() {
    let theme = Theme::new(&TuiConfig::default(), false, false);
    for (name, app) in screens() {
        for (width, height) in SIZES {
            let (buffer, _) = draw(&app, &theme, width, height);
            for cell in &buffer.content {
                assert!(
                    cell.fg == Color::Reset && cell.bg == Color::Reset,
                    "{name} at {width}x{height} has a colored cell {cell:?}"
                );
            }
        }
    }
}

#[test]
fn cursor_sits_in_the_input() {
    let mut app = app();
    app.dialog = Some(send_dialog(SendMode::Text, "hi"));
    let (buffer, cursor) = draw(&app, &unicode(), 80, 24);
    let row: String = (0..80).map(|x| buffer[(x, cursor.y)].symbol()).collect();
    assert!(row.contains("> hi"), "cursor row: {row}");
    assert_eq!(buffer[(cursor.x - 1, cursor.y)].symbol(), "i");
}

#[test]
fn tiny_terminal() {
    let (buffer, _) = draw(&app(), &unicode(), 30, 8);
    assert!(text(&buffer).starts_with("Terminal too small"));
}
