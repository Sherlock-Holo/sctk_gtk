use std::num::NonZeroU32;
use std::sync::{Arc, Once};
use std::time::Duration;
use std::{array, mem};

use cairo::{Context, Format, ImageSurface};
use gtk::prelude::{
    ContainerExt, GtkWindowExt, HeaderBarExt, ImageExt, StyleContextExt, WidgetExt,
};
use gtk::{Align, Button, HeaderBar, IconSize, Image, OffscreenWindow, StateFlags};
use smithay_client_toolkit::compositor::SurfaceData;
use smithay_client_toolkit::reexports::client::backend::ObjectId;
use smithay_client_toolkit::reexports::client::protocol::wl_shm;
use smithay_client_toolkit::reexports::client::protocol::wl_subsurface::WlSubsurface;
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::{Dispatch, Proxy, QueueHandle};
use smithay_client_toolkit::reexports::csd_frame::{
    CursorIcon, DecorationsFrame, FrameAction, FrameClick, WindowManagerCapabilities, WindowState,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::Shm;
use smithay_client_toolkit::subcompositor::{SubcompositorState, SubsurfaceData};
use tiny_skia::{Color, PixmapMut, Rect, Transform};

use crate::layout::get_button_layout;
use crate::pointer::{ButtonKind, Location, MouseState};
use crate::shadow::{Shadow, ShadowPart, ShadowSurface, Theme as ShadowTheme};

mod layout;
mod pointer;
mod shadow;

const HEADER_SIZE: u32 = 50;
const BORDER_SIZE: u32 = 10;
const VISIBLE_BORDER_SIZE: u32 = 1;

static GTK_INIT_ONCE: Once = Once::new();

#[derive(Debug)]
struct ButtonState {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    button_kind: ButtonKind,
}

#[derive(Debug)]
pub struct GtkFrame {
    /// The drawable decorations, `None` when hidden.
    hidden: bool,

    /// Memory pool to allocate the buffers for the decorations.
    pool: SlotPool,

    /// Whether the frame should be redrawn.
    dirty: bool,

    /// Whether the drawing should be synced with the main surface.
    should_sync: bool,

    /// Scale factor used for the surface.
    scale_factor: u32,

    /// Whether the frame is resizable.
    resizable: bool,
    width: Option<NonZeroU32>,
    height: Option<NonZeroU32>,

    buttons_at_end: bool,
    buttons: Vec<ButtonState>,

    state: WindowState,
    wm_capabilities: WindowManagerCapabilities,
    mouse: MouseState,
    title: String,

    header_bar_surface: WlSurface,
    header_bar_subsurface: WlSubsurface,

    shadow: Shadow,
    shadow_surfaces: [ShadowSurface; 4],
    shadow_theme: ShadowTheme,
}

impl DecorationsFrame for GtkFrame {
    fn on_click(
        &mut self,
        timestamp: Duration,
        click: FrameClick,
        pressed: bool,
    ) -> Option<FrameAction> {
        let action = match click {
            FrameClick::Normal => self.mouse.click(
                timestamp,
                pressed,
                self.resizable,
                &self.state,
                &self.wm_capabilities,
            ),
            FrameClick::Alternate => self.mouse.alternate_click(pressed, &self.wm_capabilities),
            _ => None,
        };

        self.update_dirty_by_button_cursor_pos();

        action
    }

    fn click_point_moved(
        &mut self,
        _timestamp: Duration,
        surface_id: &ObjectId,
        x: f64,
        y: f64,
    ) -> Option<CursorIcon> {
        let (width, height) = match (self.width, self.height) {
            (Some(width), Some(height)) => (width.get(), height.get()),
            _ => return Some(CursorIcon::Default),
        };

        let cursor_in_frame = self.get_cursor_area(surface_id);
        let mouse_location = self.mouse_location(x, y, width, height, cursor_in_frame);

        let cursor_icon = self
            .mouse
            .moved(mouse_location, x, y, self.resizable, self.state);

        self.update_dirty_by_button_cursor_pos();

        if mouse_location == Location::None {
            return None;
        }

        Some(cursor_icon)
    }

    fn click_point_left(&mut self) {
        self.mouse.left()
    }

    fn update_state(&mut self, state: WindowState) {
        let difference = self.state.symmetric_difference(state);
        self.state = state;
        self.dirty |= difference.intersects(
            WindowState::ACTIVATED
                | WindowState::FULLSCREEN
                | WindowState::MAXIMIZED
                | WindowState::TILED,
        );
    }

    fn update_wm_capabilities(&mut self, wm_capabilities: WindowManagerCapabilities) {
        self.dirty |= self.wm_capabilities != wm_capabilities;
        self.wm_capabilities = wm_capabilities;
    }

    fn resize(&mut self, width: NonZeroU32, height: NonZeroU32) {
        self.width = Some(width);
        self.height = Some(height);

        let width = width.get();
        let height = height.get();

        // top
        let shadow_surface = &mut self.shadow_surfaces[ShadowPart::Top.index()];
        shadow_surface.width = width + 2 * BORDER_SIZE;

        // bottom
        let shadow_surface = &mut self.shadow_surfaces[ShadowPart::Bottom.index()];
        shadow_surface.width = width + 2 * BORDER_SIZE;
        shadow_surface.y = height as _;

        // left
        let shadow_surface = &mut self.shadow_surfaces[ShadowPart::Left.index()];
        shadow_surface.height = height + HEADER_SIZE;

        // right
        let shadow_surface = &mut self.shadow_surfaces[ShadowPart::Right.index()];
        shadow_surface.height = height + HEADER_SIZE;
        shadow_surface.x = width as _;
    }

    fn set_scaling_factor(&mut self, scale_factor: f64) {
        // NOTE: Clamp it just in case to some ok-ish range.
        self.scale_factor = scale_factor.clamp(0.1, 64.).ceil() as u32;
        self.dirty = true;
        self.should_sync = true;
    }

    fn location(&self) -> (i32, i32) {
        if self.hidden || self.state.contains(WindowState::FULLSCREEN) {
            (0, 0)
        } else {
            (0, -(HEADER_SIZE as i32))
        }
    }

    fn subtract_borders(
        &self,
        width: NonZeroU32,
        height: NonZeroU32,
    ) -> (Option<NonZeroU32>, Option<NonZeroU32>) {
        if self.hidden || self.state.contains(WindowState::FULLSCREEN) {
            (Some(width), Some(height))
        } else {
            (
                Some(width),
                NonZeroU32::new(height.get().saturating_sub(HEADER_SIZE)),
            )
        }
    }

    fn add_borders(&self, width: u32, height: u32) -> (u32, u32) {
        if self.hidden || self.state.contains(WindowState::FULLSCREEN) {
            (width, height)
        } else {
            (width, height + HEADER_SIZE)
        }
    }

    fn is_dirty(&self) -> bool {
        self.dirty
    }

    fn set_hidden(&mut self, hidden: bool) {
        self.hidden = hidden;
        if hidden {
            self.dirty = false;
            let _ = self.pool.resize(1);
        } else {
            self.dirty = true;
            self.should_sync = true;
        }
    }

    fn is_hidden(&self) -> bool {
        self.hidden
    }

    fn set_resizable(&mut self, resizable: bool) {
        self.resizable = resizable;
    }

    fn draw(&mut self) -> bool {
        let should_sync = self.draw_head_bar().unwrap_or(false);
        let _ = self.draw_shadow(should_sync);

        should_sync
    }

    fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
        self.dirty = true;
    }
}

