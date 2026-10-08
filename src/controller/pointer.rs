use crate::model::WindowIdentity;
use crate::pane_layout::{PaneDivider, PaneDrag, PaneResizeIntent, ScreenPoint, SplitAxis};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PointerResizeRequest {
    pub target: WindowIdentity,
    pub intent: PaneResizeIntent,
    /// Identifier for the drag that produced this request.
    pub sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PointerDecision {
    Pass,
    Consume {
        cursor_axis: Option<SplitAxis>,
        resize: Option<PointerResizeRequest>,
    },
}

impl PointerDecision {
    #[must_use]
    pub(super) const fn consumes(self) -> bool {
        matches!(self, Self::Consume { .. })
    }

    #[must_use]
    pub(super) const fn cursor_axis(self) -> Option<SplitAxis> {
        match self {
            Self::Pass => None,
            Self::Consume { cursor_axis, .. } => cursor_axis,
        }
    }

    #[must_use]
    pub(super) const fn resize(self) -> Option<PointerResizeRequest> {
        match self {
            Self::Pass => None,
            Self::Consume { resize, .. } => resize,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActivePaneDrag {
    target: WindowIdentity,
    drag: PaneDrag,
    sequence: u64,
    canceled: bool,
}

/// Pure pointer-capture state for native pane divider drags.
///
/// This deliberately knows nothing about hooks, UI Automation, queues, or
/// input injection. That keeps the rule "hover always passes through; an
/// active primary-button drag is the only capture" independently testable.
#[derive(Debug, Default)]
pub(super) struct PointerDragState {
    active: Option<ActivePaneDrag>,
    next_sequence: u64,
}

impl PointerDragState {
    pub(super) fn begin(
        &mut self,
        target_and_divider: Option<(WindowIdentity, PaneDivider)>,
        point: ScreenPoint,
    ) -> PointerDecision {
        let Some((target, divider)) = target_and_divider else {
            self.active = None;
            return PointerDecision::Pass;
        };

        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.active = Some(ActivePaneDrag {
            target,
            drag: PaneDrag::begin(divider, point),
            sequence: self.next_sequence,
            canceled: false,
        });
        PointerDecision::Consume {
            cursor_axis: Some(divider.axis),
            resize: None,
        }
    }

    pub(super) fn move_to(
        &mut self,
        foreground_window: isize,
        point: ScreenPoint,
    ) -> PointerDecision {
        let Some(active) = self.active.as_mut() else {
            return PointerDecision::Pass;
        };
        if active.canceled {
            return PointerDecision::Consume {
                cursor_axis: None,
                resize: None,
            };
        }
        if foreground_window != active.target.hwnd {
            active.canceled = true;
            return PointerDecision::Consume {
                cursor_axis: None,
                resize: None,
            };
        }

        let cursor_axis = active.drag.axis();
        let sequence = active.sequence;
        let resize = active
            .drag
            .update(point)
            .map(|intent| PointerResizeRequest {
                target: active.target,
                intent,
                sequence,
            });
        PointerDecision::Consume {
            cursor_axis: Some(cursor_axis),
            resize,
        }
    }

    pub(super) fn end(&mut self) -> PointerDecision {
        if self.active.take().is_some() {
            PointerDecision::Consume {
                cursor_axis: None,
                resize: None,
            }
        } else {
            PointerDecision::Pass
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::model::TerminalChannel;
    use crate::pane_layout::{PaneGeometry, PaneLayout, ScreenRect};

    use super::*;

    fn target() -> WindowIdentity {
        WindowIdentity {
            hwnd: 42,
            process_id: 7,
            process_started_at_100ns: 9,
            channel: TerminalChannel::Stable,
        }
    }

    fn divider() -> PaneDivider {
        PaneLayout::from_panes(vec![
            PaneGeometry {
                bounds: ScreenRect::new(0, 0, 497, 800),
                has_keyboard_focus: false,
                title: String::new(),
            },
            PaneGeometry {
                bounds: ScreenRect::new(503, 0, 1_000, 800),
                has_keyboard_focus: false,
                title: String::new(),
            },
        ])
        .divider_at(ScreenPoint::new(500, 400), 0)
        .expect("fixture has a divider")
    }

    #[test]
    fn hover_passes_through_and_only_an_active_drag_is_captured() {
        let mut state = PointerDragState::default();
        assert!(
            !state
                .move_to(target().hwnd, ScreenPoint::new(500, 400))
                .consumes()
        );

        let start = state.begin(Some((target(), divider())), ScreenPoint::new(500, 400));
        assert!(start.consumes());
        assert_eq!(start.cursor_axis(), Some(SplitAxis::Vertical));

        let movement = state.move_to(target().hwnd, ScreenPoint::new(600, 400));
        let resize = movement
            .resize()
            .expect("drag should produce a resize request");
        assert_eq!(resize.target, target());
        assert_eq!(resize.intent.steps, 2);

        assert!(state.end().consumes());
        assert!(
            !state
                .move_to(target().hwnd, ScreenPoint::new(500, 400))
                .consumes()
        );
    }

    #[test]
    fn foreground_change_cancels_without_dispatching_to_the_new_window() {
        let mut state = PointerDragState::default();
        let _ = state.begin(Some((target(), divider())), ScreenPoint::new(500, 400));

        let canceled = state.move_to(99, ScreenPoint::new(600, 400));
        assert!(canceled.consumes());
        assert!(canceled.resize().is_none());
        assert!(
            state
                .move_to(target().hwnd, ScreenPoint::new(700, 400))
                .resize()
                .is_none()
        );
    }

    #[test]
    fn external_drag_started_outside_a_divider_is_never_captured() {
        let mut state = PointerDragState::default();

        assert!(!state.begin(None, ScreenPoint::new(1, 1)).consumes());
        assert!(
            !state
                .move_to(target().hwnd, ScreenPoint::new(500, 400))
                .consumes()
        );
        assert!(!state.end().consumes());
    }
}
