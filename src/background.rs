//! The resident process owns native events and translation; egui lives only in its child.
use std::{
    io::BufReader,
    os::windows::process::CommandExt,
    process::{Child, Command as ProcessCommand, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{LPARAM, WPARAM},
    System::Threading::GetCurrentThreadId,
    UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2},
    UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW, TranslateMessage, MSG,
        PM_NOREMOVE, WM_APP,
    },
};

use crate::{
    ipc::{self, Command, Event, Session, View},
    screen_clip::{self, HotkeyEvent},
    single_instance, tray, AppConfig, RequestResult, TranslationResult,
};

const WM_WAKE: u32 = WM_APP + 21;

fn wake(thread_id: u32) {
    let _ = unsafe { PostThreadMessageW(thread_id, WM_WAKE, WPARAM(0), LPARAM(0)) };
}

enum Message {
    Command(u64, Command),
    Disconnected(u64),
    Request(u64, RequestResult),
    WorkerFinished(std::path::PathBuf, bool),
    Capture(Result<std::path::PathBuf, String>, bool),
}

struct Ui {
    process: Child,
    events: Sender<Event>,
    generation: u64,
    closing: bool,
}

struct Task {
    id: u64,
    cancelled: Arc<AtomicBool>,
}

impl Ui {
    /// Whether the process ended by itself with a failure, such as a crash, rather than by
    /// exiting normally. Waits briefly, as the pipe closes just before the process is gone.
    fn crashed(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            match self.process.try_wait() {
                Ok(Some(status)) => return !status.success(),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                _ => return false,
            }
        }
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

fn ui_executable() -> Result<std::path::PathBuf, String> {
    #[cfg(test)]
    if let Some(path) = std::env::var_os("OCR_TRANSLATE_TEST_EXE") {
        return Ok(path.into());
    }
    std::env::current_exe().map_err(|e| e.to_string())
}

struct Background {
    ui: Option<Ui>,
    tray: Option<tray::AppTray>,
    hotkey: screen_clip::HotkeyListener,
    active_hotkey: Option<screen_clip::Hotkey>,
    session: Session,
    config: AppConfig,
    lightweight: bool,
    busy: bool,
    capture_in_progress: bool,
    active_task: Option<Task>,
    next_task_id: u64,
    quitting: bool,
    generation: u64,
    pending_view: Option<View>,
    sender: Sender<Message>,
    receiver: Receiver<Message>,
    thread_id: u32,
}

impl Background {
    fn send(&self, event: Event) {
        if let Some(ui) = &self.ui {
            let _ = ui.events.send(event);
        }
    }

    fn show(&mut self, view: View) -> Result<(), String> {
        if !matches!(view, View::Popup) {
            self.lightweight = false;
            if let Some(tray) = &self.tray {
                tray.set_lightweight(false);
            }
        }
        if let Some(ui) = &self.ui {
            if ui.closing {
                self.pending_view = Some(view);
            } else {
                self.send(Event::Lightweight(self.lightweight));
                self.send(Event::Show(view));
            }
            return Ok(());
        }
        self.generation += 1;
        let generation = self.generation;
        let mut command = ProcessCommand::new(ui_executable()?);
        command.arg("--ui-child");
        // Real-UI tests must not load or migrate the user's settings. APPDATA is not
        // sufficient on Windows, where dirs resolves the system Known Folder instead.
        #[cfg(test)]
        command
            .arg("--ui-test-config")
            .arg(std::env::temp_dir().join(format!(
            "ocr-translate-ui-test-{}-{generation}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
        let mut process = command
            .creation_flags(0x08000000)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("界面启动失败：{e}"))?;
        let mut stdin = process.stdin.take().ok_or("无法连接界面输入")?;
        let stdout = process.stdout.take().ok_or("无法连接界面输出")?;
        let (events, outgoing) = mpsc::channel();
        let sender = self.sender.clone();
        let thread_id = self.thread_id;
        thread::spawn(move || {
            for event in outgoing {
                if ipc::write_frame(&mut stdin, &event).is_err() {
                    break;
                }
            }
        });
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(command)) = ipc::read_frame(&mut reader) {
                if sender.send(Message::Command(generation, command)).is_err() {
                    return;
                }
                wake(thread_id);
            }
            let _ = sender.send(Message::Disconnected(generation));
            wake(thread_id);
        });
        self.ui = Some(Ui {
            process,
            events,
            generation,
            closing: false,
        });
        let mut session = self.session.clone();
        session.config = Some(self.config.clone());
        self.send(Event::Init {
            session: Box::new(session),
            view,
            lightweight: self.lightweight,
            tray_available: self.tray.is_some(),
            active_hotkey: self.active_hotkey,
            busy: self.busy,
        });
        Ok(())
    }

