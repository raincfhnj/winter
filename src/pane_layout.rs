use crate::model::Direction;

const MAX_NATIVE_DIVIDER_GAP_PX: i32 = 16;
const FOCUS_INSET_PX: i32 = 24;
const NATIVE_RESIZE_STEP_DIVISOR: i32 = 20;
const MAX_RESIZE_ACTIONS_PER_POINTER_EVENT: i32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenPoint {
    pub x: i32,
    pub y: i32,
}

impl ScreenPoint {
    #[must_use]
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl ScreenRect {
    #[must_use]
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.right > self.left && self.bottom > self.top
    }

    #[must_use]
    pub const fn width(self) -> i32 {
        self.right.saturating_sub(self.left)
    }

    #[must_use]
    pub const fn height(self) -> i32 {
        self.bottom.saturating_sub(self.top)
    }

    #[must_use]
    pub const fn contains(self, point: ScreenPoint) -> bool {
        point.x >= self.left && point.x < self.right && point.y >= self.top && point.y < self.bottom
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneGeometry {
    pub bounds: ScreenRect,
    pub has_keyboard_focus: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitAxis {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneDivider {
    pub axis: SplitAxis,
    coordinate: i32,
    span_start: i32,
    span_end: i32,
    hit_band_start: i32,
    hit_band_end: i32,
    leading_focus_point: ScreenPoint,
    native_step_pixels: i32,
}

impl PaneDivider {
    /// Creates a divider, clamping `native_step_pixels` to at least `1` so
    /// [`PaneDrag::update`] can never divide by zero.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub const fn new(
        axis: SplitAxis,
        coordinate: i32,
        span_start: i32,
        span_end: i32,
        hit_band_start: i32,
        hit_band_end: i32,
        leading_focus_point: ScreenPoint,
        native_step_pixels: i32,
    ) -> Self {
        Self {
            axis,
            coordinate,
            span_start,
            span_end,
            hit_band_start,
            hit_band_end,
            leading_focus_point,
            native_step_pixels: if native_step_pixels < 1 {
                1
            } else {
                native_step_pixels
            },
        }
    }

    #[must_use]
    pub const fn axis(&self) -> SplitAxis {
        self.axis
    }

    #[must_use]
    pub const fn native_step_pixels(&self) -> i32 {
        self.native_step_pixels
    }

    /// Half-open hit test: both the band and the span cover
    /// `[start - slop, end + slop)`, matching [`ScreenRect::contains`].
    #[must_use]
    pub fn hit_test(self, point: ScreenPoint, hit_slop_pixels: i32) -> bool {
        let hit_slop_pixels = hit_slop_pixels.max(0);
        match self.axis {
            SplitAxis::Vertical => {
                point.x >= self.hit_band_start.saturating_sub(hit_slop_pixels)
                    && point.x < self.hit_band_end.saturating_add(hit_slop_pixels)
                    && point.y >= self.span_start.saturating_sub(hit_slop_pixels)
                    && point.y < self.span_end.saturating_add(hit_slop_pixels)
            }
            SplitAxis::Horizontal => {
                point.y >= self.hit_band_start.saturating_sub(hit_slop_pixels)
                    && point.y < self.hit_band_end.saturating_add(hit_slop_pixels)
                    && point.x >= self.span_start.saturating_sub(hit_slop_pixels)
                    && point.x < self.span_end.saturating_add(hit_slop_pixels)
            }
        }
    }

    const fn distance_from_line(self, point: ScreenPoint) -> u32 {
        match self.axis {
            SplitAxis::Vertical => point.x.saturating_sub(self.coordinate).unsigned_abs(),
            SplitAxis::Horizontal => point.y.saturating_sub(self.coordinate).unsigned_abs(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaneLayout {
    panes: Vec<PaneGeometry>,
    dividers: Vec<PaneDivider>,
}

impl PaneLayout {
    #[must_use]
    pub fn from_panes(panes: Vec<PaneGeometry>) -> Self {
        let panes = panes
            .into_iter()
            .filter(|pane| pane.bounds.is_valid())
            .collect::<Vec<_>>();
        let mut dividers = Vec::new();
        for first_index in 0..panes.len() {
            for second_index in (first_index + 1)..panes.len() {
                if let Some(divider) = divider_between(panes[first_index], panes[second_index]) {
                    dividers.push(divider);
                }
            }
        }

        Self { panes, dividers }
    }

    #[must_use]
    pub fn panes(&self) -> &[PaneGeometry] {
        &self.panes
    }

    #[must_use]
    pub fn dividers(&self) -> &[PaneDivider] {
        &self.dividers
    }

    #[must_use]
    pub fn divider_at(&self, point: ScreenPoint, hit_slop_pixels: i32) -> Option<PaneDivider> {
        self.dividers
            .iter()
            .copied()
            .filter(|divider| divider.hit_test(point, hit_slop_pixels))
            .min_by_key(|divider| divider.distance_from_line(point))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneResizeIntent {
    pub direction: Direction,
    pub focus_point: ScreenPoint,
    pub steps: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneDrag {
    divider: PaneDivider,
    last_coordinate: i32,
    residual_pixels: i32,
}

impl PaneDrag {
    #[must_use]
    pub const fn begin(divider: PaneDivider, pointer: ScreenPoint) -> Self {
        Self {
            divider,
            last_coordinate: coordinate_for_axis(divider.axis, pointer),
            residual_pixels: 0,
        }
    }

    #[must_use]
    pub const fn axis(&self) -> SplitAxis {
        self.divider.axis
    }

    /// Consumes accumulated pointer movement and emits resize steps in whole
    /// `native_step_pixels` units, at most `MAX_RESIZE_ACTIONS_PER_POINTER_EVENT`
    /// per call. A pointer reversal discards the stale residual instead of
    /// letting opposite movements cancel each other out.
    #[must_use]
    pub fn update(&mut self, pointer: ScreenPoint) -> Option<PaneResizeIntent> {
        let coordinate = coordinate_for_axis(self.divider.axis, pointer);
        let delta = coordinate.saturating_sub(self.last_coordinate);
        let residual_reversed = delta != 0
            && self.residual_pixels != 0
            && delta.signum() != self.residual_pixels.signum();
        self.residual_pixels = if residual_reversed {
            delta
        } else {
            self.residual_pixels.saturating_add(delta)
        };
        self.last_coordinate = coordinate;

        let step_pixels = self.divider.native_step_pixels().max(1);
        let available_steps = self.residual_pixels / step_pixels;
        let dispatched_steps = available_steps.clamp(
            -MAX_RESIZE_ACTIONS_PER_POINTER_EVENT,
            MAX_RESIZE_ACTIONS_PER_POINTER_EVENT,
        );
        self.residual_pixels = self
            .residual_pixels
            .saturating_sub(dispatched_steps.saturating_mul(step_pixels));

        let direction = match (self.divider.axis, dispatched_steps.signum()) {
            (SplitAxis::Vertical, -1) => Direction::Left,
            (SplitAxis::Vertical, 1) => Direction::Right,
            (SplitAxis::Horizontal, -1) => Direction::Up,
            (SplitAxis::Horizontal, 1) => Direction::Down,
            _ => return None,
        };
        Some(PaneResizeIntent {
            direction,
            focus_point: self.divider.leading_focus_point,
            // `dispatched_steps` is bounded by MAX_RESIZE_ACTIONS_PER_POINTER_EVENT.
            steps: dispatched_steps.unsigned_abs() as u8,
        })
    }
}

const fn coordinate_for_axis(axis: SplitAxis, point: ScreenPoint) -> i32 {
    match axis {
        SplitAxis::Vertical => point.x,
        SplitAxis::Horizontal => point.y,
    }
}

fn divider_between(first: PaneGeometry, second: PaneGeometry) -> Option<PaneDivider> {
    vertical_divider(first.bounds, second.bounds)
        .or_else(|| horizontal_divider(first.bounds, second.bounds))
}

fn vertical_divider(first: ScreenRect, second: ScreenRect) -> Option<PaneDivider> {
    let (leading, trailing) = if first.left <= second.left {
        (first, second)
    } else {
        (second, first)
    };
    let gap = trailing.left.saturating_sub(leading.right);
    let span_start = leading.top.max(trailing.top);
    let span_end = leading.bottom.min(trailing.bottom);
    if gap.saturating_abs() > MAX_NATIVE_DIVIDER_GAP_PX || span_end <= span_start {
        return None;
    }

    let hit_band_start = leading.right.min(trailing.left);
    let hit_band_end = leading
        .right
        .max(trailing.left)
        .max(hit_band_start.saturating_add(1));
    let total_width = trailing.right.saturating_sub(leading.left);
    Some(PaneDivider::new(
        SplitAxis::Vertical,
        midpoint(leading.right, trailing.left),
        span_start,
        span_end,
        hit_band_start,
        hit_band_end,
        ScreenPoint::new(
            leading
                .left
                .saturating_add(FOCUS_INSET_PX.min((leading.width() / 2).max(1))),
            midpoint(span_start, span_end),
        ),
        native_step_pixels(total_width),
    ))
}

fn horizontal_divider(first: ScreenRect, second: ScreenRect) -> Option<PaneDivider> {
    let (leading, trailing) = if first.top <= second.top {
        (first, second)
    } else {
        (second, first)
    };
    let gap = trailing.top.saturating_sub(leading.bottom);
    let span_start = leading.left.max(trailing.left);
    let span_end = leading.right.min(trailing.right);
    if gap.saturating_abs() > MAX_NATIVE_DIVIDER_GAP_PX || span_end <= span_start {
        return None;
    }

    let hit_band_start = leading.bottom.min(trailing.top);
    let hit_band_end = leading
        .bottom
        .max(trailing.top)
        .max(hit_band_start.saturating_add(1));
    let total_height = trailing.bottom.saturating_sub(leading.top);
    Some(PaneDivider::new(
        SplitAxis::Horizontal,
        midpoint(leading.bottom, trailing.top),
        span_start,
        span_end,
        hit_band_start,
        hit_band_end,
        ScreenPoint::new(
            midpoint(span_start, span_end),
            leading
                .top
                .saturating_add(FOCUS_INSET_PX.min((leading.height() / 2).max(1))),
        ),
        native_step_pixels(total_height),
    ))
}

const fn native_step_pixels(parent_extent: i32) -> i32 {
    let pixels = parent_extent / NATIVE_RESIZE_STEP_DIVISOR;
    if pixels < 1 { 1 } else { pixels }
}

const fn midpoint(first: i32, second: i32) -> i32 {
    match second.checked_sub(first) {
        Some(delta) => first + delta / 2,
        None => first / 2 + second / 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(left: i32, top: i32, right: i32, bottom: i32) -> PaneGeometry {
        PaneGeometry {
            bounds: ScreenRect::new(left, top, right, bottom),
            has_keyboard_focus: false,
        }
    }

    #[test]
    fn infers_independent_nested_dividers_from_native_pane_rectangles() {
        let layout = PaneLayout::from_panes(vec![
            pane(0, 0, 497, 1000),
            pane(503, 0, 1000, 497),
            pane(503, 503, 1000, 1000),
        ]);

        assert_eq!(layout.dividers().len(), 3);
        let root_top = layout
            .divider_at(ScreenPoint::new(500, 250), 2)
            .expect("root divider should cover the upper segment");
        let root_bottom = layout
            .divider_at(ScreenPoint::new(500, 750), 2)
            .expect("root divider should cover the lower segment");
        let nested = layout
            .divider_at(ScreenPoint::new(750, 500), 2)
            .expect("nested horizontal divider should be detected");

        assert_eq!(root_top.axis, SplitAxis::Vertical);
        assert_eq!(root_bottom.axis, SplitAxis::Vertical);
        assert_eq!(nested.axis, SplitAxis::Horizontal);
        assert_eq!(root_top.native_step_pixels, 50);
        assert_eq!(nested.native_step_pixels, 50);
    }

    #[test]
    fn divider_hit_target_includes_native_gap_and_configured_slop() {
        let layout =
            PaneLayout::from_panes(vec![pane(100, 100, 497, 900), pane(503, 100, 900, 900)]);

        assert!(layout.divider_at(ScreenPoint::new(494, 500), 3).is_some());
        assert!(layout.divider_at(ScreenPoint::new(505, 500), 3).is_some());
        assert!(layout.divider_at(ScreenPoint::new(493, 500), 3).is_none());
        assert!(layout.divider_at(ScreenPoint::new(506, 500), 3).is_none());
        assert!(layout.divider_at(ScreenPoint::new(500, 98), 3).is_some());
        assert!(layout.divider_at(ScreenPoint::new(500, 96), 3).is_none());
    }

    #[test]
    fn hit_test_uses_half_open_boundaries_on_both_axes() {
        let layout =
            PaneLayout::from_panes(vec![pane(100, 100, 497, 900), pane(503, 100, 900, 900)]);

        assert!(layout.divider_at(ScreenPoint::new(500, 902), 3).is_some());
        assert!(layout.divider_at(ScreenPoint::new(500, 903), 3).is_none());
        assert!(layout.divider_at(ScreenPoint::new(494, 97), 3).is_some());
        assert!(layout.divider_at(ScreenPoint::new(494, 96), 3).is_none());

        let horizontal =
            PaneLayout::from_panes(vec![pane(100, 100, 900, 497), pane(100, 503, 900, 900)]);
        assert!(
            horizontal
                .divider_at(ScreenPoint::new(899, 500), 3)
                .is_some()
        );
        assert!(
            horizontal
                .divider_at(ScreenPoint::new(903, 500), 3)
                .is_none()
        );
        assert!(
            horizontal
                .divider_at(ScreenPoint::new(902, 500), 3)
                .is_some()
        );
        assert!(
            horizontal
                .divider_at(ScreenPoint::new(500, 494), 3)
                .is_some()
        );
        assert!(
            horizontal
                .divider_at(ScreenPoint::new(500, 493), 3)
                .is_none()
        );
    }

    #[test]
    fn adjacent_panes_keep_a_hit_pixel_at_the_shared_boundary() {
        let layout = PaneLayout::from_panes(vec![pane(0, 0, 500, 800), pane(500, 0, 1000, 800)]);

        assert_eq!(layout.dividers().len(), 1);
        assert!(layout.divider_at(ScreenPoint::new(500, 400), 0).is_some());
        assert!(layout.divider_at(ScreenPoint::new(501, 400), 0).is_none());
        assert!(layout.divider_at(ScreenPoint::new(499, 400), 0).is_none());
        assert!(layout.divider_at(ScreenPoint::new(492, 400), 8).is_some());
        assert!(layout.divider_at(ScreenPoint::new(508, 400), 8).is_some());
        assert!(layout.divider_at(ScreenPoint::new(509, 400), 8).is_none());
    }

    #[test]
    fn divider_construction_clamps_native_step_to_at_least_one() {
        let divider = PaneDivider::new(
            SplitAxis::Vertical,
            500,
            0,
            800,
            497,
            503,
            ScreenPoint::new(480, 400),
            0,
        );
        assert_eq!(divider.native_step_pixels(), 1);
        assert_eq!(divider.axis(), SplitAxis::Vertical);

        let mut drag = PaneDrag::begin(divider, ScreenPoint::new(500, 400));
        let right = drag
            .update(ScreenPoint::new(501, 400))
            .expect("a zero step must not panic nor swallow movement");
        assert_eq!(right.steps, 1);
        assert_eq!(right.direction, Direction::Right);
        let left = drag
            .update(ScreenPoint::new(500, 400))
            .expect("reverse movement must dispatch against the clamped step");
        assert_eq!(left.steps, 1);
        assert_eq!(left.direction, Direction::Left);
    }

    #[test]
    fn degenerate_rectangles_and_extreme_points_never_panic() {
        let layout = PaneLayout::from_panes(vec![
            pane(i32::MIN, i32::MIN, i32::MAX, i32::MAX),
            pane(i32::MIN, i32::MIN, 0, i32::MAX),
            pane(0, i32::MIN, i32::MAX, 0),
            pane(i32::MIN, i32::MIN, 0, 0),
            pane(0, 0, i32::MAX, 100),
            pane(i32::MIN, 0, i32::MAX, 100),
        ]);
        let _ = layout.panes();
        let _ = layout.dividers();

        assert!(
            layout
                .divider_at(ScreenPoint::new(i32::MAX, i32::MAX), i32::MAX)
                .is_none()
        );
        let _ = layout.divider_at(ScreenPoint::new(i32::MIN, i32::MIN), i32::MIN);
        let _ = layout.divider_at(ScreenPoint::new(0, 0), 0);

        assert_eq!(midpoint(497, 503), 500);
        assert_eq!(midpoint(i32::MIN, i32::MAX), -1);
        assert_eq!(midpoint(i32::MAX, i32::MIN), -1);
        assert_eq!(midpoint(i32::MIN, i32::MIN), i32::MIN);
        assert_eq!(midpoint(i32::MAX, i32::MAX), i32::MAX);

        assert_eq!(ScreenRect::new(i32::MIN, 0, i32::MAX, 10).width(), i32::MAX);
        assert_eq!(
            ScreenRect::new(0, i32::MIN, 10, i32::MAX).height(),
            i32::MAX
        );
    }

    #[test]
    fn extreme_drag_movement_clamps_steps_and_never_panics() {
        let divider = PaneLayout::from_panes(vec![
            pane(i32::MIN, i32::MIN, 0, i32::MAX),
            pane(0, i32::MIN, i32::MAX, i32::MAX),
        ])
        .divider_at(ScreenPoint::new(0, 0), 0)
        .expect("adjacent extreme panes still expose their divider");
        let mut drag = PaneDrag::begin(divider, ScreenPoint::new(0, 0));

        let right = drag
            .update(ScreenPoint::new(i32::MAX, 0))
            .expect("extreme delta should dispatch clamped steps");
        assert_eq!(right.steps, MAX_RESIZE_ACTIONS_PER_POINTER_EVENT as u8);
        assert_eq!(right.direction, Direction::Right);

        let left = drag
            .update(ScreenPoint::new(i32::MIN, 0))
            .expect("reversed extreme delta should dispatch clamped steps");
        assert_eq!(left.steps, MAX_RESIZE_ACTIONS_PER_POINTER_EVENT as u8);
        assert_eq!(left.direction, Direction::Left);
    }

    #[test]
    fn drag_reversal_resets_the_residual_instead_of_accumulating() {
        let divider = PaneLayout::from_panes(vec![pane(0, 0, 497, 800), pane(503, 0, 1000, 800)])
            .divider_at(ScreenPoint::new(500, 400), 0)
            .expect("divider should exist");
        let mut drag = PaneDrag::begin(divider, ScreenPoint::new(500, 400));

        assert!(drag.update(ScreenPoint::new(540, 400)).is_none());
        let intent = drag
            .update(ScreenPoint::new(480, 400))
            .expect("reversal should dispatch immediately instead of cancelling");
        assert_eq!(intent.direction, Direction::Left);
        assert_eq!(intent.steps, 1);
    }

    #[test]
    fn horizontal_divider_hit_target_extends_along_its_span() {
        let layout =
            PaneLayout::from_panes(vec![pane(100, 100, 900, 497), pane(100, 503, 900, 900)]);

        assert!(layout.divider_at(ScreenPoint::new(500, 500), 3).is_some());
        assert!(layout.divider_at(ScreenPoint::new(98, 500), 3).is_some());
        assert!(layout.divider_at(ScreenPoint::new(96, 500), 3).is_none());
    }

    #[test]
    fn drag_translates_pointer_distance_into_native_five_percent_steps() {
        let divider = PaneLayout::from_panes(vec![pane(0, 0, 497, 800), pane(503, 0, 1000, 800)])
            .divider_at(ScreenPoint::new(500, 400), 0)
            .expect("divider should exist");
        let mut drag = PaneDrag::begin(divider, ScreenPoint::new(500, 400));

        assert!(drag.update(ScreenPoint::new(549, 400)).is_none());
        let right = drag
            .update(ScreenPoint::new(600, 400))
            .expect("drag should produce right resize steps");
        assert_eq!(right.steps, 2);
        assert_eq!(right.direction, Direction::Right);

        let left = drag
            .update(ScreenPoint::new(500, 400))
            .expect("drag should produce left resize steps");
        assert_eq!(left.steps, 2);
        assert_eq!(left.direction, Direction::Left);
    }

    #[test]
    fn invalid_and_distant_rectangles_do_not_create_false_dividers() {
        let layout = PaneLayout::from_panes(vec![
            pane(0, 0, 0, 100),
            pane(0, 0, 100, 100),
            pane(200, 0, 300, 100),
            pane(0, 200, 100, 300),
        ]);

        assert_eq!(layout.panes().len(), 3);
        assert!(layout.dividers().is_empty());
    }
}
