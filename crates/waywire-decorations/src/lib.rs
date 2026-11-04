//! Cached software decorations, the taskbar and the window menu, drawn with
//! Cairo and Pango. Layout is shared by painting and hit testing.
use anyhow::Result;
use pangocairo::{cairo, pango};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::{ImportAll, ImportMem};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::desktop::Window;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::{Buffer, Logical, Point, Rectangle, Size, Transform};
use waywire_protocol::pipe::CursorShape;

#[derive(Clone, Copy, Debug)]
pub enum WindowAction {
    Focus,
    Move,
    Resize(xdg_toplevel::ResizeEdge),
    Minimize,
    Maximize,
    Restore,
    Close,
    ShowMenu,
    ToggleAlwaysOnTop,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskbarChip {
    pub title: String,
    pub target: ChipTarget,
}

/// What a taskbar chip stands for: a pin no window matched, or a window,
/// which a pin may have matched.
#[derive(Clone, Debug, PartialEq)]
pub enum ChipTarget {
    Pin {
        index: usize,
        process: PinProcess,
    },
    Window {
        window: Window,
        pin: Option<usize>,
        visibility: Visibility,
        attention: Attention,
    },
}

/// Whether a pin without a window has a live child process; a stopped pin
/// paints dimmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinProcess {
    Stopped,
    Running,
}

/// Whether a chip's window is shown or minimized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Shown,
    Minimized,
}

/// The highlight a window chip paints: focus wins over an urgent request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attention {
    Normal,
    Focused,
    Urgent,
}

impl TaskbarChip {
    #[must_use]
    pub fn window(&self) -> Option<&Window> {
        match &self.target {
            ChipTarget::Window { window, .. } => Some(window),
            ChipTarget::Pin { .. } => None,
        }
    }

    #[must_use]
    pub fn pin(&self) -> Option<usize> {
        match self.target {
            ChipTarget::Pin { index, .. } => Some(index),
            ChipTarget::Window { pin, .. } => pin,
        }
    }
}

pub const BAR: i32 = Theme::dark().bar;
pub const TASKBAR: i32 = Theme::dark().taskbar;

/// Whether a window is a dialog: a dialog has no minimize or maximize
/// buttons and no taskbar chip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowRole {
    Normal,
    Dialog,
}

/// Whether a window is maximized: a maximized window shows Restore and has no
/// resize grips, and its menu disables Move and Resize.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeMode {
    Floating,
    Maximized,
}

/// The close button's look: hovered paints it red, and pressed, which the
/// compositor arms only while the pointer stays on it, paints it darker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseButton {
    Idle,
    Hovered,
    Pressed,
}

