//! Ordered native input with a bounded queue and disposable, expiring motion.
use super::Outbound;
use frames_core::{
    input::{InputEvent, PressedInput},
    model::*,
    protocol::Message,
};
use frames_native::Desktop;
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

fn release_native(desktop: &dyn Desktop, generation: Option<u64>) {
    if let Some(generation) = generation {
        desktop.shared_release(generation)
    } else {
        desktop.release_input()
    }
}

enum Work {
    Resume {
        generation: u64,
        deadline: Instant,
    },
    Input {
        window: WindowId,
        generation: Option<u64>,
        event: InputEvent,
        deadline: Instant,
    },
    Action {
        request: u64,
        window: WindowId,
        action: WindowAction,
        actual: Option<RemoteWindow>,
        mode: ViewMode,
        deadline: Instant,
    },
    Release,
}
#[derive(Default)]
struct Queue {
    items: VecDeque<Work>,
    closed: bool,
}
impl Queue {
    fn push(&mut self, work: Work) -> anyhow::Result<()> {
        anyhow::ensure!(!self.closed, "input worker stopped");
        if let (
            Some(Work::Input {
                window: old_window,
                generation: old_generation,
                event: old,
                deadline: old_deadline,
            }),
            Work::Input {
                window,
                generation,
                event,
                deadline,
            },
        ) = (self.items.back_mut(), &work)
        {
            let old_relative = matches!(old, InputEvent::RelativeMotion { .. });
            if old_window == window
                && old_generation == generation
                && (!old_relative || *old_deadline > Instant::now())
                && old.coalesce(event)
            {
                *old_deadline = if old_relative {
                    (*old_deadline).min(*deadline)
                } else {
                    *deadline
                };
                return Ok(());
            }
        }
        if matches!(work, Work::Release) {
            self.items
                .retain(|item| matches!(item, Work::Action { .. }));
        }
        if self.items.len() >= 128 {
            self.items.clear();
            self.items.push_back(Work::Release);
            anyhow::bail!("host input queue overloaded; input paused");
        }
        self.items.push_back(work);
        Ok(())
    }
}

