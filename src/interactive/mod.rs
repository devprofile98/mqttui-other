use std::collections::HashSet;
use std::io::stdout;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CEvent, KeyCode, KeyEvent, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use json::JsonValue;
use rumqttc::{Client, Connection};
use tui::backend::Backend;
use tui::layout::Rect;
use tui::style::{Color, Modifier, Style};
use tui::text::{Span, Spans};
use tui::widgets::Paragraph;
use tui::Frame;
use tui::{backend::CrosstermBackend, Terminal};
use tui_textarea::TextArea;
use tui_tree_widget::flatten;

use crate::cli::Broker;
use crate::interactive::ui::CursorMove;
use crate::json_view::root_tree_items_from_json;

mod clean_retained;
mod details;
mod info_header;
mod mqtt_history;
mod mqtt_thread;
mod topic_overview;
mod ui;

enum ElementInFocus {
    TopicOverview,
    JsonPayload,
    CleanRetainedPopup(String),
    SearchMode,
}

enum Event {
    Key(KeyEvent),
    MouseClick { column: u16, row: u16 },
    MouseScrollUp,
    MouseScrollDown,
    Tick,
}

enum Refresh {
    /// Update the TUI
    Update,
    /// Skip the update of the TUI
    Skip,
    /// Quit the TUI and return to the shell
    Quit,
}

pub fn show(
    client: Client,
    connection: Connection,
    broker: &Broker,
    subscribe_topic: Vec<String>,
) -> anyhow::Result<()> {
    let mqtt_thread = mqtt_thread::MqttThread::new(client, connection, subscribe_topic)?;
    let mut app = App::new(broker, mqtt_thread);

    enable_raw_mode()?;

    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);

    let mut terminal = Terminal::new(backend)?;

    // Setup input handling
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        const TICK_RATE: Duration = Duration::from_millis(500);

        let mut last_tick = Instant::now();
        loop {
            // poll for tick rate duration, if no events, sent tick event.
            let timeout = TICK_RATE
                .checked_sub(last_tick.elapsed())
                .unwrap_or_default();
            if crossterm::event::poll(timeout).unwrap() {
                match crossterm::event::read().unwrap() {
                    CEvent::Key(key) => tx.send(Event::Key(key)).unwrap(),
                    CEvent::Mouse(mouse) => match mouse.kind {
                        MouseEventKind::ScrollUp => tx.send(Event::MouseScrollUp).unwrap(),
                        MouseEventKind::ScrollDown => tx.send(Event::MouseScrollDown).unwrap(),
                        MouseEventKind::Down(MouseButton::Left) => tx
                            .send(Event::MouseClick {
                                column: mouse.column,
                                row: mouse.row,
                            })
                            .unwrap(),
                        _ => {}
                    },
                    CEvent::FocusGained
                    | CEvent::FocusLost
                    | CEvent::Paste(_)
                    | CEvent::Resize(_, _) => {}
                }
            }
            if last_tick.elapsed() >= TICK_RATE {
                if tx.send(Event::Tick).is_err() {
                    // The receiver is gone → the main thread is finished.
                    // Just end the loop here, reporting this error is not helpful in any form.
                    // If the main loop exited successfully this is planned. If not we cant give a helpful error message here anyway.
                    break;
                }
                last_tick = Instant::now();
            }
        }
    });

    terminal.clear()?;

    let main_loop_result = main_loop(&mut app, &rx, &mut terminal);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    main_loop_result
}

fn terminal_draw<B>(app: &mut App, terminal: &mut Terminal<B>) -> anyhow::Result<()>
where
    B: Backend,
{
    let mut draw_error = None;
    terminal.draw(|f| {
        if let Err(error) = app.draw(f) {
            draw_error = Some(error);
        }
    })?;
    draw_error.map_or(Ok(()), Err)
}

fn main_loop<B>(
    app: &mut App,
    rx: &Receiver<Event>,
    terminal: &mut Terminal<B>,
) -> anyhow::Result<()>
where
    B: Backend,
{
    terminal_draw(app, terminal)?;
    loop {
        let refresh = match rx.recv()? {
            Event::Key(event) => app.on_key(event)?,
            Event::MouseClick { column, row } => app.on_click(column, row)?,
            Event::MouseScrollDown => app.on_down()?,
            Event::MouseScrollUp => app.on_up()?,
            Event::Tick => Refresh::Update,
        };
        match refresh {
            Refresh::Update => terminal_draw(app, terminal)?,
            Refresh::Skip => {}
            Refresh::Quit => break,
        }
    }
    Ok(())
}