/// The committed window state that shapes its decoration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowFlags {
    pub role: WindowRole,
    pub size: SizeMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecorationAction {
    Move,
    Resize(xdg_toplevel::ResizeEdge),
    Minimize,
    Maximize,
    Restore,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Theme {
    bar: i32,
    taskbar: i32,
    resize_band: i32,
    resize_corner: i32,
    active: (u32, u32, u32),
    inactive: (u32, u32, u32),
    accent: u32,
    attention: u32,
    dim: u32,
    close_hover: u32,
    close_pressed: u32,
    close_hover_foreground: u32,
    title_padding: i32,
    title_size: i32,
}

impl Theme {
    const fn dark() -> Self {
        Self {
            bar: 28,
            taskbar: 32,
            resize_band: 6,
            resize_corner: 12,
            active: (0x27_2e_39, 0xe7_eb_f0, 0x0046_5164),
            inactive: (0x20_25_2d, 0x98_a2_b2, 0x30_37_43),
            accent: 0x36_6e_a8,
            attention: 0x008a_5a16,
            dim: 0x60_69_78,
            close_hover: 0xb8_32_45,
            close_pressed: 0x85_29_36,
            close_hover_foreground: 0xff_ff_ff,
            title_padding: 12,
            title_size: 13,
        }
    }

    #[expect(
        dead_code,
        reason = "light palette is available for a future theme selector"
    )]
    const fn light() -> Self {
        Self {
            active: (0xe9_ec_f0, 0x20_25_2d, 0x98_a2_b2),
            inactive: (0xf4_f5_f7, 0x76_83_99, 0xc8_cf_d8),
            ..Self::dark()
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum NodeStyle {
    Taskbar {
        scale: f64,
        theme: Theme,
        chips: Vec<TaskbarChip>,
        control: &'static str,
        clock: String,
    },
    Titlebar {
        title: String,
        scale: f64,
        active: bool,
        close: CloseButton,
        window_flags: WindowFlags,
        theme: Theme,
    },
    Hit,
}

/// One part of a decoration: what a click there does and the cursor it shows.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    rect: Rectangle<i32, Logical>,
    style: NodeStyle,
    pub action: Option<DecorationAction>,
    pub cursor: CursorShape,
}

fn nodes(size: Size<i32, Logical>, appearance: &Appearance, theme: Theme) -> Vec<Node> {
    let bar = Rectangle::new((0, -theme.bar).into(), (size.w, theme.bar).into());
    let close = Rectangle::new(
        ((size.w - theme.bar).max(0), -theme.bar).into(),
        (size.w.min(theme.bar), theme.bar).into(),
    );
    let mut result = vec![
        Node {
            rect: bar,
            style: NodeStyle::Titlebar {
                title: appearance.title.clone(),
                scale: appearance.scale,
                active: appearance.active,
                close: appearance.close,
                window_flags: appearance.window_flags,
                theme,
            },
            action: Some(DecorationAction::Move),
            cursor: CursorShape::Default,
        },
        Node {
            rect: close,
            style: NodeStyle::Hit,
            action: Some(DecorationAction::Close),
            cursor: CursorShape::Default,
        },
    ];
    if appearance.window_flags.role == WindowRole::Normal {
        for (offset, action) in [
            (3, DecorationAction::Minimize),
            (
                2,
                match appearance.window_flags.size {
                    SizeMode::Maximized => DecorationAction::Restore,
                    SizeMode::Floating => DecorationAction::Maximize,
                },
            ),
        ] {
            let left = (size.w - offset * theme.bar).max(0);
            let right = (size.w - (offset - 1) * theme.bar).max(0);
            if right > left {
                result.push(Node {
                    rect: Rectangle::new(
                        (left, -theme.bar).into(),
                        (right - left, theme.bar).into(),
                    ),
                    style: NodeStyle::Hit,
                    action: Some(action),
                    cursor: CursorShape::Default,
                });
            }
        }
    }
    if appearance.window_flags.size == SizeMode::Floating {
        result.extend(resize_nodes(size, theme));
    }
    result
}

fn resize_nodes(size: Size<i32, Logical>, theme: Theme) -> Vec<Node> {
    use xdg_toplevel::ResizeEdge as Edge;
    let band = theme.resize_band;
    let corner = theme.resize_corner;
    let half_band = band / 2;
    let half_corner = corner / 2;
    let top = -theme.bar;
    let bottom = size.h;
    let right = size.w;
    [
        (
            (0, top - half_band, right, band),
            Edge::Top,
            CursorShape::NResize,
        ),
        (
            (0, bottom - half_band, right, band),
            Edge::Bottom,
            CursorShape::SResize,
        ),
        (
            (-half_band, top, band, bottom - top),
            Edge::Left,
            CursorShape::WResize,
        ),
        (
            (right - half_band, top, band, bottom - top),
            Edge::Right,
            CursorShape::EResize,
        ),
        (
            (-half_corner, top - half_corner, corner, corner),
            Edge::TopLeft,
            CursorShape::NwResize,
        ),
        (
            (right - half_corner, top - half_corner, corner, corner),
            Edge::TopRight,
            CursorShape::NeResize,
        ),
        (
            (-half_corner, bottom - half_corner, corner, corner),
            Edge::BottomLeft,
            CursorShape::SwResize,
        ),
        (
            (right - half_corner, bottom - half_corner, corner, corner),
            Edge::BottomRight,
            CursorShape::SeResize,
        ),
    ]
    .into_iter()
    .map(|(rect, edge, cursor)| {
        let (x, y, w, h) = rect;
        Node {
            rect: Rectangle::new((x, y).into(), (w, h).into()),
            style: NodeStyle::Hit,
            action: Some(DecorationAction::Resize(edge)),
            cursor,
        }
    })
    .collect()
}

fn hit_test(nodes: &[Node], point: Point<f64, Logical>) -> Option<&Node> {
    nodes
        .iter()
        .rev()
        .find(|node| node.action.is_some() && node.rect.to_f64().contains(point))
}

/// The decoration part under `point`, relative to the window's location, for a
/// decorated window of `size` (its width clamped by `decoration_width`).
#[must_use]
pub fn node_at(
    size: Size<i32, Logical>,
    window_flags: WindowFlags,
    point: Point<f64, Logical>,
) -> Option<Node> {
    let appearance = Appearance {
        title: String::new(),
        width: size.w,
        scale: 1.0,
        active: false,
        window_flags,
        close: CloseButton::Idle,
    };
    hit_test(&nodes(size, &appearance, Theme::dark()), point).cloned()
}

#[cfg(test)]
fn hit(nodes: &[Node], point: Point<f64, Logical>) -> Option<DecorationAction> {
    hit_test(nodes, point).and_then(|node| node.action)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Appearance {
    pub title: String,
    pub width: i32,
    pub scale: f64,
    pub active: bool,
    pub window_flags: WindowFlags,
    pub close: CloseButton,
}

pub struct Decoration {
    appearance: Option<Appearance>,
    nodes: Vec<Node>,
    buffer: MemoryRenderBuffer,
}

impl Default for Decoration {
    fn default() -> Self {
        Self {
            appearance: None,
            nodes: Vec::new(),
            buffer: MemoryRenderBuffer::new(Fourcc::Argb8888, (0, 0), 1, Transform::Normal, None),
        }
    }
}

impl Decoration {
    pub fn update(&mut self, appearance: Appearance) -> Result<bool> {
        if self.appearance.as_ref() == Some(&appearance) {
            return Ok(false);
        }
        let next = nodes((appearance.width, 0).into(), &appearance, Theme::dark());
        let node = &next[0];
        if !self
            .nodes
            .first()
            .is_some_and(|old| old.style == node.style && old.rect.size == node.rect.size)
        {
            let mut surface = paint_nodes(&next)?;
            let size = (surface.width(), surface.height()).into();
            let mut target = self.buffer.render();
            target.resize(size);
            target.update_opaque_regions(Some(vec![Rectangle::from_size(size)]));
            let pixels = surface.data()?;
            target.draw(|bytes| {
                bytes.copy_from_slice(&pixels);
                Ok::<_, anyhow::Error>(vec![Rectangle::from_size(size)])
            })?;
        }
        self.nodes = next;
        self.appearance = Some(appearance);
        Ok(true)
    }

    pub fn elements(
        &self,
        renderer: &mut PixmanRenderer,
        _size: Size<i32, Logical>,
        location: Point<i32, Logical>,
        scale: f64,
    ) -> Result<Vec<MemoryRenderBufferRenderElement<PixmanRenderer>>> {
        std::iter::once((&self.buffer, self.nodes[0].rect))
            .map(|(buffer, rect)| {
                let pixels = raster_size(rect.size, scale);
                Ok(MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    (location + rect.loc)
                        .to_physical_precise_round::<_, i32>(scale)
                        .to_f64(),
                    buffer,
                    None,
                    // Buffer scale is integer. Map the complete fractional-scale raster
                    // explicitly; None would crop it to the logical destination size.
                    Some(Rectangle::from_size(
                        (f64::from(pixels.w), f64::from(pixels.h)).into(),
                    )),
                    Some(rect.size),
                    Kind::Unspecified,
                )?)
            })
            .collect()
    }
}

fn raster_size(size: Size<i32, Logical>, scale: f64) -> Size<i32, Buffer> {
    let size = size.to_physical_precise_round::<_, i32>(scale);
    (size.w.max(1), size.h.max(1)).into()
}

/// Cairo rejects image surfaces wider than 32767 px and the width times scale
/// error would otherwise end the session through `State::fail`. A window wider
/// than the output has no visible area past the output edge, so decorations
/// clamp to the output's logical width.
#[must_use]
pub fn decoration_width(window_width: i32, output_width: u32) -> i32 {
    // FrameSize caps the output at 6000 px, so this conversion cannot fail.
    let output_width = i32::try_from(output_width).unwrap_or(i32::MAX);
    window_width.min(output_width).max(1)
}

fn color(context: &cairo::Context, rgb: u32) {
    context.set_source_rgb(
        f64::from((rgb >> 16) & 255) / 255.0,
        f64::from((rgb >> 8) & 255) / 255.0,
        f64::from(rgb & 255) / 255.0,
    );
}

fn paint_taskbar(
    size: Size<i32, Logical>,
    scale: f64,
    theme: Theme,
    chips: &[TaskbarChip],
    control: &str,
    clock: &str,
) -> Result<cairo::ImageSurface> {
    let pixels = raster_size(size, scale);
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, pixels.w, pixels.h)?;
    let context = cairo::Context::new(&surface)?;
    context.scale(
        f64::from(pixels.w) / f64::from(size.w),
        f64::from(pixels.h) / f64::from(size.h),
    );
    color(&context, theme.inactive.0);
    context.paint()?;
    color(&context, theme.inactive.2);
    context.rectangle(0.0, 0.0, f64::from(size.w), 1.0);
    context.fill()?;
    for (index, chip) in chips.iter().enumerate() {
        let Some(rect) = chip_rectangle(size.w, chips.len(), index) else {
            continue;
        };
        let highlight = match chip.target {
            ChipTarget::Window {
                attention: Attention::Focused,
                ..
            } => Some(theme.accent),
            ChipTarget::Window {
                attention: Attention::Urgent,
                ..
            } => Some(theme.attention),
            ChipTarget::Window {
                attention: Attention::Normal,
                ..
            }
            | ChipTarget::Pin { .. } => None,
        };
        if let Some(highlight) = highlight {
            color(&context, highlight);
            context.rectangle(
                f64::from(rect.loc.x),
                2.0,
                f64::from(rect.size.w),
                f64::from(size.h - 3),
            );
            context.fill()?;
        }
        context.save()?;
        context.translate(f64::from(rect.loc.x), 2.0);
        color(
            &context,
            if highlight.is_some() {
                theme.active.1
            } else if matches!(
                chip.target,
                ChipTarget::Pin {
                    process: PinProcess::Stopped,
                    ..
                }
            ) {
                theme.dim
            } else {
                theme.inactive.1
            },
        );
        draw_title(
            &context,
            &chip.title,
            (rect.size.w - 2 * theme.title_padding).max(0),
            theme,
        )?;
        context.restore()?;
    }
    let pin_count = chips.iter().take_while(|chip| chip.pin().is_some()).count();
    if pin_count > 0
        && let Some(rect) = chip_rectangle(size.w, chips.len(), pin_count - 1)
    {
        color(&context, theme.inactive.2);
        context.rectangle(
            f64::from(rect.loc.x + rect.size.w - 1),
            5.0,
            1.0,
            f64::from(size.h - 10),
        );
        context.fill()?;
    }
    context.save()?;
    context.translate(f64::from((size.w - 180).max(0)), 2.0);
    color(&context, theme.inactive.1);
    draw_title(&context, control, 110, theme)?;
    context.restore()?;
    context.save()?;
    context.translate(f64::from((size.w - 60).max(0)), 2.0);
    color(&context, theme.inactive.1);
    draw_title(&context, clock, 50, theme)?;
    context.restore()?;
    Ok(surface)
}

