#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, AtomicUsize},
        Arc, Mutex, RwLock,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use anyhow::Context;
use arc_swap::ArcSwapOption;
use atomic::Atomic;
use clap::Parser;
use clear_vec::{Clear, ClearVec};
use eframe::{
    egui::{self, ComboBox, Grid, RichText, TextStyle, Visuals},
    emath::Align,
    epaint::{FontFamily, FontId},
    App, Frame,
};
use egui_dock::{DockArea, DockState, NodeIndex, Style};
use egui_file::FileDialog;
use egui_plot::{Bar, BarChart, Legend, Plot, VLine};
use hdrhistogram::Histogram;
use indexmap::IndexMap;
use livesplit_auto_splitting::{
    settings, time,
    wasi_path::{path_to_wasi, wasi_to_path},
    AutoSplitter, CompiledAutoSplitter, Config, ExecutionGuard, Runtime, Timer, TimerState,
};

mod clear_vec;

enum Tab {
    Main,
    Statistics,
    Logs,
    Variables,
    SettingsGUI,
    SettingsMap,
    Processes,
    Performance,
}

#[derive(Parser)]
struct Args {
    #[arg(short, long)]
    debug: bool,
    wasm_path: Option<PathBuf>,
}

fn main() {
    let args = Args::parse();

    let shared_state = Arc::new(SharedState {
        auto_splitter: ArcSwapOption::new(None),
        memory_usage: AtomicUsize::new(0),
        handles: AtomicU64::new(0),
        tick_rate: Mutex::new(std::time::Duration::ZERO),
        slowest_tick: Mutex::new(std::time::Duration::ZERO),
        avg_tick_secs: Atomic::new(0.0),
        tick_times: Mutex::new(Histogram::new(1).unwrap()),
        processes: Mutex::new(ClearVec::new()),
    });
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
        Box::new(move |cc| {
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

            let mut dock_state = DockState::new(vec![Tab::Main, Tab::Performance]);
            let tree = dock_state.main_surface_mut();
            let [left, right] = tree.split_right(NodeIndex::root(), 0.65, vec![Tab::SettingsGUI]);
            tree.split_below(right, 0.5, vec![Tab::Variables, Tab::SettingsMap]);
            tree.split_below(left, 0.5, vec![Tab::Logs, Tab::Statistics, Tab::Processes]);

            let optimize = !args.debug;

            let mut app = Box::new(Debugger {
                dock_state,
                state: AppState {
                    path: None,
                    script_path: None,
                    module_modified_time: None,
                    script_modified_time: None,
                    optimize,
                    open_file_dialog: None,
                    module: None,
                    shared_state,
                    timer,
                    runtime: build_runtime(optimize),
                },
            });

            if let Some(path) = args.wasm_path {
                app.state.load(Load::File(path));
            }

            app
        }),
    )
    .unwrap();
}

#[derive(Default)]
struct ProcessInfo {
    path: String,
    pid: String,
}

impl Clear for ProcessInfo {
    fn clear(&mut self) {
        self.path.clear();
        self.pid.clear();
    }
}

struct SharedState {
    auto_splitter: ArcSwapOption<AutoSplitter<DebuggerTimer>>,
    tick_rate: Mutex<std::time::Duration>,
    slowest_tick: Mutex<std::time::Duration>,
    memory_usage: AtomicUsize,
    handles: AtomicU64,
    avg_tick_secs: Atomic<f64>,
    tick_times: Mutex<Histogram<u64>>,
    processes: Mutex<ClearVec<ProcessInfo>>,
}

impl SharedState {
    fn kill_auto_splitter_if_it_doesnt_react(&self) {
        let Some(auto_splitter) = &*self.auto_splitter.load() else {
            return;
        };
        if Self::try_lock(auto_splitter).is_none() {
            auto_splitter.interrupt_handle().interrupt();
        }
    }

    fn try_lock(
        auto_splitter: &AutoSplitter<DebuggerTimer>,
    ) -> Option<ExecutionGuard<'_, DebuggerTimer>> {
        for _ in 0..100 {
            if let Some(guard) = auto_splitter.try_lock() {
                return Some(guard);
            }
            thread::sleep(Duration::from_millis(1));
        }

