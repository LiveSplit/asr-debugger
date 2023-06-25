#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use std::{
    cell::RefCell,
    collections::BTreeMap,
    fs,
    path::PathBuf,
    rc::Rc,
    time::{Instant, SystemTime},
};

use eframe::{
    egui::{self, Grid, TextStyle, Visuals},
    epaint::{FontFamily, FontId},
    App, Frame,
};
use egui_dock::{DockArea, NodeIndex, Style, Tree};
use egui_file::FileDialog;
use indexmap::IndexMap;
use livesplit_auto_splitting::{time, Runtime, SettingValue, SettingsStore, Timer, TimerState};

enum Tab {
    Main,
    Logs,
    Variables,
    Settings,
}

fn main() {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Auto Splitting Runtime Debugger",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(Visuals::dark());
            let mut style = (*cc.egui_ctx.style()).clone();

            let mut text_styles = BTreeMap::new();
            text_styles.insert(
                TextStyle::Small,
                FontId::new(1.25 * 10.0, FontFamily::Proportional),
            );
            text_styles.insert(
                TextStyle::Body,
                FontId::new(1.25 * 14.0, FontFamily::Proportional),
            );
            text_styles.insert(
                TextStyle::Button,
                FontId::new(1.25 * 14.0, FontFamily::Proportional),
            );
            text_styles.insert(
                TextStyle::Heading,
                FontId::new(1.25 * 20.0, FontFamily::Proportional),
            );
            text_styles.insert(
                TextStyle::Monospace,
                FontId::new(1.25 * 14.0, FontFamily::Monospace),
            );
            // Redefine text_styles
            style.text_styles = text_styles;

            // Mutate global style with above changes
            cc.egui_ctx.set_style(style);

            let timer = DebuggerTimer::default();

            let mut tree = Tree::new(vec![Tab::Main]);
            let [left, right] = tree.split_right(NodeIndex::root(), 0.65, vec![Tab::Variables]);
            tree.split_below(right, 0.5, vec![Tab::Settings]);
            tree.split_below(left, 0.5, vec![Tab::Logs]);

            let mut app = Box::new(Debugger {
                tree,
                state: AppState {
                    module_modified_time: None,
                    module: Vec::new(),
                    path: None,
                    open_file_dialog: None,
                    runtime: None,
                    slowest_tick: std::time::Duration::ZERO,
                    avg_tick_secs: 0.0,
                    timer,
                },
            });

            if let Some(path) = std::env::args().nth(1).map(From::from) {
                app.state.set_path(path, false);
            }

            app
        }),
    )
    .unwrap();
}

struct Debugger {
    tree: Tree<Tab>,
    state: AppState,
}

struct AppState {
    path: Option<PathBuf>,
    module_modified_time: Option<SystemTime>,
    open_file_dialog: Option<FileDialog>,
    module: Vec<u8>,
    runtime: Option<Runtime<DebuggerTimer>>,
    timer: DebuggerTimer,
    slowest_tick: std::time::Duration,
    avg_tick_secs: f64,
}

struct TabViewer<'a> {
    state: &'a mut AppState,
    tick_rate: String,
    new_runtime: Option<Runtime<DebuggerTimer>>,
}