fn paint_nodes(nodes: &[Node]) -> Result<cairo::ImageSurface> {
    let node = &nodes[0];
    if let NodeStyle::Taskbar {
        scale,
        theme,
        ref chips,
        control,
        ref clock,
    } = node.style
    {
        return paint_taskbar(node.rect.size, scale, theme, chips, control, clock);
    }
    paint_titlebar(nodes)
}

fn paint_titlebar(nodes: &[Node]) -> Result<cairo::ImageSurface> {
    let node = &nodes[0];
    let (theme, scale, active, close_button, title) = match &node.style {
        NodeStyle::Titlebar {
            title,
            scale,
            active,
            close,
            theme,
            ..
        } => (*theme, *scale, *active, *close, title.as_str()),
        NodeStyle::Hit | NodeStyle::Taskbar { .. } => anyhow::bail!("invalid titlebar node"),
    };
    let width = node.rect.size.w;
    let size: Size<i32, Logical> = (width, theme.bar).into();
    let pixels = raster_size(size, scale);
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, pixels.w, pixels.h)?;
    let context = cairo::Context::new(&surface)?;
    context.scale(
        f64::from(pixels.w) / f64::from(size.w),
        f64::from(pixels.h) / f64::from(size.h),
    );
    let (background, foreground, border) = if active { theme.active } else { theme.inactive };
    color(&context, background);
    context.paint()?;
    {
        let close = nodes[1].rect;
        if close_button != CloseButton::Idle {
            color(
                &context,
                if close_button == CloseButton::Pressed {
                    theme.close_pressed
                } else {
                    theme.close_hover
                },
            );
            context.rectangle(
                f64::from(close.loc.x),
                0.0,
                f64::from(close.size.w),
                f64::from(BAR),
            );
            context.fill()?;
        }
        color(&context, foreground);
        let title_end = nodes
            .iter()
            .skip(1)
            .filter(|node| {
                matches!(
                    node.action,
                    Some(
                        DecorationAction::Close
                            | DecorationAction::Minimize
                            | DecorationAction::Maximize
                            | DecorationAction::Restore
                    )
                )
            })
            .map(|node| node.rect.loc.x)
            .min()
            .unwrap_or(close.loc.x);
        draw_title(&context, title, (title_end - 24).max(0), theme)?;
        context.save()?;
        context.rectangle(
            f64::from(close.loc.x),
            0.0,
            f64::from(close.size.w),
            f64::from(BAR),
        );
        context.clip();
        let x = f64::from(close.loc.x) + f64::from(close.size.w) / 2.0;
        color(
            &context,
            if close_button == CloseButton::Idle {
                foreground
            } else {
                theme.close_hover_foreground
            },
        );
        context.set_line_width(1.5);
        context.set_line_cap(cairo::LineCap::Round);
        draw_close(&context, x, theme)?;
        context.restore()?;
        draw_buttons(&context, nodes, foreground, theme)?;
    }
    color(&context, border);
    context.rectangle(0.0, f64::from(theme.bar - 1), f64::from(width), 1.0);
    context.fill()?;
    context.status()?;
    Ok(surface)
}

