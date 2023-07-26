#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use std::{
    collections::BTreeMap,
    env,
    fs::{self, File},
    io::Write,
    path::PathBuf,
    sync::{Arc, RwLock},
    thread,
    time::{Instant, SystemTime},
};

use eframe::{
    egui::{
        self,
        plot::{Bar, BarChart, Legend, Plot, VLine},
        Grid, RichText, TextStyle, Visuals,
    },
    emath::Align,
    epaint::{FontFamily, FontId},
    App, Frame,
};
use egui_dock::{DockArea, NodeIndex, Style, Tree};
use egui_file::FileDialog;
use hdrhistogram::Histogram;
use indexmap::IndexMap;
use livesplit_auto_splitting::{
    time, Runtime, SettingValue, SettingsStore, Timer, TimerState, UserSettingKind,
};

enum Tab {
    Main,
    Logs,
    Variables,
    Settings,
    Processes,
    Performance,
}

fn main() {
    env::set_var("WASMTIME_BACKTRACE_DETAILS", "1");

    let shared_state = Arc::new(RwLock::new(SharedState {
        runtime: None,
        tick_rate: std::time::Duration::ZERO,
        slowest_tick: std::time::Duration::ZERO,
        avg_tick_secs: 0.0,
        tick_times: Histogram::new(1).unwrap(),
    }));
    let timer = DebuggerTimer::default();

    thread::spawn({
        let timer = timer.clone();
        let shared_state = shared_state.clone();
        move || runtime_thread(shared_state, timer.clone())
    });

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

            let mut tree = Tree::new(vec![Tab::Main, Tab::Performance]);
            let [left, right] = tree.split_right(NodeIndex::root(), 0.65, vec![Tab::Variables]);
            tree.split_below(right, 0.5, vec![Tab::Settings]);
            tree.split_below(left, 0.5, vec![Tab::Logs, Tab::Processes]);

            let mut app = Box::new(Debugger {
                tree,
                state: AppState {
                    module_modified_time: None,
                    module: Vec::new(),
                    path: None,
                    open_file_dialog: None,
                    shared_state,
                    timer,
                },
            });

            if let Some(path) = env::args().nth(1).map(From::from) {
                app.state.set_path(path, false);
            }

            app
        }),
    )
    .unwrap();
}

struct SharedState {
    runtime: Option<Runtime<DebuggerTimer>>,
    tick_rate: std::time::Duration,
    slowest_tick: std::time::Duration,
    avg_tick_secs: f64,
    tick_times: Histogram<u64>,
}

fn runtime_thread(shared_state: Arc<RwLock<SharedState>>, timer: DebuggerTimer) {
    let mut next_tick = Instant::now();
    loop {
        let tick_rate = {
            let shared_state = &mut *shared_state.write().unwrap();
            if let Some(runtime) = &mut shared_state.runtime {
                let now = Instant::now();
                let res = runtime.update();
                let time_of_tick = now.elapsed();
                if time_of_tick > shared_state.slowest_tick {
                    shared_state.slowest_tick = time_of_tick;
                }
                shared_state.tick_rate = runtime.tick_rate();
                shared_state.tick_times += time_of_tick.as_nanos() as u64;
                shared_state.avg_tick_secs =
                    0.999 * shared_state.avg_tick_secs + 0.001 * time_of_tick.as_secs_f64();
                if let Err(e) = res {
                    timer
                        .0
                        .write()
                        .unwrap()
                        .logs
                        .push(format!("Error: {e}").into())
                };
            }
            shared_state.tick_rate
        };
        next_tick += tick_rate;

        let now = Instant::now();
        if let Some(sleep_time) = next_tick.checked_duration_since(now) {
            thread::sleep(sleep_time);
        } else {
            // In this case we missed the next tick already. This likely comes
            // up when the operating system was suspended for a while. Instead
            // of trying to catch up, we just reset the next tick to start from
            // now.
            next_tick = now;
        }
    }
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
    shared_state: Arc<RwLock<SharedState>>,
    timer: DebuggerTimer,
}

