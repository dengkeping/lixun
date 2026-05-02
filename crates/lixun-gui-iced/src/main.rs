mod icons;
mod ipc;
mod theme;

use std::sync::OnceLock;

use iced::widget::{column, container, image, row, scrollable, svg, text, text_input};
use iced::widget::Id as ScrollableId;use iced::{Color, Element, Length, Subscription, Task};
use iced_layershell::reexport::{Anchor, KeyboardInteractivity, Layer};
use iced_layershell::settings::{LayerShellSettings, Settings};
use iced_layershell::{application, to_layer_message};
use lixun_core::{Calculation, DocId, Hit};
use tokio::sync::{mpsc, Mutex};

use crate::ipc::{IpcClient, IpcEvent};

const SEARCH_LIMIT: u32 = 30;

static EVENT_RX: OnceLock<Mutex<mpsc::UnboundedReceiver<IpcEvent>>> = OnceLock::new();
static IPC: OnceLock<IpcClient> = OnceLock::new();

fn main() -> Result<(), iced_layershell::Error> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    tracing::info!("lixun-gui-iced starting");

    application(Launcher::new, namespace, update, view)
        .subscription(subscription)
        .style(app_style)
        .settings(Settings {
            layer_settings: LayerShellSettings {
                size: Some((640, 420)),
                anchor: Anchor::Top | Anchor::Left | Anchor::Right | Anchor::Bottom,
                layer: Layer::Top,
                keyboard_interactivity: KeyboardInteractivity::Exclusive,
                exclusive_zone: 0,
                ..Default::default()
            },
            ..Default::default()
        })
        .run()
}

fn namespace() -> String {
    "lixun".into()
}

fn app_style(_state: &Launcher, _theme: &iced::Theme) -> iced::theme::Style {
    iced::theme::Style {
        background_color: Color::TRANSPARENT,
        text_color: theme::TITLE_COLOR,
    }
}

#[derive(Default)]
struct Launcher {
    query: String,
    hits: Vec<Hit>,
    pending_hits: Vec<Hit>,
    calculation: Option<Calculation>,
    top_hit: Option<DocId>,
    last_committed_epoch: u64,
    selected_idx: Option<usize>,
    is_loading: bool,
    category_filter: Option<lixun_core::Category>,
}

impl Launcher {
    fn new() -> (Self, Task<Message>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let _ = EVENT_RX.set(Mutex::new(event_rx));
        let _ = IPC.set(IpcClient::new(event_tx));
        (Self::default(), Task::none())
    }
}

#[to_layer_message]
#[derive(Debug, Clone)]
enum Message {
    QueryChanged(String),
    IpcEvent(IpcEvent),
    KeyPressed(iced::keyboard::Key, iced::keyboard::Modifiers),
    SelectNext,
    SelectPrevious,
    ExecuteSelected,
    WebSearch,
    CategorySelected(Option<lixun_core::Category>),
    Hide,
}

fn ipc() -> &'static IpcClient {
    IPC.get().expect("IPC not initialized")
}

