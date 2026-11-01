use super::{Action, CloseBehavior, Launcher};
#[cfg(any(windows, test))]
use slint::CloseRequestResponse;
use slint::ComponentHandle;
use std::{cell::Cell, rc::Rc, sync::mpsc};

pub(super) fn bind(
    ui: &Launcher,
    initial: CloseBehavior,
    actions: mpsc::Sender<Action>,
) -> Rc<Cell<CloseBehavior>> {
    let committed = Rc::new(Cell::new(initial));
    ui.set_close_behavior(initial.index());
    #[cfg(windows)]
    {
        let weak = ui.as_weak();
        let committed = committed.clone();
        ui.window().on_close_requested(move || {
            weak.upgrade()
                .map_or(CloseRequestResponse::KeepWindowShown, |ui| {
                    request_close(&ui, committed.get())
                })
        });
    }
    let weak = ui.as_weak();
    let settings = actions.clone();
    ui.on_close_behavior_edited(move |index| {
        if let Some(ui) = weak.upgrade()
            && let Some(behavior) = CloseBehavior::from_index(index)
        {
            save(&ui, &settings, behavior, false);
        }
    });
    let weak = ui.as_weak();
    ui.on_close_choice(move |index, remember| {
        if let Some(ui) = weak.upgrade()
            && !ui.get_close_busy()
            && let Some(behavior @ (CloseBehavior::Hide | CloseBehavior::Quit)) =
                CloseBehavior::from_index(index)
        {
            if remember {
                save(&ui, &actions, behavior, true);
            } else {
                finish_close(&ui, behavior);
            }
        }
    });
    committed
}

#[cfg(any(windows, test))]
pub(super) fn request_close(ui: &Launcher, behavior: CloseBehavior) -> CloseRequestResponse {
    if ui.get_close_busy() {
        return CloseRequestResponse::KeepWindowShown;
    }
    match behavior {
        CloseBehavior::Ask => {
            if !ui.get_close_prompt_open() {
                ui.set_remember_close(false);
                ui.set_close_error("".into());
                ui.set_close_prompt_open(true);
            }
            CloseRequestResponse::KeepWindowShown
        }
        CloseBehavior::Hide => {
            ui.set_close_prompt_open(false);
            CloseRequestResponse::HideWindow
        }
        CloseBehavior::Quit => {
            finish_close(ui, behavior);
            CloseRequestResponse::KeepWindowShown
        }
    }
}

fn save(ui: &Launcher, actions: &mpsc::Sender<Action>, behavior: CloseBehavior, close_after: bool) {
    if close_after {
        ui.set_close_busy(true);
        ui.set_close_error("".into());
    }
    ui.set_settings_status("".into());
    ui.set_settings_error(false);
    if actions
        .send(Action::SaveCloseBehavior(behavior, close_after))
        .is_err()
    {
        ui.set_close_busy(false);
        let message = "Couldn't save close behavior: settings worker unavailable.";
        if close_after {
            ui.set_close_error(message.into());
        }
        ui.set_settings_status(message.into());
        ui.set_settings_error(true);
    }
}

fn finish_close(ui: &Launcher, behavior: CloseBehavior) {
    let result = match behavior {
        CloseBehavior::Ask => return,
        CloseBehavior::Hide => ui.hide().map_err(|e| e.to_string()),
        CloseBehavior::Quit => slint::quit_event_loop().map_err(|e| e.to_string()),
    };
    match result {
        Ok(()) => ui.set_close_prompt_open(false),
        Err(error) => {
            ui.set_close_error(error.into());
            ui.set_close_prompt_open(true);
        }
    }
}

