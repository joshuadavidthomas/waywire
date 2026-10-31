//! Cached software decorations; layout is shared by painting and hit testing.
use anyhow::Result;
use pangocairo::{cairo, pango};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::{ImportAll, ImportMem};
use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::desktop::{Window, WindowSurfaceType};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::{Buffer, Logical, Point, Rectangle, Size, Transform};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::ToplevelSurface;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use waywire_protocol::pipe::CursorShape;

use super::State;

pub(super) const BAR: i32 = Theme::dark().bar;
pub(super) const STRIP: i32 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DecorationAction {
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
    resize_band: i32,
    resize_corner: i32,
    active: (u32, u32, u32),
    inactive: (u32, u32, u32),
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
            resize_band: 6,
            resize_corner: 12,
            active: (0x27_2e_39, 0xe7_eb_f0, 0x0046_5164),
            inactive: (0x20_25_2d, 0x98_a2_b2, 0x30_37_43),
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
    Titlebar {
        title: String,
        scale: f64,
        active: bool,
        close_hovered: bool,
        pressed: bool,
        dialog: bool,
        maximized: bool,
        theme: Theme,
    },
    Hit,
}

#[derive(Clone, Debug, PartialEq)]
struct Node {
    rect: Rectangle<i32, Logical>,
    style: NodeStyle,
    action: Option<DecorationAction>,
    cursor: CursorShape,
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
                close_hovered: appearance.hover == Some(DecorationAction::Close),
                pressed: appearance.pressed,
                dialog: appearance.window_flags.0,
                maximized: appearance.window_flags.1,
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
    if !appearance.window_flags.0 {
        for (offset, action) in [
            (3, DecorationAction::Minimize),
            (
                2,
                if appearance.window_flags.1 {
                    DecorationAction::Restore
                } else {
                    DecorationAction::Maximize
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
    if !appearance.window_flags.1 {
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

#[cfg(test)]
fn hit(nodes: &[Node], point: Point<f64, Logical>) -> Option<DecorationAction> {
    hit_test(nodes, point).and_then(|node| node.action)
}

#[derive(Clone, Debug, PartialEq)]
struct Appearance {
    title: String,
    width: i32,
    scale: f64,
    active: bool,
    window_flags: (bool, bool),
    hover: Option<DecorationAction>,
    pressed: bool,
}

pub(super) struct Decoration {
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
    fn update(&mut self, appearance: Appearance) -> Result<bool> {
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

    fn elements(
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
fn decoration_width(window_width: i32, output_width: u32) -> i32 {
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

fn paint_nodes(nodes: &[Node]) -> Result<cairo::ImageSurface> {
    let node = &nodes[0];
    let (theme, scale, active, hovered, pressed, title) = match &node.style {
        NodeStyle::Titlebar {
            title,
            scale,
            active,
            close_hovered,
            pressed,
            theme,
            ..
        } => (
            *theme,
            *scale,
            *active,
            *close_hovered,
            *pressed,
            title.as_str(),
        ),
        NodeStyle::Hit => anyhow::bail!("hit-only node cannot be painted"),
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
        if hovered {
            color(
                &context,
                if pressed {
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
            if hovered {
                theme.close_hover_foreground
            } else {
                foreground
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
    pub(super) SceneElement<R> where R: ImportAll + ImportMem;
    Surface=WaylandSurfaceRenderElement<R>,
    Decoration=MemoryRenderBufferRenderElement<R>,
}

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, top: ToplevelSurface) {
        self.request_mode(top, Mode::ServerSide);
    }
    fn request_mode(&mut self, top: ToplevelSurface, mode: Mode) {
        top.with_pending_state(|state| state.decoration_mode = Some(mode));
        top.send_pending_configure();
        self.dirty = true;
    }
    fn unset_mode(&mut self, top: ToplevelSurface) {
        self.request_mode(top, Mode::ServerSide);
    }
}

pub(super) fn decorated(window: &Window) -> bool {
    if let Some(top) = window.toplevel() {
        top.with_pending_state(|state| {
            state.decoration_mode == Some(Mode::ServerSide)
                && !state.states.contains(xdg_toplevel::State::Fullscreen)
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

fn window_flags(window: &Window) -> (bool, bool) {
    if let Some(top) = window.toplevel() {
        (
            top.parent().is_some(),
            top.with_pending_state(|state| state.states.contains(xdg_toplevel::State::Maximized)),
        )
    } else if let Some(x11) = window.x11_surface() {
        (x11.is_transient_for().is_some(), x11.is_maximized())
    } else {
        (false, false)
    }
}

impl State {
    pub(super) fn decoration_under(
        &self,
        point: Point<f64, Logical>,
    ) -> Option<(Window, DecorationAction)> {
        self.decoration_node_under(point)
            .and_then(|(window, node)| Some((window, node.action?)))
    }

    pub(super) fn decoration_cursor_under(
        &self,
        point: Point<f64, Logical>,
    ) -> Option<CursorShape> {
        self.decoration_node_under(point)
            .map(|(_, node)| node.cursor)
    }

    fn decoration_node_under(&self, point: Point<f64, Logical>) -> Option<(Window, Node)> {
        for window in self.space.elements().rev() {
            let location = self.space.element_location(window)?;
            let surface_location = location - window.geometry().loc;
            if !decorated(window)
                || window.geometry().size.is_empty()
                || !self.windows.get(window).is_some_and(|state| state.mapped)
            {
                if window
                    .surface_under(point - surface_location.to_f64(), WindowSurfaceType::ALL)
                    .is_some()
                {
                    return None;
                }
                continue;
            }
            let size: Size<i32, Logical> = (
                decoration_width(window.geometry().size.w, self.size.width()),
                window.geometry().size.h,
            )
                .into();
            let appearance = Appearance {
                title: String::new(),
                width: size.w,
                scale: 1.0,
                active: false,
                window_flags: window_flags(window),
                hover: None,
                pressed: false,
            };
            let nodes = nodes(size, &appearance, Theme::dark());
            if let Some(node) = hit_test(&nodes, point - location.to_f64()) {
                return Some((window.clone(), node.clone()));
            }
            if window
                .surface_under(point - surface_location.to_f64(), WindowSurfaceType::ALL)
                .is_some()
            {
                return None;
            }
        }
        None
    }

    pub(super) fn scene_elements(
        &mut self,
        renderer: &mut PixmanRenderer,
    ) -> Vec<SceneElement<PixmanRenderer>> {
        let mut elements = Vec::new();
        let scale = self.output.current_scale().fractional_scale();
        let output_width = self.size.width();
        let pointer = self.seat.get_pointer();
        let hit = pointer
            .as_ref()
            .and_then(|p| self.decoration_under(p.current_location()));
        if self.decoration_press.as_ref().is_some_and(|(_, window)| {
            hit.as_ref() != Some(&(window.clone(), DecorationAction::Close))
        }) {
            self.decoration_press = None;
        }
        let hover = hit
            .filter(|(_, action)| *action != DecorationAction::Move)
            .filter(|_| {
                pointer.as_ref().is_some_and(|p| !p.is_grabbed()) || self.decoration_press.is_some()
            });
        for window in self.space.elements().rev() {
            let Some(location) = self.space.element_location(window) else {
                continue;
            };
            elements.extend(window.render_elements::<SceneElement<PixmanRenderer>>(
                renderer,
                (location - window.geometry().loc).to_physical_precise_round(scale),
                scale.into(),
                1.0,
            ));
            if !decorated(window)
                || window.geometry().size.is_empty()
                || !self.windows.get(window).is_some_and(|state| state.mapped)
            {
                continue;
            }
            let (title, active) = if let Some(top) = window.toplevel() {
                let title = with_states(top.wl_surface(), |states| {
                    states
                        .data_map
                        .get::<XdgToplevelSurfaceData>()
                        .and_then(|data| data.lock().ok()?.title.clone())
                        .unwrap_or_default()
                });
                (
                    title,
                    top.with_pending_state(|state| {
                        state.states.contains(xdg_toplevel::State::Activated)
                    }),
                )
            } else if let Some(x11) = window.x11_surface() {
                (x11.title(), x11.is_activated())
            } else {
                continue;
            };
            let state = self.windows.entry(window.clone()).or_default();
            let size = window.geometry().size;
            let width = decoration_width(size.w, output_width);
            let (dialog, maximized) = window_flags(window);
            let decoration = state.decoration.get_or_insert_with(Decoration::default);
            let appearance = Appearance {
                title: if title.is_empty() {
                    "Untitled".into()
                } else {
                    title
                },
                width,
                scale,
                active,
                window_flags: (dialog, maximized),
                hover: hover
                    .as_ref()
                    .filter(|(target, _)| target == window)
                    .map(|(_, action)| *action),
                pressed: self
                    .decoration_press
                    .as_ref()
                    .is_some_and(|(_, target)| target == window),
            };
            // One broken decoration must not fail the render — and the session
            // — so log it and skip this window's decoration.
            if let Err(error) = decoration.update(appearance) {
                tracing::warn!(%error, "decoration update failed; skipping window decoration");
                continue;
            }
            match decoration.elements(renderer, (width, size.h).into(), location, scale) {
                Ok(parts) => elements.extend(parts.into_iter().map(Into::into)),
                Err(error) => {
                    tracing::warn!(%error, "decoration elements failed; skipping window decoration");
                }
            }
        }
        elements
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
            window_flags: (false, false),
            hover: None,
            pressed: false,
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
        look.window_flags.1 = true;
        assert_eq!(
            buttons(&look),
            [
                Some(DecorationAction::Minimize),
                Some(DecorationAction::Restore),
                Some(DecorationAction::Close)
            ]
        );
        look.window_flags.0 = true;
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
        look.window_flags.1 = true;
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
        look.hover = Some(DecorationAction::Close);
        let hovered = nodes((look.width, 57).into(), &look, Theme::dark());
        assert!(matches!(
            normal[0].style,
            NodeStyle::Titlebar {
                close_hovered: false,
                ..
            }
        ));
        assert!(matches!(
            hovered[0].style,
            NodeStyle::Titlebar {
                close_hovered: true,
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
                for (hover, pressed, expected) in [
                    (None, false, 0xff27_2e39),
                    (Some(DecorationAction::Close), false, 0xffb8_3245),
                    (Some(DecorationAction::Close), true, 0xff85_2936),
                ] {
                    look.hover = hover;
                    look.pressed = pressed;
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
                    1 => {
                        look.hover =
                            Some(DecorationAction::Resize(xdg_toplevel::ResizeEdge::Bottom));
                        false
                    }
                    2 => {
                        look.title = "Changed without a client buffer commit".into();
                        true
                    }
                    3 => {
                        look.hover = Some(DecorationAction::Close);
                        true
                    }
                    4 => {
                        look.pressed = true;
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