impl GtkFrame {
    pub fn new<State>(
        base_surface: &impl WaylandSurface,
        shm: &Shm,
        sub_compositor: Arc<SubcompositorState>,
        queue_handle: QueueHandle<State>,
    ) -> anyhow::Result<Self>
    where
        State: Dispatch<WlSurface, SurfaceData> + Dispatch<WlSubsurface, SubsurfaceData> + 'static,
    {
        GTK_INIT_ONCE
            .call_once(|| gtk::init().unwrap_or_else(|err| panic!("gtk init failed: {err}")));

        let (buttons_at_end, buttons) = get_button_layout();
        let buttons = buttons
            .into_iter()
            .map(|kind| ButtonState {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
                button_kind: kind,
            })
            .collect();

        let (subsurface, surface) =
            sub_compositor.create_subsurface(base_surface.wl_surface().clone(), &queue_handle);

        subsurface.set_sync();

        let pool = SlotPool::new(1, shm)?;

        let mut shadow_surfaces = array::from_fn(|_| {
            let (subsurface, surface) =
                sub_compositor.create_subsurface(base_surface.wl_surface().clone(), &queue_handle);

            ShadowSurface {
                surface,
                subsurface,
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            }
        });
        init_shadow_surfaces_pos(&mut shadow_surfaces);

        Ok(Self {
            hidden: false,
            pool,
            dirty: true,
            should_sync: true,
            scale_factor: 1,
            resizable: true,
            width: None,
            height: None,
            buttons_at_end,
            buttons,
            state: WindowState::empty(),
            wm_capabilities: WindowManagerCapabilities::all(),
            // mouse: (),
            mouse: Default::default(),
            title: String::new(),
            header_bar_surface: surface,
            header_bar_subsurface: subsurface,
            shadow: Default::default(),
            shadow_surfaces,
            shadow_theme: ShadowTheme::auto(),
        })
    }

