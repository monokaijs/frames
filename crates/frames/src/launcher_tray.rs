//! Windows launcher controls remain available after its window is hidden.
use super::{Action, Launcher};
use frames_native::{Tray, TrayGlyph as G, TrayItem as Item};
use slint::{ComponentHandle, Model};
use std::sync::mpsc::Sender;

pub struct LauncherTray {
    tray: Tray,
}
impl LauncherTray {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            tray: Tray::new("Frames")?,
        })
    }
    pub fn poll(&mut self, ui: &Launcher, actions: &Sender<Action>) {
        for command in self.tray.commands.try_iter() {
            match command {
                1 => {
                    let _ = ui.show();
                }
                3 => {
                    ui.set_page(2);
                    let _ = ui.show();
                }
                4 => {
                    ui.set_page(1);
                    let _ = ui.show();
                }
                5 => {
                    if !ui.get_sharing() && ui.get_share_mode() != 0 {
                        let _ = ui.show();
                    }
                    ui.invoke_toggle_sharing();
                }
                6 => {
                    let _ = actions.send(Action::Discover);
                    ui.set_page(0);
                    let _ = ui.show();
                }
                7 => {
                    ui.invoke_check_updates();
                }
                8 => {
                    let _ = slint::quit_event_loop();
                }
                9 => {
                    ui.set_page(0);
                    ui.set_connection_tab(1);
                    let _ = ui.show();
                }
                _ => {}
            }
        }
        let count = ui.get_sessions().row_count();
        self.tray.update(
            vec![
                Item::new(
                    0,
                    if count > 0 {
                        format!("Frames · {count} active sessions")
                    } else {
                        "Frames · Ready".into()
                    },
                    G::Network,
                )
                .enabled(false),
                Item::separator(),
                Item::new(1, "Open Frames", G::App),
                Item::new(9, "Sessions", G::Window),
                Item::separator(),
                Item::new(3, "Settings", G::Settings),
                Item::new(4, "Devices", G::Devices),
                Item::new(
                    5,
                    if ui.get_sharing() {
                        "Stop sharing"
                    } else {
                        "Start sharing"
                    },
                    G::Display,
                )
                .checked(ui.get_sharing())
                .enabled(!ui.get_background_sharing_running()),
                Item::new(6, "Find nearby devices", G::Refresh),
                Item::new(7, "Check for updates", G::Info).enabled(!ui.get_update_busy()),
                Item::separator(),
                Item::new(8, "Quit Frames", G::Power),
            ],
            count > 0,
            if count > 0 {
                format!("Frames · {count} active sessions")
            } else {
                "Frames · Ready · Open from the tray".into()
            },
        );
    }
}
