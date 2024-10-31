use clap::Parser;
use gtk::{
    gdk::{self, Monitor, Screen},
    gio::ApplicationFlags,
    glib::{self, SignalHandlerId, SourceId},
    prelude::{ApplicationExt, ApplicationExtManual, ImageExt, LabelExt},
    traits::{ContainerExt, CssProviderExt, GtkWindowExt, ProgressBarExt, StyleContextExt, WidgetExt},
    Align, Application, ApplicationWindow, CssProvider, IconSize, Image, Label, Orientation, ProgressBar, StyleContext,
};
use gtk_layer_shell::LayerShell;
use nix::{
    fcntl,
    fcntl::OFlag,
    libc,
    sys::stat::Mode,
    unistd::{self, ForkResult},
};
use std::{
    any,
    cell::{Cell, RefCell},
    env,
    fs::File,
    io::{self, Write},
    process::Stdio,
    rc::Rc,
    time::{Duration, SystemTime},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::{Child, Command},
    runtime::{self, Handle},
    signal::unix::{self, SignalKind},
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
};
use tokio_util::sync::CancellationToken;
use tracing::{level_filters::LevelFilter, Level};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, Layer};
use types::{Root, RootElement};

const CSS_CSS_SLICE: &[u8] = include_bytes!("./css.css");

const CRATE: &str = "vosd";
const FIRST_LINE: &str = ">>>";
const MUTED_ICON_NAME: &str = "audio-volume-muted-symbolic";
const RETRY_DURATION: Duration = Duration::from_millis(10_u64);
const SEPARATOR: &str = "================================================================================";
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
    application_window: ApplicationWindow,
    balanced_channels_box: gtk::Box,
    balanced_channels_widgets: BalancedChannelsWidgets,
    last_volume_change_notification_had_not_balanced_channels: bool,
    not_balanced_channels_box: gtk::Box,
    timeout: Rc<Cell<Option<SourceId>>>,
}

#[derive(Debug)]
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

        #[expect(clippy::absolute_paths, reason = "Conflicting imports")]
        #[expect(clippy::exit, reason = "Intentional")]
        if let ForkResult::Parent { .. } = fork_result {
            std::process::exit(libc::EXIT_SUCCESS);
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
                fcntl::open(
                    "/dev/null",
                    OFlag::O_RDWR,
                    Mode::empty()
                )? == libc::STDIN_FILENO
            );

            anyhow::ensure!(unistd::dup(libc::STDIN_FILENO)? == libc::STDOUT_FILENO);

            anyhow::ensure!(unistd::dup(libc::STDIN_FILENO)? == libc::STDERR_FILENO);
        }

        // Safety: TODO
        let second_fork_result = unsafe { unistd::fork() }?;

        #[expect(clippy::absolute_paths, reason = "Conflicting imports")]
        #[expect(clippy::exit, reason = "Intentional")]
        if let ForkResult::Parent { .. } = second_fork_result {
            std::process::exit(libc::EXIT_SUCCESS);
        }
    }
    /* #endregion */

    /* #region Setup */
    let system_time = SystemTime::now();

    let duration = {
        #[expect(clippy::absolute_paths, reason = "Conflicting imports")]
        {
            system_time.duration_since(std::time::UNIX_EPOCH)?
        }
    };

    let duration_as_nanos = duration.as_nanos();

    let mut path_buf = env::temp_dir();

    path_buf.push(format!(
        "vosd---6f983a64f4c2705fb802aa8ed3942f23---{duration_as_nanos}"
    ));

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
                    .with_filter(LevelFilter::from_level(Level::INFO)),
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

    let (error_unbounded_sender, mut error_unbounded_receiver) = mpsc::unbounded_channel::<anyhow::Error>();

    let (pactl_data_unbounded_sender, pactl_data_unbounded_receiver) = mpsc::unbounded_channel::<PactlData>();

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

    let application_thread_join_handle = handle.spawn_blocking(|| {
        gui_thread_function(
            pactl_data_unbounded_receiver,
            error_unbounded_sender,
            cancellation_token_clone_c,
        )
    });

    tracing::info!("Program running. Waiting for all tasks and threads to complete.");

    {
        let join_handle_str = nameof::name_of!(application_thread_join_handle);

        tracing::debug!(join_handle_str, "Joining `JoinHandle`");

        let application_thread_join_handle_result = application_thread_join_handle.await?;

        tracing::debug!(
            ?application_thread_join_handle_result,
            join_handle_str,
            "Joined `JoinHandle`"
        );
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

    glib::log_set_default_handler(glib::rust_log_handler);

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

    let application_clone = application.clone();

    let vosd_windows = Rc::new(RefCell::new(Vec::<VosdWindow>::new()));

    let vosd_windows_clone = vosd_windows.clone();

    let _: SignalHandlerId = application_clone.connect_activate(move |ap| connect_activate_function(ap, &vosd_windows, &display));

    // There are some short periods in which the number of windows may be zero.
    // In this case, the main loop will stop and the `GApplication` shuts down.
    // Simply spawning a future using `glib::spawn_future_local` does not prevent this from happening.
    // Explicitly keep the main loop running with an `ApplicationHoldGuard` so the application survives these short periods in which the window count is 0.
    let application_hold_guard = application.hold();

    let cancellation_token_clone = cancellation_token.clone();

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

            let mut ref_mut = vosd_windows_clone.borrow_mut();

            // TODO
            // Do more work outside of this loop
            for vo in ref_mut.iter_mut() {
                if let Err(er) = show_volume_change_notification(vo, &pa) {
                    if let Err(se) = error_unbounded_sender.send(er) {
                        tracing::error!(
                            ?se,
                            "Unable to send `Error` through `Error` channel"
                        );
                    }

                    continue;
                }
            }
        }

        tracing::info!("Cancellation token was cancelled, quitting application and then finishing UI updating task");

        drop(application_hold_guard);

        application.quit();
    });

    // Do not pass arguments into `GApplication`
    let exit_code = application_clone.run_with_args(&FOR_RUN_WITH_ARGS);

    tracing::info!("`run_with_args` returned (application has stopped)");

    // TODO
    // Hacky
    // In case the application stopped on its own, stop the parent thread. Should not happen.
    cancellation_token_clone.cancel();

    let exit_code_value = exit_code.value();

    anyhow::Result::<_>::Ok(exit_code_value)
}