struct TabViewer<'a> {
    state: &'a mut AppState,
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
                            if self.state.shared_state.read().unwrap().runtime.is_some() {
                                if let Some(path) = &self.state.path {
                                    if ui.button("Reload").clicked() {
                                        self.state.set_path(path.clone(), false);
                                    }
                                }
                            }
                        });
                        ui.end_row();

                        ui.label("Tick Rate");
                        ui.label(fmt_duration(
                            time::Duration::try_from(
                                self.state.shared_state.read().unwrap().tick_rate,
                            )
                            .unwrap_or_default(),
                        ));
                        ui.end_row();

                        ui.label("Avg. Tick Time");
                        ui.label(fmt_duration(time::Duration::seconds_f64(
                            self.state.shared_state.read().unwrap().avg_tick_secs,
                        )));
                        ui.end_row();

                        ui.label("Slowest Tick");
                        ui.horizontal(|ui| {
                            ui.label(fmt_duration(
                                time::Duration::try_from(
                                    self.state.shared_state.read().unwrap().slowest_tick,
                                )
                                .unwrap_or_default(),
                            ));
                            if ui.button("Reset").clicked() {
                                self.state.shared_state.write().unwrap().slowest_tick =
                                    std::time::Duration::ZERO;
                            }
                        });
                        ui.end_row();

                        {
                            let mut state = self.state.timer.0.write().unwrap();

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
                        }

                        if let Some(runtime) = &self.state.shared_state.read().unwrap().runtime {
                            let memory = runtime.memory();
                            ui.label("Memory");
                            ui.horizontal(|ui| {
                                ui.label(
                                    byte_unit::Byte::from_bytes(memory.len() as _)
                                        .get_appropriate_unit(true)
                                        .to_string(),
                                );
                                if ui.button("Dump").clicked() {
                                    if let Err(e) = fs::write("memory_dump.bin", memory) {
                                        self.state
                                            .timer
                                            .0
                                            .write()
                                            .unwrap()
                                            .logs
                                            .push(format!("Failed to dump memory: {}", e).into());
                                    }
                                }
                            });
                            ui.end_row();
                        }
                    });
            }
            Tab::Logs => {
                let mut scroll_to_end = false;
                Grid::new("log_grid")
                    .num_columns(1)
                    .spacing([40.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        let mut timer = self.state.timer.0.write().unwrap();
                        for log in &timer.logs {
                            ui.label(&**log);
                            ui.end_row();
                        }
                        if timer.logs.len() != timer.last_logs_len {
                            timer.last_logs_len = timer.logs.len();
                            scroll_to_end = true;
                        }
                    });
                ui.horizontal(|ui| {
                    if ui.button("Clear").clicked() {
                        self.state.timer.0.write().unwrap().logs.clear();
                    }
                    if ui.button("Save").clicked() {
                        if let Err(e) = File::create("auto_splitter_logs.txt").and_then(|mut f| {
                            for log in &self.state.timer.0.read().unwrap().logs {
                                writeln!(f, "{log}")?;
                            }
                            Ok(())
                        }) {
                            self.state
                                .timer
                                .0
                                .write()
                                .unwrap()
                                .logs
                                .push(format!("Failed to save log file: {}", e).into());
                        }
                    }
                });
                if scroll_to_end {
                    ui.scroll_to_cursor(Some(Align::Max));
                }
            }
            Tab::Variables => {
                Grid::new("vars_grid")
                    .num_columns(2)
                    .spacing([40.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        let state = self.state.timer.0.read().unwrap();
                        for (key, value) in &state.variables {
                            ui.label(&**key);
                            ui.label(&**value);
                            ui.end_row();
                        }
                    });
            }
            Tab::Settings => {
                Grid::new("settings_grid")
                    .num_columns(3)
                    .spacing([40.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label(RichText::new("Key").strong().underline());
                        ui.label(RichText::new("Setting").strong().underline());
                        ui.end_row();
                        if let Some(runtime) = &self.state.shared_state.read().unwrap().runtime {
                            for setting in runtime.user_settings() {
                                ui.label(&*setting.key);
                                match setting.kind {
                                    UserSettingKind::Bool { default_value } => {
                                        let label = ui.label(&*setting.description);
                                        if let Some(tooltip) = &setting.tooltip {
                                            label.on_hover_text(&**tooltip);
                                        }
                                        let mut value =
                                            match runtime.settings_store().get(&setting.key) {
                                                Some(SettingValue::Bool(v)) => *v,
                                                _ => default_value,
                                            };
                                        if ui.checkbox(&mut value, "").changed() {
                                            let mut settings = runtime.settings_store().clone();
                                            settings.set(
                                                setting.key.clone(),
                                                SettingValue::Bool(value),
                                            );
                                            self.new_runtime = match Runtime::new(
                                                &self.state.module,
                                                self.state.timer.clone(),
                                                settings,
                                            ) {
                                                Ok(r) => Some(r),
                                                Err(e) => {
                                                    self.state.timer.0.write().unwrap().logs.push(
                                                        format!(
                                                            "Failed loading the WASM file: {e:?}"
                                                        )
                                                        .into(),
                                                    );
                                                    None
                                                }
                                            };
                                            break;
                                        }
                                    }
                                    UserSettingKind::Title { heading_level } => {
                                        let label = ui.label(
                                            RichText::new(&*setting.description)
                                                .heading()
                                                .underline()
                                                .size(25.0 * 0.9f32.powi(heading_level as i32)),
                                        );
                                        if let Some(tooltip) = &setting.tooltip {
                                            label.on_hover_text(&**tooltip);
                                        }
                                    }
                                }
                                ui.end_row();
                            }
                        }
                    });
            }
            Tab::Processes => {
                Grid::new("processes_grid")
                    .num_columns(2)
                    .spacing([40.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label(RichText::new("PID").strong().underline());
                        ui.label(RichText::new("Path").strong().underline());
                        ui.end_row();
                        if let Some(runtime) = &self.state.shared_state.read().unwrap().runtime {
                            for process in runtime.attached_processes() {
                                ui.label(process.pid().to_string());
                                ui.label(process.path().unwrap_or("Unnamed Process"));
                                ui.end_row();
                            }
                        }
                    });
            }
            Tab::Performance => {
                if ui.button("Clear").clicked() {
                    self.state.shared_state.write().unwrap().tick_times.clear();
                }

                let shared_state = self.state.shared_state.read().unwrap();
                let mut right_x = 0.0;
                let scale_y = 100.0 / shared_state.tick_times.len() as f64;

                let chart = BarChart::new(
                    self.state
                        .shared_state
                        .read()
                        .unwrap()
                        .tick_times
                        .iter_recorded()
                        .map(|bar| {
                            let left_x = right_x;
                            right_x = bar.percentile();
                            let mid_x = 0.5 * (left_x + right_x);
                            Bar::new(mid_x, scale_y * bar.count_since_last_iteration() as f64)
                                .name(format!(
                                    "{}\n{:.2}th percentile",
                                    fmt_duration(time::Duration::nanoseconds(
                                        shared_state.tick_times.value_at_percentile(mid_x as _)
                                            as _,
                                    )),
                                    mid_x
                                ))
                                .width(right_x - left_x)
                        })
                        .collect(),
                )
                .name("Tick Time");

                Plot::new("Performance Plot")
                    .legend(Legend::default())
                    .x_axis_formatter(|x, _| format!("{x}th percentile"))
                    .y_axis_formatter(|y, _| format!("{y}%"))
                    .clamp_grid(true)
                    .allow_zoom(true)
                    .allow_drag(true)
                    .show(ui, |plot_ui| {
                        plot_ui.vline(
                            VLine::new(
                                shared_state
                                    .tick_times
                                    .percentile_below(shared_state.tick_times.mean() as _),
                            )
                            .name("Mean"),
                        );
                        plot_ui.vline(VLine::new(50.0).name("Median"));
                        plot_ui.bar_chart(chart);
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
            Tab::Processes => "Processes",
            Tab::Performance => "Performance",
        }
        .into()
    }
}

