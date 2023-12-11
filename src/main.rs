// https://github.com/ErikReider/SwayOSD

#![deny(clippy::all)]
#![warn(clippy::pedantic)]

use std::io::{self, BufRead, Write};

use std::{cell, collections::HashSet, process, rc, thread, time};
use std::{fs, panic};

use gtk::{
    gdk::{self},
    glib::{self},
    prelude::{ApplicationExt, ApplicationExtManual},
    traits::{
        ContainerExt, CssProviderExt, GtkWindowExt, ProgressBarExt, StyleContextExt, WidgetExt,
    },
};
use gtk_layer_shell::LayerShell;

const MUTED: &str = "muted";
const ONE_SECOND_DURATION: time::Duration = time::Duration::from_secs(1);
const RETRY_DURATION: time::Duration = time::Duration::from_millis(10);
const TIMEOUT_DURATION: time::Duration = time::Duration::from_secs(2);

struct VosdWindow {
    application_window: rc::Rc<gtk::ApplicationWindow>,
    boxz: gtk::Box,
    timeout: rc::Rc<cell::Cell<Option<glib::SourceId>>>,
}

struct PactlData {
    base_volume: u64,
    index: u64,
    mute: bool,
    volume: Option<u64>,
}

#[allow(clippy::too_many_lines)]
fn main() {
    panic::set_hook(Box::new(|pa| {
        let system_time = time::SystemTime::now();

        let duration = system_time.duration_since(time::UNIX_EPOCH).unwrap();

        let duration_as_millis = duration.as_millis();

        let mut file = fs::File::create(format!("vosd{duration_as_millis}")).unwrap();

        writeln!(file, "{pa}").unwrap();

        let _ = writeln!(io::stderr(), "{pa}");

        process::exit(1);
    }));

    assert!(gtk::init().is_ok());

    let css_provider = gtk::CssProvider::new();

    let css_css_bytes = include_bytes!("css.css");

    css_provider.load_from_data(css_css_bytes).unwrap();

    let screen = gdk::Screen::default().unwrap();

    gtk::StyleContext::add_provider_for_screen(
        &screen,
        &css_provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    loop {
        let application = gtk::Application::new(
            ("io.github.andrewliebenow.Vosd").into(),
            gtk::gio::ApplicationFlags::FLAGS_NONE,
        );

        let vosd_windows = rc::Rc::new(cell::Cell::new(Vec::new()));

        application.connect_activate(move |ap| {
            let Some(display) = gdk::Display::default() else {
                return;
            };

            initialize_windows(&vosd_windows, ap, &display);

            {
                let ap = ap.clone();
                let vosd_windows = vosd_windows.clone();

                display.connect_opened(move |di| {
                    initialize_windows(&vosd_windows, &ap, di);
                });
            }

            {
                let vosd_windows = vosd_windows.clone();

                display.connect_closed(move |_, _| {
                    close_all_windows(&vosd_windows);
                });
            }

            {
                let ap = ap.clone();
                let vosd_windows = vosd_windows.clone();

                display.connect_monitor_added(move |_, mo| {
                    add_window(&vosd_windows, &ap, mo);
                });
            }

            {
                let ap = ap.clone();
                let vosd_windows = vosd_windows.clone();

                display.connect_monitor_removed(move |di, _| {
                    initialize_windows(&vosd_windows, &ap, di);
                });
            }

            let (sender, receiver) =
                glib::MainContext::channel::<PactlData>(glib::Priority::DEFAULT);

            thread::spawn(move || {
                let mut mute_volume: Option<(bool, Option<u64>)> = Option::None;

                loop {
                    let listen_for_string = {
                        let PactlData { index, .. } = gather_pactl_data();

                        format!("Event 'change' on sink #{index}")
                    };

                    let mut child = process::Command::new("pactl")
                        .arg("subscribe")
                        .stdout(process::Stdio::piped())
                        .spawn()
                        .unwrap();

                    let buf_reader = io::BufReader::new(child.stdout.take().unwrap());

                    for re in buf_reader.lines() {
                        let line = re.unwrap();

                        if line == listen_for_string {
                            // Don't panic if pipe is broken (eprintln!/println! panic)
                            let _ = writeln!(io::stdout(), "Sending notification");

                            let pactl_data = gather_pactl_data();

                            let mute = pactl_data.mute;
                            let volume = pactl_data.volume;

                            let call_send = if let Some((old_mute, old_volume)) = mute_volume {
                                mute != old_mute || volume != old_volume
                            } else {
                                true
                            };

                            mute_volume = Some((mute, volume));

                            if call_send {
                                sender.send(pactl_data).unwrap();
                            }
                        }
                    }

                    let result = child.wait();

                    // Don't panic if pipe is broken (eprintln!/println! panic)
                    let _ = writeln!(
                io::stderr(),
                "\"pactl subscribe\" process ended, starting another one:\n===>\n{result:?}\n<==="
            );
                }
            });

            {
                let vosd_windows = vosd_windows.clone();

                receiver.attach(None, move |pa| {
                    let vec = vosd_windows.take();

                    // TODO Do more work outside of this loop
                    for vo in &vec {
                        show_volume_change_notification(vo, &pa);
                    }

                    vosd_windows.replace(vec);

                    glib::ControlFlow::Continue
                });
            }
        });

        let exit_code = application.run();

        let exit_code_value = exit_code.value();

        // TODO Log this somewhere
        eprintln!("run finished executing (exit_code_value: {exit_code_value})");

        thread::sleep(ONE_SECOND_DURATION);
    }
}

fn add_window(
    vosd_windows: &rc::Rc<cell::Cell<Vec<VosdWindow>>>,
    application: &gtk::Application,
    monitor: &gdk::Monitor,
) {
    let vosd_window = VosdWindow::new(application, monitor);

    let mut vec = vosd_windows.take();

    vec.push(vosd_window);

    vosd_windows.replace(vec);
}

fn initialize_windows(
    vosd_windows: &rc::Rc<cell::Cell<Vec<VosdWindow>>>,
    application: &gtk::Application,
    display: &gdk::Display,
) {
    close_all_windows(vosd_windows);

    for i in 0..display.n_monitors() {
        if let Some(mo) = display.monitor(i) {
            add_window(vosd_windows, application, &mo);
        }
    }
}

fn close_all_windows(vosd_windows: &rc::Rc<cell::Cell<Vec<VosdWindow>>>) {
    for vo in vosd_windows.take() {
        vo.application_window.close();
    }
}

fn gather_pactl_data() -> PactlData {
    for i in 0..1_000 {
        let child = process::Command::new("pactl")
            .args(["--format=json", "list", "sinks"])
            .stdout(process::Stdio::piped())
            .spawn()
            .unwrap();

        let value: serde_json::Value = serde_json::from_reader(child.stdout.unwrap()).unwrap();

        let array = value.as_array().unwrap();

        if array.is_empty() {
            // Don't panic if pipe is broken (eprintln!/println! panic)
            let _ = writeln!(io::stderr(), "array is empty (attempt {})", i + 1);

            thread::sleep(RETRY_DURATION);

            continue;
        }

        let matching_element_object = array
            .iter()
            .find_map(|va| {
                let object = va.as_object().unwrap();

                let name_value = object.get("name").unwrap();

                let name = name_value.as_str().unwrap();

                if name.starts_with("alsa_output.") {
                    return Some(object);
                }

                None
            })
            .unwrap();

        let index = matching_element_object
            .get("index")
            .unwrap()
            .as_u64()
            .unwrap();

        let base_volume_object = matching_element_object
            .get("base_volume")
            .unwrap()
            .as_object()
            .unwrap();

        let mute = matching_element_object
            .get("mute")
            .unwrap()
            .as_bool()
            .unwrap();

        let base_volume = base_volume_object.get("value").unwrap().as_u64().unwrap();

        let volume_object = matching_element_object
            .get("volume")
            .unwrap()
            .as_object()
            .unwrap();

        let hash_set = volume_object
            .iter()
            .map(|(_, va)| {
                let ma = va.as_object().unwrap();

                ma.get("value").unwrap().as_u64().unwrap()
            })
            .collect::<HashSet<u64>>();

        let volume = if hash_set.len() == 1 {
            hash_set.iter().next().unwrap().to_owned().into()
        } else {
            None
        };

        return PactlData {
            base_volume,
            index,
            mute,
            volume,
        };
    }

    panic!("Could not list sinks");
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
    gtk::Image::from_icon_name(icon_name.into(), gtk::IconSize::Dnd)
}

// TODO Reuse widgets instead of removing
fn show_volume_change_notification(vosd_window: &VosdWindow, pactl_data: &PactlData) {
    let VosdWindow {
        application_window,
        boxz,
        timeout,
    } = vosd_window;

    /* #region Delete existing widgets */
    for wi in boxz.children() {
        boxz.remove(&wi);
    }
    /* #endregion */

    /* #region Add new widgets */
    if let Some(us) = pactl_data.volume {
        let mute = pactl_data.mute;

        #[allow(clippy::cast_precision_loss)]
        let volume_fraction = us as f64 / pactl_data.base_volume as f64;

        let icon_segment = match (mute, volume_fraction) {
            (true, _) => MUTED,
            (false, fs) if fs == 0.0 => MUTED,
            (false, fs) if fs > 0.0 && fs <= 0.333 => "low",
            (false, fs) if fs > 0.333 && fs <= 0.666 => "medium",
            (false, fs) if fs > 0.666 => "high",
            _ => panic!(),
        };

        let icon_name = format!("audio-volume-{icon_segment}-symbolic");

        let image = create_image(&icon_name);

        boxz.add(&image);

        let progress_bar = create_progress_bar(volume_fraction, mute);

        boxz.add(&progress_bar);

        let volume_percent = volume_fraction * 100.0;

        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let volume_percent_integer = volume_percent.round() as u8;

        let label_string = format!("{volume_percent_integer}%");

        let label = gtk::Label::new(label_string.as_str().into());

        // "min-width" prevents a width change when going from 9% to 10% or from 99% to 100%
        label.style_context().add_class("percentageLabel");

        if mute {
            label.style_context().add_class("percentageLabel-inactive");
        }

        boxz.add(&label);
    } else {
        /* #region Handle broken volume */
        let image = create_image("dialog-question-symbolic");

        boxz.add(&image);

        let label = gtk::Label::new(("Volume is not even").into());

        boxz.add(&label);
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

        // Cannot shadow "timeout"
        let timeout_closure = timeout.clone();

        timeout.set(
            (glib::timeout_add_local_once(TIMEOUT_DURATION, move || {
                timeout_closure.set(None);

                application_window.hide();
            }))
            .into(),
        );
    }
    /* #endregion */

    application_window.show_all();
}

// TODO Get rid of constructor
impl VosdWindow {
    fn new(application: &gtk::Application, monitor: &gdk::Monitor) -> Self {
        let application_window = gtk::ApplicationWindow::new(application);

        // TODO Class
        application_window
            .style_context()
            .add_class(gtk::STYLE_CLASS_OSD);

        application_window.init_layer_shell();
        // Display above all other windows, including full-screen windows
        application_window.set_layer(gtk_layer_shell::Layer::Overlay);
        application_window.set_monitor(monitor);

        let boxz = {
            let bo = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            bo.style_context().add_class("box");

            bo
        };

        application_window.add(&boxz);

        Self {
            application_window: rc::Rc::new(application_window),
            boxz,
            timeout: rc::Rc::new(cell::Cell::new(None)),
        }
    }
}
