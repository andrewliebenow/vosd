// https://github.com/ErikReider/SwayOSD

// TODO RefCell

use std::{
    cell::RefCell,
    collections::HashSet,
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    rc::Rc,
    thread,
    time::Duration,
};

use gtk::{
    gdk::{self},
    glib::{self},
    prelude::{ApplicationExt, ApplicationExtManual},
    traits::{
        ContainerExt, CssProviderExt, GtkWindowExt, ProgressBarExt, StyleContextExt, WidgetExt,
    },
};

const CHANGE_ON_SINK_NUMBER: &str = "change' on sink #";

const CHANGE_ON_SINK_NUMBER_LEN: usize = CHANGE_ON_SINK_NUMBER.len();

const EVENT_SPACE: &str = "Event '";

const EVENT_SPACE_LEN: usize = EVENT_SPACE.len();

const MUTED: &str = "muted";

const NEW_ON_CLIENT_NUMBER: &str = "new' on client #";

const NEW_ON_CLIENT_NUMBER_LEN: usize = NEW_ON_CLIENT_NUMBER.len();

const REMOVE_ON_CLIENT_NUMBER: &str = "remove' on client #";

const REMOVE_ON_CLIENT_NUMBER_LEN: usize = REMOVE_ON_CLIENT_NUMBER.len();

struct VosdWindow {
    application_window: gtk::ApplicationWindow,
    boxz: gtk::Box,
    timeout: Rc<RefCell<Option<glib::SourceId>>>,
}

struct PactlData {
    base_volume: u64,
    index: u64,
    mute: bool,
    volume: Option<u64>,
}