    fn get_cursor_area(&mut self, surface_id: &ObjectId) -> CursorArea {
        if self.header_bar_surface.id() == *surface_id {
            return CursorArea::Frame;
        } else {
            match self
                .shadow_surfaces
                .iter()
                .enumerate()
                .find_map(|(index, shadow_surface)| {
                    (shadow_surface.surface.id() == *surface_id).then_some(index)
                }) {
                None => CursorArea::Window,
                Some(index) => {
                    if index == ShadowPart::Top.index() {
                        CursorArea::TopShadow
                    } else if index == ShadowPart::Left.index() {
                        CursorArea::LeftShadow
                    } else if index == ShadowPart::Right.index() {
                        CursorArea::RightShadow
                    } else if index == ShadowPart::Bottom.index() {
                        CursorArea::BottomShadow
                    } else {
                        unreachable!()
                    }
                }
            }
        }
    }

    fn update_dirty_by_button_cursor_pos(&mut self) {
        if !self.mouse.in_frame() {
            return;
        }

        if let Some(cursor_pos) = self.mouse.cursor_pos {
            for state in &self.buttons {
                if Self::in_button(cursor_pos, state) {
                    self.dirty = true;
                    return;
                }
            }
        }
    }

    fn mouse_location(
        &self,
        x: f64,
        y: f64,
        width: u32,
        height: u32,
        cursor_area: CursorArea,
    ) -> Location {
        match cursor_area {
            CursorArea::Frame => {
                if x <= 5.0 && y <= 5.0 {
                    Location::TopLeft
                } else if x >= (width - 5) as _ && y <= 5.0 {
                    Location::TopRight
                } else if x > 5.0 && x < (width - 5) as _ && y < 5.0 {
                    Location::Top
                } else {
                    for state in &self.buttons {
                        if Self::in_button((x, y), state) {
                            return Location::Button(state.button_kind);
                        }
                    }

                    Location::Head
                }
            }

            CursorArea::TopShadow => {
                if x <= 5.0 {
                    Location::TopLeft
                } else if x >= (width - 5) as _ {
                    Location::TopRight
                } else {
                    Location::Top
                }
            }

            CursorArea::BottomShadow => {
                if x <= 5.0 {
                    Location::BottomLeft
                } else if x >= (width - 5) as _ {
                    Location::BottomRight
                } else {
                    Location::Bottom
                }
            }

            CursorArea::LeftShadow => {
                if y <= 5.0 {
                    Location::TopLeft
                } else if y >= (height - 5) as _ {
                    Location::BottomLeft
                } else {
                    Location::Left
                }
            }

            CursorArea::RightShadow => {
                if y <= 5.0 {
                    Location::TopRight
                } else if y >= (height - 5) as _ {
                    Location::BottomRight
                } else {
                    Location::Right
                }
            }

            CursorArea::Window => {
                Location::None
                /*if x < 5.0 && y > 5.0 && y < (height - 5) as _ {
                    Location::Left
                } else if x > (width - 5) as _ && y > 5.0 && y < (height - 5) as _ {
                    Location::Right
                } else if x <= 5.0 && y >= (height - 5) as _ {
                    Location::BottomLeft
                } else if x >= (width - 5) as _ && y >= (height - 5) as _ {
                    Location::BottomRight
                } else if x > 5.0 && x < (width - 5) as _ && y > (width - 5) as _ {
                    Location::Bottom
                } else {
                    Location::None
                }*/
            }
        }

        /*if x <= 5.0 && y <= 5.0 {
            Location::TopLeft
        } else if x >= (width - 5) as _ && y <= 5.0 {
            Location::TopRight
        } else if x < 5.0 && y > 5.0 && y < (height - 5) as _ {
            Location::Left
        } else if x > (width - 5) as _ && y > 5.0 && y < (height - 5) as _ {
            Location::Right
        } else if x <= 5.0 && y >= (height - 5) as _ {
            Location::BottomLeft
        } else if x >= (width - 5) as _ && y >= (height - 5) as _ {
            Location::BottomRight
        } else if x > 5.0 && x < (width - 5) as _ && y < 5.0 {
            if !cursor_in_frame {
                return Location::None;
            }

            Location::Top
        } else if x > 5.0 && x < (width - 5) as _ && y > (width - 5) as _ {
            Location::Bottom
        } else if cursor_in_frame {
            for state in &self.buttons {
                if Self::in_button((x, y), state) {
                    return Location::Button(state.button_kind);
                }
            }

            if x > 5.0 && x < (width - 5) as _ && y > 5.0 {
                Location::Head
            } else {
                Location::None
            }
        } else {
            Location::None
        }*/
    }

