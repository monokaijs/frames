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
    if let Some(scale) = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--scale-factor="))
    {
        let scale_factor = scale.parse::<f32>()?;
        if !scale_factor.is_finite() || scale_factor <= 0. {
            return Err("Use a positive finite --scale-factor".into());
        }
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged { scale_factor });
    }
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    if args.iter().any(|arg| arg == "--update-ready") {
        ui.set_update_ready(true);
        ui.set_update_status("Frames 0.2.4 is ready. Restart to install the update.".into());
    }
    if args.iter().any(|arg| arg == "--settings-error") {
        ui.set_settings_status(
            "Couldn't save settings. Check the application folder and try again.".into(),
        );
        ui.set_settings_error(true);
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
            if args.iter().any(|arg| arg == "--selected") {
                ui.set_quick_app_selected("editor".into());
            }
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
    let width = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--width="))
        .map(str::parse::<f32>)
        .transpose()?;
    let height = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--height="))
        .map(str::parse::<f32>)
        .transpose()?;
    ui.window().set_size(slint::LogicalSize::new(
        width.unwrap_or(if args.iter().any(|s| s == "--compact") {
            820.
        } else {
            960.
        }),
        height.unwrap_or(if args.iter().any(|s| s == "--tall") {
            1180.
        } else if args.iter().any(|s| s == "--compact") {
            540.
        } else {
            620.
        }),
    ));
    ui.set_mac(true);
    ui.set_device_name("Studio Mac".into());
    ui.set_system_device_name("Studio Mac".into());
    ui.set_device_name_edit("Studio Mac".into());
    ui.set_local_address("192.168.1.24:47990".into());
    ui.set_device_code("8782-19A1".into());
    ui.set_local_ip("192.168.1.24".into());
    ui.set_screen_permission(1);
    ui.set_control_permission(0);
    ui.set_address("192.168.1.18:47990".into());
    ui.set_page(match args.get(1).map(String::as_str) {
        Some("devices") => 1,
        Some("settings") => 2,
        Some("files") => 3,
        _ => 0,
    });
    if ui.get_page() == 3 {
        let mode = args
            .iter()
            .find_map(|arg| arg.strip_prefix("--files-mode="))
            .unwrap_or("0")
            .parse::<i32>()?;
        ui.set_files_local_mode(mode);
        ui.set_files_remote_mode(mode);
        ui.set_files_local_can_up(true);
        ui.set_files_remote_can_up(true);
        ui.set_files_peer("Design workstation".into());
        ui.set_files_peer_names(rows(vec![
            "Design workstation".into(),
            "Windows laptop".into(),
        ]));
        ui.set_files_connected(!args.iter().any(|arg| arg == "--files-disconnected"));
        ui.set_files_remote_name("Design workstation".into());
        ui.set_files_local_path("/Users/studio/Documents".into());
        ui.set_files_remote_path("C:\\Users\\Studio\\Documents".into());
        if args.iter().any(|arg| arg == "--files-autocomplete") {
            ui.set_files_local_path("/Users/studio/D".into());
            ui.set_files_local_suggestions(rows(vec![
                FileSuggestion {
                    name: "Documents".into(),
                    path: "/Users/studio/Documents/".into(),
                },
                FileSuggestion {
                    name: "Downloads".into(),
                    path: "/Users/studio/Downloads/".into(),
                },
            ]));
        }
        ui.set_files_status("".into());
        let entries = |remote: bool| {
            let long_names = args.iter().any(|arg| arg == "--files-long-names");
            rows(
                vec![
                    (if long_names { "Tài liệu thiết kế giao diện và các bản bàn giao tháng 11" } else { "Documents" }, true, "Folder"),
                    ("Downloads", true, "Folder"),
                    ("Pictures", true, "Folder"),
                    (
                        if long_names {
                            "Báo cáo tổng hợp dự án và phản hồi khách hàng — phiên bản cuối cùng 📁.pdf"
                        } else if remote {
                            "Project brief.pdf"
                        } else {
                            "Design notes.md"
                        },
                        false,
                        "24.5 KB",
                    ),
                    (if long_names { "Screenshot_2026-11-07_very_long_filename_without_spaces_12345678901234567890.png" } else { "Screenshot.png" }, false, "1.2 MB"),
                    (if long_names { "Presentation with a very long descriptive title and multiple revisions.pptx" } else { "Presentation.pptx" }, false, "8.4 MB"),
                    (if long_names { "备份资料与设计文件📦.zip" } else { "Budget.xlsx" }, false, "96 KB"),
                    (if long_names { ".gitignore" } else { "Readme.txt" }, false, "840 B"),
                ]
                .into_iter()
                .map(|(name, folder, size)| {
                    let (stem, extension) = if !folder {
                        name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()).map_or((name.to_string(), String::new()), |(stem, ext)| (stem.to_string(), format!(".{ext}")))
                    } else { (name.to_string(), String::new()) };
                    FileRow {
                    name: name.into(),
                    stem: stem.into(), extension: extension.into(),
                    path: name.into(),
                    folder,
                    size: size.into(),
                    modified: "2026-11-07".into(),
                    ..Default::default()
                }})
                .collect(),
            )
        };
        ui.set_files_local_entries(entries(false));
        if ui.get_files_connected() {
            ui.set_files_remote_entries(entries(true));
        }
    }
    if let Some(tab) = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--settings-tab="))
    {
        let tab = tab.parse::<i32>()?;
        if !(0..=5).contains(&tab) {
            return Err("Use --settings-tab=0..5".into());
        }
        ui.set_settings_tab(tab);
    }
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
    let file_copies = Rc::new(Cell::new([0usize; 2]));
    let count = file_copies.clone();
    let weak = ui.as_weak();
    ui.on_files_transfer(move |side, _| {
        let mut copies = count.get();
        copies[side as usize] += 1;
        count.set(copies);
        if let Some(ui) = weak.upgrade() {
            ui.set_files_status("Copied in preview".into());
        }
    });
    let weak = ui.as_weak();
    ui.on_files_mode(move |side, mode| {
        if let Some(ui) = weak.upgrade() {
            if side == 0 {
                ui.set_files_local_mode(mode);
            } else {
                ui.set_files_remote_mode(mode);
            }
        }
    });
    let weak = ui.as_weak();
    ui.on_files_navigate(move |side, path| {
        if let Some(ui) = weak.upgrade() {
            if side == 0 {
                ui.set_files_local_path(path);
                ui.set_files_local_suggestions(rows(vec![]));
            } else {
                ui.set_files_remote_path(path);
                ui.set_files_remote_suggestions(rows(vec![]));
            }
        }
    });
    let weak = ui.as_weak();
    ui.on_files_select(move |side, path, add| {
        if let Some(ui) = weak.upgrade() {
            use slint::Model;
            let rows = if side == 0 {
                ui.get_files_local_entries()
            } else {
                ui.get_files_remote_entries()
            };
            let mut selected = 0;
            for index in 0..rows.row_count() {
                let mut row = rows.row_data(index).unwrap();
                if row.path == path {
                    row.selected = !add || !row.selected;
                } else if !add {
                    row.selected = false;
                }
                if row.selected {
                    selected += 1;
                }
                rows.set_row_data(index, row);
            }
            if side == 0 {
                ui.set_files_local_selected(selected);
            } else {
                ui.set_files_remote_selected(selected);
            }
        }
    });
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
        slint::platform::update_timers_and_animations();
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
            } else if let Some(wait) = arg.strip_prefix("--wait=") {
                let until =
                    std::time::Instant::now() + std::time::Duration::from_millis(wait.parse()?);
                while std::time::Instant::now() < until {
                    slint::platform::update_timers_and_animations();
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            } else if let Some(point) = arg.strip_prefix("--move=") {
                let (x, y) = point.split_once(',').ok_or("Use --move=x,y")?;
                ui.window()
                    .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                        position: slint::LogicalPosition::new(x.parse()?, y.parse()?),
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
            } else if let Some(text) = arg.strip_prefix("--type=") {
                ui.window()
                    .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.into() });
                ui.window()
                    .dispatch_event(slint::platform::WindowEvent::KeyReleased {
                        text: text.into(),
                    });
            } else if let Some(key) = arg.strip_prefix("--key=") {
                use slint::platform::{Key, WindowEvent};
                let text: slint::SharedString = match key {
                    "Escape" => Key::Escape.into(),
                    "Tab" => Key::Tab.into(),
                    "Down" => Key::DownArrow.into(),
                    "Return" => Key::Return.into(),
                    "End" => Key::End.into(),
                    _ => return Err("Use --key=Escape, Tab, Down or Return".into()),
                };
                ui.window()
                    .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                ui.window()
                    .dispatch_event(WindowEvent::KeyReleased { text });
            } else {
                continue;
            }
            slint::platform::update_timers_and_animations();
            window.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, size.width as usize);
            });
        }
        if let Some(expected) = args
            .iter()
            .find_map(|arg| arg.strip_prefix("--expect-file-path="))
        {
            assert_eq!(ui.get_files_local_path().as_str(), expected);
        }
        if let Some(expected) = args
            .iter()
            .find_map(|arg| arg.strip_prefix("--expect-file-copies="))
        {
            let expected: Vec<usize> = expected
                .split(',')
                .map(str::parse)
                .collect::<Result<_, _>>()?;
            assert_eq!(
                file_copies.get().as_slice(),
                expected.as_slice(),
                "File copy must target the opposite pane exactly once"
            );
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
        if let Some(expected) = args
            .iter()
            .find_map(|arg| arg.strip_prefix("--expect-settings-tab="))
        {
            assert_eq!(
                ui.get_settings_tab(),
                expected.parse::<i32>()?,
                "Settings tab must respond to its click"
            );
        }
        if let Some(expected) = args
            .iter()
            .find_map(|arg| arg.strip_prefix("--expect-default-audio="))
        {
            assert_eq!(
                ui.get_default_audio(),
                expected.parse::<bool>()?,
                "Switching tabs must preserve edited settings"
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
