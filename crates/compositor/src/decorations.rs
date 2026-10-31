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

pub(super) const BAR: i32 = 32;
pub(super) const STRIP: i32 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DecorationAction {
    Move,
    Resize,
    Minimize,
    Maximize,
    Restore,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Theme {
    bar: i32,
    strip: i32,
    grip: i32,
    active: (u32, u32, u32),
    inactive: (u32, u32, u32),
    close_hover: u32,
    close_pressed: u32,
    close_hover_foreground: u32,
    grip_hover: u32,
    grip_foreground: u32,
    title_padding: i32,
    title_size: i32,
}

impl Theme {
    const fn dark() -> Self {
        Self {
            bar: BAR,
            strip: STRIP,
            grip: 24,
            active: (0x27_2e_39, 0xe7_eb_f0, 0x0046_5164),
            inactive: (0x20_25_2d, 0x98_a2_b2, 0x30_37_43),
            close_hover: 0xb8_32_45,
            close_pressed: 0x85_29_36,
            close_hover_foreground: 0xff_ff_ff,
            grip_hover: 0xe7_eb_f0,
            grip_foreground: 0x76_83_99,
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
            grip_hover: 0x20_25_2d,
            grip_foreground: 0x76_83_99,
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
    Grip {
        scale: f64,
        active: bool,
        hovered: bool,
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
    let grip = Rectangle::new(
        ((size.w - theme.grip).max(0), size.h).into(),
        (size.w.min(theme.grip), theme.strip).into(),
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
        Node {
            rect: grip,
            style: NodeStyle::Grip {
                scale: appearance.scale,
                active: appearance.active,
                hovered: appearance.hover == Some(DecorationAction::Resize),
                theme,
            },
            action: Some(DecorationAction::Resize),
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
    if appearance.window_flags.1 {
        result.remove(2);
    }
    result
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
    buffers: [MemoryRenderBuffer; 2],
}

impl Default for Decoration {
    fn default() -> Self {
        Self {
            appearance: None,
            nodes: Vec::new(),
            buffers: std::array::from_fn(|_| {
                MemoryRenderBuffer::new(Fourcc::Argb8888, (0, 0), 1, Transform::Normal, None)
            }),
        }
    }
}

impl Decoration {
    fn update(&mut self, appearance: Appearance) -> Result<bool> {
        if self.appearance.as_ref() == Some(&appearance) {
            return Ok(false);
        }
        let next = nodes((appearance.width, 0).into(), &appearance, Theme::dark());
        for (index, (buffer, height)) in self.buffers.iter_mut().zip([BAR, STRIP]).enumerate() {
            if index == 1 && appearance.window_flags.1 {
                continue;
            }
            let node = &next[[0, 2][index]];
            if self
                .nodes
                .get([0, 2][index])
                .is_some_and(|old| old.style == node.style && old.rect.size == node.rect.size)
            {
                continue;
            }
            let mut surface = paint_nodes(&next, height)?;
            let size = (surface.width(), surface.height()).into();
            let mut target = buffer.render();
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
        size: Size<i32, Logical>,
        location: Point<i32, Logical>,
        scale: f64,
    ) -> Result<Vec<MemoryRenderBufferRenderElement<PixmanRenderer>>> {
        let mut rects = vec![self.nodes[0].rect];
        if let Some(grip) = self
            .nodes
            .iter()
            .find(|node| matches!(node.style, NodeStyle::Grip { .. }))
        {
            let mut rect = grip.rect;
            rect.loc.y = size.h;
            rects.push(rect);
        }
        self.buffers
            .iter()
            .zip(rects)
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

fn paint_nodes(nodes: &[Node], height: i32) -> Result<cairo::ImageSurface> {
    let node = &nodes[if height == BAR { 0 } else { 2 }];
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
        NodeStyle::Grip {
            scale,
            active,
            hovered,
            theme,
        } => (*theme, *scale, *active, *hovered, false, ""),
        NodeStyle::Hit => anyhow::bail!("hit-only node cannot be painted"),
    };
    let width = node.rect.size.w;
    let size: Size<i32, Logical> = (width, height).into();
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
    if height == BAR {
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
            .filter(|node| node.rect.loc.y < 0)
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
        context.move_to(x - 4.0, 12.0);
        context.line_to(x + 4.0, 20.0);
        context.move_to(x + 4.0, 12.0);
        context.line_to(x - 4.0, 20.0);
        context.stroke()?;
        context.restore()?;
        draw_buttons(&context, nodes, foreground, theme)?;
    } else {
        draw_grip(&context, width, hovered, theme)?;
    }
    color(&context, border);
    context.rectangle(0.0, f64::from(height - 1), f64::from(width), 1.0);
    context.fill()?;
    context.status()?;
    Ok(surface)
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
        match button.action {
            Some(DecorationAction::Minimize) => {
                context.move_to(x - 4.0, 17.0);
                context.line_to(x + 4.0, 17.0);
            }
            Some(DecorationAction::Maximize) => context.rectangle(x - 4.0, 12.0, 8.0, 8.0),
            Some(DecorationAction::Restore) => {
                context.rectangle(x - 4.0, 13.0, 7.0, 7.0);
                context.move_to(x - 2.0, 11.0);
                context.line_to(x + 4.0, 11.0);
                context.line_to(x + 4.0, 17.0);
            }
            _ => {}
        }
        context.stroke()?;
        context.restore()?;
    }
    Ok(())
}

fn draw_grip(context: &cairo::Context, width: i32, hovered: bool, theme: Theme) -> Result<()> {
    color(
        context,
        if hovered {
            theme.grip_hover
        } else {
            theme.grip_foreground
        },
    );
    context.set_line_width(1.0);
    let right = f64::from(width) - 4.0;
    for offset in [0.0, 4.0, 8.0] {
        context.move_to(right - offset - 3.0, 5.5);
        context.line_to(right - offset, 2.5);
    }
    context.stroke()?;
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
            if window
                .surface_under(point - surface_location.to_f64(), WindowSurfaceType::ALL)
                .is_some()
            {
                return None;
            }
            if !decorated(window)
                || window.geometry().size.is_empty()
                || !self.windows.get(window).is_some_and(|state| state.mapped)
            {
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

    fn paint(appearance: &Appearance, height: i32) -> Result<cairo::ImageSurface> {
        paint_nodes(
            &nodes((appearance.width, 0).into(), appearance, Theme::dark()),
            height,
        )
    }

    #[test]
    fn node_actions_and_cursors_match_hit_test() {
        let nodes = nodes((101, 57).into(), &appearance(1.0), Theme::dark());
        for (index, rect, action, point) in [
            (
                1,
                Rectangle::new((69, -32).into(), (32, 32).into()),
                DecorationAction::Close,
                (80.0, -10.0),
            ),
            (
                2,
                Rectangle::new((77, 57).into(), (24, 8).into()),
                DecorationAction::Resize,
                (80.0, 60.0),
            ),
        ] {
            assert_eq!(nodes[index].rect, rect);
            assert_eq!(nodes[index].action, Some(action));
            let found = hit_test(&nodes, point.into()).expect("node hit");
            assert_eq!(found.action, Some(action));
            assert_eq!(found.cursor, nodes[index].cursor);
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
                .any(|node| node.action == Some(DecorationAction::Resize))
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
            hit(&layout, (f64::from(look.width) - 1.0, -1.0).into()),
            Some(DecorationAction::Close)
        );
        assert_eq!(
            hit(&layout, (f64::from(look.width) + 0.5, -1.0).into()),
            None
        );
    }

    #[test]
    fn hit_regions_match_painted_edges_including_narrow_windows() {
        let layout = nodes((101, 57).into(), &appearance(1.0), Theme::dark());
        for (point, expected) in [
            ((-0.1, -1.0), None),
            ((0.0, -32.1), None),
            ((0.0, -32.0), Some(DecorationAction::Move)),
            ((68.9, -0.1), Some(DecorationAction::Maximize)),
            ((69.0, -32.0), Some(DecorationAction::Close)),
            ((100.9, -0.1), Some(DecorationAction::Close)),
            ((101.0, -1.0), None),
            ((99.0, 0.0), None),
            ((76.9, 57.0), None),
            ((77.0, 57.0), Some(DecorationAction::Resize)),
            ((100.9, 64.9), Some(DecorationAction::Resize)),
            ((99.0, 65.0), None),
            ((101.0, 58.0), None),
        ] {
            assert_eq!(hit(&layout, point.into()), expected, "{point:?}");
        }
        let narrow = nodes((13, 57).into(), &appearance(1.0), Theme::dark());
        assert_eq!(narrow[1].rect, narrow[0].rect);
        assert_eq!(
            narrow[2].rect,
            Rectangle::new((0, 57).into(), (13, 8).into())
        );
        assert_eq!(
            hit(&narrow, (12.9, -1.0).into()),
            Some(DecorationAction::Close)
        );
        assert_eq!(hit(&narrow, (13.0, -1.0).into()), None);
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
        for (scale, widths, bar_height, grip_width) in [
            (1.0, [501.0, 533.0], 32.0, 24.0),
            (1.25, [626.0, 666.0], 40.0, 30.0),
            (2.0, [1002.0, 1066.0], 64.0, 48.0),
        ] {
            let mut renderer = PixmanRenderer::new().expect("renderer");
            let mut image = renderer
                .create_buffer(Fourcc::Argb8888, (1200, 500).into())
                .expect("target");
            let mut tracker = OutputDamageTracker::new((1200, 500), scale, Transform::Normal);
            let mut cache = Decoration::default();
            let mut look = appearance(scale);
            assert!(cache.update(look.clone()).expect("first paint"));
            let mut previous: Option<([_; 2], [_; 2])> = None;
            for step in 0..7 {
                let changed = match step {
                    0 => [true, true],
                    1 => {
                        look.hover = Some(DecorationAction::Resize);
                        [false, true]
                    }
                    2 => {
                        look.title = "Changed without a client buffer commit".into();
                        [true, false]
                    }
                    3 => {
                        look.hover = Some(DecorationAction::Close);
                        [true, true]
                    }
                    4 => {
                        look.pressed = true;
                        [true, false]
                    }
                    5 => {
                        look.active = false;
                        [true, true]
                    }
                    _ => {
                        look.width = 533;
                        [true, false]
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
                let commits = [parts[0].current_commit(), parts[1].current_commit()];
                let ids = [parts[0].id().clone(), parts[1].id().clone()];
                if let Some((old_ids, old_commits)) = previous {
                    assert_eq!(ids, old_ids, "stable buffer IDs");
                    assert_eq!(
                        [commits[0] != old_commits[0], commits[1] != old_commits[1]],
                        changed,
                        "step {step}"
                    );
                }
                previous = Some((ids, commits));
                assert_eq!(
                    parts[0].src().size,
                    (widths[usize::from(step == 6)], bar_height).into()
                );
                assert_eq!(parts[1].src().size, (grip_width, bar_height / 4.0).into());
                let mut target = renderer.bind(&mut image).expect("bind");
                assert!(
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
                        .is_some()
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