fn draw_close(context: &cairo::Context, x: f64, theme: Theme) -> Result<()> {
    let y = f64::from(theme.bar) / 2.0;
    context.move_to(x - 4.0, y - 4.0);
    context.line_to(x + 4.0, y + 4.0);
    context.move_to(x + 4.0, y - 4.0);
    context.line_to(x - 4.0, y + 4.0);
    context.stroke()?;
    Ok(())
}

fn draw_buttons(
    context: &cairo::Context,
    nodes: &[Node],
    foreground: u32,
    theme: Theme,
) -> Result<()> {
    for button in nodes.iter().filter(|node| {
        matches!(
            node.action,
            Some(
                DecorationAction::Minimize | DecorationAction::Maximize | DecorationAction::Restore
            )
        )
    }) {
        let x = f64::from(button.rect.loc.x) + f64::from(button.rect.size.w) / 2.0;
        context.save()?;
        context.rectangle(
            f64::from(button.rect.loc.x),
            0.0,
            f64::from(button.rect.size.w),
            f64::from(theme.bar),
        );
        context.clip();
        color(context, foreground);
        context.set_line_width(1.0);
        let y = f64::from(theme.bar) / 2.0;
        match button.action {
            Some(DecorationAction::Minimize) => {
                context.move_to(x - 4.0, y + 1.0);
                context.line_to(x + 4.0, y + 1.0);
            }
            Some(DecorationAction::Maximize) => context.rectangle(x - 4.0, y - 4.0, 8.0, 8.0),
            Some(DecorationAction::Restore) => {
                context.rectangle(x - 4.0, y - 3.0, 7.0, 7.0);
                context.move_to(x - 2.0, y - 5.0);
                context.line_to(x + 4.0, y - 5.0);
                context.line_to(x + 4.0, y + 1.0);
            }
            _ => {}
        }
        context.stroke()?;
        context.restore()?;
    }
    Ok(())
}

fn draw_title(context: &cairo::Context, title: &str, width: i32, theme: Theme) -> Result<()> {
    if width == 0 {
        return Ok(());
    }
    // Absolute logical-pixel size avoids depending on the host's font DPI.
    let mut font = pango::FontDescription::from_string("Sans");
    font.set_absolute_size(f64::from(theme.title_size * pango::SCALE));
    let text = pangocairo::functions::create_layout(context);
    let mut options = cairo::FontOptions::new()?;
    options.set_antialias(cairo::Antialias::Gray);
    pangocairo::functions::context_set_font_options(&text.context(), Some(&options));
    text.set_font_description(Some(&font));
    text.set_single_paragraph_mode(true);
    text.set_ellipsize(pango::EllipsizeMode::End);
    text.set_width(width * pango::SCALE);
    text.set_text(title);
    context.save()?;
    context.rectangle(
        f64::from(theme.title_padding),
        0.0,
        f64::from(width),
        f64::from(BAR),
    );
    context.clip();
    context.move_to(
        f64::from(theme.title_padding),
        f64::from(BAR - text.pixel_size().1) / 2.0,
    );
    pangocairo::functions::show_layout(context, &text);
    context.restore()?;
    Ok(())
}

smithay::backend::renderer::element::render_elements! {
    pub SceneElement<R> where R: ImportAll + ImportMem;
    Surface=WaylandSurfaceRenderElement<R>,
    Decoration=MemoryRenderBufferRenderElement<R>,
}