    fn set_config(&mut self, config: AppConfig) {
        crate::net::set_proxy(config.proxy());
        let shortcut = screen_clip::Hotkey::parse(&config.shortcut).unwrap_or_default();
        if self.active_hotkey != Some(shortcut) {
            let _ = self.hotkey.change(shortcut);
        }
        self.config = config;
    }

    fn replace_image(&mut self, path: std::path::PathBuf, temporary: bool) {
        self.session.selected_file = Some(path.clone());
        self.session.temporary_capture = temporary.then_some(path);
    }

    fn start_task(&mut self) -> (u64, Arc<AtomicBool>) {
        if let Some(previous) = self.active_task.take() {
            previous.cancelled.store(true, Ordering::Release);
        }
        self.next_task_id = self.next_task_id.wrapping_add(1).max(1);
        let task = Task {
            id: self.next_task_id,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let worker_token = task.cancelled.clone();
        let id = task.id;
        self.active_task = Some(task);
        self.session.task_id = Some(id);
        self.update_busy();
        self.send(Event::TaskStarted(id));
        (id, worker_token)
    }

    fn update_busy(&mut self) {
        self.busy = self.capture_in_progress || self.active_task.is_some();
        self.hotkey.set_busy(self.capture_in_progress);
    }
    fn start_image(&mut self, path: std::path::PathBuf, temporary: bool) -> u64 {
        if self.capture_in_progress {
            return self.active_task.as_ref().map_or(0, |task| task.id);
        }
        self.replace_image(path.clone(), temporary);
        let (task_id, cancelled) = self.start_task();
        self.session.result = TranslationResult::default();
        let config = self.config.clone();
        let sender = self.sender.clone();
        let thread_id = self.thread_id;
        thread::spawn(move || {
            let source_sender = sender.clone();
            let send_source = |source: String| {
                if !cancelled.load(Ordering::Acquire) {
                    let _ = source_sender
                        .send(Message::Request(task_id, RequestResult::Source(source)));
                    wake(thread_id);
                }
            };
            let translate = || {
                if config.mode == crate::TranslationMode::Vision {
                    return crate::translate_image_with_progress(&config, &path, |source| {
                        send_source(source.to_owned());
                    });
                }
                crate::validate_image_size(&path)?;
                let lines = crate::windows_ocr::recognize_lines(&path, &config.ocr_language)?;
                let mut page = crate::layout::ImageLayout::build(&path, lines)?;
                if cancelled.load(Ordering::Acquire) {
                    return Err("任务已由新的框选替换。".into());
                }
                let source = page.source_text();
                send_source(source.clone());
                let indexes = page.translatable();
                let texts: Vec<String> = indexes
                    .iter()
                    .map(|&i| page.blocks[i].source.clone())
                    .collect();
                let translations = crate::translate_many_cancellable(&config, &texts, &cancelled)?;
                if cancelled.load(Ordering::Acquire) {
                    return Err("任务已由新的框选替换。".into());
                }
                for (i, translated) in indexes.into_iter().zip(translations) {
                    page.blocks[i].translated = Some(translated);
                }
                Ok(TranslationResult {
                    source_text: source,
                    translated_text: page.translated_text(),
                    layout: Some(std::sync::Arc::new(page)),
                    dictionary: None,
                })
            };
            let result = translate().and_then(|result| {
                if cancelled.load(Ordering::Acquire) {
                    Err("任务已由新的框选替换。".into())
                } else {
                    Ok(crate::with_dictionary(&config, result))
                }
            });
            if cancelled.load(Ordering::Acquire) {
                let _ = sender.send(Message::WorkerFinished(path, temporary));
                wake(thread_id);
                return;
            }
            let result = result
                .map(RequestResult::Success)
                .unwrap_or_else(RequestResult::Error);
            let _ = sender.send(Message::Request(task_id, result));
            let _ = sender.send(Message::WorkerFinished(path, temporary));
            wake(thread_id);
        });
        task_id
    }

    fn start_text(&mut self, source: String) {
        let (task_id, cancelled) = self.start_task();
        let config = self.config.clone();
        let sender = self.sender.clone();
        let thread_id = self.thread_id;
        thread::spawn(move || {
            let result = crate::translate_text(&config, &source).and_then(|translated_text| {
                if cancelled.load(Ordering::Acquire) {
                    return Err("任务已由新的请求替换。".into());
                }
                Ok(crate::with_dictionary(
                    &config,
                    TranslationResult {
                        source_text: source,
                        translated_text,
                        layout: None,
                        dictionary: None,
                    },
                ))
            });
            if cancelled.load(Ordering::Acquire) {
                return;
            }
            let result = result
                .map(RequestResult::Success)
                .unwrap_or_else(RequestResult::Error);
            let _ = sender.send(Message::Request(task_id, result));
            wake(thread_id);
        });
    }

    fn capture(&mut self) {
        if self.capture_in_progress {
            return;
        }
        crate::warm_up_translation(&self.config);
        self.capture_in_progress = true;
        self.update_busy();
        let sender = self.sender.clone();
        let thread_id = self.thread_id;
        thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(250));
            let result = screen_clip::capture_region(None);
            let _ = sender.send(Message::Capture(result, false));
            wake(thread_id);
        });
    }

    fn accepted(&mut self, result: Result<std::path::PathBuf, String>, popup: bool) {
        self.capture_in_progress = false;
        self.update_busy();
        match result {
            Ok(path) => {
                let anchor = screen_clip::cursor_position();
                self.session.anchor = Some(anchor);
                self.session.result = TranslationResult::default();
                self.session.status = "正在识别并翻译…".into();
                let task_id = self.start_image(path.clone(), true);
                if popup {
                    self.report_show(View::Popup);
                }
                self.send(Event::Accepted {
                    task_id,
                    path,
                    popup,
                    anchor,
                });
            }
            Err(error) => {
                self.session.status = error.clone();
                if !popup {
                    self.report_show(View::Main);
                } else if error != screen_clip::CANCELLED {
                    self.report_show(View::Popup);
                }
                self.send(Event::Hotkey(HotkeyEvent::Captured(Err(error))));
            }
        }
    }

    fn report_show(&mut self, view: View) {
        if let Err(error) = self.show(view) {
            self.session.status = error.clone();
            // A native error remains visible even when the graphics child cannot start.
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::MessageBoxW(
                    None,
                    &windows::core::HSTRING::from(error),
                    windows::core::w!("OCR Translate"),
                    windows::Win32::UI::WindowsAndMessaging::MB_OK,
                );
            }
        }
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Open(view) => self.report_show(view),
            Command::Quit => self.quitting = true,
            Command::Config(config) => self.set_config(*config),
            Command::Capture(config) => {
                self.set_config(*config);
                self.capture();
            }
            Command::Image {
                config,
                path,
                temporary,
            } => {
                self.set_config(*config);
                self.start_image(path, temporary);
            }
            Command::Text { config, source } => {
                self.set_config(*config);
                self.start_text(source);
            }
            Command::Suspend(session) => {
                if self.lightweight && !self.busy {
                    self.session = *session;
                    if let Some(config) = self.session.config.take() {
                        self.set_config(config);
                    }
                    self.session.result.layout = None;
                    if let Some(ui) = &mut self.ui {
                        ui.closing = true;
                    }
                    self.send(Event::Quit);
                } else {
                    self.send(Event::Lightweight(self.lightweight));
                }
            }
        }
    }

    fn ui_disconnect_requires_exit(&self, closing: bool) -> bool {
        // An intentional lightweight suspension leaves the resident process alive.
        // Otherwise, without a tray there is no way to control the remaining backend.
        (!closing && self.tray.is_none()) || (!self.lightweight && !self.busy)
    }

    fn poll(&mut self) {
        if single_instance::take_activation_request() {
            self.report_show(View::Main);
        }
        while let Some(action) = self
            .tray
            .as_ref()
            .and_then(|tray| tray.actions.try_recv().ok())
        {
            match action {
                tray::Action::Open => self.report_show(View::Main),
                tray::Action::Settings => self.report_show(View::Settings),
                tray::Action::Capture => self.capture_popup(),
                tray::Action::Exit => self.quitting = true,
                tray::Action::Lightweight => {
                    self.lightweight = !self.lightweight;
                    if let Some(tray) = &self.tray {
                        tray.set_lightweight(self.lightweight);
                    }
                    self.send(Event::Lightweight(self.lightweight));
                    if !self.lightweight {
                        self.report_show(View::Main);
                    }
                }
            }
        }
        while let Ok(event) = self.hotkey.events.try_recv() {
            match event {
                HotkeyEvent::Started => {
                    crate::warm_up_translation(&self.config);
                    self.capture_in_progress = true;
                    self.update_busy();
                    self.send(Event::Hotkey(HotkeyEvent::Started));
                }
                HotkeyEvent::Captured(result) => self.accepted(result, true),
                HotkeyEvent::Ready(shortcut) => {
                    self.active_hotkey = Some(shortcut);
                    self.send(Event::Hotkey(HotkeyEvent::Ready(shortcut)));
                }
                HotkeyEvent::Failed { message, active } => {
                    self.active_hotkey = active;
                    if let Some(shortcut) = active {
                        self.config.shortcut = shortcut.to_string();
                    }
                    self.session.status = message.clone();
                    self.send(Event::Hotkey(HotkeyEvent::Failed { message, active }));
                }
                HotkeyEvent::Busy => self.send(Event::Hotkey(HotkeyEvent::Busy)),
            }
        }
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                Message::Command(generation, command) => {
                    if self
                        .ui
                        .as_ref()
                        .is_some_and(|ui| ui.generation == generation)
                    {
                        self.command(command);
                    }
                }
                Message::Disconnected(generation) => {
                    if self
                        .ui
                        .as_ref()
                        .is_some_and(|ui| ui.generation == generation)
                    {
                        let mut ui = self.ui.take().unwrap();
                        let closing = ui.closing;
                        let crashed = !closing && ui.crashed();
                        // Pipe EOF or a malformed frame does not guarantee process exit.
                        // Drop kills and reaps only our own child before restoring control.
                        drop(ui);
                        if let Some(view) = self.pending_view.take() {
                            self.report_show(view);
                        } else if crashed && self.tray.is_some() {
                            // A crashed interface must not take the tray and the shortcut down
                            // with it; the next capture or tray action starts a new one.
                            self.session.status = "界面意外退出，已自动恢复。".into();
                        } else if self.ui_disconnect_requires_exit(closing) {
                            self.quitting = true;
                        }
                    }
                }
                Message::Capture(result, popup) => self.accepted(result, popup),
                Message::WorkerFinished(path, temporary) => {
                    if temporary && self.session.temporary_capture.as_ref() != Some(&path) {
                        let _ = std::fs::remove_file(path);
                    }
                }
                Message::Request(task_id, mut result)
                    if self
                        .active_task
                        .as_ref()
                        .is_some_and(|task| task.id == task_id) =>
                {
                    match &mut result {
                        RequestResult::Source(source) => {
                            self.session.result.source_text = source.clone();
                            self.session.status = "识别完成，正在翻译…".into();
                        }
                        RequestResult::Success(result) => {
                            if let Some(page) = result.layout.as_mut() {
                                std::sync::Arc::make_mut(page).rgba = Vec::new();
                            }
                            self.session.result = result.clone();
                            self.session.status = "翻译完成。".into();
                            self.active_task = None;
                            self.session.task_id = None;
                            self.update_busy();
                        }
                        RequestResult::Error(error) => {
                            self.session.status = error.clone();
                            self.active_task = None;
                            self.session.task_id = None;
                            self.update_busy();
                        }
                    }
                    if self.ui.is_none() {
                        self.report_show(View::Popup);
                    }
                    self.send(Event::Request { task_id, result });
                }
                Message::Request(_, _) => {}
            }
        }
    }

    fn capture_popup(&mut self) {
        if self.capture_in_progress {
            return;
        }
        crate::warm_up_translation(&self.config);
        self.capture_in_progress = true;
        self.update_busy();
        let sender = self.sender.clone();
        let thread_id = self.thread_id;
        thread::spawn(move || {
            let result = screen_clip::capture_region_background();
            let _ = sender.send(Message::Capture(result, true));
            wake(thread_id);
        });
    }
}