impl egui_dock::TabViewer for TabViewer<'_> {
    type Tab = Tab;

    fn on_close(&mut self, _tab: &mut Self::Tab) -> bool {
        false
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        match tab {
            Tab::Main => {
                Grid::new("main_grid")
                    .num_columns(2)
                    .spacing([40.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label("File");
                        ui.horizontal(|ui| {
                            if ui.button("Open").clicked() {
                                let mut dialog = FileDialog::open_file(self.state.path.clone());
                                dialog.open();
                                self.state.open_file_dialog = Some(dialog);
                            }
                            if self.state.runtime.is_some() {
                                if let Some(path) = &self.state.path {
                                    if ui.button("Reload").clicked() {
                                        self.state.set_path(path.clone(), false);
                                    }
                                }
                            }
                        });
                        ui.end_row();

                        ui.label("Tick Rate");
                        ui.label(&self.tick_rate);
                        ui.end_row();

                        ui.label("Avg. Tick Time");
                        ui.label(fmt_duration(time::Duration::seconds_f64(
                            self.state.avg_tick_secs,
                        )));
                        ui.end_row();

                        ui.label("Slowest Tick");
                        ui.horizontal(|ui| {
                            ui.label(fmt_duration(
                                time::Duration::try_from(self.state.slowest_tick)
                                    .unwrap_or_default(),
                            ));
                            if ui.button("Reset").clicked() {
                                self.state.slowest_tick = std::time::Duration::ZERO;
                            }
                        });
                        ui.end_row();

                        let mut state = self.state.timer.0.borrow_mut();

                        ui.label("Timer State");
                        ui.horizontal(|ui| {
                            ui.label(timer_state_to_str(state.timer_state));
                            if state.timer_state == TimerState::NotRunning {
                                if ui.button("Start").clicked() {
                                    state.start();
                                }
                            } else if ui.button("Reset").clicked() {
                                state.reset();
                            }
                        });
                        ui.end_row();

                        ui.label("Game Time");
                        ui.label(fmt_duration(state.game_time));
                        ui.end_row();

                        ui.label("Game Time State");
                        ui.label(state.game_time_state.to_str());
                        ui.end_row();

                        ui.label("Split Index");
                        ui.label(state.split_index.to_string());
                        ui.end_row();
                    });
            }
            Tab::Logs => {
                Grid::new("log_grid")
                    .num_columns(1)
                    .spacing([40.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        for log in &self.state.timer.0.borrow().logs {
                            ui.label(&**log);
                            ui.end_row();
                        }
                    });
                if ui.button("Clear").clicked() {
                    self.state.timer.0.borrow_mut().logs.clear();
                }
            }
            Tab::Variables => {
                Grid::new("vars_grid")
                    .num_columns(2)
                    .spacing([40.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        let state = self.state.timer.0.borrow();
                        for (key, value) in &state.variables {
                            ui.label(&**key);
                            ui.label(&**value);
                            ui.end_row();
                        }
                    });
            }
            Tab::Settings => {
                Grid::new("settings_grid")
                    .num_columns(2)
                    .spacing([40.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        if let Some(runtime) = &self.state.runtime {
                            for setting in runtime.user_settings() {
                                ui.label(&*setting.description).on_hover_text(&*setting.key);
                                if let SettingValue::Bool(v) = setting.default_value {
                                    let mut value = match runtime.settings_store().get(&setting.key)
                                    {
                                        Some(SettingValue::Bool(v)) => *v,
                                        _ => v,
                                    };
                                    if ui.checkbox(&mut value, "").changed() {
                                        let mut settings = runtime.settings_store().clone();
                                        settings
                                            .set(setting.key.clone(), SettingValue::Bool(value));
                                        self.new_runtime = match Runtime::new(
                                            &self.state.module,
                                            self.state.timer.clone(),
                                            settings,
                                        ) {
                                            Ok(r) => Some(r),
                                            Err(e) => {
                                                println!("Failed loading the WASM file: {e:?}");
                                                None
                                            }
                                        };
                                        break;
                                    }
                                }
                                ui.end_row();
                            }
                        }
                    });
            }
        }
    }

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        match tab {
            Tab::Main => "Main",
            Tab::Logs => "Logs",
            Tab::Variables => "Variables",
            Tab::Settings => "Settings",
        }
        .into()
    }
}

impl App for Debugger {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut Frame) {
        ctx.request_repaint();

        let mut tick_rate = String::new();

        if let Some(path) = &self.state.path {
            if fs::metadata(path).ok().and_then(|m| m.modified().ok())
                > self.state.module_modified_time
            {
                self.state.set_path(path.clone(), false);
            }
        }

        if let Some(runtime) = &mut self.state.runtime {
            let now = Instant::now();
            let res = runtime.update();
            let time_of_tick = now.elapsed();
            if time_of_tick > self.state.slowest_tick {
                self.state.slowest_tick = time_of_tick;
            }
            self.state.avg_tick_secs =
                0.999 * self.state.avg_tick_secs + 0.001 * time_of_tick.as_secs_f64();
            tick_rate = match res {
                Ok(tick_rate) => {
                    fmt_duration(time::Duration::try_from(tick_rate).unwrap_or_default())
                }
                Err(e) => format!("Error: {e}"),
            };
        }

        if let Some(dialog) = &mut self.state.open_file_dialog {
            if dialog.show(ctx).selected() {
                if let Some(file) = dialog.path() {
                    self.state.set_path(file, true);
                }
            }
        }

        let mut tab_viewer = TabViewer {
            state: &mut self.state,
            tick_rate,
            new_runtime: None,
        };

        DockArea::new(&mut self.tree)
            .style(Style::from_egui(ctx.style().as_ref()))
            .show(ctx, &mut tab_viewer);

        if tab_viewer.new_runtime.is_some() {
            self.state.runtime = tab_viewer.new_runtime;
        }
    }
}

impl AppState {
    fn set_path(&mut self, file: PathBuf, clear: bool) {
        let is_reload = Some(file.as_path()) == self.path.as_deref();
        self.module = fs::read(&file).unwrap_or_default();
        self.module_modified_time = fs::metadata(&file).ok().and_then(|m| m.modified().ok());
        self.path = Some(file);
        self.runtime = match Runtime::new(&self.module, self.timer.clone(), SettingsStore::new()) {
            Ok(r) => Some(r),
            Err(e) => {
                println!("Failed loading the WASM file: {e:?}");
                None
            }
        };
        self.slowest_tick = std::time::Duration::ZERO;
        self.avg_tick_secs = 0.0;
        let mut timer = self.timer.0.borrow_mut();
        if clear {
            timer.clear();
        }
        timer.variables.clear();
        timer.logs.push(
            if is_reload {
                "Auto Splitter reloaded."
            } else {
                "Auto Splitter loaded."
            }
            .into(),
        );
    }
}

