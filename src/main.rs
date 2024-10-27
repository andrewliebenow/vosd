use ahash::AHashSet;
use clap::Parser;
use gtk::gdk::{Monitor, Screen};
use gtk::gio::ApplicationFlags;
use gtk::glib::{SignalHandlerId, SourceId};
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
use std::cell::{Cell, RefCell};
use std::fs::File;
use std::io::Write;
use std::process::Stdio;
use std::rc::Rc;
use std::time::{Duration, SystemTime};
use std::{any, env, io};
use std::{process, thread, time};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::runtime::{self, Handle};
use tokio::signal::unix::{self, SignalKind};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;
use tracing::level_filters::LevelFilter;
use tracing::Level;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, Layer};

const CSS_CSS_SLICE: &[u8] = include_bytes!("./css.css");

const CRATE: &str = "vosd";
const FIRST_LINE: &str = ">>>";
const MUTED: &str = "muted";
const RETRY_DURATION: Duration = Duration::from_millis(10_u64);
const SEPARATOR: &str =
    "================================================================================";
const TIMEOUT_DURATION: Duration = Duration::from_secs(2_u64);

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

    if let Err(er) = start() {
        eprintln!(
            "{}",
            indoc::formatdoc! {
                r"({CRATE}) {FIRST_LINE}
                Error occurred, program exiting
                {SEPARATOR}
                {er:?}
                {SEPARATOR}"
            }
        );

        Err(1_i32)
    } else {
        eprintln!("({CRATE}) No error occurred, program exiting");

        Ok(())
    }
}

fn start() -> anyhow::Result<()> {
    let VosdArgs { daemon } = VosdArgs::parse();

    /* #region Handle daemon option */
    // https://0xjet.github.io/3OHA/2022/04/11/post.html
    // https://unix.stackexchange.com/questions/56495/whats-the-difference-between-running-a-program-as-a-daemon-and-forking-it-into/56497#56497
    // https://stackoverflow.com/questions/5384168/how-to-make-a-daemon-process#comment64993302_5384168
    if daemon {
        tracing::info!("Attempting to run as a daemon");

        // Safety: TODO
        let fork_result = unsafe { unistd::fork() }?;

        #[expect(clippy::exit, reason = "Intentional")]
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

        // Safety: TODO
        let second_fork_result = unsafe { unistd::fork() }?;

        #[expect(clippy::exit, reason = "Intentional")]
        if let ForkResult::Parent { .. } = second_fork_result {
            process::exit(libc::EXIT_SUCCESS);
        }
    }
    /* #endregion */

    /* #region Setup */
    let system_time = SystemTime::now();

    let duration = system_time.duration_since(time::UNIX_EPOCH)?;

    let duration_as_nanos = duration.as_nanos();

    let mut path_buf = env::temp_dir();

    path_buf.push(format!("vosd{duration_as_nanos}"));

    let path_buf_path = path_buf.as_path();

    let file = File::options()
        .create_new(true)
        .write(true)
        .open(path_buf_path)?;
    /* #endregion */

    writeln!(io::stderr(), "({CRATE}) Starting Tokio")?;

    let runtime = runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let handle = runtime.handle();

    let result = runtime.block_on(async {
        tracing_subscriber::registry()
            .with(fmt::layer().with_filter(LevelFilter::from_level(Level::DEBUG)))
            .with(
                fmt::layer()
                    .with_writer(file)
                    .with_filter(LevelFilter::from_level(Level::WARN)),
            )
            .init();

        let path_buf_path_display = path_buf_path.display();

        tracing::info!(
            %path_buf_path_display,
            "Starting vosd, logging warnings and errors to temporary file"
        );

        let start_inner_result = start_inner(handle).await;

        match start_inner_result {
            Ok(()) => {
                tracing::info!("Application successfully finished running");

                Ok(())
            }
            Err(er) => {
                tracing::error!(
                    backtrace = %er.backtrace(),
                    error = %er,
                    "Error occurred after logging was initialized"
                );

                Err(er)
            }
        }
    });

    writeln!(io::stderr(), "({CRATE}) Tokio stopped")?;

    result
}

