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

/// One native pane rectangle plus its optional UI Automation title.
///
/// The `title` is `String`, so this type is `Clone` but no longer `Copy`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneGeometry {
    pub bounds: ScreenRect,
    pub has_keyboard_focus: bool,
    /// Pane/tab title as reported by UI Automation; empty when unavailable.
    pub title: String,
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
    resize_focus_point: Option<ScreenPoint>,
    native_step_pixels: i32,
}

impl PaneDivider {
    /// Creates a divider, clamping `native_step_pixels` to at least `1` so
    /// [`PaneDrag::update`] can never divide by zero.
    ///
    /// `resize_focus_point` is `None` for a divider this bridge cannot drive;
    /// see [`Self::resize_focus_point`].
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub const fn new(
        axis: SplitAxis,
        coordinate: i32,
        span_start: i32,
        span_end: i32,
        hit_band_start: i32,
        hit_band_end: i32,
        resize_focus_point: Option<ScreenPoint>,
        native_step_pixels: i32,
    ) -> Self {
        Self {
            axis,
            coordinate,
            span_start,
            span_end,
            hit_band_start,
            hit_band_end,
            resize_focus_point,
            native_step_pixels: if native_step_pixels < 1 {
                1
            } else {
                native_step_pixels
            },
        }
    }

    /// Point inside the pane that must hold keyboard focus for Windows
    /// Terminal to move *this* divider, or `None` when the observed rectangles
    /// cannot prove that focusing a pane would move this divider rather than
    /// another one.
    ///
    /// Windows Terminal has no "resize this splitter" command. `resizePane`
    /// only resizes a splitter on the path to the focused leaf:
    ///
    /// * `Pane::_Resize` (`src/cascadia/TerminalApp/Pane.cpp:250-278`) returns
    ///   `false` unless `DirectionMatchesSplit(direction, _splitState)` holds,
    ///   and otherwise changes only **this** node's `_desiredSplitPosition`
    ///   (5% per call, negated for `Right`/`Down`).
    /// * `Pane::ResizePane` (ibid. `291-331`) walks into the child subtree that
    ///   owns the focused leaf and evaluates
    ///   `child.ResizePane(direction) || _Resize(direction)`, so the deepest
    ///   ancestor whose splitter axis matches the requested direction is the
    ///   one that moves. Which splitter moves therefore depends only on which
    ///   pane is focused — never on the direction or the pointer.
    ///
    /// These line numbers and excerpts were transcribed from
    /// `_wtres/wtrepo/src/cascadia/TerminalApp/Pane.cpp` (`main@7d06b26`) by
    /// the Team Lead before that untracked reference checkout was deleted from
    /// this working tree, and are also quoted in
    /// `docs/audits/2026-10-10-feature-review.md` §P1-2:
    ///
    /// ```cpp
    /// // Pane.cpp:250-278
    /// bool Pane::_Resize(const ResizeDirection& direction) {
    ///     if (!DirectionMatchesSplit(direction, _splitState)) return false;
    ///     auto amount = .05f;
    ///     if (direction == ResizeDirection::Right || direction == ResizeDirection::Down)
    ///         amount = -amount;
    ///     _desiredSplitPosition = _ClampSplitPosition(changeWidth, _desiredSplitPosition - amount, actualDimension);
    ///     return true;
    /// }
    /// // Pane.cpp:291-331
    /// bool Pane::ResizePane(const ResizeDirection& direction) {
    ///     if (_IsLeaf()) return false;
    ///     if (_firstChild->_lastActive || _secondChild->_lastActive) return _Resize(direction);
    ///     if (!_firstChild->_IsLeaf() && _firstChild->_HasFocusedChild())
    ///         return _firstChild->ResizePane(direction) || _Resize(direction);
    ///     if (!_secondChild->_IsLeaf() && _secondChild->_HasFocusedChild())
    ///         return _secondChild->ResizePane(direction) || _Resize(direction);
    ///     return false;
    /// }
    /// ```
    ///
    /// The first branch fires only when the focused leaf is a *direct* child of
    /// that node (`Tab::_UpdateActivePane`, `Tab.cpp:1336-1354`, calls
    /// `ClearActive()` and then `SetActive()` on the active leaf, so only leaves
    /// carry `_lastActive`). The `|| _Resize(direction)` fallbacks therefore
    /// evaluate the focused leaf's parent first and then each ancestor in turn,
    /// so the splitter that moves is the one owned by the *nearest* ancestor of
    /// the focused leaf whose splitter axis matches the requested direction.
    ///
    /// Only two focus targets are provable from geometry:
    ///
    /// * a pane whose leading edge *is* the layout's leading edge and whose
    ///   trailing edge is this divider, and
    /// * the mirror image: a pane whose trailing edge *is* the layout's
    ///   trailing edge and whose leading edge is this divider.
    ///
    /// For the first case, every ancestor that splits perpendicular to this
    /// divider keeps the pane's span on this axis unchanged, so the nearest
    /// ancestor splitting *along* it must sit exactly on this divider's line: a
    /// parallel ancestor further out would need the pane to have a sibling
    /// between it and the layout edge, which the leading edge rules out.
    /// Focusing any other pane can reach a *different* splitter — the defect
    /// this `None` prevents.
    #[must_use]
    pub const fn resize_focus_point(&self) -> Option<ScreenPoint> {
        self.resize_focus_point
    }