struct App<'a> {
    details: details::Details,
    focus: ElementInFocus,
    info_header: info_header::InfoHeader,
    mqtt_thread: mqtt_thread::MqttThread,
    topic_overview: topic_overview::TopicOverview,
    search_box: TextArea<'a>,
}

impl<'a> App<'a> {
    fn new(broker: &Broker, mqtt_thread: mqtt_thread::MqttThread) -> Self {
        Self {
            details: details::Details::default(),
            focus: ElementInFocus::TopicOverview,
            info_header: info_header::InfoHeader::new(broker),
            mqtt_thread,
            topic_overview: topic_overview::TopicOverview::default(),
            search_box: TextArea::default(),
        }
    }

    fn get_json_of_current_topic(&self) -> anyhow::Result<Option<JsonValue>> {
        if let Some(topic) = self.topic_overview.get_selected() {
            let json = self
                .mqtt_thread
                .get_history()?
                .get_last(topic)
                .and_then(|last| last.payload.as_optional_json().cloned());
            Ok(json)
        } else {
            Ok(None)
        }
    }

    fn search_for_word(&self, query: String) -> HashSet<String> {
        if let Ok(historylocked) = self.mqtt_thread.get_history() {
            return historylocked.search(&query);
        }
        HashSet::new()
    }

