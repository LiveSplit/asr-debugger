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
    egui::{self, CentralPanel, Grid, ScrollArea, TextStyle, Visuals},
    epaint::{FontFamily, FontId},
    App, Frame,
};
use egui_file::FileDialog;
use indexmap::IndexMap;
use livesplit_auto_splitting::{time, Runtime, SettingValue, SettingsStore, Timer, TimerState};

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

            let mut app = Box::new(MyApp {
                path: None,
                module_modified_time: None,
                open_file_dialog: None,
                module: Vec::new(),
                runtime: None,
                timer,
                slowest_tick: std::time::Duration::ZERO,
                avg_tick_secs: 0.0,
            });

            if let Some(path) = std::env::args().nth(1).map(From::from) {
                app.set_path(path, false);
            }

            app
        }),
    );
}

struct MyApp {
    path: Option<PathBuf>,
    module_modified_time: Option<SystemTime>,
    open_file_dialog: Option<FileDialog>,
    module: Vec<u8>,
    runtime: Option<Runtime<DebuggerTimer>>,
    timer: DebuggerTimer,
    slowest_tick: std::time::Duration,
    avg_tick_secs: f64,
}

impl App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut Frame) {
        ctx.request_repaint();

        let mut tick_rate = String::new();
        let mut new_runtime = None;

        if let Some(path) = &self.path {
            if fs::metadata(path).ok().and_then(|m| m.modified().ok()) > self.module_modified_time {
                self.set_path(path.clone(), false);
            }
        }

        if let Some(runtime) = &mut self.runtime {
            let now = Instant::now();
            let res = runtime.update();
            let time_of_tick = now.elapsed();
            if time_of_tick > self.slowest_tick {
                self.slowest_tick = time_of_tick;
            }
            self.avg_tick_secs = 0.999 * self.avg_tick_secs + 0.001 * time_of_tick.as_secs_f64();
            tick_rate = match res {
                Ok(tick_rate) => {
                    fmt_duration(time::Duration::try_from(tick_rate).unwrap_or_default())
                }
                Err(e) => format!("Error: {e}"),
            };

            egui::TopBottomPanel::bottom("logs")
                .resizable(true)
                .show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.heading("Logs");
                    });
                    egui::ScrollArea::both().show(ui, |ui| {
                        Grid::new("log_grid")
                            .num_columns(1)
                            .spacing([40.0, 4.0])
                            .striped(true)
                            .show(ui, |ui| {
                                for log in &self.timer.0.borrow().logs {
                                    ui.label(&**log);
                                    ui.end_row();
                                }
                            });
                    });
                });

            egui::SidePanel::left("variables")
                .resizable(true)
                .show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.heading("Variables");
                    });
                    egui::ScrollArea::both().show(ui, |ui| {
                        Grid::new("vars_grid")
                            .num_columns(2)
                            .spacing([40.0, 4.0])
                            .striped(true)
                            .show(ui, |ui| {
                                let state = self.timer.0.borrow();
                                for (key, value) in &state.variables {
                                    ui.label(&**key);
                                    ui.label(&**value);
                                    ui.end_row();
                                }
                            });
                    });
                });

            egui::SidePanel::right("settings")
                .resizable(true)
                .show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.heading("Settings");
                    });
                    egui::ScrollArea::both().show(ui, |ui| {
                        Grid::new("settings_grid")
                            .num_columns(2)
                            .spacing([40.0, 4.0])
                            .striped(true)
                            .show(ui, |ui| {
                                for setting in runtime.user_settings() {
                                    ui.label(&*setting.description).on_hover_text(&*setting.key);
                                    if let SettingValue::Bool(v) = setting.default_value {
                                        let mut value =
                                            match runtime.settings_store().get(&setting.key) {
                                                Some(SettingValue::Bool(v)) => *v,
                                                _ => v,
                                            };
                                        if ui.checkbox(&mut value, "").changed() {
                                            let mut settings = runtime.settings_store().clone();
                                            settings.set(
                                                setting.key.clone(),
                                                SettingValue::Bool(value),
                                            );
                                            new_runtime = Runtime::new(
                                                &self.module,
                                                self.timer.clone(),
                                                settings,
                                            )
                                            .ok();
                                            break;
                                        }
                                    }
                                    ui.end_row();
                                }
                            });
                    });
                });
        }

        CentralPanel::default().show(ctx, |ui| {
            if let Some(dialog) = &mut self.open_file_dialog {
                if dialog.show(ctx).selected() {
                    if let Some(file) = dialog.path() {
                        self.set_path(file, true);
                    }
                }
            }

            ScrollArea::both().show(ui, |ui| {
                Grid::new("main_grid")
                    .num_columns(2)
                    .spacing([40.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label("File");
                        ui.horizontal(|ui| {
                            if ui.button("Open").clicked() {
                                let mut dialog = FileDialog::open_file(self.path.clone());
                                dialog.open();
                                self.open_file_dialog = Some(dialog);
                            }
                            if self.runtime.is_some() {
                                if let Some(path) = &self.path {
                                    if ui.button("Reload").clicked() {
                                        self.set_path(path.clone(), false);
                                    }
                                }
                            }
                        });
                        ui.end_row();

                        ui.label("Tick Rate");
                        ui.label(tick_rate);
                        ui.end_row();

                        ui.label("Avg. Tick Time");
                        ui.label(fmt_duration(time::Duration::seconds_f64(
                            self.avg_tick_secs,
                        )));
                        ui.end_row();

                        ui.label("Slowest Tick");
                        ui.horizontal(|ui| {
                            ui.label(fmt_duration(
                                time::Duration::try_from(self.slowest_tick).unwrap_or_default(),
                            ));
                            if ui.button("Reset").clicked() {
                                self.slowest_tick = std::time::Duration::ZERO;
                            }
                        });
                        ui.end_row();

                        let mut state = self.timer.0.borrow_mut();

                        ui.label("Timer State");
                        ui.horizontal(|ui| {
                            ui.label(format!("{:?}", state.timer_state));
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

                        ui.label("Split Index");
                        ui.label(format!("{}", state.split_index));
                        ui.end_row();
                    });
            });
        });

        if new_runtime.is_some() {
            self.runtime = new_runtime;
        }
    }
}

impl MyApp {
    fn set_path(&mut self, file: PathBuf, clear: bool) {
        let is_reload = Some(file.as_path()) == self.path.as_deref();
        self.module = fs::read(&file).unwrap_or_default();
        self.module_modified_time = fs::metadata(&file).ok().and_then(|m| m.modified().ok());
        self.path = Some(file);
        self.runtime = Runtime::new(&self.module, self.timer.clone(), SettingsStore::new()).ok();
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

#[derive(Default)]
struct DebuggerTimerState {
    timer_state: TimerState,
    game_time: time::Duration,
    split_index: usize,
    variables: IndexMap<Box<str>, String>,
    logs: Vec<Box<str>>,
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

    fn reset(&mut self) {
        self.0.borrow_mut().reset();
    }

    fn set_game_time(&mut self, time: time::Duration) {
        self.0.borrow_mut().game_time = time;
    }

    fn pause_game_time(&mut self) {}

    fn resume_game_time(&mut self) {}

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
        self.variables.clear();
    }

    fn clear(&mut self) {
        *self = Default::default();
    }
}