#[must_use]
pub fn decorated(window: &Window) -> bool {
    if let Some(top) = window.toplevel() {
        top.with_committed_state(|state| {
            state.is_some_and(|state| {
                state.decoration_mode == Some(Mode::ServerSide)
                    && !state.states.contains(xdg_toplevel::State::Fullscreen)
            })
        })
    } else {
        window.x11_surface().is_some_and(|x| {
            use smithay::xwayland::xwm::WmWindowType;
            !x.is_override_redirect()
                && !x.is_decorated()
                && !x.is_fullscreen()
                && matches!(
                    x.window_type(),
                    None | Some(WmWindowType::Normal | WmWindowType::Dialog)
                )
        })
    }
}

#[must_use]
pub fn taskbar_window(window: &Window) -> bool {
    if window_flags(window).role == WindowRole::Dialog {
        return false;
    }
    if window.toplevel().is_some() {
        return true;
    }
    window.x11_surface().is_some_and(|surface| {
        use smithay::xwayland::xwm::WmWindowType;
        !surface.is_override_redirect()
            && matches!(surface.window_type(), None | Some(WmWindowType::Normal))
    })
}

#[must_use]
pub fn chip_rectangle(width: i32, count: usize, index: usize) -> Option<Rectangle<i32, Logical>> {
    let count = i32::try_from(count).ok()?;
    let index = i32::try_from(index).ok()?;
    if count == 0 || index >= count {
        return None;
    }
    let chip_width = ((width - 180).max(0) / count).min(180);
    Some(Rectangle::new(
        (index * chip_width, 0).into(),
        (chip_width, TASKBAR).into(),
    ))
}

#[must_use]
pub fn window_flags(window: &Window) -> WindowFlags {
    let (dialog, maximized) = if let Some(top) = window.toplevel() {
        (
            top.parent().is_some(),
            top.with_committed_state(|state| {
                state.is_some_and(|state| state.states.contains(xdg_toplevel::State::Maximized))
            }),
        )
    } else if let Some(x11) = window.x11_surface() {
        (x11.is_transient_for().is_some(), x11.is_maximized())
    } else {
        (false, false)
    };
    WindowFlags {
        role: if dialog {
            WindowRole::Dialog
        } else {
            WindowRole::Normal
        },
        size: if maximized {
            SizeMode::Maximized
        } else {
            SizeMode::Floating
        },
    }
}

#[must_use]
pub fn above_taskbar(window: &Window) -> bool {
    window.toplevel().is_some_and(|top| {
        top.with_committed_state(|state| {
            state.is_some_and(|state| state.states.contains(xdg_toplevel::State::Fullscreen))
        })
    }) || window
        .x11_surface()
        .is_some_and(|surface| surface.is_fullscreen() || surface.is_override_redirect())
}

pub struct Taskbar {
    node: Option<Node>,
    buffer: MemoryRenderBuffer,
}

impl Default for Taskbar {
    fn default() -> Self {
        Self {
            node: None,
            buffer: MemoryRenderBuffer::new(Fourcc::Argb8888, (0, 0), 1, Transform::Normal, None),
        }
    }
}

impl Taskbar {
    pub fn element(
        &mut self,
        renderer: &mut PixmanRenderer,
        output: Rectangle<i32, Logical>,
        scale: f64,
        chips: Vec<TaskbarChip>,
        control: &'static str,
        clock: String,
    ) -> Result<MemoryRenderBufferRenderElement<PixmanRenderer>> {
        let rect = Rectangle::new(
            (output.loc.x, output.loc.y + output.size.h - TASKBAR).into(),
            (output.size.w, TASKBAR).into(),
        );
        let node = Node {
            rect,
            style: NodeStyle::Taskbar {
                scale,
                theme: Theme::dark(),
                chips,
                control,
                clock,
            },
            action: None,
            cursor: CursorShape::Default,
        };
        if self.node.as_ref() != Some(&node) {
            let mut surface = paint_nodes(std::slice::from_ref(&node))?;
            let size = (surface.width(), surface.height()).into();
            let mut target = self.buffer.render();
            target.resize(size);
            target.update_opaque_regions(Some(vec![Rectangle::from_size(size)]));
            let pixels = surface.data()?;
            target.draw(|bytes| {
                bytes.copy_from_slice(&pixels);
                Ok::<_, anyhow::Error>(vec![Rectangle::from_size(size)])
            })?;
            self.node = Some(node);
        }
        let pixels = raster_size(rect.size, scale);
        Ok(MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            rect.loc.to_physical_precise_round::<_, i32>(scale).to_f64(),
            &self.buffer,
            None,
            Some(Rectangle::from_size(
                (f64::from(pixels.w), f64::from(pixels.h)).into(),
            )),
            Some(rect.size),
            Kind::Unspecified,
        )?)
    }
}

const MENU_WIDTH: i32 = 180;
const MENU_ROW: i32 = 26;
const MENU_HEIGHT: i32 = MENU_ROW * 6;

pub struct WindowMenu {
    pub window: Window,
    location: Point<i32, Logical>,
    size: SizeMode,
    always_on_top: bool,
    painted: bool,
    hover: Option<usize>,
    buffer: MemoryRenderBuffer,
}