    #[allow(clippy::too_many_lines)]
    fn on_key(&mut self, key: KeyEvent) -> anyhow::Result<Refresh> {
        let refresh = match &self.focus {
            ElementInFocus::TopicOverview => match key.code {
                KeyCode::Char('q') => Refresh::Quit,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    Refresh::Quit
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    let is_json_on_topic = self.get_json_of_current_topic()?.is_some();
                    if is_json_on_topic {
                        self.focus = ElementInFocus::JsonPayload;
                    }
                    Refresh::Update
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    self.topic_overview.toggle();
                    Refresh::Update
                }
                KeyCode::Down | KeyCode::Char('j') => self.on_down()?,
                KeyCode::Up | KeyCode::Char('k') => self.on_up()?,
                KeyCode::Left | KeyCode::Char('h') => {
                    self.topic_overview.close();
                    Refresh::Update
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    self.topic_overview.open();
                    Refresh::Update
                }
                KeyCode::Home => {
                    let visible = self.mqtt_thread.get_history()?.get_visible_topics(
                        self.topic_overview.get_opened(),
                        self.topic_overview.get_query_items(),
                    );
                    self.topic_overview
                        .change_selected(&visible, CursorMove::Absolute(0));
                    Refresh::Update
                }
                KeyCode::End => {
                    let visible = self.mqtt_thread.get_history()?.get_visible_topics(
                        self.topic_overview.get_opened(),
                        self.topic_overview.get_query_items(),
                    );
                    self.topic_overview
                        .change_selected(&visible, CursorMove::Absolute(usize::MAX));
                    Refresh::Update
                }
                KeyCode::PageUp => {
                    let visible = self.mqtt_thread.get_history()?.get_visible_topics(
                        self.topic_overview.get_opened(),
                        self.topic_overview.get_query_items(),
                    );
                    self.topic_overview
                        .change_selected(&visible, CursorMove::PageUp);
                    Refresh::Update
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let visible = self.mqtt_thread.get_history()?.get_visible_topics(
                        self.topic_overview.get_opened(),
                        self.topic_overview.get_query_items(),
                    );
                    self.topic_overview
                        .change_selected(&visible, CursorMove::PageUp);
                    Refresh::Update
                }
                KeyCode::PageDown => {
                    let visible = self.mqtt_thread.get_history()?.get_visible_topics(
                        self.topic_overview.get_opened(),
                        self.topic_overview.get_query_items(),
                    );
                    self.topic_overview
                        .change_selected(&visible, CursorMove::PageDown);
                    Refresh::Update
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let visible = self.mqtt_thread.get_history()?.get_visible_topics(
                        self.topic_overview.get_opened(),
                        self.topic_overview.get_query_items(),
                    );
                    self.topic_overview
                        .change_selected(&visible, CursorMove::PageDown);
                    Refresh::Update
                }
                KeyCode::Backspace | KeyCode::Delete => {
                    if let Some(topic) = self.topic_overview.get_selected() {
                        self.focus = ElementInFocus::CleanRetainedPopup(topic.to_string());
                        Refresh::Update
                    } else {
                        Refresh::Skip
                    }
                }

                KeyCode::Char('/') => {
                    self.focus = ElementInFocus::SearchMode;
                    Refresh::Update
                }
                _ => Refresh::Skip,
            },
            ElementInFocus::JsonPayload => match key.code {
                KeyCode::Char('q') => Refresh::Quit,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    Refresh::Quit
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    self.focus = ElementInFocus::TopicOverview;
                    Refresh::Update
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    self.details.json_view.toggle_selected();
                    Refresh::Update
                }
                KeyCode::Down | KeyCode::Char('j') => self.on_down()?,
                KeyCode::Up | KeyCode::Char('k') => self.on_up()?,
                KeyCode::Left | KeyCode::Char('h') => {
                    self.details.json_view.key_left();
                    Refresh::Update
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    self.details.json_view.key_right();
                    Refresh::Update
                }
                KeyCode::Home => {
                    self.details.json_view.select_first();
                    Refresh::Update
                }
                KeyCode::End => {
                    let json = self.get_json_of_current_topic()?.unwrap_or(JsonValue::Null);
                    let items = root_tree_items_from_json(&json);
                    self.details.json_view.select_last(&items);
                    Refresh::Update
                }
                _ => Refresh::Skip,
            },
            ElementInFocus::CleanRetainedPopup(topic) => {
                if matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) {
                    self.mqtt_thread.clean_below(topic)?;
                }
                self.focus = ElementInFocus::TopicOverview;
                Refresh::Update
            }
            ElementInFocus::SearchMode => match key.code {
                KeyCode::Esc => {
                    self.focus = ElementInFocus::TopicOverview;
                    Refresh::Update
                }
                KeyCode::Enter => {
                    let temp = &self.search_for_word(self.search_box.lines()[0].clone());
                    self.topic_overview.set_opened(temp);
                    Refresh::Update
                }
                _ => {
                    self.search_box.input(key);
                    Refresh::Update
                }
            },
        };
        Ok(refresh)
    }

    fn on_up(&mut self) -> anyhow::Result<Refresh> {
        match self.focus {
            ElementInFocus::TopicOverview => {
                let visible = self.mqtt_thread.get_history()?.get_visible_topics(
                    self.topic_overview.get_opened(),
                    self.topic_overview.get_query_items(),
                );
                self.topic_overview
                    .change_selected(&visible, CursorMove::OneUp);
            }
            ElementInFocus::JsonPayload => {
                let json = self.get_json_of_current_topic()?.unwrap_or(JsonValue::Null);
                let items = root_tree_items_from_json(&json);
                self.details.json_view.key_up(&items);
            }
            ElementInFocus::CleanRetainedPopup(_) => self.focus = ElementInFocus::TopicOverview,
            ElementInFocus::SearchMode => {}
        }
        Ok(Refresh::Update)
    }

    fn on_down(&mut self) -> anyhow::Result<Refresh> {
        match self.focus {
            ElementInFocus::TopicOverview => {
                let visible = self.mqtt_thread.get_history()?.get_visible_topics(
                    self.topic_overview.get_opened(),
                    self.topic_overview.get_query_items(),
                );
                self.topic_overview
                    .change_selected(&visible, CursorMove::OneDown);
            }
            ElementInFocus::JsonPayload => {
                let json = self.get_json_of_current_topic()?.unwrap_or(JsonValue::Null);
                let items = root_tree_items_from_json(&json);
                self.details.json_view.key_down(&items);
            }
            ElementInFocus::CleanRetainedPopup(_) => self.focus = ElementInFocus::TopicOverview,
            ElementInFocus::SearchMode => {}
        }
        Ok(Refresh::Update)
    }

    fn on_click(&mut self, column: u16, row: u16) -> anyhow::Result<Refresh> {
        if let Some(index) = self.topic_overview.index_of_click(column, row) {
            let visible = self.mqtt_thread.get_history()?.get_visible_topics(
                self.topic_overview.get_opened(),
                self.topic_overview.get_query_items(),
            );
            let changed = self
                .topic_overview
                .change_selected(&visible, CursorMove::Absolute(index));
            if !changed {
                self.topic_overview.toggle();
            }
            self.focus = ElementInFocus::TopicOverview;
            return Ok(Refresh::Update);
        }

        if let Some(index) = self.details.json_index_of_click(column, row) {
            let json = self.get_json_of_current_topic()?.unwrap_or(JsonValue::Null);
            let items = root_tree_items_from_json(&json);
            let opened = self.details.json_view.get_all_opened();
            let flattened = flatten(&opened, &items);
            if let Some(picked) = flattened.get(index) {
                if picked.identifier == self.details.json_view.selected() {
                    self.details.json_view.toggle_selected();
                } else {
                    self.details.json_view.select(picked.identifier.clone());
                }
                self.focus = ElementInFocus::JsonPayload;
                return Ok(Refresh::Update);
            }
        }
        Ok(Refresh::Skip)
    }

    fn draw<B>(&mut self, f: &mut Frame<B>) -> anyhow::Result<()>
    where
        B: Backend,
    {
        let area = f.size();
        let Rect { width, height, .. } = area;
        debug_assert_eq!(area.x, 0);
        debug_assert_eq!(area.y, 0);
        let header_area = Rect {
            height: 2,
            y: 0,
            ..area
        };
        let main_area = Rect {
            height: height - 3,
            y: 2,
            ..area
        };
        let key_hint_area = Rect {
            height: 1,
            y: height - 1,
            ..area
        };

        self.info_header.draw(
            f,
            header_area,
            self.mqtt_thread.has_connection_err().unwrap(),
            self.topic_overview.get_selected(),
        );
        draw_key_hints(&self, f, key_hint_area, &self.focus);

        let history = self.mqtt_thread.get_history()?;

        let overview_area = self
            .topic_overview
            .get_selected()
            .as_ref()
            .and_then(|selected_topic| history.get(selected_topic))
            .map_or(main_area, |topic_history| {
                let x = width / 3;
                let details_area = Rect {
                    width: width - x,
                    x,
                    ..main_area
                };

                self.details.draw(
                    f,
                    details_area,
                    topic_history,
                    matches!(self.focus, ElementInFocus::JsonPayload),
                );

                Rect {
                    width: x,
                    x: 0,
                    ..main_area
                }
            });

        let (topic_amount, tree_items) =
            history.to_tree_items(self.topic_overview.get_query_items());
        self.topic_overview.ensure_state(&history);
        self.topic_overview.draw(
            f,
            overview_area,
            topic_amount,
            &tree_items,
            matches!(self.focus, ElementInFocus::TopicOverview),
        );
        drop(history);

        if let ElementInFocus::CleanRetainedPopup(topic) = &self.focus {
            clean_retained::draw_popup(f, topic);
        }
        Ok(())
    }
}

