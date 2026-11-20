//! Native phone receiver. Only the newest decoded frame waits for the UI.
use enzui_core::theme_file::integration::{ThemeEvent, ThemeIntegration};
use enzui_core::{
    decode_limits::DecodeLimits,
    media::{FrameSeq, Generation, Rgba8Frame, preflight_rgba8},
};
use enzui_widgets::app_shell::{
    ShellLayout, enz_app_shell, enz_window_settings,
    window_state::{ShellWindowEvent, ShellWindowState},
};
use enzui_widgets::video::{LatestFramePresenter, ReadyFrame};
use enzui_widgets::{WindowCommand, enz_button, enz_text_field, enz_title_bar};
use gst_video::prelude::*;
use iced::widget::{column, container, image, mouse_area, responsive, row, text};
use iced::{Element, Length, Point, Size, Subscription, Task};
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;

#[derive(Default)]
pub(super) struct Frames {
    latest: Mutex<Option<Rgba8Frame>>,
    sequence: AtomicU64,
    pub done: AtomicBool,
}

pub(super) fn sink(frames: Arc<Frames>) -> gst_app::AppSink {
    let sink = gst_app::AppSink::builder()
        .caps(
            &gst::Caps::builder("video/x-raw")
                .field("format", "RGBA")
                .build(),
        )
        .sync(false)
        .max_buffers(1)
        .leaky_type(gst_app::AppLeakyType::Downstream)
        .build();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let info = gst_video::VideoInfo::from_caps(
                    sample.caps().ok_or(gst::FlowError::NotNegotiated)?,
                )
                .map_err(|_| gst::FlowError::NotNegotiated)?;
                let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(
                    sample.buffer().ok_or(gst::FlowError::Error)?,
                    &info,
                )
                .map_err(|_| gst::FlowError::Error)?;
                let stride =
                    u64::try_from(frame.plane_stride()[0]).map_err(|_| gst::FlowError::Error)?;
                let plane = frame.plane_data(0).map_err(|_| gst::FlowError::Error)?;
                let geometry =
                    preflight_rgba8(info.width(), info.height(), stride, &DecodeLimits::DEFAULT)
                        .and_then(|g| g.check_byte_len(plane.len()))
                        .map_err(|_| gst::FlowError::Error)?;
                let mut pixels = Vec::with_capacity(geometry.tight_len());
                for y in 0..info.height() as usize {
                    let start = y * geometry.stride();
                    pixels.extend_from_slice(&plane[start..start + geometry.row_bytes()]);
                }
                let seq = frames.sequence.fetch_add(1, Ordering::Relaxed);
                let frame = Rgba8Frame::new(
                    Generation::FIRST,
                    FrameSeq(seq),
                    info.width(),
                    info.height(),
                    pixels,
                )
                .map_err(|_| gst::FlowError::Error)?;
                *frames.latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(frame);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    sink
}