const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 60 * SECONDS_PER_MINUTE;

fn fmt_duration(time: time::Duration) -> String {
    let nanoseconds = time.subsec_nanoseconds();
    let total_seconds = time.whole_seconds();
    let (minus, total_seconds, nanoseconds) = if (total_seconds | nanoseconds as i64) < 0 {
        ("-", (-total_seconds) as u64, (-nanoseconds) as u32)
    } else {
        ("", total_seconds as u64, nanoseconds as u32)
    };
    let seconds = (total_seconds % SECONDS_PER_MINUTE) as u8;
    let minutes = ((total_seconds % SECONDS_PER_HOUR) / SECONDS_PER_MINUTE) as u8;
    let hours = total_seconds / SECONDS_PER_HOUR;
    if hours != 0 {
        format!("{minus}{hours}:{minutes:02}:{seconds:02}.{nanoseconds:09}")
    } else {
        format!("{minus}{minutes}:{seconds:02}.{nanoseconds:09}")
    }
}

fn timer_state_to_str(state: TimerState) -> &'static str {
    match state {
        TimerState::NotRunning => "Not running",
        TimerState::Running => "Running",
        TimerState::Paused => "Paused",
        TimerState::Ended => "Ended",
    }
}

#[derive(Default)]
struct DebuggerTimerState {
    timer_state: TimerState,
    game_time: time::Duration,
    game_time_state: GameTimeState,
    split_index: usize,
    variables: IndexMap<Box<str>, String>,
    logs: Vec<Box<str>>,
}

#[derive(Copy, Clone, Default, PartialEq)]
enum GameTimeState {
    #[default]
    NotInitialized,
    Paused,
    Running,
}

impl GameTimeState {
    fn to_str(self) -> &'static str {
        match self {
            GameTimeState::NotInitialized => "Not initialized",
            GameTimeState::Paused => "Paused",
            GameTimeState::Running => "Running",
        }
    }
}

#[derive(Clone, Default)]
struct DebuggerTimer(Rc<RefCell<DebuggerTimerState>>);

impl Timer for DebuggerTimer {
    fn state(&self) -> TimerState {
        self.0.borrow().timer_state
    }

    fn start(&mut self) {
        self.0.borrow_mut().start();
    }

    fn split(&mut self) {
        let mut state = self.0.borrow_mut();
        if state.timer_state == TimerState::Running {
            state.split_index += 1;
        }
    }

    fn skip_split(&mut self) {
        // For now we just split, considering we have no real list.
        self.split();
    }

    fn undo_split(&mut self) {
        let mut state = self.0.borrow_mut();
        if state.timer_state == TimerState::Ended {
            state.timer_state = TimerState::Running;
        }
        if state.timer_state == TimerState::Running {
            state.split_index = state.split_index.saturating_sub(1);
        }
    }

    fn reset(&mut self) {
        self.0.borrow_mut().reset();
    }

    fn set_game_time(&mut self, time: time::Duration) {
        let mut state = self.0.borrow_mut();
        state.game_time = time;
        if state.game_time_state == GameTimeState::NotInitialized {
            state.game_time_state = GameTimeState::Running;
        }
    }

    fn pause_game_time(&mut self) {
        self.0.borrow_mut().game_time_state = GameTimeState::Paused;
    }

    fn resume_game_time(&mut self) {
        self.0.borrow_mut().game_time_state = GameTimeState::Running;
    }

    fn set_variable(&mut self, key: &str, value: &str) {
        let mut guard = self.0.borrow_mut();
        let s = guard.variables.entry(key.into()).or_default();
        s.clear();
        s.push_str(value);
    }

    fn log(&mut self, message: std::fmt::Arguments<'_>) {
        self.0.borrow_mut().logs.push(match message.as_str() {
            Some(m) => m.into(),
            None => message.to_string().into(),
        });
    }
}

impl DebuggerTimerState {
    fn start(&mut self) {
        if self.timer_state == TimerState::NotRunning {
            self.timer_state = TimerState::Running;
        }
    }

    fn reset(&mut self) {
        self.timer_state = TimerState::NotRunning;
        self.split_index = 0;
        self.game_time = time::Duration::ZERO;
        self.game_time_state = GameTimeState::NotInitialized;
        self.variables.clear();
    }

    fn clear(&mut self) {
        *self = Default::default();
    }
}