impl App for Debugger {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut Frame) {
        ctx.request_repaint();

        if let Some(path) = &self.state.path {
            if fs::metadata(path).ok().and_then(|m| m.modified().ok())
                > self.state.module_modified_time
            {
                self.state.set_path(path.clone(), false);
            }
        }

        if let Some(dialog) = &mut self.state.open_file_dialog {
            if dialog.show(ctx).selected() {
                if let Some(file) = dialog.path().map(ToOwned::to_owned) {
                    self.state.set_path(file, true);
                }
            }
        }

        let mut tab_viewer = TabViewer {
            state: &mut self.state,
            new_runtime: None,
        };

        DockArea::new(&mut self.tree)
            .style(Style::from_egui(ctx.style().as_ref()))
            .show(ctx, &mut tab_viewer);

        if tab_viewer.new_runtime.is_some() {
            self.state.shared_state.write().unwrap().runtime = tab_viewer.new_runtime;
        }
    }
}

impl AppState {
    fn set_path(&mut self, file: PathBuf, clear: bool) {
        let is_reload = Some(file.as_path()) == self.path.as_deref();
        self.module = fs::read(&file).unwrap_or_default();
        self.module_modified_time = fs::metadata(&file).ok().and_then(|m| m.modified().ok());
        self.path = Some(file);
        {
            let mut shared_state = self.shared_state.write().unwrap();
            shared_state.runtime =
                match Runtime::new(&self.module, self.timer.clone(), SettingsStore::new()) {
                    Ok(r) => Some(r),
                    Err(e) => {
                        self.timer
                            .0
                            .write()
                            .unwrap()
                            .logs
                            .push(format!("Failed loading the WASM file: {e:?}").into());
                        None
                    }
                };
            shared_state.slowest_tick = std::time::Duration::ZERO;
            shared_state.avg_tick_secs = 0.0;
            shared_state.tick_times.clear();
        }
        let mut timer = self.timer.0.write().unwrap();
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
    last_logs_len: usize,
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
struct DebuggerTimer(Arc<RwLock<DebuggerTimerState>>);

impl Timer for DebuggerTimer {
    fn state(&self) -> TimerState {
        self.0.read().unwrap().timer_state
    }

    fn start(&mut self) {
        let mut state = self.0.write().unwrap();
        if state.timer_state == TimerState::NotRunning {
            state.start();
            state.logs.push("Timer started.".into());
        }
    }

    fn split(&mut self) {
        let mut state = self.0.write().unwrap();
        if state.timer_state == TimerState::Running {
            state.split_index += 1;
            state.logs.push("Splitted.".into());
        }
    }

    fn skip_split(&mut self) {
        let mut state = self.0.write().unwrap();
        if state.timer_state == TimerState::Running {
            state.split_index += 1;
            state.logs.push("Split skipped.".into());
        }
    }

    fn undo_split(&mut self) {
        let mut state = self.0.write().unwrap();
        if state.timer_state == TimerState::Ended {
            state.timer_state = TimerState::Running;
        }
        if state.timer_state == TimerState::Running {
            state.split_index = state.split_index.saturating_sub(1);
            state.logs.push("Split undone.".into());
        }
    }

    fn reset(&mut self) {
        let mut state = self.0.write().unwrap();
        state.reset();
        state.logs.push("Run reset.".into());
    }

    fn set_game_time(&mut self, time: time::Duration) {
        let mut state = self.0.write().unwrap();
        state.game_time = time;
        if state.game_time_state == GameTimeState::NotInitialized {
            state.game_time_state = GameTimeState::Running;
        }
    }

    fn pause_game_time(&mut self) {
        self.0.write().unwrap().game_time_state = GameTimeState::Paused;
    }

    fn resume_game_time(&mut self) {
        self.0.write().unwrap().game_time_state = GameTimeState::Running;
    }

    fn set_variable(&mut self, key: &str, value: &str) {
        let mut guard = self.0.write().unwrap();
        let s = guard.variables.entry(key.into()).or_default();
        s.clear();
        s.push_str(value);
    }

    fn log(&mut self, message: std::fmt::Arguments<'_>) {
        self.0.write().unwrap().logs.push(match message.as_str() {
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