impl WindowMenu {
    #[must_use]
    pub fn new(
        window: Window,
        point: Point<f64, Logical>,
        output: Size<i32, Logical>,
        always_on_top: bool,
    ) -> Self {
        let size = window_flags(&window).size;
        let point = point.to_i32_round::<i32>();
        Self {
            window,
            location: (
                point.x.clamp(0, (output.w - MENU_WIDTH).max(0)),
                point.y.clamp(0, (output.h - MENU_HEIGHT).max(0)),
            )
                .into(),
            size,
            always_on_top,
            painted: false,
            hover: None,
            buffer: MemoryRenderBuffer::new(Fourcc::Argb8888, (0, 0), 1, Transform::Normal, None),
        }
    }

    #[must_use]
    pub fn contains(&self, point: Point<f64, Logical>) -> bool {
        let point = point - self.location.to_f64();
        point.x >= 0.0
            && point.x < f64::from(MENU_WIDTH)
            && point.y >= 0.0
            && point.y < f64::from(MENU_HEIGHT)
    }

    fn item_at(&self, point: Point<f64, Logical>) -> Option<usize> {
        if !self.contains(point) {
            return None;
        }
        (0..6)
            .find(|index| {
                let y = point.y - f64::from(self.location.y);
                y >= f64::from(*index * MENU_ROW) && y < f64::from((*index + 1) * MENU_ROW)
            })
            .and_then(|index| usize::try_from(index).ok())
    }

    fn action(&self, index: usize) -> Option<WindowAction> {
        match index {
            0 if window_flags(&self.window).role == WindowRole::Normal => {
                Some(WindowAction::Minimize)
            }
            1 => Some(match self.size {
                SizeMode::Maximized => WindowAction::Restore,
                SizeMode::Floating => WindowAction::Maximize,
            }),
            2 => Some(WindowAction::ToggleAlwaysOnTop),
            3 if self.size == SizeMode::Floating => Some(WindowAction::Move),
            4 if self.size == SizeMode::Floating => {
                Some(WindowAction::Resize(xdg_toplevel::ResizeEdge::BottomRight))
            }
            5 => Some(WindowAction::Close),
            _ => None,
        }
    }

    #[must_use]
    pub fn click(&self, point: Point<f64, Logical>) -> Option<WindowAction> {
        self.item_at(point).and_then(|index| self.action(index))
    }

    pub fn motion(&mut self, point: Point<f64, Logical>) -> bool {
        let hover = self
            .item_at(point)
            .filter(|index| self.action(*index).is_some());
        if self.hover == hover {
            return false;
        }
        self.hover = hover;
        self.painted = false;
        true
    }

    pub fn element(
        &mut self,
        renderer: &mut PixmanRenderer,
        scale: f64,
    ) -> Result<MemoryRenderBufferRenderElement<PixmanRenderer>> {
        if !self.painted {
            let size = raster_size((MENU_WIDTH, MENU_HEIGHT).into(), scale);
            let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, size.w, size.h)?;
            let context = cairo::Context::new(&surface)?;
            context.scale(scale, scale);
            let theme = Theme::dark();
            color(&context, theme.inactive.0);
            context.paint()?;
            let labels = [
                "Minimize",
                match self.size {
                    SizeMode::Maximized => "Restore",
                    SizeMode::Floating => "Maximize",
                },
                "Always on Top",
                "Move",
                "Resize",
                "Close",
            ];
            for (index, label) in labels.iter().enumerate() {
                let disabled = self.action(index).is_none();
                if self.hover == Some(index) {
                    color(&context, theme.active.0);
                    context.rectangle(
                        1.0,
                        f64::from(i32::try_from(index)? * MENU_ROW),
                        f64::from(MENU_WIDTH - 2),
                        f64::from(MENU_ROW),
                    );
                    context.fill()?;
                }
                color(
                    &context,
                    if disabled {
                        theme.inactive.1
                    } else {
                        theme.active.1
                    },
                );
                context.save()?;
                context.translate(
                    0.0,
                    f64::from(i32::try_from(index)? * MENU_ROW) - f64::from(BAR - MENU_ROW) / 2.0,
                );
                draw_title(&context, label, MENU_WIDTH - theme.title_padding * 2, theme)?;
                context.restore()?;
                if index == 2 && self.always_on_top {
                    context.move_to(f64::from(MENU_WIDTH - 20), f64::from(MENU_ROW * 2 + 13));
                    context.line_to(f64::from(MENU_WIDTH - 17), f64::from(MENU_ROW * 2 + 16));
                    context.line_to(f64::from(MENU_WIDTH - 11), f64::from(MENU_ROW * 2 + 9));
                    context.stroke()?;
                }
            }
            color(&context, theme.active.2);
            context.set_line_width(1.0);
            context.rectangle(
                0.5,
                0.5,
                f64::from(MENU_WIDTH - 1),
                f64::from(MENU_HEIGHT - 1),
            );
            context.stroke()?;
            drop(context);
            let mut target = self.buffer.render();
            target.resize(size);
            let pixels = surface.data()?;
            target.draw(|bytes| {
                bytes.copy_from_slice(&pixels);
                Ok::<_, anyhow::Error>(vec![Rectangle::from_size(size)])
            })?;
            self.painted = true;
        }
        let pixels = raster_size((MENU_WIDTH, MENU_HEIGHT).into(), scale);
        Ok(MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            self.location
                .to_physical_precise_round::<_, i32>(scale)
                .to_f64(),
            &self.buffer,
            None,
            Some(Rectangle::from_size(
                (f64::from(pixels.w), f64::from(pixels.h)).into(),
            )),
            Some((MENU_WIDTH, MENU_HEIGHT).into()),
            Kind::Unspecified,
        )?)
    }
}

