#![deny(clippy::all)]
#![warn(clippy::pedantic)]

use anyhow::Context;
use clap::Parser;
use gtk::gdk::{Monitor, Screen};
use gtk::gio::ApplicationFlags;
use gtk::glib::{Sender, SourceId};
use gtk::{
    gdk::{self},
    glib::{self},
    prelude::{ApplicationExt, ApplicationExtManual},
    traits::{
        ContainerExt, CssProviderExt, GtkWindowExt, ProgressBarExt, StyleContextExt, WidgetExt,
    },
};
use gtk::{Application, ApplicationWindow, CssProvider, StyleContext};
use gtk_layer_shell::LayerShell;
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;
use nix::unistd::{self, ForkResult};
use nix::{fcntl, libc};
use serde_json::{Map, Value};
use std::cell::Cell;
use std::env;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Error, Write};
use std::panic::{self, PanicHookInfo};
use std::path::Path;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, SystemTime};
use std::{collections::HashSet, process, thread, time};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

const MUTED: &str = "muted";
const ONE_SECOND_DURATION: Duration = Duration::from_secs(1_u64);
const RETRY_DURATION: Duration = Duration::from_millis(10_u64);
const TIMEOUT_DURATION: Duration = Duration::from_secs(2_u64);

const CSS_CSS_SLICE: &[u8] = include_bytes!("./css.css");

/// Render an OSD when the volume level is changed
#[derive(Parser)]
#[command(author, version, about)]
struct VosdArgs {
    /// Run as a daemon
    #[arg(long = "daemon", short = 'd')]
    daemon: bool,
}

struct VosdWindow {
    application_window: Rc<ApplicationWindow>,
    box_x: gtk::Box,
    timeout: Rc<Cell<Option<SourceId>>>,
}

struct PactlData {
    base_volume: u64,
    index: u64,
    mute: bool,
    volume: Option<u64>,
}

fn main() -> Result<(), i32> {
    // TODO
    env::set_var("RUST_BACKTRACE", "1");
    // TODO
    env::set_var("RUST_LOG", "debug");

    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().pretty())
        .init();

    let result = start();

    if let Err(er) = result {
        tracing::error!(
            backtrace = %er.backtrace(),
            error = %er,
        );

        return Err(1_i32);
    }

    Ok(())
}

