use crate::model::WindowId;

pub struct Demand {
    pub window: WindowId,
    pub visible: bool,
    pub focused: bool,
    pub running: bool,
}

/// Reserve capacity for the focused window, retaining other live streams to avoid churn.
pub fn select(demands: impl IntoIterator<Item = Demand>, limit: usize) -> Vec<WindowId> {
    let mut visible: Vec<_> = demands.into_iter().filter(|d| d.visible).collect();
    visible.sort_by_key(|d| (!d.focused, !d.running, d.window));
    visible.into_iter().take(limit).map(|d| d.window).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn newly_focused_window_preempts_background_without_churning_other_streams() {
        let key = |id| WindowId { id, generation: 1 };
        let demand = |id, visible, focused, running| Demand {
            window: key(id),
            visible,
            focused,
            running,
        };
        let selected = select(
            [
                demand(1, true, false, false),
                demand(2, true, false, true),
                demand(3, true, true, false),
                demand(4, false, true, true),
            ],
            2,
        );
        assert_eq!(selected, vec![key(3), key(2)]);
    }
}
