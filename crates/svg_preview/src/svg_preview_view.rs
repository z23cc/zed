use std::sync::Arc;

use editor::Editor;
use file_icons::FileIcons;
use futures::channel::oneshot;
use gpui::{
    App, Context, DragMoveEvent, Empty, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Point, Render, RenderImage, ScrollWheelEvent, Styled, Subscription, SvgSize,
    Task, WeakEntity, Window, div, img,
};
use language::{Buffer, BufferEvent};
use smol::channel::Sender;
use ui::prelude::*;
use workspace::item::Item;
use workspace::{Pane, Workspace};

use crate::{OpenFollowingPreview, OpenPreview, OpenPreviewToTheSide};

pub struct SvgPreviewView {
    focus_handle: FocusHandle,
    buffer: Option<Entity<Buffer>>,
    current_svg: Option<Arc<RenderImage>>,
    scale_factor: f32,
    channel: Sender<(Reason, oneshot::Sender<Arc<RenderImage>>)>,
    drag_start: Point<Pixels>,
    image_offset: Point<Pixels>,
    _background_task: Task<()>,
    _buffer_subscription: Option<Subscription>,
    _workspace_subscription: Option<Subscription>,
}

enum Reason {
    ContentChanged(String),
    ScaleChanged(f32),
    RefreshRequested,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SvgPreviewMode {
    /// The preview will always show the contents of the provided editor.
    Default,
    /// The preview will "follow" the last active editor of an SVG file.
    Follow,
}

const DEFAULT_SCALE_FACTOR: f32 = 2.0;

impl SvgPreviewView {
    pub fn new(
        mode: SvgPreviewMode,
        active_editor: Entity<Editor>,
        workspace_handle: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let (channel, rx) =
                smol::channel::unbounded::<(Reason, oneshot::Sender<Arc<RenderImage>>)>();

            let workspace_subscription = (mode == SvgPreviewMode::Follow)
                .then(|| {
                    workspace_handle.upgrade().map(|workspace_handle| {
                        cx.subscribe_in(
                            &workspace_handle,
                            window,
                            move |this: &mut SvgPreviewView,
                                  workspace,
                                  event: &workspace::Event,
                                  window,
                                  cx| {
                                if let workspace::Event::ActiveItemChanged = event {
                                    let workspace_read = workspace.read(cx);
                                    if let Some(active_item) = workspace_read.active_item(cx)
                                        && let Some(editor) = active_item.downcast::<Editor>()
                                        && Self::is_svg_file(&editor, cx)
                                    {
                                        let buffer =
                                            editor.read(cx).buffer().read(cx).as_singleton();
                                        if this.buffer != buffer {
                                            this._buffer_subscription =
                                                Self::create_buffer_subscription(
                                                    buffer.as_ref(),
                                                    window,
                                                    cx,
                                                );
                                            this.current_svg =
                                                Self::render_svg_for_buffer(buffer.as_ref(), cx);
                                            this.buffer = buffer;
                                            cx.notify();
                                        }
                                    }
                                }
                            },
                        )
                    })
                })
                .flatten();

            let buffer = active_editor
                .read(cx)
                .buffer()
                .clone()
                .read_with(cx, |buffer, _cx| buffer.as_singleton());

            let subscription = Self::create_buffer_subscription(buffer.as_ref(), window, cx);

            let image = Self::render_svg_for_buffer(buffer.as_ref(), cx);

            let content = buffer
                .as_ref()
                .map(|buffer| buffer.read(cx).text())
                .unwrap_or_default();
            let renderer = cx.svg_renderer();

            let background_task = cx.background_spawn(async move {
                let mut content = content;
                let mut scale_factor = DEFAULT_SCALE_FACTOR;
                while let Ok((task, tx)) = rx.recv().await {
                    match task {
                        Reason::ContentChanged(new_content) => content = new_content,
                        Reason::ScaleChanged(new_scale) => scale_factor = new_scale,
                        Reason::RefreshRequested => {}
                    };

                    let image = renderer
                        .render_single_frame(
                            content.as_bytes(),
                            SvgSize::ScaleFactor(scale_factor),
                            true,
                        )
                        .map(|frame| Arc::new(RenderImage::new(frame)));

                    if let Ok(image) = image {
                        tx.send(image).ok();
                    }
                }
            });

            let this = Self {
                focus_handle: cx.focus_handle(),
                buffer,
                current_svg: image,
                channel,
                scale_factor: DEFAULT_SCALE_FACTOR,
                drag_start: Default::default(),
                image_offset: Default::default(),
                _buffer_subscription: subscription,
                _workspace_subscription: workspace_subscription,
                _background_task: background_task,
            };
            this.render_image(Reason::RefreshRequested, window, cx);

            this
        })
    }

    fn render_image(&self, reason: Reason, window: &Window, cx: &mut Context<Self>) {
        let (tx, rx) = oneshot::channel();

        let channel = self.channel.clone();

        cx.spawn_in(window, async move |this, cx| {
            channel.send((reason, tx)).await.ok();

            if let Ok(image) = rx.await {
                this.update_in(cx, |view, window, cx| {
                    if let Some(image) = view.current_svg.take() {
                        window.drop_image(image).ok();
                    }
                    view.current_svg = Some(image);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn find_existing_preview_item_idx(
        pane: &Pane,
        editor: &Entity<Editor>,
        cx: &App,
    ) -> Option<usize> {
        let buffer_id = editor.read(cx).buffer().entity_id();
        pane.items_of_type::<SvgPreviewView>()
            .find(|view| {
                view.read(cx)
                    .buffer
                    .as_ref()
                    .is_some_and(|buffer| buffer.entity_id() == buffer_id)
            })
            .and_then(|view| pane.index_for_item(&view))
    }

    pub fn resolve_active_item_as_svg_editor(
        workspace: &Workspace,
        cx: &mut Context<Workspace>,
    ) -> Option<Entity<Editor>> {
        workspace
            .active_item(cx)?
            .act_as::<Editor>(cx)
            .filter(|editor| Self::is_svg_file(&editor, cx))
    }

    fn create_svg_view(
        mode: SvgPreviewMode,
        workspace: &mut Workspace,
        editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<SvgPreviewView> {
        let workspace_handle = workspace.weak_handle();
        SvgPreviewView::new(mode, editor, workspace_handle, window, cx)
    }

    fn create_buffer_subscription(
        buffer: Option<&Entity<Buffer>>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<Subscription> {
        buffer.map(|buffer| {
            cx.subscribe_in(
                buffer,
                window,
                move |this, buffer, event: &BufferEvent, window, cx| match event {
                    BufferEvent::Edited | BufferEvent::Saved => {
                        let content = buffer.read(cx).text();
                        this.render_image(Reason::ContentChanged(content), window, cx);
                    }
                    _ => {}
                },
            )
        })
    }

    fn render_svg_for_buffer(
        buffer: Option<&Entity<Buffer>>,
        cx: &App,
    ) -> Option<Arc<RenderImage>> {
        buffer.and_then(|buffer| {
            cx.svg_renderer()
                .render_single_frame(
                    buffer.read(cx).text().as_bytes(),
                    SvgSize::ScaleFactor(2.),
                    true,
                )
                .map(|frame| Arc::new(RenderImage::new(frame)))
                .ok()
        })
    }

    pub fn is_svg_file(editor: &Entity<Editor>, cx: &App) -> bool {
        let buffer = editor.read(cx).buffer().read(cx);
        if let Some(buffer) = buffer.as_singleton()
            && let Some(file) = buffer.read(cx).file()
        {
            return file
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("svg"))
                .unwrap_or(false);
        }
        false
    }

    pub fn register(workspace: &mut Workspace, _window: &mut Window, _cx: &mut Context<Workspace>) {
        workspace.register_action(move |workspace, _: &OpenPreview, window, cx| {
            if let Some(editor) = Self::resolve_active_item_as_svg_editor(workspace, cx)
                && Self::is_svg_file(&editor, cx)
            {
                let view = Self::create_svg_view(
                    SvgPreviewMode::Default,
                    workspace,
                    editor.clone(),
                    window,
                    cx,
                );
                workspace.active_pane().update(cx, |pane, cx| {
                    if let Some(existing_view_idx) =
                        Self::find_existing_preview_item_idx(pane, &editor, cx)
                    {
                        pane.activate_item(existing_view_idx, true, true, window, cx);
                    } else {
                        pane.add_item(Box::new(view), true, true, None, window, cx)
                    }
                });
                cx.notify();
            }
        });

        workspace.register_action(move |workspace, _: &OpenPreviewToTheSide, window, cx| {
            if let Some(editor) = Self::resolve_active_item_as_svg_editor(workspace, cx)
                && Self::is_svg_file(&editor, cx)
            {
                let editor_clone = editor.clone();
                let view = Self::create_svg_view(
                    SvgPreviewMode::Default,
                    workspace,
                    editor_clone,
                    window,
                    cx,
                );
                let pane = workspace
                    .find_pane_in_direction(workspace::SplitDirection::Right, cx)
                    .unwrap_or_else(|| {
                        workspace.split_pane(
                            workspace.active_pane().clone(),
                            workspace::SplitDirection::Right,
                            window,
                            cx,
                        )
                    });
                pane.update(cx, |pane, cx| {
                    if let Some(existing_view_idx) =
                        Self::find_existing_preview_item_idx(pane, &editor, cx)
                    {
                        pane.activate_item(existing_view_idx, true, true, window, cx);
                    } else {
                        pane.add_item(Box::new(view), false, false, None, window, cx)
                    }
                });
                cx.notify();
            }
        });

        workspace.register_action(move |workspace, _: &OpenFollowingPreview, window, cx| {
            if let Some(editor) = Self::resolve_active_item_as_svg_editor(workspace, cx)
                && Self::is_svg_file(&editor, cx)
            {
                let view =
                    Self::create_svg_view(SvgPreviewMode::Follow, workspace, editor, window, cx);
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.add_item(Box::new(view), true, true, None, window, cx)
                });
                cx.notify();
            }
        });
    }
}

impl Render for SvgPreviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        struct DragStart {
            initial_offset: Point<Pixels>,
        }

        v_flex()
            .id("SvgPreview")
            .key_context("SvgPreview")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .flex()
            .justify_center()
            .items_center()
            .map(|this| {
                if let Some(content) = self.current_svg.clone() {
                    this.on_drag(
                        DragStart {
                            initial_offset: self.image_offset,
                        },
                        {
                            let this = cx.weak_entity();
                            move |_start, position, _, cx| {
                                this.update(cx, |this, _cx| {
                                    this.drag_start = position;
                                })
                                .ok();

                                cx.new(|_| Empty)
                            }
                        },
                    )
                    .on_drag_move(cx.listener(
                        |this, drag_move: &DragMoveEvent<DragStart>, _, cx| {
                            let drag_start = drag_move.drag(cx);
                            this.image_offset = drag_start.initial_offset
                                + drag_move.event.position
                                - drag_move.bounds.origin
                                - this.drag_start;
                            cx.notify();
                        },
                    ))
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                        let delta = event.delta.pixel_delta(px(1.)).y.0;
                        if delta.abs() != 0. {
                            this.scale_factor = (this.scale_factor + delta).clamp(0.25, 20.);
                            dbg!(this.scale_factor);
                            this.render_image(Reason::ScaleChanged(this.scale_factor), window, cx);
                        }
                    }))
                    .child(
                        img(content)
                            .object_fit(gpui::ObjectFit::None)
                            .absolute()
                            .left(self.image_offset.x)
                            .top(self.image_offset.y)
                            .max_w_full()
                            .max_h_full()
                            .with_fallback(|| {
                                h_flex()
                                    .p_4()
                                    .gap_2()
                                    .child(Icon::new(IconName::Warning))
                                    .child("Failed to load SVG file")
                                    .into_any_element()
                            }),
                    )
                } else {
                    this.child(div().p_4().child("No SVG file selected").into_any_element())
                }
            })
    }
}

impl Focusable for SvgPreviewView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for SvgPreviewView {}

impl Item for SvgPreviewView {
    type Event = ();

    fn tab_icon(&self, _window: &Window, cx: &App) -> Option<Icon> {
        self.buffer
            .as_ref()
            .and_then(|buffer| buffer.read(cx).file())
            .and_then(|file| FileIcons::get_icon(file.path(), cx))
            .map(Icon::from_path)
            .or_else(|| Some(Icon::new(IconName::Image)))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.buffer
            .as_ref()
            .and_then(|svg_path| svg_path.read(cx).file())
            .map(|name| format!("Preview {}", name.file_name(cx).display()).into())
            .unwrap_or_else(|| "SVG Preview".into())
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("svg preview: open")
    }

    fn to_item_events(_event: &Self::Event, _f: impl FnMut(workspace::item::ItemEvent)) {}
}