#[cfg(test)]
mod tests {
    use smithay::backend::renderer::Bind;
    use smithay::backend::renderer::Offscreen;
    use smithay::backend::renderer::damage::OutputDamageTracker;
    use smithay::backend::renderer::element::Element;

    use super::*;

    fn appearance(scale: f64) -> Appearance {
        Appearance {
            title: "A readable title — العربية".into(),
            width: 501,
            scale,
            active: true,
            window_flags: WindowFlags {
                role: WindowRole::Normal,
                size: SizeMode::Floating,
            },
            close: CloseButton::Idle,
        }
    }

    fn paint(appearance: &Appearance, _height: i32) -> Result<cairo::ImageSurface> {
        paint_nodes(&nodes(
            (appearance.width, 0).into(),
            appearance,
            Theme::dark(),
        ))
    }

    #[test]
    fn node_actions_and_cursors_match_hit_test() {
        let nodes = nodes((101, 57).into(), &appearance(1.0), Theme::dark());
        assert_eq!(
            nodes[1].rect,
            Rectangle::new((73, -28).into(), (28, 28).into())
        );
        let found = hit_test(&nodes, (80.0, -10.0).into()).expect("close");
        assert_eq!(found.action, Some(DecorationAction::Close));
        assert_eq!(found.cursor, CursorShape::Default);
    }

    #[test]
    fn resize_nodes_match_edges_and_cursor_shapes() {
        use xdg_toplevel::ResizeEdge as Edge;
        let nodes = nodes((500, 200).into(), &appearance(1.0), Theme::dark());
        for (point, edge, cursor) in [
            ((250.0, -28.0), Edge::Top, CursorShape::NResize),
            ((250.0, 200.0), Edge::Bottom, CursorShape::SResize),
            ((0.0, 80.0), Edge::Left, CursorShape::WResize),
            ((500.0, 80.0), Edge::Right, CursorShape::EResize),
            ((0.0, -28.0), Edge::TopLeft, CursorShape::NwResize),
            ((500.0, -28.0), Edge::TopRight, CursorShape::NeResize),
            ((0.0, 200.0), Edge::BottomLeft, CursorShape::SwResize),
            ((500.0, 200.0), Edge::BottomRight, CursorShape::SeResize),
        ] {
            let found = hit_test(&nodes, point.into()).expect("resize node");
            assert_eq!(
                found.action,
                Some(DecorationAction::Resize(edge)),
                "{point:?}"
            );
            assert_eq!(found.cursor, cursor, "{point:?}");
        }
    }

    #[test]
    fn titlebar_buttons_match_window_kind_and_state() {
        let mut look = appearance(1.0);
        let buttons = |look: &Appearance| {
            let nodes = nodes((501, 57).into(), look, Theme::dark());
            [430.0, 460.0, 490.0].map(|x| hit(&nodes, (x, -10.0).into()))
        };
        assert_eq!(
            buttons(&look),
            [
                Some(DecorationAction::Minimize),
                Some(DecorationAction::Maximize),
                Some(DecorationAction::Close)
            ]
        );
        look.window_flags.size = SizeMode::Maximized;
        assert_eq!(
            buttons(&look),
            [
                Some(DecorationAction::Minimize),
                Some(DecorationAction::Restore),
                Some(DecorationAction::Close)
            ]
        );
        look.window_flags.role = WindowRole::Dialog;
        assert_eq!(
            buttons(&look),
            [
                Some(DecorationAction::Move),
                Some(DecorationAction::Move),
                Some(DecorationAction::Close)
            ]
        );
    }

    #[test]
    fn maximized_window_has_no_resize_grip() {
        let mut look = appearance(1.0);
        look.window_flags.size = SizeMode::Maximized;
        let nodes = nodes((501, 57).into(), &look, Theme::dark());
        assert!(
            !nodes
                .iter()
                .any(|node| matches!(node.action, Some(DecorationAction::Resize(_))))
        );
        assert_eq!(hit(&nodes, (500.0, 60.0).into()), None);
    }