pub struct Dispatcher {
    shared: Arc<(Mutex<Queue>, Condvar)>,
    epoch: Option<Instant>,
}
impl Dispatcher {
    pub fn new(
        desktop: Arc<dyn Desktop>,
        out: Outbound,
        release_on_close: bool,
    ) -> anyhow::Result<Self> {
        let shared = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        let worker = shared.clone();
        let session_generation = desktop.shared_session().map(|s| s.generation);
        std::thread::Builder::new()
            .name("frames-input".into())
            .spawn(move || {
                let mut pressed = PressedInput::default();
                loop {
                    let work = {
                        let (lock, ready) = &*worker;
                        let mut queue = lock.lock().unwrap();
                        while queue.items.is_empty() && !queue.closed {
                            queue = ready.wait(queue).unwrap();
                        }
                        if queue.closed {
                            break;
                        }
                        queue.items.pop_front().unwrap()
                    };
                    match work {
                        Work::Resume {
                            generation,
                            deadline,
                        } => {
                            if Instant::now() <= deadline {
                                let _ = desktop.shared_resume(generation);
                            }
                        }
                        Work::Release => {
                            release_native(desktop.as_ref(), session_generation);
                            pressed.release_all();
                        }
                        Work::Input {
                            window,
                            generation,
                            event,
                            deadline,
                        } => {
                            if Instant::now() > deadline {
                                if matches!(
                                    event,
                                    InputEvent::Button { down: false, .. }
                                        | InputEvent::RelativeButton { down: false, .. }
                                        | InputEvent::Key { down: false, .. }
                                ) {
                                    release_native(desktop.as_ref(), session_generation);
                                    pressed.release_all();
                                }
                                continue;
                            }
                            if let InputEvent::Button {
                                button,
                                down: false,
                                ..
                            }
                            | InputEvent::RelativeButton {
                                button,
                                down: false,
                            } = event
                                && !pressed.button_is_down(button)
                            {
                                continue;
                            }
                            let result = if let Some(generation) = generation {
                                desktop.shared_input(window, generation, &event)
                            } else {
                                desktop.input(window, &event)
                            };
                            match result {
                                Ok(()) => pressed.track(&event),
                                Err(error) => {
                                    release_native(desktop.as_ref(), session_generation);
                                    pressed.release_all();
                                    let _ = out.blocking_send(Message::Ack {
                                        request: 0,
                                        window,
                                        actual: None,
                                        error: Some(error.to_string()),
                                    });
                                }
                            }
                        }
                        Work::Action {
                            request,
                            window,
                            action,
                            actual,
                            mode,
                            deadline,
                        } => {
                            let result = if Instant::now() > deadline {
                                Err(anyhow::anyhow!(
                                    "Window command expired while the host was busy"
                                ))
                            } else {
                                desktop.action(window, &action)
                            };
                            let actual = if matches!(
                                action,
                                WindowAction::Resize { .. } | WindowAction::SetHeader { .. }
                            ) && result.is_ok()
                            {
                                desktop
                                    .surfaces(mode)
                                    .ok()
                                    .and_then(|list| list.into_iter().find(|w| w.key == window))
                            } else {
                                actual
                            };
                            let _ = out.blocking_send(Message::Ack {
                                request,
                                window,
                                actual,
                                error: result.err().map(|e| e.to_string()),
                            });
                        }
                    }
                }
                if release_on_close {
                    release_native(desktop.as_ref(), session_generation);
                }
            })?;
        Ok(Self {
            shared,
            epoch: None,
        })
    }
    pub fn clock(&mut self, elapsed_ms: u64) {
        if elapsed_ms > 90 * 24 * 60 * 60 * 1000 {
            return;
        }
        if let Some(candidate) = Instant::now().checked_sub(Duration::from_millis(elapsed_ms)) {
            self.epoch = Some(self.epoch.map_or(candidate, |old| old.min(candidate)));
        }
    }
    fn push(&self, work: Work) -> anyhow::Result<()> {
        let (lock, ready) = &*self.shared;
        let result = lock.lock().unwrap().push(work);
        ready.notify_one();
        result
    }
    pub fn input(
        &self,
        window: WindowId,
        event: InputEvent,
        elapsed_ms: u64,
    ) -> anyhow::Result<()> {
        self.enqueue(window, None, event, elapsed_ms)
    }
    pub fn shared_input(
        &self,
        window: WindowId,
        generation: u64,
        event: InputEvent,
        elapsed_ms: u64,
    ) -> anyhow::Result<()> {
        self.enqueue(window, Some(generation), event, elapsed_ms)
    }
    fn enqueue(
        &self,
        window: WindowId,
        generation: Option<u64>,
        event: InputEvent,
        elapsed_ms: u64,
    ) -> anyhow::Result<()> {
        let now = Instant::now();
        let ttl = Duration::from_millis(
            if matches!(
                event,
                InputEvent::Pointer { .. } | InputEvent::RelativeMotion { .. }
            ) {
                150
            } else {
                750
            },
        );
        let Some(epoch) = self.epoch else {
            return Ok(());
        };
        let Some(sent) = epoch.checked_add(Duration::from_millis(elapsed_ms)) else {
            return Ok(());
        };
        let Some(deadline) = sent.checked_add(ttl) else {
            return Ok(());
        };
        self.push(Work::Input {
            window,
            generation,
            event,
            deadline: deadline.min(now + ttl),
        })
    }
    pub fn action(
        &self,
        request: u64,
        window: WindowId,
        action: WindowAction,
        actual: Option<RemoteWindow>,
        mode: ViewMode,
    ) -> anyhow::Result<()> {
        self.push(Work::Action {
            request,
            window,
            action,
            actual,
            mode,
            deadline: Instant::now() + Duration::from_secs(1),
        })
    }
    pub fn release(&self) -> anyhow::Result<()> {
        self.push(Work::Release)
    }
    pub fn resume(&self, generation: u64, elapsed_ms: u64) -> anyhow::Result<()> {
        let ttl = Duration::from_millis(300);
        let Some(deadline) = self
            .epoch
            .and_then(|epoch| epoch.checked_add(Duration::from_millis(elapsed_ms)))
            .and_then(|sent| sent.checked_add(ttl))
        else {
            return Ok(());
        };
        self.push(Work::Resume {
            generation,
            deadline: deadline.min(Instant::now() + ttl),
        })
    }
}
impl Drop for Dispatcher {
    fn drop(&mut self) {
        let (lock, ready) = &*self.shared;
        let mut queue = lock.lock().unwrap();
        queue.closed = true;
        queue.items.clear();
        ready.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relative_motion_sums_only_within_one_target_generation_and_transition() {
        let window = WindowId {
            id: 1,
            generation: 1,
        };
        let deadline = Instant::now() + Duration::from_secs(1);
        let relative = |generation| Work::Input {
            window,
            generation: Some(generation),
            event: InputEvent::RelativeMotion { dx: 3, dy: -2 },
            deadline,
        };
        let mut queue = Queue::default();
        queue.push(relative(1)).unwrap();
        queue.push(relative(1)).unwrap();
        assert!(matches!(
            queue.items.front(),
            Some(Work::Input {
                event: InputEvent::RelativeMotion { dx: 6, dy: -4 },
                ..
            })
        ));
        queue.push(relative(2)).unwrap();
        assert_eq!(queue.items.len(), 2);
        queue
            .push(Work::Input {
                window,
                generation: Some(2),
                event: InputEvent::Key {
                    usage: 4,
                    down: true,
                },
                deadline,
            })
            .unwrap();
        queue.push(relative(2)).unwrap();
        assert_eq!(queue.items.len(), 4);
        queue.push(Work::Release).unwrap();
        assert!(matches!(queue.items.front(), Some(Work::Release)));
    }
    #[test]
    fn overload_discards_pending_presses_and_admits_release() {
        let mut queue = Queue::default();
        for _ in 0..128 {
            queue
                .push(Work::Resume {
                    generation: 1,
                    deadline: Instant::now(),
                })
                .unwrap();
        }
        assert!(
            queue
                .push(Work::Resume {
                    generation: 1,
                    deadline: Instant::now()
                })
                .is_err()
        );
        assert_eq!(queue.items.len(), 1);
        assert!(matches!(queue.items.front(), Some(Work::Release)));
    }
    fn motion(x: f64) -> Work {
        Work::Input {
            generation: None,
            window: WindowId {
                id: 1,
                generation: 1,
            },
            event: InputEvent::Pointer { x, y: 0. },
            deadline: Instant::now() + Duration::from_secs(1),
        }
    }
    #[test]
    fn motion_flood_keeps_only_latest_and_does_not_cross_clicks() {
        let mut queue = Queue::default();
        for i in 0..100_000 {
            queue.push(motion(i as f64)).unwrap();
        }
        assert_eq!(queue.items.len(), 1);
        assert!(
            matches!(queue.items.front(),Some(Work::Input{event:InputEvent::Pointer{x,..},..}) if *x==99_999.)
        );
        queue
            .push(Work::Input {
                generation: None,
                window: WindowId {
                    id: 1,
                    generation: 1,
                },
                event: InputEvent::Button {
                    button: 0,
                    down: true,
                    x: 1.,
                    y: 1.,
                },
                deadline: Instant::now(),
            })
            .unwrap();
        queue.push(motion(1.)).unwrap();
        queue.push(motion(2.)).unwrap();
        assert_eq!(queue.items.len(), 3);
        queue.push(Work::Release).unwrap();
        assert_eq!(queue.items.len(), 1);
        assert!(matches!(queue.items.front(), Some(Work::Release)));
    }
    #[test]
    fn stalled_native_call_does_not_replay_old_mouse_clicks_or_resume() {
        struct Fake {
            entered: std::sync::mpsc::Sender<()>,
            seen: std::sync::mpsc::Sender<InputEvent>,
            gate: Arc<(Mutex<bool>, Condvar)>,
        }
        impl Desktop for Fake {
            fn windows(&self) -> anyhow::Result<Vec<RemoteWindow>> {
                Ok(vec![])
            }
            fn action(&self, _: WindowId, _: &WindowAction) -> anyhow::Result<()> {
                Ok(())
            }
            fn input(&self, _: WindowId, event: &InputEvent) -> anyhow::Result<()> {
                if matches!(event, InputEvent::Text { .. }) {
                    self.entered.send(()).unwrap();
                    let (lock, ready) = &*self.gate;
                    let mut open = lock.lock().unwrap();
                    while !*open {
                        open = ready.wait(open).unwrap();
                    }
                } else {
                    self.seen.send(event.clone()).unwrap();
                }
                Ok(())
            }
            fn release_input(&self) {}
            fn shared_resume(&self, generation: u64) -> anyhow::Result<()> {
                self.seen
                    .send(InputEvent::Text {
                        text: format!("resume {generation}"),
                    })
                    .unwrap();
                Ok(())
            }
            fn capture(
                &self,
                _: WindowId,
                _: bool,
            ) -> anyhow::Result<frames_native::CaptureStream> {
                anyhow::bail!("no capture")
            }
            fn clipboard(&self) -> anyhow::Result<Option<String>> {
                Ok(None)
            }
            fn set_clipboard(&self, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
        }
        let (entered, waiting) = std::sync::mpsc::channel();
        let (seen, received) = std::sync::mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (control, _rx) = tokio::sync::mpsc::channel(8);
        let (bulk, _bulk_rx) = tokio::sync::mpsc::channel(8);
        let out = Outbound {
            devices: frames_core::devices::channel().0,
            media: None,
            realtime: control.clone(),
            input: control.clone(),
            control,
            bulk,
            progress: None,
        };
        let mut dispatch = Dispatcher::new(
            Arc::new(Fake {
                entered,
                seen,
                gate: gate.clone(),
            }),
            out,
            true,
        )
        .unwrap();
        let window = WindowId {
            id: 1,
            generation: 1,
        };
        dispatch.clock(0);
        dispatch
            .input(
                window,
                InputEvent::Text {
                    text: "block".into(),
                },
                0,
            )
            .unwrap();
        waiting.recv_timeout(Duration::from_secs(1)).unwrap();
        dispatch.epoch = Instant::now().checked_sub(Duration::from_secs(2));
        dispatch.resume(7, 0).unwrap();
        dispatch
            .input(window, InputEvent::Pointer { x: 0.1, y: 0.1 }, 0)
            .unwrap();
        dispatch
            .input(
                window,
                InputEvent::Button {
                    button: 0,
                    down: true,
                    x: 0.1,
                    y: 0.1,
                },
                0,
            )
            .unwrap();
        dispatch
            .input(
                window,
                InputEvent::Button {
                    button: 0,
                    down: false,
                    x: 0.1,
                    y: 0.1,
                },
                2000,
            )
            .unwrap();
        dispatch.resume(7, 2000).unwrap();
        dispatch
            .input(window, InputEvent::Pointer { x: 0.8, y: 0.8 }, 2000)
            .unwrap();
        // A delayed clock-sync message cannot make old input look fresh.
        let epoch = dispatch.epoch;
        dispatch.clock(0);
        assert_eq!(dispatch.epoch, epoch);
        *gate.0.lock().unwrap() = true;
        gate.1.notify_one();
        assert!(
            matches!(received.recv_timeout(Duration::from_secs(1)).unwrap(), InputEvent::Text { text } if text == "resume 7")
        );
        assert!(
            matches!(received.recv_timeout(Duration::from_secs(1)).unwrap(),InputEvent::Pointer{x,y} if x==0.8&&y==0.8)
        );
        assert!(received.try_recv().is_err());
    }
}