    fn draw_head_bar(&mut self) -> anyhow::Result<bool> {
        // Reset the dirty bit.
        self.dirty = false;
        let should_sync = mem::take(&mut self.should_sync);

        // Don't draw borders if the frame explicitly hidden or fullscreened.
        if self.state.contains(WindowState::FULLSCREEN) {
            return Ok(true);
        }

        let width = match self.width {
            None => return Ok(false),
            Some(width) => width,
        };

        let width = width.get() * self.scale_factor;
        let height = HEADER_SIZE * self.scale_factor;
        let (buffer, canvas) = self.pool.create_buffer(
            width as _,
            height as _,
            (width * 4) as _,
            wl_shm::Format::Argb8888,
        )?;

        let image_surface = unsafe {
            ImageSurface::create_for_data_unsafe(
                canvas.as_mut_ptr() as _,
                Format::ARgb32,
                width as _,
                height as _,
                (width * 4) as _,
            )?
        };
        let cairo_context = Context::new(image_surface)?;
        let header_bar = HeaderBar::builder().title(&self.title).build();

        let buttons = self
            .buttons
            .iter()
            .map(|button_state| {
                let button = match button_state.button_kind {
                    ButtonKind::Close => self.create_close_button(),
                    ButtonKind::Maximize => self.create_max_button(),
                    ButtonKind::Minimize => self.create_min_button(),
                };

                button.show_all();

                if self.buttons_at_end {
                    header_bar.pack_end(&button);
                } else {
                    header_bar.pack_start(&button);
                }

                button
            })
            .collect::<Vec<_>>();

        let offscreen_window = OffscreenWindow::new();
        offscreen_window.set_default_size(width as _, height as _);
        offscreen_window.add(&header_bar);
        offscreen_window.show_all();

        for (button, state) in buttons.into_iter().zip(&mut self.buttons) {
            let allocation = button.allocation();

            state.x = allocation.x();
            state.y = allocation.y();
            state.width = allocation.width() as _;
            state.height = allocation.height() as _;

            Self::apply_button_state(&self.mouse, &button, state);
        }

        // make sure gtk can draw cairo context
        while gtk::events_pending() {
            gtk::main_iteration();
        }

        offscreen_window.draw(&cairo_context);

        if should_sync {
            self.header_bar_subsurface.set_sync();
        } else {
            self.header_bar_subsurface.set_desync();
        }

        self.header_bar_surface
            .set_buffer_scale(self.scale_factor as _);
        self.header_bar_subsurface
            .set_position(0, -(HEADER_SIZE as i32));
        buffer.attach_to(&self.header_bar_surface)?;

        if self.header_bar_surface.version() >= 4 {
            self.header_bar_surface
                .damage_buffer(0, 0, i32::MAX, i32::MAX);
        } else {
            self.header_bar_surface.damage(0, 0, i32::MAX, i32::MAX);
        }

        self.header_bar_surface.commit();

        Ok(should_sync)
    }

