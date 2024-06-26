use std::time::Duration;

use smithay_client_toolkit::reexports::csd_frame::{
    CursorIcon, FrameAction, ResizeEdge, WindowManagerCapabilities, WindowState,
};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ButtonKind {
    Close,
    Maximize,
    Minimize,
}

/// Time to register the next click as a double click.
///
/// The value is the same as the default in gtk4.
const DOUBLE_CLICK_DURATION: Duration = Duration::from_millis(400);

/// The state of the mouse input inside the decorations frame.
#[derive(Debug, Default)]
pub(crate) struct MouseState {
    pub location: Location,

    /// The surface local location inside the surface.
    pub cursor_pos: Option<(f64, f64)>,

    pub button_pressed: bool,

    /// The instant of the last click.
    last_normal_click: Option<Duration>,
}

impl MouseState {
    /// The normal click on decorations frame was made.
    pub fn click(
        &mut self,
        timestamp: Duration,
        pressed: bool,
        resizable: bool,
        state: &WindowState,
        wm_capabilities: &WindowManagerCapabilities,
    ) -> Option<FrameAction> {
        let maximized = state.contains(WindowState::MAXIMIZED);

        let action = match self.location {
            Location::Top if resizable => FrameAction::Resize(ResizeEdge::Top),
            Location::TopLeft if resizable => FrameAction::Resize(ResizeEdge::TopLeft),
            Location::Left if resizable => FrameAction::Resize(ResizeEdge::Left),
            Location::BottomLeft if resizable => FrameAction::Resize(ResizeEdge::BottomLeft),
            Location::Bottom if resizable => FrameAction::Resize(ResizeEdge::Bottom),
            Location::BottomRight if resizable => FrameAction::Resize(ResizeEdge::BottomRight),
            Location::Right if resizable => FrameAction::Resize(ResizeEdge::Right),
            Location::TopRight if resizable => FrameAction::Resize(ResizeEdge::TopRight),

            Location::Button(button_kind) => {
                self.button_pressed = pressed;

                match button_kind {
                    ButtonKind::Close => {
                        if !pressed {
                            FrameAction::Close
                        } else {
                            return None;
                        }
                    }
                    ButtonKind::Maximize => {
                        if !pressed {
                            if maximized {
                                FrameAction::UnMaximize
                            } else {
                                FrameAction::Maximize
                            }
                        } else {
                            return None;
                        }
                    }
                    ButtonKind::Minimize => {
                        if !pressed {
                            FrameAction::Minimize
                        } else {
                            return None;
                        }
                    }
                }
            }

            Location::Head
                if pressed && wm_capabilities.contains(WindowManagerCapabilities::MAXIMIZE) =>
            {
                match self.last_normal_click.replace(timestamp) {
                    Some(last) if timestamp.saturating_sub(last) < DOUBLE_CLICK_DURATION => {
                        if maximized {
                            FrameAction::UnMaximize
                        } else {
                            FrameAction::Maximize
                        }
                    }
                    _ => FrameAction::Move,
                }
            }

            Location::Head if pressed => FrameAction::Move,

            _ => return None,
        };

        Some(action)
    }

    /// Alternative click on decorations frame was made.
    pub fn alternate_click(
        &mut self,
        pressed: bool,
        wm_capabilities: &WindowManagerCapabilities,
    ) -> Option<FrameAction> {
        // Invalidate the normal click.
        self.last_normal_click = None;

        match self.location {
            Location::Head | Location::Button(_)
                if pressed && wm_capabilities.contains(WindowManagerCapabilities::WINDOW_MENU) =>
            {
                self.cursor_pos.map(|pos| {
                    FrameAction::ShowMenu(
                        // XXX this could be one 1pt off when the frame is not maximized, but it's not
                        // like it really matters in the end.
                        pos.0 as _,
                        // We must offset it by header size for precise position.
                        pos.1 as _,
                    )
                })
            }

            _ => None,
        }
    }

    /// The mouse moved inside the decorations frame.
    pub fn moved(
        &mut self,
        location: Location,
        x: f64,
        y: f64,
        resizable: bool,
        window_state: WindowState,
    ) -> CursorIcon {
        self.location = location;
        self.cursor_pos = Some((x, y));

        if !resizable || window_state.intersects(WindowState::MAXIMIZED) {
            return CursorIcon::Default;
        }

        match self.location {
            Location::Top => CursorIcon::NResize,
            Location::TopRight => CursorIcon::NeResize,
            Location::Right => CursorIcon::EResize,
            Location::BottomRight => CursorIcon::SeResize,
            Location::Bottom => CursorIcon::SResize,
            Location::BottomLeft => CursorIcon::SwResize,
            Location::Left => CursorIcon::WResize,
            Location::TopLeft => CursorIcon::NwResize,
            _ => CursorIcon::Default,
        }
    }

    /// The mouse left the decorations frame.
    pub fn left(&mut self) {
        // Reset only the location.
        self.location = Location::None;
    }

    pub fn in_frame(&self) -> bool {
        matches!(
            self.location,
            Location::Head
                | Location::TopLeft
                | Location::TopRight
                | Location::Top
                | Location::Button(_)
        )
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub enum Location {
    #[default]
    None,
    Head,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
    TopLeft,
    Button(ButtonKind),
}