async fn start_inner(handle: &Handle) -> anyhow::Result<()> {
    let mut pactl_subscribe_child = Command::new("pactl")
        .arg("subscribe")
        .stdout(Stdio::piped())
        .spawn()?;

    let cancellation_token = CancellationToken::new();

    let cancellation_token_clone_a = cancellation_token.clone();

    let shutdown_join_handle = {
        let mut interrupt_signal = unix::signal(SignalKind::interrupt())?;
        let mut terminate_signal = unix::signal(SignalKind::terminate())?;

        handle.spawn(async move {
            tokio::select! {
                _ = interrupt_signal.recv() => {
                    tracing::error!("SIGINT received, shutting down");
                }
                _ = terminate_signal.recv() => {
                    tracing::error!("SIGTERM received, shutting down");
                }
                _z = cancellation_token.cancelled() => {
                    // Stop listening for signals
                    return;
                }
            };

            cancellation_token.cancel();
        })
    };

    let (error_unbounded_sender, mut error_unbounded_receiver) =
        mpsc::unbounded_channel::<anyhow::Error>();

    let (pactl_data_unbounded_sender, pactl_data_unbounded_receiver) =
        mpsc::unbounded_channel::<PactlData>();

    let cancellation_token_clone_b = cancellation_token_clone_a.clone();

    let error_handler_join_handle = handle.spawn(async move {
        let option = tokio::select! {
            op = error_unbounded_receiver.recv() => {
                op
            }
            _z = cancellation_token_clone_a.cancelled() => {
                return;
            }
        };

        let Some(er) = option else {
            tracing::warn!("`Error` channel was closed");

            return;
        };

        tracing::error!(
            backtrace = %er.backtrace(),
            error = %er,
            "Error received by error handling task, starting shutdown"
        );

        cancellation_token_clone_a.cancel();
    });

    let cancellation_token_clone_c = cancellation_token_clone_b.clone();

    let pactl_subscribe_join_handle = handle.spawn(async move {
        let result = process_pactl_subscribe_stdout(
            &mut pactl_subscribe_child,
            pactl_data_unbounded_sender,
            &cancellation_token_clone_b,
        )
        .await;

        if let Err(er) = result {
            tracing::error!(
                backtrace = %er.backtrace(),
                error = %er,
                "\"pactl subscribe\" task failed, starting shutdown"
            );

            cancellation_token_clone_b.cancel();
        }
    });

    let application_thread_join_handle = thread::spawn(|| {
        gui_thread_function(
            pactl_data_unbounded_receiver,
            error_unbounded_sender,
            cancellation_token_clone_c,
        )
    });

    tracing::info!("Program running. Waiting for all tasks and threads to complete.");

    {
        handle
            .spawn_blocking(|| {
                let join_handle_str = nameof::name_of!(application_thread_join_handle);

                tracing::debug!(join_handle_str, "Joining `JoinHandle`");

                let application_thread_join_handle_join_result =
                    application_thread_join_handle.join();

                tracing::debug!(
                    ?application_thread_join_handle_join_result,
                    join_handle_str,
                    "Joined `JoinHandle`"
                );
            })
            .await?;
    }

    {
        let join_handle_str = nameof::name_of!(error_handler_join_handle);

        tracing::debug!(join_handle_str, "Joining `JoinHandle`");

        error_handler_join_handle.await?;
    }

    {
        let join_handle_str = nameof::name_of!(pactl_subscribe_join_handle);

        tracing::debug!(join_handle_str, "Joining `JoinHandle`");

        pactl_subscribe_join_handle.await?;
    }

    {
        let join_handle_str = nameof::name_of!(shutdown_join_handle);

        tracing::debug!(join_handle_str, "Joining `JoinHandle`");

        shutdown_join_handle.await?;
    }

    tracing::info!("All tasks and threads joined. Exiting.");

    Ok(())
}