    fn draw_shadow(&mut self, should_sync: bool) -> anyhow::Result<()> {
        let border_paint = self.shadow_theme.border_paint();

        for (shadow_part, shadow_surface) in [
            ShadowPart::Top,
            ShadowPart::Left,
            ShadowPart::Right,
            ShadowPart::Bottom,
        ]
        .iter()
        .zip(&mut self.shadow_surfaces)
        {
            let width = shadow_surface.width * self.scale_factor;
            let height = shadow_surface.height * self.scale_factor;

            let (buffer, canvas) = self.pool.create_buffer(
                width as _,
                height as _,
                (width * 4) as _,
                wl_shm::Format::Argb8888,
            )?;

            // Create the pixmap and fill with transparent color.
            let mut pixmap = PixmapMut::from_bytes(canvas, width, height)
                .expect("create pixmap should always success");

            // Fill everything with transparent background, since we draw rounded corners and
            // do invisible borders to enlarge the input zone.
            pixmap.fill(Color::TRANSPARENT);

            if !self.state.intersects(WindowState::TILED) {
                self.shadow.draw(
                    &mut pixmap,
                    self.scale_factor,
                    self.state.contains(WindowState::ACTIVATED),
                    *shadow_part,
                );
            }

            // The visible border is one pt.
            let visible_border_size = VISIBLE_BORDER_SIZE * self.scale_factor;

            // XXX we do all the match using integral types and then convert to f32 in the
            // end to ensure that result is finite.
            let border_rect = match shadow_part {
                ShadowPart::Left => {
                    let x =
                        (shadow_surface.x.unsigned_abs() * self.scale_factor) - visible_border_size;
                    let y = shadow_surface.y.unsigned_abs() * self.scale_factor;
                    Rect::from_xywh(
                        x as f32,
                        y as f32,
                        visible_border_size as f32,
                        (shadow_surface.height - y) as f32,
                    )
                }

                ShadowPart::Right => {
                    let y = shadow_surface.y.unsigned_abs() * self.scale_factor;
                    Rect::from_xywh(
                        0.,
                        y as f32,
                        visible_border_size as f32,
                        (shadow_surface.height - y) as f32,
                    )
                }
                // We draw small visible border only bellow the window surface, no need to
                // handle `TOP`.
                ShadowPart::Bottom => {
                    let x =
                        (shadow_surface.x.unsigned_abs() * self.scale_factor) - visible_border_size;
                    Rect::from_xywh(
                        x as f32,
                        0.,
                        (shadow_surface.width - 2 * x) as f32,
                        visible_border_size as f32,
                    )
                }
                _ => None,
            };

            // Fill the visible border, if present.
            if let Some(border_rect) = border_rect {
                pixmap.fill_rect(border_rect, &border_paint, Transform::identity(), None);
            }

            if should_sync {
                shadow_surface.subsurface.set_sync();
            } else {
                shadow_surface.subsurface.set_desync();
            }

            shadow_surface
                .surface
                .set_buffer_scale(self.scale_factor as _);

            shadow_surface
                .subsurface
                .set_position(shadow_surface.x, shadow_surface.y);
            buffer.attach_to(&shadow_surface.surface)?;

            if shadow_surface.surface.version() >= 4 {
                shadow_surface
                    .surface
                    .damage_buffer(0, 0, i32::MAX, i32::MAX);
            } else {
                shadow_surface.surface.damage(0, 0, i32::MAX, i32::MAX);
            }

            shadow_surface.surface.commit();
        }

        Ok(())
    }