fn update(launcher: &mut Launcher, message: Message) -> Task<Message> {
    match message {
        Message::QueryChanged(q) => {
            tracing::debug!(query = %q, "query changed");
            launcher.query = q.clone();

            ipc().bump_session_epoch();
            launcher.hits.clear();
            launcher.pending_hits.clear();
            launcher.calculation = None;
            launcher.top_hit = None;
            launcher.selected_idx = None;

            if !q.is_empty() {
                launcher.is_loading = true;
                ipc().search(q, SEARCH_LIMIT);
            } else {
                launcher.is_loading = false;
            }
            Task::none()
        }
        Message::IpcEvent(event) => {
            match event {
                IpcEvent::SearchChunk {
                    epoch,
                    phase,
                    hits,
                    calculation,
                    top_hit,
                } => {
                    if epoch < launcher.last_committed_epoch {
                        tracing::debug!("dropping stale chunk epoch={}", epoch);
                        return Task::none();
                    }

                    if epoch > launcher.last_committed_epoch {
                        launcher.pending_hits.clear();
                        launcher.last_committed_epoch = epoch;
                    }

                    launcher.pending_hits.extend(hits);
                    if calculation.is_some() {
                        launcher.calculation = calculation;
                    }
                    if top_hit.is_some() {
                        launcher.top_hit = top_hit;
                    }

                    if matches!(phase, lixun_ipc::Phase::Final) {
                        launcher.hits = std::mem::take(&mut launcher.pending_hits);
                        launcher.is_loading = false;
                        launcher.selected_idx = if launcher.hits.is_empty() {
                            None
                        } else {
                            Some(0)
                        };
                        tracing::debug!(
                            "rendered final epoch={} total_hits={}",
                            epoch,
                            launcher.hits.len()
                        );
                    } else {
                        tracing::debug!(
                            "buffered chunk phase={:?} pending={}",
                            phase,
                            launcher.pending_hits.len()
                        );
                    }
                }
            }
            Task::none()
        }
        Message::KeyPressed(key, modifiers) => {
            use iced::keyboard::key::Named;
            if modifiers.is_empty() {
                match key.as_ref() {
                    iced::keyboard::Key::Named(Named::ArrowDown) => {
                        return update(launcher, Message::SelectNext);
                    }
                    iced::keyboard::Key::Named(Named::ArrowUp) => {
                        return update(launcher, Message::SelectPrevious);
                    }
                    iced::keyboard::Key::Named(Named::Enter) => {
                        return update(launcher, Message::ExecuteSelected);
                    }
                    iced::keyboard::Key::Named(Named::Escape) => {
                        return update(launcher, Message::Hide);
                    }
                    _ => {}
                }
            }
            Task::none()
        }
        Message::SelectNext => {
            if launcher.hits.is_empty() {
                return Task::none();
            }
            launcher.selected_idx = Some(match launcher.selected_idx {
                Some(idx) if idx + 1 < launcher.hits.len() => idx + 1,
                Some(idx) => idx,
                None => 0,
            });
            Task::none()
        }
        Message::SelectPrevious => {
            if launcher.hits.is_empty() {
                return Task::none();
            }
            launcher.selected_idx = Some(match launcher.selected_idx {
                Some(idx) if idx > 0 => idx - 1,
                Some(idx) => idx,
                None => 0,
            });
            Task::none()
        }
        Message::ExecuteSelected => {
            if let Some(idx) = launcher.selected_idx {
                if let Some(hit) = launcher.hits.get(idx) {
                    if let Err(e) = execute_action(&hit.action) {
                        tracing::error!("failed to execute action: {}", e);
                    }
                }
            }
            Task::none()
        }
        Message::Hide => iced::exit(),
        Message::WebSearch => {
            if !launcher.query.is_empty() {
                let url = format!("https://duckduckgo.com/?q={}", urlencode(&launcher.query));
                if let Err(e) = std::process::Command::new("xdg-open").arg(&url).spawn() {
                    tracing::error!("failed to open web search: {}", e);
                }
            }
            Task::none()
        }
        Message::CategorySelected(cat) => {
            launcher.category_filter = cat;
            launcher.selected_idx = None;
            Task::none()
        }
        _ => Task::none(),
    }
}

fn execute_action(action: &lixun_core::Action) -> Result<(), std::io::Error> {
    use lixun_core::Action;
    match action {
        Action::OpenUri { uri } => {
            std::process::Command::new("xdg-open").arg(uri).spawn()?;
            Ok(())
        }
        Action::Exec { cmdline, working_dir, terminal } => {
            let mut cmd = if *terminal {
                let mut c = std::process::Command::new("xterm");
                c.arg("-e");
                c.args(cmdline);
                c
            } else {
                let mut c = std::process::Command::new(&cmdline[0]);
                c.args(&cmdline[1..]);
                c
            };
            if let Some(wd) = working_dir {
                cmd.current_dir(wd);
            }
            cmd.spawn()?;
            Ok(())
        }
        Action::OpenFile { path } => {
            std::process::Command::new("xdg-open").arg(path).spawn()?;
            Ok(())
        }
        _ => {
            tracing::warn!("unimplemented action: {:?}", action);
            Ok(())
        }
    }
}

fn subscription(_launcher: &Launcher) -> Subscription<Message> {
    Subscription::batch(vec![
        Subscription::run(ipc_event_stream),
        iced::event::listen_with(|event, _status, _id| match event {
            iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                key, modifiers, ..
            }) => Some(Message::KeyPressed(key, modifiers)),
            _ => None,
        }),
    ])
}

fn ipc_event_stream() -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(
        100,
        |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            use iced::futures::SinkExt;
            let rx = EVENT_RX.get().expect("EVENT_RX not initialized");
            loop {
                let event = {
                    let mut rx = rx.lock().await;
                    rx.recv().await
                };
                match event {
                    Some(ev) => {
                        let _ = output.send(Message::IpcEvent(ev)).await;
                    }
                    None => {
                        tracing::warn!("ipc event channel closed");
                        std::future::pending::<()>().await;
                    }
                }
            }
        },
    )
}

