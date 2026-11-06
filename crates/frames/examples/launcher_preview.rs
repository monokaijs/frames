//! Isolated design fixture: never starts sessions or reads real trust/history.
//! cargo run -p frames --example launcher_preview -- connections /tmp/connections.png
use slint::{ComponentHandle, ModelRc, VecModel};
use std::cell::Cell;
use std::rc::Rc;

#[path = "../src/launcher_models.rs"]
mod launcher_models;
#[path = "../src/window_style.rs"]
mod window_style;

slint::include_modules!();
struct SnapshotPlatform(Rc<slint::platform::software_renderer::MinimalSoftwareWindow>);
impl slint::platform::Platform for SnapshotPlatform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}
fn rows<T: Clone + 'static>(data: Vec<T>) -> ModelRc<T> {
    Rc::new(VecModel::from(data)).into()
}
fn desktop_preview(blurred: bool) -> slint::Image {
    let mut pixels = image::RgbaImage::from_fn(480, 270, |x, y| {
        let color = if (40..340).contains(&x) && (30..235).contains(&y) {
            if y < 52 {
                [229, 232, 238, 255]
            } else if x < 105 {
                [240, 242, 247, 255]
            } else if (70..78).contains(&y) || (98..104).contains(&y) || (126..132).contains(&y) {
                [154, 172, 193, 255]
            } else {
                [255, 255, 255, 255]
            }
        } else {
            [
                65 + (x / 12) as u8,
                91 + (y / 8) as u8,
                134 + (x / 16) as u8,
                255,
            ]
        };
        image::Rgba(color)
    });
    if blurred {
        pixels = image::DynamicImage::ImageRgba8(pixels)
            .resize_exact(24, 14, image::imageops::FilterType::Triangle)
            .resize_exact(480, 270, image::imageops::FilterType::Triangle)
            .blur(8.0)
            .to_rgba8();
    }
    slint::Image::from_rgba8(
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(pixels.as_raw(), 480, 270),
    )
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let snapshot_path = args.get(2).filter(|path| !path.starts_with("--"));
    let software_window = if snapshot_path.is_some() {
        let window =
            slint::platform::software_renderer::MinimalSoftwareWindow::new(Default::default());
        slint::platform::set_platform(Box::new(SnapshotPlatform(window.clone())))?;
        Some(window)
    } else {
        window_style::initialize()?;
        None
    };
    let ui = Launcher::new()?;
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    if args.iter().any(|arg| arg == "--update-ready") {
        ui.set_update_ready(true);
        ui.set_update_status("Frames 0.2.4 is ready. Restart to install the update.".into());
    }
    ui.set_design_preview(true);
    let quick_apps = || {
        rows(vec![
            QuickAppRow {
                id: "editor".into(),
                name: "Code editor".into(),
                command: "/Applications/Code.app".into(),
                ..Default::default()
            },
            QuickAppRow {
                id: "notes".into(),
                name: "Notes".into(),
                command: "/Applications/Notes.app".into(),
                ..Default::default()
            },
            QuickAppRow {
                id: "game".into(),
                name: "Game library".into(),
                command: "/Applications/Games.app".into(),
                ..Default::default()
            },
        ])
    };
    ui.set_local_quick_apps(quick_apps());
    if args.get(1).map(String::as_str) == Some("quick-apps") {
        ui.set_quick_apps_open(true);
        ui.set_quick_app_target("fixture-host".into());
        if !args.iter().any(|arg| arg == "--empty") {
            ui.set_remote_quick_apps(quick_apps());
        }
        ui.set_quick_apps_busy(args.iter().any(|arg| arg == "--loading"));
        if args.iter().any(|arg| arg == "--error") {
            ui.set_quick_app_error(
                "This device is not trusted yet. Connect and trust it first, then refresh.".into(),
            );
        }
    }
    if args.get(1).map(String::as_str) == Some("quick-app-editor") {
        ui.set_quick_app_editor_open(true);
        if args.iter().any(|arg| arg == "--error") {
            ui.set_quick_app_editor_error(
                "Choose the full path of an application on this computer.".into(),
            );
        }
    }
    if args.iter().any(|arg| arg == "--update-confirm") {
        ui.set_update_ready(true);
        ui.set_update_prompt_open(true);
    }
    if args.iter().any(|arg| arg == "--update-confirm-error") {
        ui.set_update_restart_error("Couldn't prepare update: installation is read-only.".into());
    }
    ui.on_filter_share_windows(|data, query| {
        let query = query.to_lowercase();
        rows(
            data.iter()
                .filter(|r| {
                    format!("{} {}", r.title, r.app)
                        .to_lowercase()
                        .contains(&query)
                })
                .collect(),
        )
    });
    if args.get(1).map(String::as_str) == Some("share") {
        ui.set_share_mode(if args.iter().any(|s| s == "--multiple") {
            1
        } else {
            2
        });
        ui.set_window_picker_open(true);
        ui.set_share_selected_count(1);
        ui.set_share_windows(rows(
            [
                ("Game window", "Game", true),
                ("Project notes", "Notepad", false),
                ("Browser window", "Browser", false),
                ("Music", "Music player", false),
                ("Terminal", "Terminal", false),
                ("Image editor", "Editor", false),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (title, app, selected))| ShareWindowRow {
                id: index.to_string().into(),
                title: title.into(),
                app: app.into(),
                selected,
                available: true,
                preview: desktop_preview(false),
                ..Default::default()
            })
            .collect(),
        ));
    }
    ui.window().set_size(slint::LogicalSize::new(
        if args.iter().any(|s| s == "--compact") {
            820.
        } else {
            960.
        },
        if args.iter().any(|s| s == "--tall") {
            1180.
        } else if args.iter().any(|s| s == "--compact") {
            540.
        } else {
            620.
        },
    ));
    ui.set_mac(true);
    ui.set_device_name("Studio Mac".into());
    ui.set_system_device_name("Studio Mac".into());
    ui.set_device_name_edit("Studio Mac".into());
    ui.set_local_address("192.168.1.24:47990".into());
    ui.set_device_code("Code 8782-19A1".into());
    ui.set_local_ip("192.168.1.24".into());
    ui.set_screen_permission(1);
    ui.set_control_permission(0);
    ui.set_address("192.168.1.18:47990".into());
    ui.set_page(match args.get(1).map(String::as_str) {
        Some("devices") => 1,
        Some("settings") => 2,
        _ => 0,
    });
    ui.set_connection_tab(if args.get(1).map(String::as_str) == Some("sessions") {
        1
    } else {
        0
    });
    if args.iter().any(|arg| arg == "--no-matches") {
        ui.set_search("Unmatched device".into());
    }
    if let Some(query) = args.iter().find_map(|arg| arg.strip_prefix("--search=")) {
        ui.set_search(query.into());
    }
    ui.set_permissions_open(args.get(1).map(String::as_str) == Some("permissions"));
    ui.set_close_prompt_open(args.get(1).map(String::as_str) == Some("close"));
    ui.set_remember_close(args.iter().any(|arg| arg == "--remember"));
    ui.set_device_tab(if args.iter().any(|s| s == "--recent") {
        0
    } else {
        1
    });
    if !args.iter().any(|s| s == "--empty") {
        ui.set_nearby(rows(vec![DeviceRow {
            name: "Design workstation".into(),
            address: "192.168.1.18:47990".into(),
            online: true,
            trusted: true,
            preview_live: true,
            preview: desktop_preview(false),
            ..Default::default()
        }]));
        ui.set_recent(rows(vec![DeviceRow {
            name: "Design workstation".into(),
            fingerprint: "b8e2c49f1650a320bfac96d07e8d321c45c2fb9dd6ca982cf48703f0cddae427".into(),
            can_trust: true,
            preview: desktop_preview(true),
            online: false,
            address: "192.168.1.18:47990".into(),
            detail: "12 min ago".into(),
            ..Default::default()
        }]));
        ui.set_trusted(rows(vec![
            DeviceRow {
                name: "Design workstation".into(),
                address: "192.168.1.18:47990".into(),
                online: true,
                preview_live: true,
                preview: desktop_preview(false),
                detail: "Ask to connect".into(),
                fingerprint: "b8e2c49f1650a320bfac96d07e8d321c45c2fb9dd6ca982cf48703f0cddae427"
                    .into(),
                trusted: true,
                ..Default::default()
            },
            DeviceRow {
                name: "Travel MacBook".into(),
                address: "192.168.1.25:47990".into(),
                preview: desktop_preview(false),
                detail: "Unattended".into(),
                fingerprint: "31c6a2f09d7e48b5943fcb8d624c3813e1d45f8b7fa203963487e2f12b973a68"
                    .into(),
                trusted: true,
                ..Default::default()
            },
            DeviceRow {
                name: "Office PC".into(),
                address: "192.168.1.30:47990".into(),
                online: true,
                detail: "Host only".into(),
                fingerprint: "5fe4b8c19207d3ae5406f182b937c8a635d72fe24c0b8d9a4ea36725c9b014ff"
                    .into(),
                trusted: true,
                ..Default::default()
            },
        ]));
    }
    if args.iter().any(|s| s == "--active") {
        ui.set_sharing(true);
        ui.set_sessions(rows(vec![
            SessionRow {
                id: 1,
                name: "Design workstation".into(),
                detail: "Connected · per-window view".into(),
                hosting: false,
            },
            SessionRow {
                id: 2,
                name: "This device".into(),
                detail: "Sharing everything · new devices need a pairing code".into(),
                hosting: true,
            },
        ]));
    }
    if args.iter().any(|s| s == "--stress") {
        ui.set_device_name("Studio workstation with a very long device name".into());
        ui.set_control_label("Device Control and Data Access (Accessibility)".into());
        ui.set_trusted(rows(
            (1..=18)
                .map(|index| DeviceRow {
                    name: format!("Workstation {index} with a long descriptive device name").into(),
                    detail: "Ask to connect".into(),
                    fingerprint: format!("{index:064x}").into(),
                    trusted: true,
                    ..Default::default()
                })
                .collect(),
        ));
    }
    if args.iter().any(|s| s == "--error") {
        ui.set_notice(
            "Connection failed. Check the IP address and network permissions, then try again."
                .into(),
        );
        ui.set_notice_error(true);
    }
    if args.iter().any(|s| s == "--windows") {
        ui.set_mac(false);
    }
    use slint::Model;
    ui.on_filter_devices(launcher_models::device_filter());
    // Design fixtures must never masquerade as the functional application.
    fn preview_notice(weak: &slint::Weak<Launcher>) {
        if let Some(ui) = weak.upgrade() {
            ui.set_notice("Visual preview. Open Frames Workspace for live controls.".into());
            ui.set_notice_error(true);
        }
    }
    let weak = ui.as_weak();
    ui.on_refresh(move || preview_notice(&weak));
    let connects = Rc::new(Cell::new(0usize));
    let weak = ui.as_weak();
    let count = connects.clone();
    ui.on_connect(move |_| {
        count.set(count.get() + 1);
        preview_notice(&weak);
    });
    let weak = ui.as_weak();
    let count = connects.clone();
    ui.on_connect_device(move |_, _, _, _| {
        count.set(count.get() + 1);
        preview_notice(&weak);
    });
    let weak = ui.as_weak();
    let count = connects.clone();
    ui.on_launch_quick_app(move |_, _, _| {
        count.set(count.get() + 1);
        if let Some(ui) = weak.upgrade() {
            ui.set_quick_apps_open(false);
        }
        preview_notice(&weak);
    });
    ui.on_load_quick_apps(move |_| {});
    let weak = ui.as_weak();
    ui.on_save_quick_app(move |_, _| preview_notice(&weak));
    let weak = ui.as_weak();
    ui.on_remove_quick_app(move |_| preview_notice(&weak));
    let weak = ui.as_weak();
    ui.on_toggle_sharing(move || preview_notice(&weak));
    let weak = ui.as_weak();
    ui.on_discover(move || preview_notice(&weak));
    let weak = ui.as_weak();
    ui.on_stop_session(move |_| preview_notice(&weak));
    let weak = ui.as_weak();
    ui.on_permission(move |_| preview_notice(&weak));
    let weak = ui.as_weak();
    ui.on_revoke(move |_| preview_notice(&weak));
    let weak = ui.as_weak();
    ui.on_clear_history(move || preview_notice(&weak));
    let weak = ui.as_weak();
    ui.on_login_settings(move || preview_notice(&weak));
    let weak = ui.as_weak();
    ui.on_settings_edited(move || preview_notice(&weak));
    let weak = ui.as_weak();
    ui.on_close_behavior_edited(move |_| preview_notice(&weak));
    let weak = ui.as_weak();
    ui.on_close_choice(move |_, _| preview_notice(&weak));
    let weak = ui.as_weak();
    ui.on_device_name_edited(move || preview_notice(&weak));
    let weak = ui.as_weak();
    ui.on_trust(move |_| preview_notice(&weak));
    if let (Some(path), Some(window)) = (snapshot_path, software_window) {
        ui.show()?;
        let size = ui.window().size();
        let mut pixels =
            vec![slint::Rgb8Pixel::default(); size.width as usize * size.height as usize];
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, size.width as usize);
        });
        // Exercise production popups in a headless snapshot.
        for arg in &args {
            if arg == "--refresh-devices" || arg == "--append-device" {
                for data in [ui.get_nearby(), ui.get_recent(), ui.get_trusted()] {
                    let mut refreshed: Vec<_> = data.iter().collect();
                    if arg == "--append-device" {
                        refreshed.push(DeviceRow {
                            name: "New arrival".into(),
                            address: "192.168.1.80".into(),
                            ..Default::default()
                        });
                    } else {
                        for row in &mut refreshed {
                            row.detail = "Refreshed while the pointer is down".into();
                        }
                    }
                    launcher_models::refresh_rows(
                        data,
                        refreshed,
                        launcher_models::device_key,
                        |_| panic!("fixture has a VecModel"),
                    );
                }
            } else if let Some(point) = arg
                .strip_prefix("--press=")
                .or_else(|| arg.strip_prefix("--release="))
            {
                let (x, y) = point
                    .split_once(',')
                    .ok_or("Use --press=x,y or --release=x,y")?;
                let position = slint::LogicalPosition::new(x.parse()?, y.parse()?);
                ui.window().dispatch_event(if arg.starts_with("--press=") {
                    slint::platform::WindowEvent::PointerPressed {
                        position,
                        button: slint::platform::PointerEventButton::Left,
                    }
                } else {
                    slint::platform::WindowEvent::PointerReleased {
                        position,
                        button: slint::platform::PointerEventButton::Left,
                    }
                });
            } else if let Some(point) = arg.strip_prefix("--click=") {
                let (x, y) = point.split_once(',').ok_or("Use --click=x,y")?;
                let position = slint::LogicalPosition::new(x.parse()?, y.parse()?);
                ui.window()
                    .dispatch_event(slint::platform::WindowEvent::PointerPressed {
                        position,
                        button: slint::platform::PointerEventButton::Left,
                    });
                ui.window()
                    .dispatch_event(slint::platform::WindowEvent::PointerReleased {
                        position,
                        button: slint::platform::PointerEventButton::Left,
                    });
            } else if let Some(key) = arg.strip_prefix("--key=") {
                use slint::platform::{Key, WindowEvent};
                let text: slint::SharedString = match key {
                    "Escape" => Key::Escape.into(),
                    "Tab" => Key::Tab.into(),
                    "Down" => Key::DownArrow.into(),
                    "Return" => Key::Return.into(),
                    _ => return Err("Use --key=Escape, Tab, Down or Return".into()),
                };
                ui.window()
                    .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                ui.window()
                    .dispatch_event(WindowEvent::KeyReleased { text });
            } else {
                continue;
            }
            window.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, size.width as usize);
            });
        }
        if let Some(expected) = args
            .iter()
            .find_map(|arg| arg.strip_prefix("--expect-connect="))
        {
            let expected: usize = expected.parse()?;
            assert_eq!(
                connects.get(),
                expected,
                "Connect must fire exactly once per completed click"
            );
        }
        let bytes: Vec<u8> = pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
        image::save_buffer(
            path,
            &bytes,
            size.width,
            size.height,
            image::ColorType::Rgb8,
        )?;
        ui.hide()?;
        return Ok(());
    }
    ui.run()?;
    Ok(())
}