    #[test]
    fn close_hover_changes_titlebar_paint_input() {
        let mut look = appearance(1.0);
        let normal = nodes((look.width, 57).into(), &look, Theme::dark());
        look.close = CloseButton::Hovered;
        let hovered = nodes((look.width, 57).into(), &look, Theme::dark());
        assert!(matches!(
            normal[0].style,
            NodeStyle::Titlebar {
                close: CloseButton::Idle,
                ..
            }
        ));
        assert!(matches!(
            hovered[0].style,
            NodeStyle::Titlebar {
                close: CloseButton::Hovered,
                theme: Theme {
                    close_hover: 0xb8_3245,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn a_very_wide_window_clamps_its_decoration_to_the_output_width() {
        let output_width = 3840_u32;
        assert_eq!(
            decoration_width(50_000, output_width),
            i32::try_from(output_width).expect("test output width")
        );
        assert_eq!(decoration_width(13, output_width), 13);

        let mut look = appearance(1.0);
        look.width = 50_000;
        assert!(
            paint(&look, BAR).is_err(),
            "cairo must reject an unclamped very wide decoration"
        );
        look.width = decoration_width(look.width, output_width);
        let surface = paint(&look, BAR).expect("clamped very wide decoration should paint");
        assert_eq!(surface.width(), raster_size((look.width, 0).into(), 1.0).w);

        // Hit testing follows the clamped paint: the close button sits at the
        // clamped right edge, not at the window's own right edge.
        let layout = nodes((look.width, 57).into(), &look, Theme::dark());
        assert_eq!(
            hit(&layout, (f64::from(look.width) - 10.0, -10.0).into()),
            Some(DecorationAction::Close)
        );
        assert_eq!(
            hit(&layout, (f64::from(look.width) + 4.0, -10.0).into()),
            None
        );
    }

    #[test]
    fn hit_regions_match_painted_edges_including_narrow_windows() {
        let layout = nodes((101, 57).into(), &appearance(1.0), Theme::dark());
        for (point, expected) in [
            ((-4.0, 10.0), None),
            ((10.0, -20.0), Some(DecorationAction::Move)),
            ((72.9, -10.0), Some(DecorationAction::Maximize)),
            ((80.0, -10.0), Some(DecorationAction::Close)),
            ((50.0, 30.0), None),
            ((50.0, 60.0), None),
        ] {
            assert_eq!(hit(&layout, point.into()), expected, "{point:?}");
        }
        let narrow = nodes((13, 57).into(), &appearance(1.0), Theme::dark());
        assert_eq!(narrow[1].rect, narrow[0].rect);
        assert_eq!(
            hit(&narrow, (6.5, -14.0).into()),
            Some(DecorationAction::Close)
        );
    }

    #[test]
    fn raster_is_opaque_argb_and_clips_text_before_close() {
        for scale in [1.0, 1.25, 2.0] {
            for width in [1, 13, 32, 501] {
                let mut look = appearance(scale);
                look.width = width;
                for (close, expected) in [
                    (CloseButton::Idle, 0xff27_2e39),
                    (CloseButton::Hovered, 0xffb8_3245),
                    (CloseButton::Pressed, 0xff85_2936),
                ] {
                    look.close = close;
                    let mut surface = paint(&look, BAR).expect("titlebar");
                    let stride = usize::try_from(surface.stride()).expect("stride");
                    let pixels = surface.data().expect("pixels");
                    assert_eq!(
                        u32::from_ne_bytes(pixels[stride - 4..stride].try_into().expect("pixel")),
                        expected
                    );
                    assert!(
                        pixels
                            .chunks_exact(4)
                            .all(|p| u32::from_ne_bytes(p.try_into().expect("pixel")) >> 24 == 255)
                    );
                    if width == 501 {
                        let close_start =
                            usize::try_from(raster_size((width - BAR, 0).into(), scale).w)
                                .expect("close start")
                                * 4;
                        let close_pixels: Vec<_> = pixels
                            .chunks_exact(stride)
                            .flat_map(|row| row[close_start..].iter().copied())
                            .collect();
                        drop(pixels);
                        look.title = "Very long title لا يزال هذا نصاً shaped text ".repeat(80);
                        let mut long = paint(&look, BAR).expect("ellipsized titlebar");
                        let long_pixels = long.data().expect("pixels");
                        assert_eq!(
                            close_pixels,
                            long_pixels
                                .chunks_exact(stride)
                                .flat_map(|row| row[close_start..].iter().copied())
                                .collect::<Vec<_>>()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn cached_buffers_map_full_fractional_raster_and_leave_idle_output_undamaged() {
        for (scale, widths, bar_height) in [
            (1.0, [501.0, 533.0], 28.0),
            (1.25, [626.0, 666.0], 35.0),
            (2.0, [1002.0, 1066.0], 56.0),
        ] {
            let mut renderer = PixmanRenderer::new().expect("renderer");
            let mut image = renderer
                .create_buffer(Fourcc::Argb8888, (1200, 500).into())
                .expect("target");
            let mut tracker = OutputDamageTracker::new((1200, 500), scale, Transform::Normal);
            let mut cache = Decoration::default();
            let mut look = appearance(scale);
            assert!(cache.update(look.clone()).expect("first paint"));
            let mut previous = None;
            for step in 0..7 {
                let changed = match step {
                    0 => true,
                    // Hovering a resize edge leaves the close button idle.
                    1 => {
                        look.close = CloseButton::Idle;
                        false
                    }
                    2 => {
                        look.title = "Changed without a client buffer commit".into();
                        true
                    }
                    3 => {
                        look.close = CloseButton::Hovered;
                        true
                    }
                    4 => {
                        look.close = CloseButton::Pressed;
                        true
                    }
                    5 => {
                        look.active = false;
                        true
                    }
                    _ => {
                        look.width = 533;
                        true
                    }
                };
                cache.update(look.clone()).expect("paint update");
                assert!(!cache.update(look.clone()).expect("idle"));
                let parts = cache
                    .elements(
                        &mut renderer,
                        (look.width, 53).into(),
                        (10, 42).into(),
                        scale,
                    )
                    .expect("elements");
                let commit = parts[0].current_commit();
                let id = parts[0].id().clone();
                if let Some((old_id, old_commit)) = previous {
                    assert_eq!(id, old_id, "stable buffer ID");
                    assert_eq!(commit != old_commit, changed, "step {step}");
                }
                previous = Some((id, commit));
                assert_eq!(
                    parts[0].src().size,
                    (widths[usize::from(step == 6)], bar_height).into()
                );
                let mut target = renderer.bind(&mut image).expect("bind");
                assert_eq!(
                    tracker
                        .render_output(
                            &mut renderer,
                            &mut target,
                            usize::from(step != 0),
                            &parts,
                            [0.0, 0.0, 0.0, 1.0]
                        )
                        .expect("changed render")
                        .damage
                        .is_some(),
                    changed
                );
                assert!(
                    tracker
                        .render_output(&mut renderer, &mut target, 1, &parts, [0.0, 0.0, 0.0, 1.0])
                        .expect("idle render")
                        .damage
                        .is_none()
                );
            }
        }
    }
}