fn view(launcher: &Launcher) -> Element<'_, Message> {
    let input = text_input("Search...", &launcher.query)
        .on_input(Message::QueryChanged)
        .padding([12, 16])
        .size(24)
        .style(theme::search_input_style);

    let mut results_col = column![].spacing(2);

    if let Some(calc) = &launcher.calculation {
        results_col = results_col.push(
            container(
                column![
                    text(&calc.expr).size(13).style(theme::calc_expr_style),
                    text(&calc.result).size(20).style(theme::calc_result_style),
                ]
                .spacing(4),
            )
            .padding([10, 12])
            .width(Length::Fill)
            .style(theme::top_hit_hero_container_style),
        );
    }

    for (idx, hit) in launcher
        .hits
        .iter()
        .enumerate()
        .filter(|(_, h)| match launcher.category_filter {
            None => true,
            Some(cat) => h.category == cat,
        })
    {
        let is_top = launcher
            .top_hit
            .as_ref()
            .map(|t| t.0 == hit.id.0)
            .unwrap_or(false);
        let is_selected = launcher.selected_idx == Some(idx);

        let title_size = if is_top { 16 } else { 14 };
        let icon_size: u16 = if is_top { 36 } else { 28 };

        let mut row_col = column![
            text(&hit.title).size(title_size).style(theme::title_style)
        ]
        .spacing(2);
        if !hit.subtitle.is_empty() {
            row_col = row_col.push(text(&hit.subtitle).size(11).style(theme::subtitle_style));
        }

        let icon_widget: Element<'_, Message> = match icons::resolve_icon(hit, icon_size) {
            Some(path) if path.extension().and_then(|e| e.to_str()) == Some("svg") => {
                svg(svg::Handle::from_path(path))
                    .width(Length::Fixed(icon_size as f32))
                    .height(Length::Fixed(icon_size as f32))
                    .into()
            }
            Some(path) => image(image::Handle::from_path(path))
                .width(Length::Fixed(icon_size as f32))
                .height(Length::Fixed(icon_size as f32))
                .into(),
            None => container(text("")).width(Length::Fixed(icon_size as f32)).into(),
        };

        let row_container = container(
            row![icon_widget, row_col]
                .spacing(10)
                .padding([8, 12])
                .align_y(iced::Alignment::Center),
        )
        .width(Length::Fill);

        let row_container = if is_top {
            row_container.style(theme::top_hit_hero_container_style)
        } else if is_selected {
            row_container.style(theme::top_hit_container_style)
        } else {
            row_container.style(theme::hit_container_style)
        };

        results_col = results_col.push(row_container);
    }

    let status_bar: Option<Element<'_, Message>> = build_status_bar(launcher);

    let chips_bar: Option<Element<'_, Message>> = if launcher.query.is_empty() {
        None
    } else {
        Some(build_category_chips(launcher))
    };

    let mut inner_col = column![input].spacing(0);

    if let Some(cb) = chips_bar {
        inner_col = inner_col.push(cb);
    }

    inner_col = inner_col.push(
        scrollable(results_col)
            .id(ScrollableId::new("results"))
            .height(Length::Fill)
            .style(theme::scrollable_style),
    );

    if let Some(sb) = status_bar {
        inner_col = inner_col.push(sb);
    }

    let inner_content = inner_col;

    let capsule = container(inner_content)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding([12, 0])
        .style(theme::window_container_style);

    container(capsule)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(8)
        .into()
}

fn build_status_bar(launcher: &Launcher) -> Option<Element<'_, Message>> {
    if launcher.query.is_empty() {
        return None;
    }
    if launcher.is_loading {
        return Some(
            container(text("Searching\u{2026}").size(12).style(theme::subtitle_style))
                .padding([6, 14])
                .width(Length::Fill)
                .into(),
        );
    }
    if launcher.hits.is_empty() && launcher.calculation.is_none() {
        let label_text = format!("No results for \u{201C}{}\u{201D}", launcher.query);
        let label = text(label_text).size(12).style(theme::subtitle_style);
        let web_btn = iced::widget::button(text("Search the web").size(12))
            .on_press(Message::WebSearch)
            .padding([4, 10])
            .style(|_theme, _status| iced::widget::button::Style {
                background: Some(iced::Background::Color(theme::TOP_HIT_BG)),
                text_color: theme::TITLE_COLOR,
                border: iced::Border {
                    radius: iced::border::Radius::new(6.0),
                    width: 0.0,
                    color: Color::TRANSPARENT,
                },
                shadow: iced::Shadow::default(),
                snap: false,
            });
        return Some(
            container(
                row![label, iced::widget::Space::new().width(Length::Fill), web_btn]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
            )
            .padding([6, 14])
            .width(Length::Fill)
            .into(),
        );
    }
    None
}

fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn build_category_chips(launcher: &Launcher) -> Element<'_, Message> {
    use iced::widget::button;
    use lixun_core::Category;

    let chips = [
        ("All", None),
        ("Apps", Some(Category::App)),
        ("Files", Some(Category::File)),
        ("Mail", Some(Category::Mail)),
        ("Attachments", Some(Category::Attachment)),
    ];

    let mut row_widgets = row![].spacing(6);

    for (label, cat) in chips {
        let is_active = launcher.category_filter == cat;
        let btn = button(text(label).size(11))
            .padding([4, 12])
            .style(move |theme, status| theme::chip_button_style(theme, status, is_active))
            .on_press(Message::CategorySelected(cat));
        row_widgets = row_widgets.push(btn);
    }

    container(row_widgets)
        .padding(iced::Padding::default().top(4).bottom(2).left(0).right(0))
        .width(Length::Fill)
        .into()
}