fn connect_activate_function(
    application: &Application,
    vosd_windows: &Rc<RefCell<Vec<VosdWindow>>>,
    display: &gdk::Display,
) {
    initialize_windows(
        &mut vosd_windows.borrow_mut(),
        application,
        display,
    );

    let _connect_opened_signal_handler_id: SignalHandlerId = {
        let application_to_owned = application.to_owned();
        let vosd_windows_clone = vosd_windows.clone();

        display.connect_opened(move |di| {
            let mut ref_mut = vosd_windows_clone.borrow_mut();

            initialize_windows(&mut ref_mut, &application_to_owned, di);
        })
    };

    let _connect_closed_signal_handler_id: SignalHandlerId = {
        let vosd_windows_clone = vosd_windows.clone();

        display.connect_closed(move |_, _| {
            let mut ref_mut = vosd_windows_clone.borrow_mut();

            close_all_windows(&mut ref_mut);
        })
    };

    let _connect_monitor_added_signal_handler_id: SignalHandlerId = {
        let application_rc_clone = application.clone();
        let vosd_windows_clone = vosd_windows.clone();

        display.connect_monitor_added(move |_, mo| {
            let mut ref_mut = vosd_windows_clone.borrow_mut();

            add_window(&mut ref_mut, &application_rc_clone, mo);
        })
    };

    let _connect_monitor_removed_signal_handler_id: SignalHandlerId = {
        let application_rc_clone = application.clone();
        let vosd_windows_clone = vosd_windows.clone();

        display.connect_monitor_removed(move |di, _| {
            let mut ref_mut = vosd_windows_clone.borrow_mut();

            initialize_windows(&mut ref_mut, &application_rc_clone, di);
        })
    };
}

async fn process_pactl_subscribe_stdout(
    child: &mut Child,
    unbounded_sender: UnboundedSender<PactlData>,
    cancellation_token: &CancellationToken,
) -> anyhow::Result<()> {
    let mut mute_and_volume = Option::<(bool, Option<u64>)>::None;

    let mut buffer = Vec::<u8>::new();

    let listen_for_string = {
        let PactlData { index, .. } = gather_pactl_data(&mut buffer).await?;

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
            tracing::debug!("Received 'change' event on sink being monitored");

            let pactl_data = gather_pactl_data(&mut buffer).await?;

            let mute = pactl_data.mute;
            let volume = pactl_data.volume;

            let call_send = if let Some((old_mute, old_volume)) = mute_and_volume {
                if mute == old_mute {
                    match (volume, old_volume) {
                        (Some(us), Some(usi)) => us != usi,
                        _ => true,
                    }
                } else {
                    true
                }
            } else {
                true
            };

            mute_and_volume = Some((mute, volume));

            if call_send {
                tracing::debug!("Mute status or volume level changed, sending notification");

                unbounded_sender.send(pactl_data)?;
            }
        }
    }
}