fn main() {
    if gtk::init().is_err() {
        panic!();
    }

    let css_provider = gtk::CssProvider::new();

    let css_css_bytes = include_bytes!("css.css");

    css_provider.load_from_data(css_css_bytes).unwrap();

    let screen = gdk::Screen::default().unwrap();

    gtk::StyleContext::add_provider_for_screen(
        &screen,
        &css_provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let application = gtk::Application::new(
        ("io.github.andrewliebenow.Vosd").into(),
        gtk::gio::ApplicationFlags::FLAGS_NONE,
    );

    let vosd_windows = Rc::new(RefCell::new(Vec::new()));

    application.connect_activate(move |ap| {
        let display = match gdk::Display::default() {
            Some(di) => di,
            _ => return,
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

        let index_string = {
            let pactl_data = gather_pactl_data();

            pactl_data.index.to_string()
        };

        let (sender, receiver) = glib::MainContext::channel::<PactlData>(glib::PRIORITY_DEFAULT);

        thread::spawn(move || {
            let child = Command::new("pactl")
                .arg("subscribe")
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();

            let buf_reader = BufReader::new(child.stdout.unwrap());

            let mut reading_client_number = None;
            let mut send_notification = false;

            for re in buf_reader.lines() {
                let line = re.unwrap();

                let line_without_event_space = &line[EVENT_SPACE_LEN..];

                match line_without_event_space {
                    st if st.starts_with(NEW_ON_CLIENT_NUMBER) => {
                        reading_client_number = st[NEW_ON_CLIENT_NUMBER_LEN..].to_owned().into();
                        send_notification = false;
                    }
                    st if st.starts_with(REMOVE_ON_CLIENT_NUMBER) => {
                        if let Some(str) = reading_client_number {
                            let client_number = &st[REMOVE_ON_CLIENT_NUMBER_LEN..];

                            if client_number == str {
                                if send_notification {
                                    let pactl_data = gather_pactl_data();

                                    sender.send(pactl_data).unwrap();

                                    send_notification = false;
                                }
                            } else {
                                // TODO
                                // eprintln!("Jumbled event order detected");
                            }

                            reading_client_number = None;
                        }
                    }
                    st if st.starts_with(CHANGE_ON_SINK_NUMBER) => {
                        let sink_number = &st[CHANGE_ON_SINK_NUMBER_LEN..];

                        if sink_number == index_string {
                            send_notification = true;
                        }
                    }
                    _ => {
                        // eprintln!("Unexpected line encountered:\n===>\n{}\n<===\n", line);
                    }
                }
            }
        });

        {
            let vosd_windows = vosd_windows.clone();

            receiver.attach(None, move |pa| {
                for vo in vosd_windows.borrow().iter() {
                    show_volume_change_notification(vo, &pa);
                }

                gtk::prelude::Continue(true)
            });
        }
    });

    application.run();
}

fn add_window(
    vosd_windows: &Rc<RefCell<Vec<VosdWindow>>>,
    application: &gtk::Application,
    monitor: &gdk::Monitor,
) {
    let vosd_window = VosdWindow::new(application, monitor);

    vosd_windows.borrow_mut().push(vosd_window);
}

fn initialize_windows(
    vosd_windows: &Rc<RefCell<Vec<VosdWindow>>>,
    application: &gtk::Application,
    display: &gdk::Display,
) {
    close_all_windows(vosd_windows);

    for i in 0..display.n_monitors() {
        let monitor = match display.monitor(i) {
            Some(mo) => mo,
            _ => continue,
        };

        add_window(vosd_windows, application, &monitor);
    }
}

fn close_all_windows(vosd_windows: &Rc<RefCell<Vec<VosdWindow>>>) {
    vosd_windows.borrow_mut().retain(|vo| {
        vo.application_window.close();

        false
    });
}

fn gather_pactl_data() -> PactlData {
    let child = Command::new("pactl")
        .args(["--format=json", "list", "sinks"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let value: serde_json::Value = serde_json::from_reader(child.stdout.unwrap()).unwrap();

    let array = value.as_array().unwrap();

    let first_element_object = array.get(0).unwrap().as_object().unwrap();

    let index = first_element_object.get("index").unwrap().as_u64().unwrap();

    let base_volume_object = first_element_object
        .get("base_volume")
        .unwrap()
        .as_object()
        .unwrap();

    let mute = first_element_object.get("mute").unwrap().as_bool().unwrap();

    let base_volume = base_volume_object.get("value").unwrap().as_u64().unwrap();

    let volume_object = first_element_object
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
        // TODO
        // eprintln!("hash_set.len() is not 1:\n===>\n{}\n<===\n", value);

        None
    };

    PactlData {
        index,
        base_volume,
        mute,
        volume,
    }
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
    let application_window = &vosd_window.application_window;
    let boxz = &vosd_window.boxz;
    let timeout = &vosd_window.timeout;

    /* #region Delete existing widgets */
    for wi in boxz.children() {
        boxz.remove(&wi);
    }
    /* #endregion */

    /* #region Add new widgets */
    match pactl_data.volume {
        Some(us) => {
            let mute = pactl_data.mute;

            let volume_fraction = us as f64 / pactl_data.base_volume as f64;

            let icon_segment = match (mute, volume_fraction) {
                (true, _) => MUTED,
                (false, fs) if fs == 0.0 => MUTED,
                (false, fs) if fs > 0.0 && fs <= 0.333 => "low",
                (false, fs) if fs > 0.333 && fs <= 0.666 => "medium",
                (false, fs) if fs > 0.666 => "high",
                _ => panic!(),
            };

            let icon_name = format!("audio-volume-{}-symbolic", icon_segment);

            let image = create_image(&icon_name);

            boxz.add(&image);

            let progress_bar = create_progress_bar(volume_fraction, mute);

            boxz.add(&progress_bar);

            let volume_percent = volume_fraction * 100.0;

            let volume_percent_integer = volume_percent.round() as u8;

            let label_string = format!("{}%", volume_percent_integer);

            let label = gtk::Label::new(label_string.as_str().into());

            // "min-width" prevents a width change when going from 9% to 10% or from 99% to 100%
            label.style_context().add_class("percentageLabel");

            if mute {
                label.style_context().add_class("percentageLabel-inactive");
            }

            boxz.add(&label);
        }
        _ => {
            /* #region Handle broken volume */
            let image = create_image("dialog-question-symbolic");

            boxz.add(&image);

            let label = gtk::Label::new(("Volume is not even").into());

            boxz.add(&label);
            /* #endregion */
        }
    }
    /* #endregion */

    /* #region Timeout logic */
    {
        let application_window = application_window.clone();

        // Cannot shadow "timeout"
        let timeout_clone = timeout.clone();

        let option = timeout.replace(
            (glib::timeout_add_local_once(Duration::from_millis(2_000), move || {
                timeout_clone.replace(None);

                application_window.hide();
            }))
            .into(),
        );

        // Cancel previous timeout
        if let Some(so) = option {
            so.remove();
        }
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

        gtk_layer_shell::init_for_window(&application_window);
        gtk_layer_shell::set_monitor(&application_window, monitor);
        gtk_layer_shell::set_exclusive_zone(&application_window, -1);
        gtk_layer_shell::set_layer(&application_window, gtk_layer_shell::Layer::Overlay);

        let boxz = {
            let bo = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            bo.style_context().add_class("box");

            bo
        };

        application_window.add(&boxz);

        Self {
            application_window,
            boxz,
            timeout: Rc::new(RefCell::new(None)),
        }
    }
}