fn gui_thread_function(
    mut pactl_data_unbounded_receiver: UnboundedReceiver<PactlData>,
    error_unbounded_sender: UnboundedSender<anyhow::Error>,
    cancellation_token: CancellationToken,
) -> anyhow::Result<i32> {
    const FOR_RUN_WITH_ARGS: [&str; 0_usize] = [];

    gtk::init()?;

    let css_provider = CssProvider::new();

    css_provider.load_from_data(CSS_CSS_SLICE)?;

    let display = gdk::Display::default().to_result()?;

    let screen = Screen::default().to_result()?;

    StyleContext::add_provider_for_screen(
        &screen,
        &css_provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let application = Application::new(
        Some("io.github.andrewliebenow.Vosd"),
        ApplicationFlags::FLAGS_NONE,
    );

    let vosd_windows = Rc::new(RefCell::new(Vec::<VosdWindow>::new()));

    let application_clone = application.clone();

    let vosd_windows_clone = vosd_windows.clone();

    let _: glib::SignalHandlerId = application
        .connect_activate(move |ap| connect_activate_function(ap, &vosd_windows, &display));

    let _z = glib::spawn_future_local(async move {
        loop {
            let option = tokio::select! {
                op = pactl_data_unbounded_receiver.recv() => {
                    op
                }
                _z = cancellation_token.cancelled() => {
                    break;
                }
            };

            let Some(pa) = option else {
                tracing::warn!("`PactlData` channel was closed");

                break;
            };

            let ref_mut = vosd_windows_clone.borrow_mut();

            // TODO
            // Do more work outside of this loop
            for vo in ref_mut.iter() {
                if let Err(er) = show_volume_change_notification(vo, &pa) {
                    if let Err(se) = error_unbounded_sender.send(er) {
                        tracing::error!(?se, "Unable to send `Error` through `Error` channel");
                    }

                    continue;
                }
            }
        }

        application_clone.quit();
    });

    // Do not pass arguments into `GApplication`
    let exit_code = application.run_with_args(&FOR_RUN_WITH_ARGS);

    let exit_code_value = exit_code.value();

    anyhow::Result::<_>::Ok(exit_code_value)
}

fn connect_activate_function(
    application: &Application,
    vosd_windows: &Rc<RefCell<Vec<VosdWindow>>>,
    display: &gdk::Display,
) {
    initialize_windows(&mut vosd_windows.borrow_mut(), application, display);

    {
        let application_to_owned = application.to_owned();
        let vosd_windows_clone = vosd_windows.clone();

        let _: SignalHandlerId = display.connect_opened(move |di| {
            let mut ref_mut = vosd_windows_clone.borrow_mut();

            initialize_windows(&mut ref_mut, &application_to_owned, di);
        });
    }

    {
        let vosd_windows_clone = vosd_windows.clone();

        let _: SignalHandlerId = display.connect_closed(move |_, _| {
            let mut ref_mut = vosd_windows_clone.borrow_mut();

            close_all_windows(&mut ref_mut);
        });
    }

    {
        let application_rc_clone = application.clone();
        let vosd_windows_clone = vosd_windows.clone();

        let _: SignalHandlerId = display.connect_monitor_added(move |_, mo| {
            let mut ref_mut = vosd_windows_clone.borrow_mut();

            add_window(&mut ref_mut, &application_rc_clone, mo);
        });
    }

    {
        let application_rc_clone = application.clone();
        let vosd_windows_clone = vosd_windows.clone();

        let _: SignalHandlerId = display.connect_monitor_removed(move |di, _| {
            let mut ref_mut = vosd_windows_clone.borrow_mut();

            initialize_windows(&mut ref_mut, &application_rc_clone, di);
        });
    }
}

async fn process_pactl_subscribe_stdout(
    child: &mut Child,
    unbounded_sender: UnboundedSender<PactlData>,
    cancellation_token: &CancellationToken,
) -> anyhow::Result<()> {
    let mut mute_volume = Option::<(bool, Option<u64>)>::None;

    let listen_for_string = {
        let PactlData { index, .. } = gather_pactl_data()?;

        format!("Event 'change' on sink #{index}")
    };

    let buf_reader = BufReader::new(child.stdout.take().to_result()?);

    let mut lines = buf_reader.lines();

    loop {
        let result = tokio::select! {
            re = lines.next_line() => {
                re
            }
            _z = cancellation_token.cancelled() => {
                return Ok(());
            }
        };

        let option = result?;

        let Some(line) = option else {
            anyhow::bail!("\"pactl subscribe\" stdout signalled EOF");
        };

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
                unbounded_sender.send(pactl_data)?;
            }
        }
    }
}

fn add_window(vosd_windows: &mut Vec<VosdWindow>, application: &Application, monitor: &Monitor) {
    let application_window = gtk::ApplicationWindow::new(application);

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

    let vosd_window = VosdWindow {
        application_window: Rc::new(application_window),
        box_x,
        timeout: Rc::new(Cell::new(None)),
    };

    vosd_windows.push(vosd_window);
}

fn initialize_windows(
    vosd_windows: &mut Vec<VosdWindow>,
    application: &Application,
    display: &gdk::Display,
) {
    close_all_windows(vosd_windows);

    for it in 0_i32..display.n_monitors() {
        if let Some(mo) = display.monitor(it) {
            add_window(vosd_windows, application, &mo);
        }
    }
}