fn start() -> anyhow::Result<()> {
    const FOR_RUN_WITH_ARGS: [&str; 0_usize] = [];

    let VosdArgs { daemon } = VosdArgs::parse();

    // https://0xjet.github.io/3OHA/2022/04/11/post.html
    // https://unix.stackexchange.com/questions/56495/whats-the-difference-between-running-a-program-as-a-daemon-and-forking-it-into/56497#56497
    // https://stackoverflow.com/questions/5384168/how-to-make-a-daemon-process#comment64993302_5384168
    if daemon {
        tracing::info!("Attempting to run as a daemon");

        let fork_result = unsafe { unistd::fork() }?;

        if let ForkResult::Parent { .. } = fork_result {
            process::exit(libc::EXIT_SUCCESS);
        }

        unistd::setsid()?;

        unistd::chdir("/")?;

        // TODO
        // Close all file descriptors, not just these?
        {
            unistd::close(libc::STDIN_FILENO)?;

            unistd::close(libc::STDOUT_FILENO)?;

            unistd::close(libc::STDERR_FILENO)?;
        }

        {
            anyhow::ensure!(
                fcntl::open("/dev/null", OFlag::O_RDWR, Mode::empty())? == libc::STDIN_FILENO
            );

            anyhow::ensure!(unistd::dup(libc::STDIN_FILENO)? == libc::STDOUT_FILENO);

            anyhow::ensure!(unistd::dup(libc::STDIN_FILENO)? == libc::STDERR_FILENO);
        }

        let fork_result = unsafe { unistd::fork() }?;

        if let ForkResult::Parent { .. } = fork_result {
            process::exit(libc::EXIT_SUCCESS);
        }
    }

    let temp_dir_path_buf = env::temp_dir();

    panic::set_hook(Box::new(move |pa| {
        fn write_panic_hook_info_to_file(
            temp_dir_path: &Path,
            panic_hook_info: &PanicHookInfo,
        ) -> anyhow::Result<()> {
            let system_time = SystemTime::now();

            let duration = system_time.duration_since(time::UNIX_EPOCH)?;

            let duration_as_nanos = duration.as_nanos();

            let mut path_buf = temp_dir_path.to_path_buf();

            path_buf.push(format!("vosd{duration_as_nanos}"));

            let mut file = File::options()
                .create_new(true)
                .write(true)
                .open(path_buf)?;

            writeln!(file, "{panic_hook_info:?}")?;

            Ok(())
        }

        // TODO
        let _: Result<(), Error> = writeln!(io::stderr(), "{pa:?}");

        // TODO
        let _: anyhow::Result<()> = write_panic_hook_info_to_file(&temp_dir_path_buf, pa);

        // TODO
        process::exit(2_i32);
    }));

    gtk::init()?;

    let css_provider = CssProvider::new();

    css_provider.load_from_data(CSS_CSS_SLICE)?;

    let screen = Screen::default().context("TODO")?;

    StyleContext::add_provider_for_screen(
        &screen,
        &css_provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // TODO
    // Remove loop?
    loop {
        let application = Application::new(
            Some("io.github.andrewliebenow.Vosd"),
            ApplicationFlags::FLAGS_NONE,
        );

        let vosd_windows = Rc::new(Cell::new(Vec::<VosdWindow>::new()));

        let application_rc = Rc::new(application);

        // TODO
        // clone
        let application_rc_clone = application_rc.clone();

        application_rc.connect_activate(move |_| {
            // TODO
            // clone
            connect_activate_function(&vosd_windows, &application_rc_clone);
        });

        // Do not pass arguments into GApplication
        let exit_code = application_rc.run_with_args(&FOR_RUN_WITH_ARGS);

        let exit_code_value = exit_code.value();

        // TODO
        // Log this somewhere
        tracing::error!(exit_code_value, "\"run\" finished executing",);

        thread::sleep(ONE_SECOND_DURATION);
    }
}

fn connect_activate_function(
    vosd_windows: &Rc<Cell<Vec<VosdWindow>>>,
    application_rc: &Rc<Application>,
) {
    let Some(display) = gdk::Display::default() else {
        return;
    };

    initialize_windows(vosd_windows, application_rc, &display);

    {
        let application_rc_clone = application_rc.clone();
        let vosd_windows = vosd_windows.clone();

        display.connect_opened(move |di| {
            initialize_windows(&vosd_windows, &application_rc_clone, di);
        });
    }

    {
        let vosd_windows_clone = vosd_windows.clone();

        display.connect_closed(move |_, _| {
            close_all_windows(&vosd_windows_clone);
        });
    }

    {
        let application_rc_clone = application_rc.clone();
        let vosd_windows_clone = vosd_windows.clone();

        display.connect_monitor_added(move |_, mo| {
            add_window(&vosd_windows_clone, &application_rc_clone, mo);
        });
    }

    {
        let application_rc_clone = application_rc.clone();
        let vosd_windows_clone = vosd_windows.clone();

        display.connect_monitor_removed(move |di, _| {
            initialize_windows(&vosd_windows_clone, &application_rc_clone, di);
        });
    }

    let (sender, receiver) = glib::MainContext::channel::<PactlData>(glib::Priority::DEFAULT);

    thread::spawn(move || {
        let result = spawn_function(&sender);

        tracing::error!(?result);

        // TODO
        // unwrap
        result.unwrap();
    });

    {
        let vosd_windows = vosd_windows.clone();

        receiver.attach(None, move |pa| {
            let vec = vosd_windows.take();

            // TODO
            // Do more work outside of this loop
            for vo in &vec {
                // TODO
                // unwrap
                show_volume_change_notification(vo, &pa).unwrap();
            }

            vosd_windows.replace(vec);

            glib::ControlFlow::Continue
        });
    }
}

fn spawn_function(sender: &Sender<PactlData>) -> anyhow::Result<()> {
    let mut mute_volume: Option<(bool, Option<u64>)> = Option::None;

    loop {
        let listen_for_string = {
            let PactlData { index, .. } = gather_pactl_data()?;

            format!("Event 'change' on sink #{index}")
        };

        let mut child = Command::new("pactl")
            .arg("subscribe")
            .stdout(Stdio::piped())
            .spawn()?;

        let buf_reader = BufReader::new(child.stdout.as_mut().context("TODO")?);

        for re in buf_reader.lines() {
            let line = re?;

            if line == listen_for_string {
                tracing::debug!("Sending notification");

                let pactl_data = gather_pactl_data()?;

                let mute = pactl_data.mute;
                let volume = pactl_data.volume;

                let call_send = if let Some((old_mute, old_volume)) = mute_volume {
                    mute != old_mute || volume != old_volume
                } else {
                    true
                };

                mute_volume = Some((mute, volume));

                if call_send {
                    sender.send(pactl_data)?;
                }
            }
        }

        let result = child.wait();

        // TODO
        tracing::error!(
            ?result,
            "\"pactl subscribe\" process ended, starting another one"
        );
    }
}

fn add_window(
    vosd_windows: &Rc<Cell<Vec<VosdWindow>>>,
    application: &Rc<Application>,
    monitor: &Monitor,
) {
    let vosd_window = VosdWindow::new(application, monitor);

    let mut vec = vosd_windows.take();

    vec.push(vosd_window);

    vosd_windows.replace(vec);
}

fn initialize_windows(
    vosd_windows: &Rc<Cell<Vec<VosdWindow>>>,
    application_rc: &Rc<Application>,
    display: &gdk::Display,
) {
    close_all_windows(vosd_windows);

    for it in 0_i32..display.n_monitors() {
        if let Some(mo) = display.monitor(it) {
            add_window(vosd_windows, application_rc, &mo);
        }
    }
}

fn close_all_windows(vosd_windows: &Rc<Cell<Vec<VosdWindow>>>) {
    for vo in vosd_windows.take() {
        vo.application_window.close();
    }
}

fn gather_pactl_data() -> anyhow::Result<PactlData> {
    for it in 0_i32..1_024_i32 {
        let mut child = process::Command::new("pactl")
            .args(["--format=json", "list", "sinks"])
            .stdout(process::Stdio::piped())
            .spawn()?;

        let value = serde_json::from_reader::<_, Value>(child.stdout.as_mut().context("TODO")?)?;

        // Avoid zombie processes
        child.wait()?;

        let array = value.as_array().context("TODO")?;

        if array.is_empty() {
            tracing::warn!(it, "\"{}\" is empty", nameof::name_of!(array));

            thread::sleep(RETRY_DURATION);

            continue;
        }

        let mut matching_element_object_option = Option::<&Map<String, Value>>::None;

        for va in array {
            let object = va.as_object().context("TODO")?;

            let name_value = object.get("name").context("TODO")?;

            let name = name_value.as_str().context("TODO")?;

            if name.starts_with("alsa_output.") {
                matching_element_object_option = Some(object);

                break;
            }
        }

        let matching_element_object = matching_element_object_option.context("TODO")?;

        let index = matching_element_object
            .get("index")
            .context("TODO")?
            .as_u64()
            .context("TODO")?;

        let base_volume_object = matching_element_object
            .get("base_volume")
            .context("TODO")?
            .as_object()
            .context("TODO")?;

        let mute = matching_element_object
            .get("mute")
            .context("TODO")?
            .as_bool()
            .context("TODO")?;

        let base_volume = base_volume_object
            .get("value")
            .context("TODO")?
            .as_u64()
            .context("TODO")?;

        let volume_object = matching_element_object
            .get("volume")
            .context("TODO")?
            .as_object()
            .context("TODO")?;

        let mut hash_set = HashSet::<u64>::with_capacity(volume_object.len());

        for (_, va) in volume_object {
            let ma = va.as_object().context("TODO")?;

            let us = ma.get("value").context("TODO")?.as_u64().context("TODO")?;

            hash_set.insert(us);
        }

        let mut into_iter = hash_set.into_iter();

        let volume = if let Some(us) = into_iter.next() {
            match into_iter.next() {
                Some(_) => None,
                None => Some(us),
            }
        } else {
            None
        };

        return Ok(PactlData {
            base_volume,
            index,
            mute,
            volume,
        });
    }

    anyhow::bail!("Could not list sinks");
}

fn create_progress_bar(fraction: f64, apply_inactive_class: bool) -> gtk::ProgressBar {
    let progress_bar = gtk::ProgressBar::new();
    progress_bar.set_fraction(fraction);
    progress_bar.set_valign(gtk::Align::Center);

    if apply_inactive_class {
        progress_bar
            .style_context()
            .add_class("progressBar-inactive");
    }

    progress_bar
}

fn create_image(icon_name: &str) -> gtk::Image {
    gtk::Image::from_icon_name(Some(icon_name), gtk::IconSize::Dnd)
}

// TODO
// Reuse widgets instead of removing
fn show_volume_change_notification(
    vosd_window: &VosdWindow,
    pactl_data: &PactlData,
) -> anyhow::Result<()> {
    #[allow(clippy::needless_borrowed_reference)]
    let &VosdWindow {
        ref application_window,
        ref box_x,
        ref timeout,
    } = vosd_window;

    /* #region Delete existing widgets */
    for wi in box_x.children() {
        box_x.remove(&wi);
    }
    /* #endregion */

    /* #region Add new widgets */
    if let Some(us) = pactl_data.volume {
        let mute = pactl_data.mute;

        #[allow(clippy::cast_precision_loss)]
        let volume_fraction = (us as f64) / (pactl_data.base_volume as f64);

        let icon_segment = match (mute, volume_fraction) {
            (true, _) => MUTED,
            (false, fs) if fs == 0.0 => MUTED,
            (false, fs) if fs > 0.0 && fs <= 0.333 => "low",
            (false, fs) if fs > 0.333 && fs <= 0.666 => "medium",
            (false, fs) if fs > 0.666 => "high",
            _ => anyhow::bail!("This should be unreachable"),
        };

        let icon_name = format!("audio-volume-{icon_segment}-symbolic");

        let image = create_image(&icon_name);

        box_x.add(&image);

        let progress_bar = create_progress_bar(volume_fraction, mute);

        box_x.add(&progress_bar);

        let volume_percent = volume_fraction * 100.0;

        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let volume_percent_integer = volume_percent.round() as u8;

        let label_string = format!("{volume_percent_integer}%");

        let label = gtk::Label::new(Some(label_string.as_str()));

        // "min-width" prevents a width change when going from 9% to 10% or from 99% to 100%
        label.style_context().add_class("percentageLabel");

        if mute {
            label.style_context().add_class("percentageLabel-inactive");
        }

        box_x.add(&label);
    } else {
        /* #region Handle broken volume */
        let image = create_image("dialog-question-symbolic");

        box_x.add(&image);

        let label = gtk::Label::new(Some("Volume is not even"));

        box_x.add(&label);
        /* #endregion */
    }
    /* #endregion */

    /* #region Timeout logic */
    // Cancel previous timeout
    if let Some(so) = timeout.take() {
        so.remove();
    }

    {
        let application_window = application_window.clone();

        let timeout_clone = timeout.clone();

        timeout.set(Some(glib::timeout_add_local_once(
            TIMEOUT_DURATION,
            move || {
                timeout_clone.set(None);

                application_window.hide();
            },
        )));
    }
    /* #endregion */

    application_window.show_all();

    Ok(())
}

// TODO
// Get rid of constructor
impl VosdWindow {
    fn new(application: &Rc<gtk::Application>, monitor: &gdk::Monitor) -> Self {
        let application_window = gtk::ApplicationWindow::new(application.as_ref());

        // TODO Class
        application_window
            .style_context()
            .add_class(gtk::STYLE_CLASS_OSD);

        application_window.init_layer_shell();
        // Display above all other windows, including full-screen windows
        // Use of gtk_layer_shell follows https://github.com/ErikReider/SwayOSD
        application_window.set_layer(gtk_layer_shell::Layer::Overlay);
        application_window.set_monitor(monitor);

        let box_x = {
            let bo = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            bo.style_context().add_class("box");

            bo
        };

        application_window.add(&box_x);

        Self {
            application_window: Rc::new(application_window),
            box_x,
            timeout: Rc::new(Cell::new(None)),
        }
    }
}