fn add_window(
    vosd_windows: &mut Vec<VosdWindow>,
    application: &Application,
    monitor: &Monitor,
) {
    let get_box = || {
        let bo = gtk::Box::new(Orientation::Horizontal, 12_i32);

        bo.style_context().add_class("box");

        bo
    };

    let root_box = gtk::Box::new(Orientation::Vertical, 0_i32);
    root_box.set_visible(true);

    let (balanced_channels_box, balanced_channels_widgets) = {
        let bo = get_box();

        let ba = construct_balanced_channels_widgets(&bo);

        root_box.add(&bo);

        (bo, ba)
    };

    let not_balanced_channels_box = {
        let bo = get_box();

        construct_not_balanced_channels_widgets(&bo);

        root_box.add(&bo);

        bo
    };

    let application_window = ApplicationWindow::new(application);
    application_window.init_layer_shell();
    // Display above all other windows, including full-screen windows
    // Use of gtk_layer_shell follows https://github.com/ErikReider/SwayOSD
    application_window.set_layer(gtk_layer_shell::Layer::Overlay);
    application_window.set_monitor(monitor);

    // TODO
    // Class
    application_window
        .style_context()
        .add_class(gtk::STYLE_CLASS_OSD);

    application_window.add(&root_box);

    let vosd_window = VosdWindow {
        application_window,
        balanced_channels_box,
        balanced_channels_widgets,
        last_volume_change_notification_had_not_balanced_channels: false,
        not_balanced_channels_box,
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

async fn gather_pactl_data(buffer: &mut Vec<u8>) -> anyhow::Result<PactlData> {
    for it in 0_i32..1_024_i32 {
        let mut child = Command::new("pactl")
            .args(["--format=json", "list", "sinks"])
            .stdout(Stdio::piped())
            .spawn()?;

        let child_stdout = child.stdout.as_mut().to_result()?;

        buffer.clear();

        child_stdout.read_to_end(buffer).await?;

        let root = serde_json::from_slice::<Root>(buffer.as_slice())?;

        // Avoid zombie processes
        child.wait().await?;

        if root.is_empty() {
            tracing::warn!(
                it,
                "\"{}\" is empty",
                nameof::name_of!(root)
            );

            #[expect(clippy::absolute_paths, reason = "Conflicting imports")]
            {
                tokio::time::sleep(RETRY_DURATION).await;
            }

            continue;
        }

        for ro in root {
            let RootElement {
                base_volume,
                index,
                mute,
                name,
                volume,
            } = ro;

            if name.starts_with("alsa_output.") {
                let mut different_volume_values_found = false;
                let mut last_volume_value = Option::<u64>::None;

                for va in volume.values() {
                    let ma = va.as_object().to_result()?;

                    let us = ma
                        .get("value")
                        .to_result()?
                        .as_u64()
                        .to_result()?;

                    if let Some(usi) = last_volume_value {
                        if us != usi {
                            different_volume_values_found = true;

                            break;
                        }
                    }

                    last_volume_value = Some(us);
                }

                let volume_option = if different_volume_values_found {
                    None
                } else {
                    if last_volume_value.is_none() {
                        tracing::warn!("\"pactl\" did not report any volume values for the default output sink");
                    }

                    last_volume_value
                };

                return Ok(PactlData {
                    base_volume: base_volume.value,
                    index,
                    mute,
                    volume: volume_option,
                });
            }
        }

        anyhow::bail!("Could not identify default output sink");
    }

    anyhow::bail!("Could not list sinks");
}

fn create_image(icon_name: &str) -> Image {
    Image::from_icon_name(Some(icon_name), IconSize::Dnd)
}

struct BalancedChannelsWidgets {
    image: Image,
    label: Label,
    progress_bar: ProgressBar,
}

fn construct_balanced_channels_widgets(box_x: &gtk::Box) -> BalancedChannelsWidgets {
    let image = create_image(MUTED_ICON_NAME);
    image.set_visible(true);

    box_x.add(&image);

    let progress_bar = ProgressBar::new();
    progress_bar.set_fraction(0_f64);
    progress_bar.set_valign(Align::Center);
    progress_bar.set_visible(true);

    box_x.add(&progress_bar);

    let label = Label::new(None);
    label.set_visible(true);

    // "min-width" prevents a width change when going from 9% to 10% or from 99% to 100%
    label
        .style_context()
        .add_class("percentageLabel");

    box_x.add(&label);

    BalancedChannelsWidgets { image, label, progress_bar }
}

// Handle broken volume
fn construct_not_balanced_channels_widgets(box_x: &gtk::Box) {
    let image = create_image("dialog-question-symbolic");
    image.set_visible(true);

    box_x.add(&image);

    let label = Label::new(Some(
        "Channels have different volume levels",
    ));
    label.set_visible(true);

    box_x.add(&label);
}

fn show_volume_change_notification(
    vosd_window: &mut VosdWindow,
    pactl_data: &PactlData,
) -> anyhow::Result<()> {
    let VosdWindow {
        ref application_window,
        ref balanced_channels_box,
        ref balanced_channels_widgets,
        ref mut last_volume_change_notification_had_not_balanced_channels,
        ref not_balanced_channels_box,
        ref timeout,
    } = *vosd_window;

    let PactlData { base_volume, mute, volume, .. } = *pactl_data;

    /* #region Add new widgets */
    if let Some(us) = volume {
        if *last_volume_change_notification_had_not_balanced_channels {
            application_window.hide();

            not_balanced_channels_box.hide();
        }

        let &&BalancedChannelsWidgets {
            ref image,
            ref label,
            ref progress_bar,
        } = &balanced_channels_widgets;

        let volume_fraction = {
            #[expect(clippy::as_conversions, clippy::cast_precision_loss, reason = "Unimportant")]
            {
                (us as f64) / (base_volume as f64)
            }
        };

        let icon_name = match (mute, volume_fraction) {
            (true, _) => MUTED_ICON_NAME,
            (false, fs) if fs == 0.0_f64 => MUTED_ICON_NAME,
            (false, fs) if fs > 0.0_f64 && fs <= 0.333_f64 => "audio-volume-low-symbolic",
            (false, fs) if fs > 0.333_f64 && fs <= 0.666_f64 => "audio-volume-medium-symbolic",
            (false, fs) if fs > 0.666_f64 => "audio-volume-high-symbolic",
            _ => anyhow::bail!("This should be unreachable"),
        };

        image.set_icon_name(Some(icon_name));

        progress_bar.set_fraction(volume_fraction);

        {
            const PROGRESS_BAR_INACTIVE_CLASS: &str = "progressBar-inactive";

            let style_context = progress_bar.style_context();

            if mute {
                style_context.add_class(PROGRESS_BAR_INACTIVE_CLASS);
            } else {
                style_context.remove_class(PROGRESS_BAR_INACTIVE_CLASS);
            }
        }

        let volume_percent = volume_fraction * 100.0_f64;

        let volume_percent_integer = {
            #[expect(clippy::as_conversions, clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "Unimportant")]
            {
                volume_percent.round() as u8
            }
        };

        let label_string = format!("{volume_percent_integer}%");

        label.set_text(label_string.as_str());

        {
            const PERCENTAGE_LABEL_INACTIVE_CLASS: &str = "percentageLabel-inactive";

            let style_context = label.style_context();

            if mute {
                style_context.add_class(PERCENTAGE_LABEL_INACTIVE_CLASS);
            } else {
                style_context.remove_class(PERCENTAGE_LABEL_INACTIVE_CLASS);
            }
        }

        balanced_channels_box.set_visible(true);

        *last_volume_change_notification_had_not_balanced_channels = false;
    } else {
        if !(*last_volume_change_notification_had_not_balanced_channels) {
            application_window.hide();

            balanced_channels_box.hide();
        }

        not_balanced_channels_box.set_visible(true);

        *last_volume_change_notification_had_not_balanced_channels = true;
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

    application_window.set_visible(true);

    Ok(())
}

pub trait OptionToResultExt<T> {
    fn to_result(self) -> anyhow::Result<T>;
}

impl<T> OptionToResultExt<T> for Option<T> {
    fn to_result(self) -> anyhow::Result<T> {
        self.ok_or_else(|| {
            anyhow::anyhow!(
                "Value is missing (type `{}`)",
                any::type_name::<T>()
            )
        })
    }
}

// Generated with https://app.quicktype.io
// Customized and unnecessary fields removed
mod types {
    use serde::{Deserialize, Serialize};
    use serde_json::{Map, Value};

    pub type Root = Vec<RootElement>;

    #[derive(Serialize, Deserialize)]
    pub struct RootElement {
        pub base_volume: BaseVolume,
        pub index: u64,
        pub mute: bool,
        pub name: String,
        pub volume: Map<String, Value>,
    }

    #[derive(Serialize, Deserialize)]
    pub struct BaseVolume {
        pub db: String,
        pub value_percent: String,
        pub value: u64,
    }
}