fn close_all_windows(vosd_windows: &mut Vec<VosdWindow>) {
    for vo in vosd_windows.iter_mut() {
        vo.application_window.close();
    }

    vosd_windows.clear();
}

fn gather_pactl_data() -> anyhow::Result<PactlData> {
    for it in 0_i32..1_024_i32 {
        let mut child = process::Command::new("pactl")
            .args(["--format=json", "list", "sinks"])
            .stdout(process::Stdio::piped())
            .spawn()?;

        let value = serde_json::from_reader::<_, Value>(child.stdout.as_mut().to_result()?)?;

        // Avoid zombie processes
        child.wait()?;

        let array = value.as_array().to_result()?;

        if array.is_empty() {
            tracing::warn!(it, "\"{}\" is empty", nameof::name_of!(array));

            thread::sleep(RETRY_DURATION);

            continue;
        }

        let mut matching_element_object_option = Option::<&Map<String, Value>>::None;

        for va in array {
            let object = va.as_object().to_result()?;

            let name_value = object.get("name").to_result()?;

            let name = name_value.as_str().to_result()?;

            if name.starts_with("alsa_output.") {
                matching_element_object_option = Some(object);

                break;
            }
        }

        let matching_element_object = matching_element_object_option.to_result()?;

        let index = matching_element_object
            .get("index")
            .to_result()?
            .as_u64()
            .to_result()?;

        let base_volume_object = matching_element_object
            .get("base_volume")
            .to_result()?
            .as_object()
            .to_result()?;

        let mute = matching_element_object
            .get("mute")
            .to_result()?
            .as_bool()
            .to_result()?;

        let base_volume = base_volume_object
            .get("value")
            .to_result()?
            .as_u64()
            .to_result()?;

        let volume_object = matching_element_object
            .get("volume")
            .to_result()?
            .as_object()
            .to_result()?;

        let mut a_hash_set = AHashSet::<u64>::with_capacity(volume_object.len());

        for (_, va) in volume_object {
            let ma = va.as_object().to_result()?;

            let us = ma.get("value").to_result()?.as_u64().to_result()?;

            a_hash_set.insert(us);
        }

        let mut into_iter = a_hash_set.into_iter();

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
    let VosdWindow {
        ref application_window,
        ref box_x,
        ref timeout,
    } = *vosd_window;

    /* #region Delete existing widgets */
    for wi in box_x.children() {
        box_x.remove(&wi);
    }
    /* #endregion */

    /* #region Add new widgets */
    if let Some(us) = pactl_data.volume {
        let mute = pactl_data.mute;

        #[expect(
            clippy::as_conversions,
            clippy::cast_precision_loss,
            reason = "Unimportant"
        )]
        let volume_fraction = (us as f64) / (pactl_data.base_volume as f64);

        let icon_segment = match (mute, volume_fraction) {
            (true, _) => MUTED,
            (false, fs) if fs == 0.0_f64 => MUTED,
            (false, fs) if fs > 0.0_f64 && fs <= 0.333_f64 => "low",
            (false, fs) if fs > 0.333_f64 && fs <= 0.666_f64 => "medium",
            (false, fs) if fs > 0.666_f64 => "high",
            _ => anyhow::bail!("This should be unreachable"),
        };

        let icon_name = format!("audio-volume-{icon_segment}-symbolic");

        let image = create_image(&icon_name);

        box_x.add(&image);

        let progress_bar = create_progress_bar(volume_fraction, mute);

        box_x.add(&progress_bar);

        let volume_percent = volume_fraction * 100.0_f64;

        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "Unimportant"
        )]
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
        let application_window_clone = application_window.clone();

        let timeout_clone = timeout.clone();

        timeout.set(Some(glib::timeout_add_local_once(
            TIMEOUT_DURATION,
            move || {
                timeout_clone.set(None);

                application_window_clone.hide();
            },
        )));
    }
    /* #endregion */

    application_window.show_all();

    Ok(())
}

pub trait OptionToResultExt<T> {
    fn to_result(self) -> anyhow::Result<T>;
}

impl<T> OptionToResultExt<T> for Option<T> {
    fn to_result(self) -> anyhow::Result<T> {
        self.ok_or_else(|| anyhow::anyhow!("Value is missing (type `{}`)", any::type_name::<T>()))
    }
}