        None
    }
}

fn runtime_thread(shared_state: Arc<SharedState>, timer: DebuggerTimer) {
    let mut next_tick = Instant::now();
    loop {
        let tick_rate = {
            if let Some(auto_splitter) = &*shared_state.auto_splitter.load() {
                let mut auto_splitter_lock = auto_splitter.lock();
                let now = Instant::now();
                let res = auto_splitter_lock.update();
                let time_of_tick = now.elapsed();
                let memory_usage = auto_splitter_lock.memory().len();
                {
                    let mut processes = shared_state.processes.lock().unwrap();
                    processes.clear();
                    auto_splitter_lock.attached_processes().for_each(|process| {
                        use std::fmt::Write;
                        let element = processes.push();
                        let _ = write!(element.pid, "{}", process.pid());
                        element
                            .path
                            .push_str(process.path().unwrap_or("Unnamed Process"));
                    });
                }
                let handles = auto_splitter_lock.handles();
                drop(auto_splitter_lock);

                shared_state
                    .memory_usage
                    .store(memory_usage, atomic::Ordering::Relaxed);
                shared_state
                    .handles
                    .store(handles, atomic::Ordering::Relaxed);

                {
                    let mut slowest_tick = shared_state.slowest_tick.lock().unwrap();
                    if time_of_tick > *slowest_tick {
                        *slowest_tick = time_of_tick;
                    }
                }

                *shared_state.tick_rate.lock().unwrap() = auto_splitter.tick_rate();
                *shared_state.tick_times.lock().unwrap() += time_of_tick.as_nanos() as u64;
                shared_state.avg_tick_secs.store(
                    0.999 * shared_state.avg_tick_secs.load(atomic::Ordering::Relaxed)
                        + 0.001 * time_of_tick.as_secs_f64(),
                    atomic::Ordering::Relaxed,
                );
                if let Err(e) = res {
                    timer.0.write().unwrap().logs.push(
                        format!("{:?}", e.context("Failed executing the auto splitter.")).into(),
                    )
                };
                auto_splitter.tick_rate()
            } else {
                shared_state.processes.lock().unwrap().clear();

                // Tick at 10 Hz when no runtime is loaded.
                std::time::Duration::from_secs(1) / 10
            }
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
    dock_state: DockState<Tab>,
    state: AppState,
}

struct AppState {
    path: Option<PathBuf>,
    script_path: Option<PathBuf>,
    module_modified_time: Option<SystemTime>,
    script_modified_time: Option<SystemTime>,
    optimize: bool,
    open_file_dialog: Option<(FileDialog, FileDialogInfo)>,
    module: Option<CompiledAutoSplitter>,
    shared_state: Arc<SharedState>,
    timer: DebuggerTimer,
    runtime: livesplit_auto_splitting::Runtime,
}

enum FileDialogInfo {
    WASM,
    Script,
    SettingsWidget(Arc<str>),
}

struct TabViewer<'a> {
    state: &'a mut AppState,
}

impl egui_dock::TabViewer for TabViewer<'_> {
    type Tab = Tab;

    fn closeable(&mut self, _: &mut Self::Tab) -> bool {
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
                        ui.label("WASM File").on_hover_text("The main auto splitter file to run.");
                        ui.horizontal(|ui| {
                            if ui.button("Open").clicked() {
                                let mut dialog = FileDialog::open_file(self.state.path.clone());
                                dialog.open();
                                self.state.open_file_dialog = Some((dialog, FileDialogInfo::WASM));
                            }
                            if let Some(auto_splitter) = &*self.state.shared_state.auto_splitter.load() {
                                    if ui.button("Restart").clicked() {
                                        self.state.load(Load::Restart);
                                    }
                                    if ui.button("Kill").clicked() {
                                        auto_splitter.interrupt_handle().interrupt();
                                    }
                            }
                        });
                        ui.end_row();

                        ui.label("Script File")
                            .on_hover_text("A script file that by itself is run by the auto splitter. This is only necessary if the WASM file by itself is a script runtime.");

                        ui.horizontal(|ui| {
                            if ui.button("Open").clicked() {
                                let mut dialog =
                                    FileDialog::open_file(self.state.script_path.clone());
                                dialog.open();
                                self.state.open_file_dialog = Some((dialog, FileDialogInfo::Script));
                            }
                            if self.state.shared_state.auto_splitter.load().is_some() {
                                if let Some(script_path) = &self.state.script_path {
                                    if ui.button("Reload").clicked() {
                                        self.state.set_script_path(script_path.clone());
                                    }
                                }
                            }
                        });
                        ui.end_row();

                        ui.label("Optimize").on_hover_text("Whether to optimize the WASM file. Don't activate this when you want to step through the source code.");
                        if ui.checkbox(&mut self.state.optimize, "").changed() {
                            self.state.runtime = build_runtime(self.state.optimize);
                            self.state.load(Load::Reload);
                        }
                        ui.end_row();

                        {
                            let mut state = self.state.timer.0.write().unwrap();

                            ui.label("Timer State").on_hover_text("The current state of the timer.");
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

                            ui.label("Game Time").on_hover_text("The currently specified game time.");
                            ui.label(fmt_duration(state.game_time));
                            ui.end_row();

                            ui.label("Game Time State").on_hover_text("The current state of the game timer.");
                            ui.label(state.game_time_state.to_str());
                            ui.end_row();

                            ui.label("Split Index").on_hover_text("The index of the current split.");
                            ui.label(state.split_index.to_string());
                            ui.end_row();
                        }
                    });
            }
            Tab::Statistics => {
                Grid::new("stats_grid")
                    .num_columns(2)
                    .spacing([40.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label("Tick Rate").on_hover_text(
                            "The duration between individual calls to the update function.",
                        );
                        ui.label(fmt_duration(
                            time::Duration::try_from(
                                *self.state.shared_state.tick_rate.lock().unwrap(),
                            )
                            .unwrap_or_default(),
                        ));
                        ui.end_row();

                        ui.label("Avg. Tick Time").on_hover_text(
                            "The average duration of the execution of the update function.",
                        );
                        ui.label(fmt_duration(time::Duration::seconds_f64(
                            self.state
                                .shared_state
                                .avg_tick_secs
                                .load(atomic::Ordering::Relaxed),
                        )));
                        ui.end_row();

                        ui.label("Slowest Tick").on_hover_text(
                            "The slowest duration of the execution of the update function.",
                        );
                        ui.horizontal(|ui| {
                            ui.label(fmt_duration(
                                time::Duration::try_from(
                                    *self.state.shared_state.slowest_tick.lock().unwrap(),
                                )
                                .unwrap_or_default(),
                            ));
                            if ui.button("Reset").clicked() {
                                *self.state.shared_state.slowest_tick.lock().unwrap() =
                                    std::time::Duration::ZERO;
                            }
                        });
                        ui.end_row();

                        let handles = self.state.shared_state.handles.load(atomic::Ordering::Relaxed);
                        ui.label("Handles").on_hover_text("The current amount of handles (processes, settings maps, setting values) used by the auto splitter.");
                        ui.label(handles.to_string());
                        ui.end_row();

                        let memory_usage = self.state.shared_state.memory_usage.load(atomic::Ordering::Relaxed);
                        ui.label("Memory").on_hover_text("The current amount of memory used by the auto splitter (stack, heap, global variables). This excludes the size of the code itself.");
                        ui.horizontal(|ui| {
                            ui.label(
                                byte_unit::Byte::from_u64(memory_usage as _)
                                    .get_appropriate_unit(byte_unit::UnitType::Binary)
                                    .to_string(),
                            );
                            if let Some(auto_splitter) = &*self.state.shared_state.auto_splitter.load() {
                                if ui.button("Dump").clicked() {
                                    if let Some(auto_splitter) = SharedState::try_lock(auto_splitter) {
                                        let result = fs::write("memory_dump.bin", auto_splitter.memory());
                                        if let Err(e) = result {
                                            self.state
                                                .timer
                                                .0
                                                .write()
                                                .unwrap()
                                                .logs
                                                .push(format!("Failed to dump memory: {}", e).into());
                                        }
                                    } else {
                                        self.state
                                                .timer
                                                .0
                                                .write()
                                                .unwrap()
                                                .logs
                                                .push("Timed out waiting for auto splitter.".into());
                                    }
                                }
                            }
                        });
                        ui.end_row();
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
            Tab::SettingsGUI => {
                if let Some(runtime) = &*self.state.shared_state.auto_splitter.load() {
                    let mut spacing = 0.0;
                    for setting in runtime.settings_widgets().iter() {
                        ui.horizontal(|ui| match setting.kind {
                            settings::WidgetKind::Bool { default_value } => {
                                ui.add_space(spacing);
                                let mut value = match runtime.settings_map().get(&setting.key) {
                                    Some(settings::Value::Bool(v)) => *v,
                                    _ => default_value,
                                };
                                if ui.checkbox(&mut value, "").changed() {
                                    loop {
                                        let old = runtime.settings_map();
                                        let mut new = old.clone();
                                        new.insert(
                                            setting.key.clone(),
                                            settings::Value::Bool(value),
                                        );
                                        if runtime.set_settings_map_if_unchanged(&old, new) {
                                            break;
                                        }
                                    }
                                }
                                let label = ui.label(&*setting.description);
                                if let Some(tooltip) = &setting.tooltip {
                                    label.on_hover_text(&**tooltip);
                                }
                            }
                            settings::WidgetKind::Title { heading_level } => {
                                spacing = 20.0 * heading_level as f32;
                                ui.add_space(spacing);
                                let label = ui.label(
                                    RichText::new(&*setting.description)
                                        .heading()
                                        .size(25.0 * 0.9f32.powi(heading_level as i32)),
                                );
                                if let Some(tooltip) = &setting.tooltip {
                                    label.on_hover_text(&**tooltip);
                                }
                                spacing += 20.0;
                            }
                            settings::WidgetKind::Choice {
                                ref default_option_key,
                                ref options,
                            } => {
                                ui.add_space(spacing);

                                let label = ui.label(&*setting.description);
                                if let Some(tooltip) = &setting.tooltip {
                                    label.on_hover_text(&**tooltip);
                                }

                                let combo_box = ComboBox::new(&setting.key, "");

                                let settings_map = runtime.settings_map();

                                let current_key = match settings_map.get(&setting.key) {
                                    Some(settings::Value::String(option_key)) => option_key,
                                    _ => &**default_option_key,
                                };

                                let mut selected = options
                                    .iter()
                                    .position(|option| &*option.key == current_key)
                                    .unwrap_or_default();

                                if combo_box
                                    .show_index(ui, &mut selected, options.len(), |i| {
                                        &*options[i].description
                                    })
                                    .changed()
                                {
                                    loop {
                                        let old = runtime.settings_map();
                                        let mut new = old.clone();
                                        new.insert(
                                            setting.key.clone(),
                                            settings::Value::String(options[selected].key.clone()),
                                        );
                                        if runtime.set_settings_map_if_unchanged(&old, new) {
                                            break;
                                        }
                                    }
                                }
                            }
                            settings::WidgetKind::FileSelection { ref filter } => {
                                ui.add_space(spacing);
                                let settings_map = runtime.settings_map();
                                let current_path: Option<PathBuf> =
                                    match settings_map.get(&setting.key) {
                                        Some(settings::Value::String(path)) => wasi_to_path(path),
                                        _ => None,
                                    };
                                if ui.button(&*setting.description).clicked() {
                                    let mut dialog = FileDialog::open_file(current_path)
                                        .filter(parse_filter(filter));
                                    dialog.open();
                                    self.state.open_file_dialog = Some((
                                        dialog,
                                        FileDialogInfo::SettingsWidget(setting.key.clone()),
                                    ));
                                }
                            }
                        });
                        ui.end_row();
                    }
                }
            }
            Tab::SettingsMap => {
                let settings_map = self
                    .state
                    .shared_state
                    .auto_splitter
                    .load()
                    .as_ref()
                    .map(|r| r.settings_map());

                if let Some(settings_map) = &settings_map {
                    render_settings_map(ui, settings_map, format_args!("map"));

                    ui.add_space(10.0);
                    if ui.button("Clear").clicked() {
                        if let Some(runtime) = &*self.state.shared_state.auto_splitter.load() {
                            runtime.set_settings_map(settings::Map::new());
                        }
                    }
                }
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
                        for process in &*self.state.shared_state.processes.lock().unwrap() {
                            ui.label(&process.pid);
                            ui.label(&process.path);
                            ui.end_row();
                        }
                    });
            }
            Tab::Performance => {
                let mut histogram = self.state.shared_state.tick_times.lock().unwrap();

                if ui.button("Clear").clicked() {
                    histogram.clear();
                }

                let mut right_x = 0.0;
                let scale_y = 100.0 / histogram.len() as f64;

                let chart = BarChart::new(
                    histogram
                        .iter_recorded()
                        .map(|bar| {
                            let left_x = right_x;
                            right_x = bar.percentile();
                            let mid_x = 0.5 * (left_x + right_x);
                            Bar::new(mid_x, scale_y * bar.count_since_last_iteration() as f64)
                                .name(format!(
                                    "{}\n{:.2}th percentile",
                                    fmt_duration(time::Duration::nanoseconds(
                                        histogram.value_at_percentile(mid_x as _) as _,
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
                    .x_axis_formatter(|x, chars, _| {
                        let mut text = x.to_string();
                        if chars >= text.len() + 2 {
                            text.push_str("th");
                        }
                        if chars >= text.len() + 11 {
                            text.push_str(" percentile");
                        }
                        text
                    })
                    .y_axis_formatter(|y, _, _| format!("{y}%"))
                    .clamp_grid(true)
                    .allow_zoom(true)
                    .allow_drag(true)
                    .show(ui, |plot_ui| {
                        plot_ui.vline(
                            VLine::new(histogram.percentile_below(histogram.mean() as _))
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
            Tab::Statistics => "Statistics",
            Tab::Logs => "Logs",
            Tab::Variables => "Variables",
            Tab::SettingsGUI => "Settings GUI",
            Tab::SettingsMap => "Settings Map",
            Tab::Processes => "Processes",
            Tab::Performance => "Performance",
        }
        .into()
    }
}

fn render_settings_map(ui: &mut egui::Ui, settings_map: &settings::Map, path: fmt::Arguments<'_>) {
    Grid::new(format!("settings_{path}"))
        .num_columns(2)
        .spacing([40.0, 4.0])
        .striped(true)
        .show(ui, |ui| {
            ui.label(RichText::new("Key").strong().underline());
            ui.label(RichText::new("Value").strong().underline());
            ui.end_row();

            for (key, value) in settings_map.iter() {
                ui.label(key);
                render_value(value, ui, format_args!("{path}.{key}"));
                ui.end_row();
            }
        });
}

fn render_settings_list(
    ui: &mut egui::Ui,
    settings_list: &settings::List,
    path: fmt::Arguments<'_>,
) {
    Grid::new(format!("settings_{path}"))
        .num_columns(1)
        .spacing([40.0, 4.0])
        .striped(true)
        .show(ui, |ui| {
            for (i, value) in settings_list.iter().enumerate() {
                render_value(value, ui, format_args!("{path}[{i}]"));
                ui.end_row();
            }
        });
}

fn render_value(value: &settings::Value, ui: &mut egui::Ui, path: fmt::Arguments<'_>) {
    match value {
        settings::Value::Map(v) => render_settings_map(ui, v, path),
        settings::Value::List(v) => render_settings_list(ui, v, path),
        settings::Value::Bool(v) => {
            ui.label(if *v { "true" } else { "false" });
        }
        settings::Value::I64(v) => {
            ui.label(v.to_string());
        }
        settings::Value::F64(v) => {
            ui.label(v.to_string());
        }
        settings::Value::String(v) => {
            ui.label(&**v);
        }
        _ => {
            ui.label("<Unsupported>");
        }
    }
}

impl App for Debugger {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut Frame) {
        ctx.request_repaint();

        if let Some(path) = &self.state.path {
            if fs::metadata(path).ok().and_then(|m| m.modified().ok())
                > self.state.module_modified_time
            {
                self.state.load(Load::Reload);
            }
        }
        if let Some(script_path) = &self.state.script_path {
            if fs::metadata(script_path)
                .ok()
                .and_then(|m| m.modified().ok())
                > self.state.script_modified_time
            {
                self.state.set_script_path(script_path.clone());
            }
        }

        if let Some((dialog, info)) = &mut self.state.open_file_dialog {
            if dialog.show(ctx).selected() {
                if let Some(file) = dialog.path().map(ToOwned::to_owned) {
                    match info {
                        FileDialogInfo::WASM => self.state.load(Load::File(file)),
                        FileDialogInfo::Script => self.state.set_script_path(file),
                        FileDialogInfo::SettingsWidget(key) => {
                            if let Some(s) = path_to_wasi(&file) {
                                if let Some(runtime) =
                                    &*self.state.shared_state.runtime.read().unwrap()
                                {
                                    loop {
                                        let old = runtime.settings_map();
                                        let mut new = old.clone();
                                        new.insert(
                                            key.clone(),
                                            settings::Value::String(s.as_ref().into()),
                                        );
                                        if runtime.set_settings_map_if_unchanged(&old, new) {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let mut tab_viewer = TabViewer {
            state: &mut self.state,
        };

        DockArea::new(&mut self.dock_state)
            .show_window_close_buttons(false)
            .style(Style::from_egui(ctx.style().as_ref()))
            .show(ctx, &mut tab_viewer);
    }
}

enum Load {
    File(PathBuf),
    Reload,
    Restart,
}

impl AppState {
    fn load(&mut self, load: Load) {
        let settings_map = if let Load::File(path) = &load {
            self.path = Some(path.clone());
            None
        } else {
            self.shared_state
                .auto_splitter
                .load()
                .as_ref()
                .map(|r| r.settings_map())
        };

        let mut succeeded = true;

        if let (Load::File(_) | Load::Reload, Some(path)) = (&load, &self.path) {
            self.module = match fs::read(path)
                .context("Failed loading the auto splitter from the file system.")
                .and_then(|data| {
                    self.runtime
                        .compile(&data)
                        .context("Failed loading the auto splitter.")
                }) {
                Ok(module) => Some(module),
                Err(e) => {
                    succeeded = false;
                    self.timer
                        .0
                        .write()
                        .unwrap()
                        .logs
                        .push(format!("{e:?}").into());
                    None
                }
            };
            self.module_modified_time = fs::metadata(path).ok().and_then(|m| m.modified().ok());
        }

        let new_auto_splitter = if let Some(module) = &self.module {
            match module
                .instantiate(
                    self.timer.clone(),
                    settings_map,
                    self.script_path.as_deref(),
                )
                .context("Failed starting the auto splitter.")
            {
                Ok(r) => Some(Arc::new(r)),
                Err(e) => {
                    succeeded = false;
                    self.timer
                        .0
                        .write()
                        .unwrap()
                        .logs
                        .push(format!("{e:?}").into());
                    None
                }
            }
        } else {
            None
        };

        self.shared_state.kill_auto_splitter_if_it_doesnt_react();
        self.shared_state.auto_splitter.store(new_auto_splitter);

        *self.shared_state.slowest_tick.lock().unwrap() = std::time::Duration::ZERO;
        self.shared_state
            .avg_tick_secs
            .store(0.0, atomic::Ordering::Relaxed);
        self.shared_state.tick_times.lock().unwrap().clear();

        let mut timer = self.timer.0.write().unwrap();
        if let Load::File(_) = &load {
            timer.clear();
        }
        timer.variables.clear();

        if succeeded {
            timer.logs.push(
                match load {
                    Load::File(_) => "Auto splitter loaded.",
                    Load::Reload => "Auto splitter reloaded.",
                    Load::Restart => "Auto splitter restarted.",
                }
                .into(),
            );
        }
    }

    fn set_script_path(&mut self, file: PathBuf) {
        let is_reload = Some(file.as_path()) == self.script_path.as_deref();
        self.script_modified_time = fs::metadata(&file).ok().and_then(|m| m.modified().ok());
        self.script_path = Some(file);
        self.timer.0.write().unwrap().logs.push(
            if is_reload {
                "Script reloaded."
            } else {
                "Script loaded."
            }
            .into(),
        );
        self.load(Load::Restart);
    }
}

fn build_runtime(optimize: bool) -> Runtime {
    let mut config = Config::default();
    config.debug_info = true;
    config.optimize = optimize;
    Runtime::new(config).unwrap()
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
        self.reset();
    }
}

// --------------------------------------------------------

fn parse_filter(filter: &str) -> egui_file::Filter {
    let variants: Vec<Vec<String>> = filter
        .split(';')
        .map(|variant| variant.split('*').map(String::from).collect())
        .collect();
    Box::new(move |p: &Path| {
        let name = p.file_name().unwrap_or_default().to_string_lossy();
        variants
            .iter()
            .any(|pieces| contains_all_in_order(&name, &pieces))
    })
}

fn contains_all_in_order(haystack: &str, needles: &[String]) -> bool {
    let mut hay: &str = haystack;
    for piece in needles {
        let Some((_, rst)) = hay.split_once(piece) else {
            return false;
        };
        hay = rst;
    }
    true
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_contains_all_in_order() {
        assert!(contains_all_in_order("bar.exe", &[".exe".to_string()]));
        assert!(contains_all_in_order(
            "bar.exe",
            &["".to_string(), ".exe".to_string()]
        ));
        assert!(contains_all_in_order(
            "bar.txt",
            &["".to_string(), ".txt".to_string()]
        ));
        assert!(!contains_all_in_order(
            "bar.txt",
            &["".to_string(), ".exe".to_string()]
        ));
        assert!(!contains_all_in_order(
            "bar.exe",
            &["".to_string(), ".txt".to_string()]
        ));
        assert!(contains_all_in_order(
            "quick brown fox",
            &["ick".to_string(), "row".to_string(), "ox".to_string()]
        ));
        assert!(!contains_all_in_order(
            "quick brown fox",
            &["row".to_string(), "ox".to_string(), "ick".to_string()]
        ));
    }

    #[test]
    fn single_pattern_filter() {
        let filter_exe = parse_filter("*.exe");
        let filter_txt = parse_filter("*.txt");
        assert!(filter_exe(Path::new(r"/foo/bar.exe")));
        assert!(filter_txt(Path::new(r"/mnt/foo/bar.txt")));
        assert!(filter_exe(Path::new(r"/mnt/c/foo/bar.exe")));
        assert!(filter_txt(Path::new(r"C:\foo\bar.txt")));
        assert!(!filter_exe(Path::new(r"/foo/bar.txt")));
        assert!(!filter_txt(Path::new(r"/mnt/foo/bar.exe")));
        let filter_bar_exe = parse_filter("*bar*.exe");
        assert!(filter_bar_exe(Path::new(r"/foo/bar.exe")));
        assert!(!filter_bar_exe(Path::new(r"/foo/bar/baz.exe")));
        assert!(!filter_bar_exe(Path::new(r"/foo/baz.exe.bar.txt")));
    }

    #[test]
    fn multi_pattern_filter() {
        let filter_txt_md = parse_filter("*.txt;*md");
        assert!(filter_txt_md(Path::new(r"/foo/bar.txt")));
        assert!(filter_txt_md(Path::new(r"/mnt/foo/bar.md")));
        assert!(filter_txt_md(Path::new(r"/mnt/c/foo/bar.txt")));
        assert!(filter_txt_md(Path::new(r"C:\foo\bar.md")));
        assert!(!filter_txt_md(Path::new(r"/foo/bar.exe")));
        assert!(!filter_txt_md(Path::new(r"/foo/bar.txt/baz.exe")));
        assert!(!filter_txt_md(Path::new(r"/foo/bar.md/baz.exe")));
    }
}
