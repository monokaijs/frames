//! Isolated design fixture: never starts sessions or reads real trust/history.
//! cargo run -p frames --example launcher_preview -- connections /tmp/connections.png
use slint::{ComponentHandle, ModelRc, VecModel};
use std::rc::Rc;

#[path = "../src/window_style.rs"]
mod window_style;

slint::include_modules!();
fn rows<T: Clone + 'static>(data: Vec<T>) -> ModelRc<T> {
    Rc::new(VecModel::from(data)).into()
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    window_style::initialize()?;
    let ui = Launcher::new()?;
    ui.set_design_preview(true);
    ui.window().set_size(slint::LogicalSize::new(
        if args.iter().any(|s| s == "--compact") {
            820.
        } else {
            960.
        },
        if args.iter().any(|s| s == "--compact") {
            540.
        } else {
            620.
        },
    ));
    ui.set_mac(true);
    ui.set_device_name("Studio Mac".into());
    ui.set_local_address("192.168.1.24:47990".into());
    ui.set_screen_permission(1);
    ui.set_control_permission(0);
    ui.set_address("192.168.1.18:47990".into());
    ui.set_page(match args.get(1).map(String::as_str) {
        Some("devices") => 1,
        Some("permissions") => 2,
        _ => 0,
    });
    ui.set_device_tab(1);
    if !args.iter().any(|s| s == "--empty") {
        ui.set_nearby(rows(vec![DeviceRow {
            name: "Design workstation".into(),
            address: "192.168.1.18:47990".into(),
            ..Default::default()
        }]));
        ui.set_recent(rows(vec![DeviceRow {
            name: "192.168.1.18:47990".into(),
            address: "192.168.1.18:47990".into(),
            detail: "12 min ago".into(),
            ..Default::default()
        }]));
        ui.set_trusted(rows(vec![
            DeviceRow {
                name: "Design workstation".into(),
                detail: "Ask to connect".into(),
                fingerprint: "b8e2c49f1650a320bfac96d07e8d321c45c2fb9dd6ca982cf48703f0cddae427"
                    .into(),
                trusted: true,
                ..Default::default()
            },
            DeviceRow {
                name: "Travel MacBook".into(),
                detail: "Unattended".into(),
                fingerprint: "31c6a2f09d7e48b5943fcb8d624c3813e1d45f8b7fa203963487e2f12b973a68"
                    .into(),
                trusted: true,
                ..Default::default()
            },
            DeviceRow {
                name: "Office PC".into(),
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
                detail: "Starting · per-window view".into(),
                hosting: false,
            },
            SessionRow {
                id: 2,
                name: "This device".into(),
                detail: "Ask to connect".into(),
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
    ui.on_filter_devices(|data, query| {
        let q = query.to_lowercase();
        rows(
            data.iter()
                .filter(|d| {
                    format!("{} {} {}", d.name, d.address, d.fingerprint)
                        .to_lowercase()
                        .contains(&q)
                })
                .collect(),
        )
    });
    // Design fixtures must never masquerade as the functional application.
    fn preview_notice(weak: &slint::Weak<Launcher>) {
        if let Some(ui) = weak.upgrade() {
            ui.set_notice("Visual preview. Open Frames Workspace for live controls.".into());
            ui.set_notice_error(true);
        }
    }
    let weak = ui.as_weak();
    ui.on_refresh(move || preview_notice(&weak));
    let weak = ui.as_weak();
    ui.on_connect(move |_| preview_notice(&weak));
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
    if let Some(path) = args.get(2).filter(|s| !s.starts_with("--")) {
        let weak = ui.as_weak();
        let path = path.clone();
        slint::Timer::single_shot(std::time::Duration::from_millis(500), move || {
            let ui = weak.upgrade().unwrap();
            let pixels = ui.window().take_snapshot().expect("snapshot");
            let file = std::fs::File::create(&path).unwrap();
            let mut encoder = png::Encoder::new(file, pixels.width(), pixels.height());
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(pixels.as_bytes())
                .unwrap();
            slint::quit_event_loop().unwrap();
        });
    }
    ui.run()?;
    Ok(())
}