#[derive(Debug, Clone)]
enum Message {
    Tick,
    Theme(Box<ThemeEvent>),
    Ready(ReadyFrame),
    Window(WindowCommand),
    WindowEvent(ShellWindowEvent),
    Move(Point, Size),
    Press,
    Release,
    Back,
    Global(&'static str),
    Draft(String),
    SendText,
}
struct Viewer {
    title: String,
    themes: ThemeIntegration,
    frames: Arc<Frames>,
    presenter: LatestFramePresenter,
    window: ShellWindowState,
    point: Option<Point>,
    pressed: Option<Point>,
    draft: String,
}
impl Viewer {
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Theme(event) => {
                return self
                    .themes
                    .update(*event)
                    .map(|event| Message::Theme(Box::new(event)));
            }
            Message::Tick => {
                if self.frames.done.load(Ordering::Acquire) {
                    return iced::exit();
                }
                let frame = self
                    .frames
                    .latest
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                if let Some(frame) = frame {
                    return self
                        .presenter
                        .offer(frame)
                        .map_or_else(Task::none, |t| t.map(Message::Ready));
                }
            }
            Message::Ready(frame) => {
                return self
                    .presenter
                    .ready(frame)
                    .next_upload
                    .map_or_else(Task::none, |t| t.map(Message::Ready));
            }
            Message::Window(command) => return self.window.run_command(command),
            Message::WindowEvent(event) => {
                return self.window.update(event).map(Message::WindowEvent);
            }
            Message::Move(point, size) => {
                self.point = self
                    .presenter
                    .size()
                    .and_then(|video| video_point(video, size, point))
            }
            Message::Press => self.pressed = self.point,
            Message::Release => {
                if let (Some(start), Some(end)) = (self.pressed.take(), self.point) {
                    if start.distance(end) < 0.012 {
                        super::input(json!({"kind":"tap", "x":start.x, "y":start.y}));
                    } else {
                        super::input(
                            json!({"kind":"swipe", "x":start.x, "y":start.y, "x2":end.x, "y2":end.y}),
                        );
                    }
                }
            }
            Message::Back => {
                self.point = None;
                self.pressed = None;
            }
            Message::Global(action) => super::input(json!({"kind":"global", "action":action})),
            Message::Draft(value) => self.draft = value,
            Message::SendText => {
                if !self.draft.is_empty() {
                    super::input(json!({"kind":"text", "text":std::mem::take(&mut self.draft)}));
                }
            }
        }
        Task::none()
    }
    fn view(&self) -> Element<'_, Message> {
        enz_app_shell(|| {
            let video = responsive(|size| -> Element<'_, Message> {
                let content: Element<'_, Message> = match self.presenter.handle() {
                    Some(handle) => image(handle.clone())
                        .content_fit(iced::ContentFit::Contain)
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .into(),
                    None => container(text("Waiting for phone video…"))
                        .center(Length::Fill)
                        .into(),
                };
                mouse_area(content)
                    .on_move(move |point| Message::Move(point, size))
                    .on_press(Message::Press)
                    .on_release(Message::Release)
                    .on_exit(Message::Back)
                    .on_right_press(Message::Global("back"))
                    .into()
            });
            column![
                row![
                    enz_button("Back", Message::Global("back")).into_element(),
                    enz_button("Home", Message::Global("home")).into_element(),
                    enz_button("Recent apps", Message::Global("recents")).into_element()
                ]
                .spacing(8)
                .padding(8),
                video,
                row![
                    enz_text_field("", "Type on phone", &self.draft, Message::Draft)
                        .on_submit(Message::SendText)
                        .into_element(),
                    enz_button("Send", Message::SendText).into_element()
                ]
                .spacing(8)
                .padding(8)
            ]
            .height(Length::Fill)
            .into()
        })
        .layout(ShellLayout::TitlebarContent)
        .titlebar(|| enz_title_bar().title(&self.title).into_element())
        .on_window_gesture(Message::Window)
        .into()
    }
    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::time::every(Duration::from_millis(16)).map(|_| Message::Tick),
            self.window.subscription().map(Message::WindowEvent),
            self.themes
                .subscription()
                .map(|event| Message::Theme(Box::new(event))),
        ])
    }
}
/// Map the displayed image, excluding its letterbox, to phone coordinates.
fn video_point(video: (u32, u32), size: Size, point: Point) -> Option<Point> {
    if video.0 == 0 || video.1 == 0 || size.width <= 0.0 || size.height <= 0.0 {
        return None;
    }
    let scale = (size.width / video.0 as f32).min(size.height / video.1 as f32);
    let w = video.0 as f32 * scale;
    let h = video.1 as f32 * scale;
    let x = (point.x - (size.width - w) / 2.0) / w;
    let y = (point.y - (size.height - h) / 2.0) / h;
    ((0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y)).then_some(Point::new(x, y))
}
pub(super) fn run(title: String, frames: Arc<Frames>) -> iced::Result {
    let mut settings = enz_window_settings(ShellLayout::TitlebarContent);
    settings.size = Size::new(420.0, 860.0);
    settings.platform_specific.application_id = "org.enzimos.PhoneLink".into();
    enzui_widgets::app_setup::run(
        iced::application(
            move || {
                let (themes, boot) = ThemeIntegration::boot();
                (
                    Viewer {
                        title: title.clone(),
                        themes,
                        frames: frames.clone(),
                        presenter: LatestFramePresenter::new(),
                        window: ShellWindowState::new(),
                        point: None,
                        pressed: None,
                        draft: String::new(),
                    },
                    boot.map(|event| Message::Theme(Box::new(event))),
                )
            },
            Viewer::update,
            Viewer::view,
        )
        .title("Phone Link")
        .subscription(Viewer::subscription)
        .theme(|viewer: &Viewer| viewer.themes.theme().iced_theme())
        .window(settings),
    )
}