pub(super) fn apply_saved(
    ui: &Launcher,
    committed: &Cell<CloseBehavior>,
    behavior: CloseBehavior,
    close_after: bool,
    result: Result<(), String>,
) {
    if close_after {
        ui.set_close_busy(false);
    }
    match result {
        Ok(()) => {
            committed.set(behavior);
            if close_after || ui.get_close_behavior() == behavior.index() {
                ui.set_close_behavior(behavior.index());
                ui.set_settings_status("".into());
                ui.set_settings_error(false);
            }
            if close_after {
                finish_close(ui, behavior);
            }
        }
        Err(error) => {
            let message = format!("Couldn't save close behavior: {error}");
            if close_after {
                ui.set_close_error(message.clone().into());
            }
            if close_after || ui.get_close_behavior() == behavior.index() {
                ui.set_close_behavior(committed.get().index());
                ui.set_settings_status(message.into());
                ui.set_settings_error(true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    struct Headless(Arc<AtomicBool>);
    struct Proxy(Arc<AtomicBool>);
    impl slint::platform::EventLoopProxy for Proxy {
        fn quit_event_loop(&self) -> Result<(), slint::EventLoopError> {
            self.0.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn invoke_from_event_loop(
            &self,
            event: Box<dyn FnOnce() + Send>,
        ) -> Result<(), slint::EventLoopError> {
            event();
            Ok(())
        }
    }
    impl slint::platform::Platform for Headless {
        fn create_window_adapter(
            &self,
        ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
            Ok(slint::platform::software_renderer::MinimalSoftwareWindow::new(Default::default()))
        }
        fn new_event_loop_proxy(&self) -> Option<Box<dyn slint::platform::EventLoopProxy>> {
            Some(Box::new(Proxy(self.0.clone())))
        }
    }

    #[test]
    fn escape_dismisses_the_close_dialog_after_deferred_focus() {
        use slint::platform::{WindowEvent, software_renderer::MinimalSoftwareWindow};

        struct FocusPlatform(Rc<MinimalSoftwareWindow>);
        impl slint::platform::Platform for FocusPlatform {
            fn create_window_adapter(
                &self,
            ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
                Ok(self.0.clone())
            }
        }

        let window = MinimalSoftwareWindow::new(Default::default());
        slint::platform::set_platform(Box::new(FocusPlatform(window.clone()))).unwrap();
        let ui = Launcher::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(960., 620.));
        ui.set_close_prompt_open(true);
        ui.show().unwrap();
        let size = ui.window().size();
        let mut pixels =
            vec![slint::Rgb8Pixel::default(); size.width as usize * size.height as usize];
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, size.width as usize);
        });
        slint::platform::update_timers_and_animations();
        ui.window().dispatch_event(WindowEvent::KeyPressed {
            text: slint::platform::Key::Escape.into(),
        });
        assert!(!ui.get_close_prompt_open());
    }

    #[test]
    fn first_close_asks_and_only_a_successful_save_changes_future_closes() {
        let quit = Arc::new(AtomicBool::new(false));
        slint::platform::set_platform(Box::new(Headless(quit.clone()))).unwrap();
        let ui = Launcher::new().unwrap();
        let (actions, requests) = mpsc::channel();
        let committed = bind(&ui, CloseBehavior::Ask, actions);
        ui.show().unwrap();
        assert_eq!(
            request_close(&ui, committed.get()),
            CloseRequestResponse::KeepWindowShown
        );
        assert!(ui.get_close_prompt_open() && ui.window().is_visible());
        ui.set_remember_close(true);
        request_close(&ui, committed.get());
        assert!(
            ui.get_remember_close(),
            "repeated close must not reset the checkbox"
        );
        ui.invoke_close_choice(1, false);
        assert!(!ui.window().is_visible());
        assert_eq!(committed.get(), CloseBehavior::Ask);
        assert!(requests.try_recv().is_err());

        ui.show().unwrap();
        request_close(&ui, committed.get());
        ui.invoke_close_choice(1, true);
        assert!(matches!(
            requests.try_recv().unwrap(),
            Action::SaveCloseBehavior(CloseBehavior::Hide, true)
        ));
        assert!(ui.get_close_busy() && ui.window().is_visible());
        quit.store(false, Ordering::SeqCst);
        assert_eq!(
            request_close(&ui, CloseBehavior::Quit),
            CloseRequestResponse::KeepWindowShown
        );
        assert!(
            !quit.load(Ordering::SeqCst),
            "closing again must wait for the pending save"
        );
        apply_saved(
            &ui,
            &committed,
            CloseBehavior::Hide,
            true,
            Err("disk full".into()),
        );
        assert!(ui.window().is_visible() && ui.get_close_prompt_open());
        assert!(ui.get_close_error().contains("disk full"));
        assert_eq!(committed.get(), CloseBehavior::Ask);

        ui.invoke_close_choice(1, true);
        requests.try_recv().unwrap();
        apply_saved(&ui, &committed, CloseBehavior::Hide, true, Ok(()));
        assert!(!ui.window().is_visible());
        assert_eq!(
            request_close(&ui, committed.get()),
            CloseRequestResponse::HideWindow
        );

        ui.show().unwrap();
        ui.set_close_behavior(0);
        ui.invoke_close_behavior_edited(0);
        assert!(matches!(
            requests.try_recv().unwrap(),
            Action::SaveCloseBehavior(CloseBehavior::Ask, false)
        ));
        apply_saved(&ui, &committed, CloseBehavior::Ask, false, Ok(()));
        request_close(&ui, committed.get());
        assert!(ui.get_close_prompt_open());

        quit.store(false, Ordering::SeqCst);
        request_close(&ui, CloseBehavior::Quit);
        assert!(quit.load(Ordering::SeqCst));
    }
}