pub fn run() -> Result<(), String> {
    // Physical pixels for the cursor position a popup is anchored at, as in the UI process,
    // which winit makes per-monitor DPI aware. Otherwise scaled displays shift every popup.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let Some(mut instance) =
        single_instance::SingleInstance::acquire().map_err(|e| e.to_string())?
    else {
        return Ok(());
    };
    let _apartment = crate::winrt_runtime::Apartment::mta()?;
    let thread_id = unsafe { GetCurrentThreadId() };
    let mut message = MSG::default();
    let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };
    instance.listen(move || wake(thread_id));
    let mut config = crate::load_config(&crate::config_path());
    config.reveal_secrets();
    crate::net::set_proxy(config.proxy());
    let shortcut = screen_clip::Hotkey::parse(&config.shortcut).unwrap_or_default();
    let hotkey = screen_clip::listen_for_hotkey(move || wake(thread_id), shortcut);
    let tray_result = tray::AppTray::new(move || wake(thread_id));
    let mut session = Session::default();
    if let Err(error) = &tray_result {
        session.status = format!("托盘初始化失败，关闭窗口将退出程序：{error}");
    }
    let (sender, receiver) = mpsc::channel();
    let mut app = Background {
        ui: None,
        tray: tray_result.ok(),
        hotkey,
        active_hotkey: None,
        session,
        config,
        lightweight: false,
        busy: false,
        capture_in_progress: false,
        active_task: None,
        next_task_id: 0,
        quitting: false,
        generation: 0,
        pending_view: None,
        sender,
        receiver,
        thread_id,
    };
    thread::spawn(screen_clip::remove_stale_captures);
    app.show(View::Main)?;
    loop {
        app.poll();
        if app.quitting {
            break;
        }
        if !unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    if let Some(mut ui) = app.ui.take() {
        let _ = ui.events.send(Event::Quit);
        // Kill only our own UI child, including when a modal file dialog prevents a clean close.
        let _ = ui.process.kill();
        let _ = ui.process.wait();
    }
    if let Some(path) = app.session.temporary_capture.take() {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Background {
        let (sender, receiver) = mpsc::channel();
        Background {
            ui: None,
            tray: None,
            hotkey: screen_clip::HotkeyListener::test_stub(),
            active_hotkey: None,
            session: Session::default(),
            config: AppConfig::default(),
            lightweight: false,
            busy: false,
            capture_in_progress: false,
            active_task: None,
            next_task_id: 0,
            quitting: false,
            generation: 0,
            pending_view: None,
            sender,
            receiver,
            thread_id: 0,
        }
    }

    fn pump_until(app: &mut Background, condition: impl Fn(&Background) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !condition(app) {
            app.poll();
            assert!(Instant::now() < deadline, "UI lifecycle timed out");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn starting_a_new_task_cancels_the_previous_task() {
        let mut app = fixture();
        let (first_id, first_cancelled) = app.start_task();
        let (second_id, second_cancelled) = app.start_task();
        assert_eq!(first_id, 1);
        assert_eq!(second_id, 2);
        assert!(first_cancelled.load(Ordering::Acquire));
        assert!(!second_cancelled.load(Ordering::Acquire));
        assert_eq!(app.session.task_id, Some(second_id));
        assert!(app
            .active_task
            .as_ref()
            .is_some_and(|task| task.id == second_id));
    }
    #[test]
    fn disconnect_without_a_tray_exits_even_while_busy() {
        let mut app = fixture();
        assert!(app.ui_disconnect_requires_exit(false));
        app.busy = true;
        assert!(app.ui_disconnect_requires_exit(false));
        app.lightweight = true;
        assert!(app.ui_disconnect_requires_exit(false));
    }

    #[test]
    fn only_a_failed_exit_counts_as_a_crash() {
        for (code, crashed) in [("0", false), ("3", true)] {
            let mut process = ProcessCommand::new("cmd")
                .args(["/C", "exit", code])
                .creation_flags(0x08000000)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while process.try_wait().unwrap().is_none() {
                assert!(Instant::now() < deadline, "cmd did not exit");
                thread::sleep(Duration::from_millis(10));
            }
            let (events, _outgoing) = mpsc::channel();
            let mut ui = Ui {
                process,
                events,
                generation: 1,
                closing: false,
            };
            assert_eq!(ui.crashed(), crashed, "exit code {code}");
        }
    }

    #[test]
    fn intentional_lightweight_suspension_keeps_the_backend_alive() {
        let mut app = fixture();
        app.lightweight = true;
        assert!(!app.ui_disconnect_requires_exit(true));
        app.busy = true;
        assert!(!app.ui_disconnect_requires_exit(true));
    }

    #[test]
    fn disconnected_pipe_reaps_a_child_that_is_still_running() {
        let mut app = fixture();
        let process = ProcessCommand::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 30",
            ])
            .creation_flags(0x08000000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let (events, _outgoing) = mpsc::channel();
        app.ui = Some(Ui {
            process,
            events,
            generation: 1,
            closing: false,
        });
        assert!(app
            .ui
            .as_mut()
            .unwrap()
            .process
            .try_wait()
            .unwrap()
            .is_none());
        app.sender.send(Message::Disconnected(1)).unwrap();
        let started = Instant::now();
        app.poll();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(app.ui.is_none());
        assert!(app.quitting);
    }

    #[test]
    fn suspension_requires_lightweight_and_an_idle_backend() {
        let mut app = fixture();
        let snapshot = || {
            Box::new(Session {
                status: "snapshot".into(),
                ..Session::default()
            })
        };
        app.command(Command::Suspend(snapshot()));
        assert_ne!(app.session.status, "snapshot");
        app.lightweight = true;
        app.busy = true;
        app.command(Command::Suspend(snapshot()));
        assert_ne!(app.session.status, "snapshot");
        app.busy = false;
        app.command(Command::Suspend(snapshot()));
        assert_eq!(app.session.status, "snapshot");
    }

    #[test]
    #[ignore = "requires OCR_TRANSLATE_TEST_EXE pointing to the built app and a desktop"]
    fn real_ui_suspends_reopens_and_preserves_text() {
        let mut app = fixture();
        app.session.result.source_text = "preserved source".into();
        app.session.result.translated_text = "preserved translation".into();
        app.show(View::Main).unwrap();
        let first_pid = app.ui.as_ref().unwrap().process.id();
        app.lightweight = true;
        app.send(Event::Lightweight(true));
        pump_until(&mut app, |app| app.ui.is_none());
        assert_eq!(app.session.result.source_text, "preserved source");
        assert_eq!(app.session.result.translated_text, "preserved translation");
        assert!(app.lightweight);
        assert!(!app.quitting);
        app.show(View::Main).unwrap();
        let second_pid = app.ui.as_ref().unwrap().process.id();
        assert_ne!(first_pid, second_pid);
        assert!(!app.lightweight);
        app.show(View::Settings).unwrap();
        assert_eq!(second_pid, app.ui.as_ref().unwrap().process.id());
        app.busy = true;
        app.send(Event::Hotkey(HotkeyEvent::Started));
        app.lightweight = true;
        app.send(Event::Lightweight(true));
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            app.poll();
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            app.ui.is_some(),
            "must not suspend during capture or translation"
        );
        app.busy = false;
        app.active_task = Some(Task {
            id: 1,
            cancelled: Arc::new(AtomicBool::new(false)),
        });
        app.send(Event::TaskStarted(1));
        app.send(Event::Hotkey(HotkeyEvent::Captured(Err(
            screen_clip::CANCELLED.into(),
        ))));
        app.send(Event::Request {
            task_id: 1,
            result: RequestResult::Error("test task completed".into()),
        });
        pump_until(&mut app, |app| app.ui.is_none());
        app.show(View::Settings).unwrap();
        assert!(!app.lightweight);
    }
}