fn draw_key_hints<B>(app: &App, f: &mut Frame<B>, area: Rect, focus: &ElementInFocus)
where
    B: Backend,
{
    const STYLE: Style = Style {
        fg: Some(Color::Black),
        bg: Some(Color::White),
        add_modifier: Modifier::BOLD,
        sub_modifier: Modifier::empty(),
    };
    if let ElementInFocus::SearchMode = focus {
        f.render_widget(app.search_box.widget(), area);
        return;
    } else {
        f.render_widget(
            Paragraph::new(Spans::from(match focus {
                ElementInFocus::TopicOverview => vec![
                    Span::styled("q", STYLE),
                    Span::from(" Quit  "),
                    Span::styled("Tab", STYLE),
                    Span::from(" Switch to JSON Payload  "),
                    Span::styled("Del", STYLE),
                    Span::from(" Clean retained  "),
                    Span::styled(" / ", STYLE),
                    Span::from(" Search"),
                ],
                ElementInFocus::JsonPayload => vec![
                    Span::styled("q", STYLE),
                    Span::from(" Quit  "),
                    Span::styled("Tab", STYLE),
                    Span::from(" Switch to Topics  "),
                ],
                ElementInFocus::CleanRetainedPopup(_) => vec![
                    Span::styled("Enter", STYLE),
                    Span::from(" Clean topic tree  "),
                    Span::styled("Any", STYLE),
                    Span::from(" Abort  "),
                ],
                _ => {
                    vec![]
                }
            })),
            area,
        );
    }
}
