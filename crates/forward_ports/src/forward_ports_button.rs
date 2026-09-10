use gpui::{App, FocusHandle};
use ui::{ButtonCommon, Clickable, Context, Render, Tooltip, Window, prelude::*};
use workspace::{HideStatusItem, ItemHandle, StatusItemView};

use crate::ToggleFocus;

pub struct ForwardPortsButton {
    pane_item_focus_handle: Option<FocusHandle>,
}

impl ForwardPortsButton {
    pub fn new() -> Self {
        Self {
            pane_item_focus_handle: None,
        }
    }
}

impl Default for ForwardPortsButton {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for ForwardPortsButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus_handle = self.pane_item_focus_handle.clone();
        IconButton::new("forward-ports-indicator", IconName::ArrowRightLeft)
            .icon_size(IconSize::Small)
            .tab_index(0isize)
            .aria_label("Forward Ports")
            .tooltip(move |_window, cx| match &focus_handle {
                Some(focus_handle) => {
                    Tooltip::for_action_in("Forward Ports", &ToggleFocus, focus_handle, cx)
                }
                None => Tooltip::for_action("Forward Ports", &ToggleFocus, cx),
            })
            .on_click(cx.listener(|_this, _, window, cx| {
                window.dispatch_action(Box::new(ToggleFocus), cx);
            }))
    }
}

impl StatusItemView for ForwardPortsButton {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pane_item_focus_handle = active_pane_item.map(|item| item.item_focus_handle(cx));
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}
