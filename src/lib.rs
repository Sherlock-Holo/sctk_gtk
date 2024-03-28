use std::mem;
use std::num::NonZeroU32;
use std::sync::{Arc, Once};
use std::time::Duration;

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

use crate::pointer::{ButtonKind, Location, MouseState};

mod pointer;

const HEADER_SIZE: u32 = 50;
static GTK_INIT_ONCE: Once = Once::new();

#[derive(Debug, Default)]
struct ButtonState {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[derive(Debug)]
pub struct GtkFrame<State> {
    /// The base surface used to create the window.
    // base_surface: WlTyped<WlSurface, SurfaceData>,

    // compositor: Arc<CompositorState>,

    /// Subcompositor to create/drop subsurfaces ondemand.
    // subcompositor: Arc<SubcompositorState>,

    /// Queue handle to perform object creation.
    queue_handle: QueueHandle<State>,

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

    // cursor_pos: Option<(f64, f64)>,
    allow_min_button: bool,
    min_button_state: Option<ButtonState>,

    allow_max_button: bool,
    max_button_state: Option<ButtonState>,

    allow_close_button: bool,
    close_button_state: Option<ButtonState>,

    state: WindowState,
    wm_capabilities: WindowManagerCapabilities,
    mouse: MouseState,
    title: String,

    header_bar_surface: WlSurface,
    header_bar_subsurface: WlSubsurface,
}

impl<State> DecorationsFrame for GtkFrame<State>
where
    State: Dispatch<WlSurface, SurfaceData> + Dispatch<WlSubsurface, SubsurfaceData> + 'static,
{
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
        println!("cursor position ({x}:{y})");

        let (width, height) = match (self.width, self.height) {
            (Some(width), Some(height)) => (width.get(), height.get()),
            _ => return Some(CursorIcon::Default),
        };

        let cursor_in_frame = self.header_bar_surface.id() == *surface_id;
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
        self.draw_head_bar().unwrap_or(false)
    }

    fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
        self.dirty = true;

        println!("set title {}", self.title);
    }
}

impl<State> GtkFrame<State>
where
    State: 'static + Dispatch<WlSubsurface, SubsurfaceData> + Dispatch<WlSurface, SurfaceData>,
{
    fn update_dirty_by_button_cursor_pos(&mut self) {
        if !self.mouse.in_frame() {
            return;
        }

        if let (Some(state), Some(cursor_pos)) = (&self.min_button_state, self.mouse.cursor_pos) {
            if Self::in_button(cursor_pos, state) {
                self.dirty = true;
            }
        }

        if let (Some(state), Some(cursor_pos)) = (&self.close_button_state, self.mouse.cursor_pos) {
            if Self::in_button(cursor_pos, state) {
                self.dirty = true;
            }
        }
    }
}

impl<State> GtkFrame<State>
where
    State: Dispatch<WlSurface, SurfaceData> + Dispatch<WlSubsurface, SubsurfaceData> + 'static,
{
    pub fn new(
        base_surface: &impl WaylandSurface,
        shm: &Shm,
        subcompositor: Arc<SubcompositorState>,
        queue_handle: QueueHandle<State>,
    ) -> anyhow::Result<Self> {
        GTK_INIT_ONCE
            .call_once(|| gtk::init().unwrap_or_else(|err| panic!("gtk init failed: {err}")));

        let (subsurface, surface) =
            subcompositor.create_subsurface(base_surface.wl_surface().clone(), &queue_handle);

        subsurface.set_sync();

        let pool = SlotPool::new(1, shm)?;

        Ok(Self {
            // base_surface: (),
            // compositor: (),
            // subcompositor: (),
            queue_handle,
            hidden: false,
            pool,
            dirty: true,
            should_sync: true,
            scale_factor: 1,
            resizable: true,
            width: None,
            height: None,
            allow_min_button: true,
            min_button_state: None,
            allow_max_button: true,
            max_button_state: None,
            allow_close_button: true,
            close_button_state: None,
            state: WindowState::empty(),
            wm_capabilities: WindowManagerCapabilities::all(),
            // mouse: (),
            mouse: Default::default(),
            title: String::new(),
            header_bar_surface: surface,
            header_bar_subsurface: subsurface,
        })
    }
}

impl<State> GtkFrame<State> {
    fn mouse_location(
        &self,
        x: f64,
        y: f64,
        width: u32,
        height: u32,
        cursor_in_frame: bool,
    ) -> Location {
        if x <= 5.0 && y <= 5.0 {
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
            if let Some(state) = &self.min_button_state {
                if Self::in_button((x, y), state) {
                    return Location::Button(ButtonKind::Minimize);
                }
            }

            if let Some(state) = &self.close_button_state {
                if Self::in_button((x, y), state) {
                    return Location::Button(ButtonKind::Close);
                }
            }

            if let Some(state) = &self.max_button_state {
                if Self::in_button((x, y), state) {
                    return Location::Button(ButtonKind::Maximize);
                }
            }

            if x > 5.0 && x < (width - 5) as _ && y > 5.0 {
                Location::Head
            } else {
                Location::None
            }
        } else {
            Location::None
        }
    }

    fn draw_head_bar(&mut self) -> anyhow::Result<bool> {
        println!("draw head bar");

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

        let width = width.get();
        let height = HEADER_SIZE;
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

        let close_button = if self.allow_close_button {
            let button = self.create_close_button();
            self.close_button_state.get_or_insert_with(Default::default);

            button.show_all();
            header_bar.pack_end(&button);

            Some(button)
        } else {
            None
        };

        let min_button = if self.allow_min_button
            && self
                .wm_capabilities
                .intersects(WindowManagerCapabilities::MINIMIZE)
        {
            let button = self.create_min_button();
            self.min_button_state.get_or_insert_with(Default::default);

            button.show_all();
            header_bar.pack_end(&button);

            Some(button)
        } else {
            None
        };

        let offscreen_window = OffscreenWindow::new();
        offscreen_window.set_default_size(width as _, height as _);
        offscreen_window.add(&header_bar);
        offscreen_window.show_all();

        if let Some(button) = close_button {
            let allocation = button.allocation();

            if let Some(state) = self.close_button_state.as_mut() {
                state.x = allocation.x();
                state.y = allocation.y();
                state.width = allocation.width() as _;
                state.height = allocation.height() as _;

                Self::apply_button_state(&self.mouse, &button, state);
            }
        }

        if let Some(button) = min_button {
            let allocation = button.allocation();

            if let Some(state) = self.min_button_state.as_mut() {
                state.x = allocation.x();
                state.y = allocation.y();
                state.width = allocation.width() as _;
                state.height = allocation.height() as _;

                Self::apply_button_state(&self.mouse, &button, state);
            }
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

    fn apply_button_state(mouse: &MouseState, button: &Button, state: &ButtonState) {
        let style_context = button.style_context();
        let mut state_flags = style_context.state();

        if let Some(cursor_pos) = mouse.cursor_pos {
            if Self::in_button(cursor_pos, state) {
                println!("in button");

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