    fn apply_button_state(mouse: &MouseState, button: &Button, state: &ButtonState) {
        let style_context = button.style_context();
        let mut state_flags = style_context.state();

        if let Some(cursor_pos) = mouse.cursor_pos {
            if Self::in_button(cursor_pos, state) {
                state_flags |= StateFlags::PRELIGHT;
            }
        }

        // match state.cursor_state {
        //     ButtonCursorState::Hovered => {
        //         state_flags |= StateFlags::PRELIGHT;
        //     }
        //     ButtonCursorState::Clicked => {
        //         state_flags |= StateFlags::PRELIGHT | StateFlags::ACTIVE;
        //     }

        //     _ => {}
        // }

        style_context.set_state(state_flags);
    }

    fn in_button(cursor_pos: (f64, f64), state: &ButtonState) -> bool {
        cursor_pos.0 >= state.x as _
            && cursor_pos.0 <= (state.x + state.width as i32) as _
            && cursor_pos.1 >= state.y as _
            && cursor_pos.1 <= (state.y + state.height as i32) as _
    }

    fn create_min_button(&self) -> Button {
        let button = Button::new();
        button.set_valign(Align::Center);
        let style_context = button.style_context();
        style_context.add_class("titlebutton");
        style_context.add_class("minimize");
        let image = Image::from_icon_name(Some("window-minimize-symbolic"), IconSize::Menu);
        image.set_use_fallback(true);
        button.add(&image);
        button.set_can_focus(false);

        button
    }

    fn create_max_button(&self) -> Button {
        let button = Button::new();
        button.set_valign(Align::Center);
        let style_context = button.style_context();
        style_context.add_class("titlebutton");
        style_context.add_class("maximize");
        let image = Image::from_icon_name(Some("window-maximize-symbolic"), IconSize::Menu);
        image.set_use_fallback(true);
        button.add(&image);
        button.set_can_focus(false);

        button
    }

    fn create_close_button(&self) -> Button {
        let button = Button::new();
        button.set_valign(Align::Center);
        let style_context = button.style_context();
        style_context.add_class("titlebutton");
        style_context.add_class("close");
        let image = Image::from_icon_name(Some("window-close-symbolic"), IconSize::Menu);
        image.set_use_fallback(true);
        button.add(&image);
        button.set_can_focus(false);

        button
    }
}

fn init_shadow_surfaces_pos(shadow_surfaces: &mut [ShadowSurface; 4]) {
    // top
    let surface = &mut shadow_surfaces[0];
    surface.x = -(BORDER_SIZE as i32);
    surface.y = -(HEADER_SIZE as i32 + BORDER_SIZE as i32);
    surface.height = BORDER_SIZE;

    // left
    let surface = &mut shadow_surfaces[1];
    surface.x = -(BORDER_SIZE as i32);
    surface.y = -(HEADER_SIZE as i32);
    surface.width = BORDER_SIZE;

    // right
    let surface = &mut shadow_surfaces[2];
    surface.y = -(HEADER_SIZE as i32);
    surface.width = BORDER_SIZE;

    // bottom
    let surface = &mut shadow_surfaces[3];
    surface.x = -(BORDER_SIZE as i32);
    surface.height = BORDER_SIZE;
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum CursorArea {
    Frame,
    TopShadow,
    BottomShadow,
    LeftShadow,
    RightShadow,
    Window,
}