    #[must_use]
    pub const fn axis(&self) -> SplitAxis {
        self.axis
    }

    /// Screen coordinate of the divider line on its own axis.
    #[must_use]
    pub const fn coordinate(&self) -> i32 {
        self.coordinate
    }

    /// Start of the divider span on the axis it does not lie on.
    #[must_use]
    pub const fn span_start(&self) -> i32 {
        self.span_start
    }

    /// End of the divider span on the axis it does not lie on.
    #[must_use]
    pub const fn span_end(&self) -> i32 {
        self.span_end
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
        if let Some(bounds) = layout_bounds(&panes) {
            for first_index in 0..panes.len() {
                for second_index in (first_index + 1)..panes.len() {
                    if let Some(divider) = divider_between(
                        panes[first_index].bounds,
                        panes[second_index].bounds,
                        bounds,
                    ) {
                        dividers.push(divider);
                    }
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

    /// Hit test for a divider this bridge may actually capture.
    ///
    /// Returns `None` when the pointer is not on a divider, and also when the
    /// observed rectangles cannot prove that focusing a pane would move *this*
    /// divider instead of another one (see
    /// [`PaneDivider::resize_focus_point`]). Refusing the capture is
    /// deliberate: Windows Terminal's `resizePane` can only resize the splitter
    /// that owns the focused pane, so an unprovable focus target would move an
    /// unrelated separator. A refused drag is simply not consumed, so the click
    /// reaches Windows Terminal untouched.
    #[must_use]
    pub fn capturable_divider_at(
        &self,
        point: ScreenPoint,
        hit_slop_pixels: i32,
    ) -> Option<PaneDivider> {
        let divider = self.divider_at(point, hit_slop_pixels)?;
        // Fails closed for a divider whose moved splitter is unprovable.
        divider.resize_focus_point()?;
        Some(divider)
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
        // A divider without a provable focus target can never dispatch. The
        // capture path already refuses such dividers, and this keeps a
        // hand-built divider (tests, future callers) from aiming a resize at an
        // unproven target.
        let focus_point = self.divider.resize_focus_point()?;
        Some(PaneResizeIntent {
            direction,
            focus_point,
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

/// Bounding box of every valid pane rectangle.
///
/// Windows Terminal's root pane is not exposed through UI Automation, so this
/// box is the only available stand-in. A pane whose leading edge equals
/// `bounds.left` provably has no sibling to its left anywhere in the pane tree.
fn layout_bounds(panes: &[PaneGeometry]) -> Option<ScreenRect> {
    let mut iter = panes.iter().map(|pane| pane.bounds);
    let first = iter.next()?;
    Some(iter.fold(first, |acc, rect| {
        ScreenRect::new(
            acc.left.min(rect.left),
            acc.top.min(rect.top),
            acc.right.max(rect.right),
            acc.bottom.max(rect.bottom),
        )
    }))
}

fn divider_between(
    first: ScreenRect,
    second: ScreenRect,
    bounds: ScreenRect,
) -> Option<PaneDivider> {
    vertical_divider(first, second, bounds).or_else(|| horizontal_divider(first, second, bounds))
}

/// Vertical divider between two horizontally adjacent panes.
///
/// `bounds` is the layout's bounding box; it decides whether a pane is a
/// provable focus target (see [`PaneDivider::resize_focus_point`]).
fn vertical_divider(
    first: ScreenRect,
    second: ScreenRect,
    bounds: ScreenRect,
) -> Option<PaneDivider> {
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
    let focus = if leading.left == bounds.left {
        Some(leading_inset_point(leading, SplitAxis::Vertical))
    } else if trailing.right == bounds.right {
        Some(leading_inset_point(trailing, SplitAxis::Vertical))
    } else {
        None
    };
    Some(PaneDivider::new(
        SplitAxis::Vertical,
        midpoint(leading.right, trailing.left),
        span_start,
        span_end,
        hit_band_start,
        hit_band_end,
        focus,
        native_step_pixels(total_width),
    ))
}

/// Horizontal divider between two vertically adjacent panes.
///
/// `bounds` is the layout's bounding box; it decides whether a pane is a
/// provable focus target (see [`PaneDivider::resize_focus_point`]).
fn horizontal_divider(
    first: ScreenRect,
    second: ScreenRect,
    bounds: ScreenRect,
) -> Option<PaneDivider> {
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
    let focus = if leading.top == bounds.top {
        Some(leading_inset_point(leading, SplitAxis::Horizontal))
    } else if trailing.bottom == bounds.bottom {
        Some(leading_inset_point(trailing, SplitAxis::Horizontal))
    } else {
        None
    };
    Some(PaneDivider::new(
        SplitAxis::Horizontal,
        midpoint(leading.bottom, trailing.top),
        span_start,
        span_end,
        hit_band_start,
        hit_band_end,
        focus,
        native_step_pixels(total_height),
    ))
}

/// A point strictly inside `pane`, inset from the edge the layout grows from on
/// `axis`: the left edge for a vertical split, the top edge for a horizontal
/// one.
///
/// Focusing any interior point of a pane focuses that pane, so the inset only
/// has to stay inside the rectangle, however small it is.
const fn leading_inset_point(pane: ScreenRect, axis: SplitAxis) -> ScreenPoint {
    match axis {
        SplitAxis::Vertical => ScreenPoint::new(
            inset_from_start(pane.left, pane.width()),
            midpoint(pane.top, pane.bottom),
        ),
        SplitAxis::Horizontal => ScreenPoint::new(
            midpoint(pane.left, pane.right),
            inset_from_start(pane.top, pane.height()),
        ),
    }
}

/// `start + min(FOCUS_INSET_PX, half the extent)`, saturating and never
/// reaching `start + extent`.
const fn inset_from_start(start: i32, extent: i32) -> i32 {
    let max_inset = extent.saturating_sub(1) / 2;
    let inset = if FOCUS_INSET_PX < max_inset {
        FOCUS_INSET_PX
    } else {
        max_inset
    };
    start.saturating_add(if inset < 0 { 0 } else { inset })
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
            title: String::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Windows Terminal `resizePane` reference.
    //
    // UI Automation exposes only visible Leaf rectangles and no pane tree, so a
    // hit divider is checked against *every* slicing tree that can produce the
    // observed rectangles. The reference below replays what Windows Terminal
    // would do for a given tree, focused leaf and direction.
    // -----------------------------------------------------------------------

    /// The screen axis a resize direction acts on.
    ///
    /// `Pane::_Resize` gates on `DirectionMatchesSplit(direction, _splitState)`
    /// (Pane.cpp:252-255), so a left/right request only ever moves a vertical
    /// splitter and an up/down request only a horizontal one.
    fn direction_axis(direction: Direction) -> SplitAxis {
        match direction {
            Direction::Left | Direction::Right => SplitAxis::Vertical,
            Direction::Up | Direction::Down => SplitAxis::Horizontal,
        }
    }

    /// One node of a slicing pane tree reconstructed from observed rectangles.
    #[derive(Debug, Clone)]
    enum RefTree {
        Leaf {
            pane: usize,
            rect: ScreenRect,
        },
        Split {
            axis: SplitAxis,
            rect: ScreenRect,
            first: Box<RefTree>,
            second: Box<RefTree>,
        },
    }

    impl RefTree {
        fn rect(&self) -> ScreenRect {
            match self {
                Self::Leaf { rect, .. } | Self::Split { rect, .. } => *rect,
            }
        }

        fn axis(&self) -> Option<SplitAxis> {
            match self {
                Self::Leaf { .. } => None,
                Self::Split { axis, .. } => Some(*axis),
            }
        }

        /// Screen coordinate of the line this node splits on.
        fn splitter_coordinate(&self) -> Option<i32> {
            let Self::Split {
                axis,
                first,
                second,
                ..
            } = self
            else {
                return None;
            };
            let (first, second) = (first.rect(), second.rect());
            Some(match axis {
                SplitAxis::Vertical => midpoint(first.right, second.left),
                SplitAxis::Horizontal => midpoint(first.bottom, second.top),
            })
        }

        fn contains(&self, pane: usize) -> bool {
            match self {
                Self::Leaf { pane: leaf, .. } => *leaf == pane,
                Self::Split { first, second, .. } => first.contains(pane) || second.contains(pane),
            }
        }

        /// The child subtree owning `pane`, mirroring `ResizePane`'s
        /// `firstIsFocused` / `secondIsFocused` descent.
        fn child_containing(&self, pane: usize) -> Option<&Self> {
            let Self::Split { first, second, .. } = self else {
                return None;
            };
            [first.as_ref(), second.as_ref()]
                .into_iter()
                .find(|child| child.contains(pane))
        }
    }

    /// Replays `Pane::ResizePane` / `Pane::_Resize` for one candidate tree and
    /// returns the coordinate of the splitter that moves, or `None` when no
    /// node accepts the direction.
    ///
    /// The excerpts below were transcribed from
    /// `_wtres/wtrepo/src/cascadia/TerminalApp/Pane.cpp` (`main@7d06b26`) by the
    /// Team Lead before that untracked checkout was deleted, and are also quoted
    /// in `docs/audits/2026-10-10-feature-review.md` §P1-2:
    ///
    /// ```cpp
    /// // Pane.cpp:250-278
    /// bool Pane::_Resize(const ResizeDirection& direction) {
    ///     if (!DirectionMatchesSplit(direction, _splitState)) return false;
    ///     auto amount = .05f;
    ///     if (direction == ResizeDirection::Right || direction == ResizeDirection::Down)
    ///         amount = -amount;
    ///     _desiredSplitPosition = _ClampSplitPosition(changeWidth, _desiredSplitPosition - amount, actualDimension);
    ///     return true;
    /// }
    /// // Pane.cpp:291-331
    /// bool Pane::ResizePane(const ResizeDirection& direction) {
    ///     if (_IsLeaf()) return false;
    ///     if (_firstChild->_lastActive || _secondChild->_lastActive) return _Resize(direction);
    ///     if (!_firstChild->_IsLeaf() && _firstChild->_HasFocusedChild())
    ///         return _firstChild->ResizePane(direction) || _Resize(direction);
    ///     if (!_secondChild->_IsLeaf() && _secondChild->_HasFocusedChild())
    ///         return _secondChild->ResizePane(direction) || _Resize(direction);
    ///     return false;
    /// }
    /// ```
    ///
    /// `Tab::_UpdateActivePane` (`Tab.cpp:1336-1354`) clears the active state of
    /// the whole tree and marks only the active leaf, so only a leaf carries
    /// `_lastActive`; the first branch therefore fires exactly when the focused
    /// leaf is a direct child of this node. The `|| _Resize(direction)` fallbacks
    /// run the focused leaf's parent first and then each ancestor in turn, so the
    /// splitter that moves is the one owned by the *nearest* ancestor of the
    /// focused leaf whose splitter axis matches the direction. The leaf we focus
    /// is therefore the only thing that selects the splitter.
    fn reference_resize(node: &RefTree, direction: Direction, focus: usize) -> Option<i32> {
        // `Pane.cpp:293-297`: a leaf cannot resize anything.
        let axis = node.axis()?;
        // `child.ResizePane(direction)` first: the deepest matching node wins.
        let child = node.child_containing(focus)?;
        if let Some(coordinate) = reference_resize(child, direction, focus) {
            return Some(coordinate);
        }
        // `|| _Resize(direction)`: an ancestor only runs when the deeper node
        // refused, and `_Resize` refuses unless the splitter axis matches.
        if axis == direction_axis(direction) {
            node.splitter_coordinate()
        } else {
            None
        }
    }

    /// Bounding rectangle of `indices`.
    fn bounding_rect(panes: &[PaneGeometry], indices: &[usize]) -> ScreenRect {
        let mut rects = indices.iter().map(|index| panes[*index].bounds);
        let first = rects.next().expect("a pane group is never empty");
        rects.fold(first, |acc, rect| {
            ScreenRect::new(
                acc.left.min(rect.left),
                acc.top.min(rect.top),
                acc.right.max(rect.right),
                acc.bottom.max(rect.bottom),
            )
        })
    }

    /// Split lines that separate `indices` cleanly along `axis`.
    fn cut_candidates(panes: &[PaneGeometry], indices: &[usize], axis: SplitAxis) -> Vec<i32> {
        let rect = bounding_rect(panes, indices);
        let mut candidates = Vec::new();
        for index in indices {
            let pane = panes[*index].bounds;
            let edges = match axis {
                SplitAxis::Vertical => [pane.left, pane.right],
                SplitAxis::Horizontal => [pane.top, pane.bottom],
            };
            for coordinate in edges {
                let inside = match axis {
                    SplitAxis::Vertical => coordinate > rect.left && coordinate < rect.right,
                    SplitAxis::Horizontal => coordinate > rect.top && coordinate < rect.bottom,
                };
                if inside && !candidates.contains(&coordinate) {
                    candidates.push(coordinate);
                }
            }
        }
        candidates.sort_unstable();
        candidates
    }

    /// Splits `indices` into the panes before and after `coordinate`, or `None`
    /// when a rectangle straddles the line or one side is empty.
    fn partition(
        panes: &[PaneGeometry],
        indices: &[usize],
        axis: SplitAxis,
        coordinate: i32,
    ) -> Option<(Vec<usize>, Vec<usize>)> {
        let mut first = Vec::new();
        let mut second = Vec::new();
        for index in indices {
            let rect = panes[*index].bounds;
            let (leading, trailing) = match axis {
                SplitAxis::Vertical => (rect.right, rect.left),
                SplitAxis::Horizontal => (rect.bottom, rect.top),
            };
            if leading <= coordinate {
                first.push(*index);
            } else if trailing >= coordinate {
                second.push(*index);
            } else {
                return None;
            }
        }
        (!first.is_empty() && !second.is_empty()).then_some((first, second))
    }

    /// Whether `rect` is exactly the side of `parent` that `first` selects.
    fn spans(
        axis: SplitAxis,
        rect: ScreenRect,
        parent: ScreenRect,
        first: bool,
        coordinate: i32,
    ) -> bool {
        match (axis, first) {
            (SplitAxis::Vertical, true) => {
                rect.left == parent.left
                    && rect.top == parent.top
                    && rect.bottom == parent.bottom
                    && rect.right <= coordinate
            }
            (SplitAxis::Vertical, false) => {
                rect.right == parent.right
                    && rect.top == parent.top
                    && rect.bottom == parent.bottom
                    && rect.left >= coordinate
            }
            (SplitAxis::Horizontal, true) => {
                rect.top == parent.top
                    && rect.left == parent.left
                    && rect.right == parent.right
                    && rect.bottom <= coordinate
            }
            (SplitAxis::Horizontal, false) => {
                rect.bottom == parent.bottom
                    && rect.left == parent.left
                    && rect.right == parent.right
                    && rect.top >= coordinate
            }
        }
    }

    /// Every slicing tree over `indices` that reproduces the observed rectangles.
    fn consistent_trees(panes: &[PaneGeometry], indices: &[usize]) -> Vec<RefTree> {
        let rect = bounding_rect(panes, indices);
        if indices.len() == 1 {
            return vec![RefTree::Leaf {
                pane: indices[0],
                rect,
            }];
        }

        let mut trees = Vec::new();
        for axis in [SplitAxis::Vertical, SplitAxis::Horizontal] {
            for coordinate in cut_candidates(panes, indices, axis) {
                let Some((first, second)) = partition(panes, indices, axis, coordinate) else {
                    continue;
                };
                let (first_rect, second_rect) =
                    (bounding_rect(panes, &first), bounding_rect(panes, &second));
                if !spans(axis, first_rect, rect, true, coordinate)
                    || !spans(axis, second_rect, rect, false, coordinate)
                {
                    continue;
                }
                for first_tree in consistent_trees(panes, &first) {
                    for second_tree in &consistent_trees(panes, &second) {
                        trees.push(RefTree::Split {
                            axis,
                            rect,
                            first: Box::new(first_tree.clone()),
                            second: Box::new(second_tree.clone()),
                        });
                    }
                }
            }
        }
        trees
    }

    /// Focuses the pane containing `intent.focus_point` and returns, for every
    /// candidate tree, the splitter coordinate Windows Terminal would move.
    fn moved_splitters(layout: &PaneLayout, intent: PaneResizeIntent) -> Vec<Option<i32>> {
        let focus = layout
            .panes()
            .iter()
            .position(|pane| pane.bounds.contains(intent.focus_point))
            .expect("the dispatched focus point must land inside one pane rectangle");
        let indices = (0..layout.panes().len()).collect::<Vec<_>>();
        consistent_trees(layout.panes(), &indices)
            .iter()
            .map(|tree| reference_resize(tree, intent.direction, focus))
            .collect()
    }

    /// Replays the production pointer path: hit-test a divider at `origin`,
    /// start a drag there and move the pointer `pixels` along the divider axis.
    fn drag_intent(layout: &PaneLayout, origin: ScreenPoint, pixels: i32) -> PaneResizeIntent {
        let divider = layout
            .divider_at(origin, 0)
            .expect("fixture must expose a divider at the drag origin");
        let mut drag = PaneDrag::begin(divider, origin);
        let moved = match divider.axis() {
            SplitAxis::Vertical => ScreenPoint::new(origin.x + pixels, origin.y),
            SplitAxis::Horizontal => ScreenPoint::new(origin.x, origin.y + pixels),
        };
        drag.update(moved)
            .expect("a pointer that moved a whole step must dispatch an intent")
    }

    /// Asserts that dragging the divider under `origin` moves exactly that
    /// divider, in both pointer directions and in every tree consistent with
    /// the observed rectangles.
    fn assert_drag_moves_the_hit_divider(layout: &PaneLayout, origin: ScreenPoint) {
        let divider = layout
            .divider_at(origin, 0)
            .expect("fixture must expose a divider at the drag origin");
        let expected_axis = divider.axis();
        let expected = divider.coordinate();
        let step = divider.native_step_pixels() * 3;

        for pixels in [step, -step] {
            let intent = drag_intent(layout, origin, pixels);
            assert_eq!(
                direction_axis(intent.direction),
                expected_axis,
                "the dispatched direction must act on the dragged divider's axis"
            );
            let moved = moved_splitters(layout, intent);
            assert!(
                !moved.is_empty(),
                "the fixture must admit at least one consistent pane tree"
            );
            for coordinate in moved {
                assert_eq!(
                    coordinate,
                    Some(expected),
                    "dragging the divider at {origin:?} (focus {:?}, direction {:?}) moved the \
                     splitter at {coordinate:?} in some tree consistent with the observed \
                     rectangles; only the divider under the pointer may move",
                    intent.focus_point,
                    intent.direction
                );
            }
        }
    }

    /// The pre-fix focus target: the pane on the leading side of the divider,
    /// inset by [`FOCUS_INSET_PX`] from its own leading edge.
    ///
    /// This is exactly what [`PaneDivider`] focused unconditionally before the
    /// edge rule replaced it; the fixtures that use it tile without a gap, so
    /// probing one pixel before the divider line lands inside the leading pane.
    fn previous_leading_focus_point(layout: &PaneLayout, divider: PaneDivider) -> ScreenPoint {
        let span_middle = midpoint(divider.span_start(), divider.span_end());
        let probe = match divider.axis() {
            SplitAxis::Vertical => ScreenPoint::new(divider.coordinate() - 1, span_middle),
            SplitAxis::Horizontal => ScreenPoint::new(span_middle, divider.coordinate() - 1),
        };
        let leading = layout
            .panes()
            .iter()
            .map(|pane| pane.bounds)
            .find(|bounds| bounds.contains(probe))
            .expect("the fixture must tile exactly, with a pane on the leading side");
        match divider.axis() {
            SplitAxis::Vertical => ScreenPoint::new(
                leading.left + FOCUS_INSET_PX.min((leading.width() / 2).max(1)),
                span_middle,
            ),
            SplitAxis::Horizontal => ScreenPoint::new(
                span_middle,
                leading.top + FOCUS_INSET_PX.min((leading.height() / 2).max(1)),
            ),
        }
    }

    /// Dragging the only divider of a two-pane layout moves that divider.
    #[test]
    fn two_pane_drag_moves_the_divider_under_the_pointer() {
        let layout = PaneLayout::from_panes(vec![pane(0, 0, 500, 800), pane(500, 0, 1000, 800)]);

        assert_eq!(layout.dividers().len(), 1);
        assert_drag_moves_the_hit_divider(&layout, ScreenPoint::new(500, 400));
    }

    /// `[A|B]|C` and `[A|[B|C]]` produce identical rectangles, so both nestings
    /// are checked: dragging either divider must move that divider.
    #[test]
    fn nested_row_drag_moves_the_divider_under_the_pointer() {
        let layout = PaneLayout::from_panes(vec![
            pane(0, 0, 300, 500),
            pane(300, 0, 600, 500),
            pane(600, 0, 1000, 500),
        ]);

        assert_eq!(layout.dividers().len(), 2);
        assert_drag_moves_the_hit_divider(&layout, ScreenPoint::new(300, 250));
        assert_drag_moves_the_hit_divider(&layout, ScreenPoint::new(600, 250));
    }

    /// `[A/B]` stacked over `C`: the nested horizontal split inside the top half
    /// plus the two segments of the full-width split below it.
    #[test]
    fn nested_column_drag_moves_the_divider_under_the_pointer() {
        let layout = PaneLayout::from_panes(vec![
            pane(0, 0, 500, 300),
            pane(500, 0, 1000, 300),
            pane(0, 300, 1000, 800),
        ]);

        assert_eq!(layout.dividers().len(), 3);
        assert_drag_moves_the_hit_divider(&layout, ScreenPoint::new(500, 150));
        assert_drag_moves_the_hit_divider(&layout, ScreenPoint::new(250, 300));
        assert_drag_moves_the_hit_divider(&layout, ScreenPoint::new(750, 300));
    }

    /// A 2x2 grid: the centre line is inferred as two segments, and the centre
    /// row as two more. Dragging any of them must move the line under the
    /// pointer, whichever nesting Terminal actually built.
    #[test]
    fn two_by_two_grid_drag_moves_the_divider_under_the_pointer() {
        let layout = PaneLayout::from_panes(vec![
            pane(0, 0, 500, 500),
            pane(500, 0, 1000, 500),
            pane(0, 500, 500, 1000),
            pane(500, 500, 1000, 1000),
        ]);

        assert_eq!(layout.dividers().len(), 4);
        for origin in [
            ScreenPoint::new(500, 250),
            ScreenPoint::new(500, 750),
            ScreenPoint::new(250, 500),
            ScreenPoint::new(750, 500),
        ] {
            assert_drag_moves_the_hit_divider(&layout, origin);
        }
    }

    /// Staggered columns: a full-height middle column between a left column
    /// that is split in two and a full-height right column.
    ///
    /// This is the shape where the pre-fix rule picked the pane on the leading
    /// side of the `middle|right` divider — which is *inside* the left-hand
    /// subtree — so the shipped rule has to use the trailing-edge branch.
    #[test]
    fn staggered_columns_drag_moves_the_divider_under_the_pointer() {
        let layout = PaneLayout::from_panes(vec![
            pane(0, 0, 300, 400),
            pane(300, 0, 700, 1000),
            pane(0, 400, 300, 1000),
            pane(700, 0, 1000, 1000),
        ]);

        assert_eq!(layout.dividers().len(), 4);
        assert_drag_moves_the_hit_divider(&layout, ScreenPoint::new(300, 200));
        assert_drag_moves_the_hit_divider(&layout, ScreenPoint::new(300, 700));
        assert_drag_moves_the_hit_divider(&layout, ScreenPoint::new(700, 500));
        assert_drag_moves_the_hit_divider(&layout, ScreenPoint::new(150, 400));
    }

    /// The P1-2 defect this fix removes, pinned as an executable reference.
    ///
    /// In a three-pane row the leading pane `B` is a legal drag origin for the
    /// `B|C` divider, but focusing it makes Windows Terminal resize `A|B`
    /// whenever the real tree is `[[A|B]|C]`, because `ResizePane` stops at `B`'s
    /// parent instead of at the divider under the pointer. The shipped rule
    /// focuses the pane that touches the layout's trailing edge instead, which
    /// every consistent tree resolves to `B|C`.
    #[test]
    fn focusing_the_leading_pane_can_move_a_different_divider() {
        let layout = PaneLayout::from_panes(vec![
            pane(0, 0, 300, 500),
            pane(300, 0, 600, 500),
            pane(600, 0, 1000, 500),
        ]);
        let origin = ScreenPoint::new(600, 250);
        let divider = layout
            .divider_at(origin, 0)
            .expect("the B|C divider exists");

        let leading = previous_leading_focus_point(&layout, divider);
        assert_eq!(leading, ScreenPoint::new(324, 250));
        let moved = moved_splitters(
            &layout,
            PaneResizeIntent {
                direction: Direction::Right,
                focus_point: leading,
                steps: 1,
            },
        );
        assert!(
            moved.contains(&Some(300)),
            "focusing the leading pane must be able to move the A|B splitter: {moved:?}"
        );
        assert!(
            moved.contains(&Some(600)),
            "the mirrored tree reaches B|C, which is what made the defect \
             layout-dependent: {moved:?}"
        );

        let focus = divider.resize_focus_point().expect("B|C is capturable");
        assert!(layout.panes()[2].bounds.contains(focus), "{focus:?}");
        assert_ne!(focus, leading);
        assert_drag_moves_the_hit_divider(&layout, origin);
    }

    /// A divider whose owning splitter cannot be identified is not captured at
    /// all, and that refusal is justified rather than merely conservative.
    #[test]
    fn a_middle_divider_of_four_panes_is_refused_because_its_owner_is_ambiguous() {
        let layout = PaneLayout::from_panes(vec![
            pane(0, 0, 250, 500),
            pane(250, 0, 500, 500),
            pane(500, 0, 750, 500),
            pane(750, 0, 1000, 500),
        ]);
        assert_eq!(layout.dividers().len(), 3);

        let ambiguous = ScreenPoint::new(500, 250);
        let divider = layout
            .divider_at(ambiguous, 0)
            .expect("the geometric divider is still reported");
        assert!(divider.resize_focus_point().is_none());
        assert!(layout.capturable_divider_at(ambiguous, 0).is_none());

        // `[[A|B]|C]|D]` and `[[A|[B|C]]|D]` are both consistent with these
        // rectangles and move different splitters for the same focus choice, so
        // no focus target can be proven.
        let leading = previous_leading_focus_point(&layout, divider);
        let moved = moved_splitters(
            &layout,
            PaneResizeIntent {
                direction: Direction::Right,
                focus_point: leading,
                steps: 1,
            },
        );
        assert!(moved.contains(&Some(250)), "{moved:?}");
        assert!(moved.contains(&Some(500)), "{moved:?}");

        // The two outer dividers of the row stay capturable.
        assert!(
            layout
                .capturable_divider_at(ScreenPoint::new(250, 250), 0)
                .is_some()
        );
        assert!(
            layout
                .capturable_divider_at(ScreenPoint::new(750, 250), 0)
                .is_some()
        );
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
            Some(ScreenPoint::new(480, 400)),
            0,
        );
        assert_eq!(divider.native_step_pixels(), 1);
        assert_eq!(divider.axis(), SplitAxis::Vertical);
        assert_eq!(
            divider.resize_focus_point(),
            Some(ScreenPoint::new(480, 400))
        );

        let mut drag = PaneDrag::begin(divider, ScreenPoint::new(500, 400));
        let right = drag
            .update(ScreenPoint::new(501, 400))
            .expect("a zero step must not panic nor swallow movement");
        assert_eq!(right.steps, 1);
        assert_eq!(right.direction, Direction::Right);
        assert_eq!(right.focus_point, ScreenPoint::new(480, 400));
        let left = drag
            .update(ScreenPoint::new(500, 400))
            .expect("reverse movement must dispatch against the clamped step");
        assert_eq!(left.steps, 1);
        assert_eq!(left.direction, Direction::Left);
    }

    #[test]
    fn a_divider_without_a_provable_focus_target_never_dispatches() {
        let divider = PaneDivider::new(SplitAxis::Vertical, 500, 0, 800, 497, 503, None, 0);
        assert!(divider.resize_focus_point().is_none());

        let mut drag = PaneDrag::begin(divider, ScreenPoint::new(500, 400));
        assert!(drag.update(ScreenPoint::new(600, 400)).is_none());
        assert!(drag.update(ScreenPoint::new(500, 400)).is_none());
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

    #[test]
    fn pane_titles_survive_layout_construction() {
        let mut titled = pane(0, 0, 497, 800);
        titled.title = "build: main".to_owned();
        let layout = PaneLayout::from_panes(vec![titled, pane(503, 0, 1000, 800)]);

        assert_eq!(layout.panes().len(), 2);
        assert_eq!(layout.panes()[0].title, "build: main");
        assert_eq!(layout.panes()[1].title, "");
    }
}
