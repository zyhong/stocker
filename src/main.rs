use crate::{
    app::{App, Indicator, InputState, TimeFrame, UiState, UiTarget},
    event::{InputEvent, OverlayEvent, OverlayState, SelectMenuEvent, TextFieldEvent},
    reactive::StreamExt as ReactiveStreamExt,
    stock::Stock,
    widgets::SelectMenuState,
};
use argh::FromArgs;
use async_std::stream::{self, StreamExt};
use bimap::BiMap;
use crossterm::{
    cursor,
    event::{Event, EventStream, KeyCode, KeyEvent},
    execute, terminal,
};
use im::hashmap;
use log::debug;
use reactive_rs::{Broadcast, Stream};
use simplelog::{Config as LoggerConfig, LevelFilter, WriteLogger};
use std::{
    cell::RefCell,
    collections::VecDeque,
    fs::File,
    io::{self, Write},
    panic,
    rc::Rc,
    sync::atomic::{self, AtomicBool},
    time,
};
use strum::IntoEnumIterator;
use tui::{backend::CrosstermBackend, layout::Rect, Terminal};

mod app;
mod event;
mod reactive;
mod stock;
mod ui;
mod widgets;

const DEFAULT_SYMBOL: &str = "TSLA";
const TICK_RATE: u64 = 100;

/// Stocks dashboard
#[derive(Debug, FromArgs)]
struct Args {
    /// debug draw
    #[argh(switch)]
    debug_draw: bool,
    /// indicator for technical analysis
    #[argh(option, short = 'i')]
    indicator: Option<Indicator>,
    /// path to log file
    #[argh(option)]
    log_file: Option<String>,
    /// stock symbol
    #[argh(option, short = 's', default = "DEFAULT_SYMBOL.to_owned()")]
    symbol: String,
    /// time frame for historical prices
    #[argh(option, short = 't', default = "TimeFrame::default()")]
    time_frame: TimeFrame,
}

fn setup_terminal() {
    let mut stdout = io::stdout();

    execute!(
        stdout,
        terminal::EnterAlternateScreen,
        cursor::Hide,
        cursor::DisableBlinking,
        crossterm::event::EnableMouseCapture
    )
    .unwrap();

    // Needed for when run in a TTY since TTYs don't actually have an alternate screen.
    //
    // Must be executed after attempting to enter the alternate screen so that it only clears the
    // primary screen if we are running in a TTY.
    //
    // If not running in a TTY, then we just end up clearing the alternate screen which should have
    // no effect.
    execute!(stdout, terminal::Clear(terminal::ClearType::All)).unwrap();

    terminal::enable_raw_mode().unwrap();
}

// Adapted from https://github.com/cjbassi/ytop/blob/89a210f0e5e2de6aa0e8d7a153a21f959d77607e/src/main.rs#L51-L66
fn cleanup_terminal() {
    let mut stdout = io::stdout();

    // Needed for when run in a TTY since TTYs don't actually have an alternate screen.
    //
    // Must be executed before attempting to leave the alternate screen so that it only modifies the
    // primary screen if we are running in a TTY.
    //
    // If not running in a TTY, then we just end up modifying the alternate screen which should have
    // no effect.
    execute!(
        stdout,
        cursor::MoveTo(0, 0),
        terminal::Clear(terminal::ClearType::All)
    )
    .unwrap();

    execute!(
        stdout,
        terminal::LeaveAlternateScreen,
        cursor::Show,
        cursor::EnableBlinking,
        crossterm::event::DisableMouseCapture
    )
    .unwrap();

    terminal::disable_raw_mode().unwrap();
}

// Adapted from https://github.com/cjbassi/ytop/blob/89a210f0e5e2de6aa0e8d7a153a21f959d77607e/src/main.rs#L113-L120
//
// We need to catch panics since we need to close the UI and cleanup the terminal before logging any
// error messages to the screen.
fn setup_panic_hook() {
    panic::set_hook(Box::new(|panic_info| {
        cleanup_terminal();
        better_panic::Settings::auto().create_panic_handler()(panic_info);
    }));
}

#[smol_potat::main]
async fn main() -> anyhow::Result<()> {
    better_panic::install();

    let args: Args = argh::from_env();

    if let Some(log_file) = args.log_file {
        WriteLogger::init(
            LevelFilter::Debug,
            LoggerConfig::default(),
            File::create(log_file)?,
        )?;
    }

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    setup_panic_hook();
    setup_terminal();

    let should_quit = AtomicBool::new(false);

    let ui_target_areas: Broadcast<(), (UiTarget, Option<Rect>)> = Broadcast::new();

    let overlay_state_queue = Rc::new(RefCell::new(VecDeque::new()));

    let overlay_states: Broadcast<(), (UiTarget, OverlayState)> = Broadcast::new();

    let grouped_overlay_states = overlay_states
        .clone()
        .group_by(
            |(ui_target, _)| *ui_target,
            |(_, overlay_state)| *overlay_state,
        )
        .broadcast();

    let active_overlays = event::to_active_overlays(overlay_states.clone()).broadcast();

    let input_events: Broadcast<(), InputEvent> = Broadcast::new();

    let grouped_input_events = input_events
        .clone()
        .group_by(|ev| !matches!(ev, InputEvent::Tick), |ev| *ev)
        .broadcast();

    let user_input_events = grouped_input_events
        .clone()
        .filter(|grouped| grouped.key)
        .switch()
        .broadcast();

    let tick_input_events = grouped_input_events
        .clone()
        .filter(|grouped| !grouped.key)
        .switch()
        .broadcast();

    let mut hotkey_overlay_map = BiMap::new();
    hotkey_overlay_map.insert(KeyCode::Char('i'), UiTarget::IndicatorList);
    hotkey_overlay_map.insert(KeyCode::Char('s'), UiTarget::StockSymbolInput);
    hotkey_overlay_map.insert(KeyCode::Char('t'), UiTarget::TimeFrameList);

    let associated_overlay_map = hashmap! {
        UiTarget::IndicatorBox => UiTarget::IndicatorList,
        UiTarget::IndicatorList => UiTarget::IndicatorList,
        UiTarget::StockName => UiTarget::StockSymbolInput,
        UiTarget::StockSymbol => UiTarget::StockSymbolInput,
        UiTarget::StockSymbolInput => UiTarget::StockSymbolInput,
        UiTarget::TimeFrameBox => UiTarget::TimeFrameList,
        UiTarget::TimeFrameList => UiTarget::TimeFrameList,
    };

    let grouped_user_input_events = event::to_grouped_user_input_events(
        user_input_events.clone(),
        ui_target_areas.clone(),
        active_overlays.clone(),
        hotkey_overlay_map.clone(),
        associated_overlay_map,
    )
    .broadcast();

    let non_overlay_user_input_events = grouped_user_input_events
        .clone()
        .filter(|grouped| grouped.key == None)
        .switch()
        .broadcast();

    let stock_symbol_text_field_map_mouse_funcs = event::to_text_field_map_mouse_funcs(
        ui_target_areas.clone(),
        UiTarget::StockSymbolInput,
        hashmap! {
            Some(UiTarget::StockSymbol) => TextFieldEvent::Toggle,
            Some(UiTarget::StockName) => TextFieldEvent::Toggle,
            None => TextFieldEvent::Deactivate,
        },
    )
    .broadcast();

    let stock_symbol_text_field_events = event::to_text_field_events(
        grouped_user_input_events
            .clone()
            .filter(|grouped| grouped.key == Some(UiTarget::StockSymbolInput))
            .switch(),
        grouped_overlay_states
            .clone()
            .filter(|grouped| grouped.key == UiTarget::StockSymbolInput)
            .switch(),
        hotkey_overlay_map
            .get_by_right(&UiTarget::StockSymbolInput)
            .copied()
            .unwrap(),
        stock_symbol_text_field_map_mouse_funcs.clone(),
        |v| v.to_ascii_uppercase(),
    )
    .broadcast();

    let init_time_frame_select_menu_state = {
        let mut select_menu_state = SelectMenuState::new(TimeFrame::iter());
        select_menu_state.select(Some(args.time_frame))?;
        select_menu_state
    };

    let time_frame_select_menu_events = event::to_select_menu_events(
        grouped_user_input_events
            .clone()
            .filter(|grouped| grouped.key == Some(UiTarget::TimeFrameList))
            .switch(),
        init_time_frame_select_menu_state.clone(),
        grouped_overlay_states
            .clone()
            .filter(|grouped| grouped.key == UiTarget::TimeFrameList)
            .switch(),
        hotkey_overlay_map
            .get_by_right(&UiTarget::TimeFrameList)
            .copied()
            .unwrap(),
        ui_target_areas.clone(),
        UiTarget::TimeFrameList,
        hashmap! {
            Some(UiTarget::TimeFrameBox) => SelectMenuEvent::Toggle,
            None => SelectMenuEvent::Deactivate,
        },
    )
    .broadcast();

    let init_indicator_select_menu_state = {
        let mut select_menu_state = SelectMenuState::new(Indicator::iter());
        select_menu_state.allow_empty_selection = true;
        select_menu_state.select(args.indicator)?;
        select_menu_state
    };

    let indicator_select_menu_events = event::to_select_menu_events(
        grouped_user_input_events
            .clone()
            .filter(|grouped| grouped.key == Some(UiTarget::IndicatorList))
            .switch(),
        init_indicator_select_menu_state.clone(),
        grouped_overlay_states
            .clone()
            .filter(|grouped| grouped.key == UiTarget::IndicatorList)
            .switch(),
        hotkey_overlay_map
            .get_by_right(&UiTarget::IndicatorList)
            .copied()
            .unwrap(),
        ui_target_areas.clone(),
        UiTarget::IndicatorList,
        hashmap! {
            Some(UiTarget::IndicatorBox) => SelectMenuEvent::Toggle,
            None => SelectMenuEvent::Deactivate,
        },
    )
    .broadcast();

    let overlay_events = stock_symbol_text_field_events
        .clone()
        .map(|ev| {
            (
                UiTarget::StockSymbolInput,
                OverlayEvent::TextField(ev.clone()),
            )
        })
        .merge(time_frame_select_menu_events.clone().map(|(ev, ..)| {
            (
                UiTarget::TimeFrameList,
                OverlayEvent::SelectMenu(ev.clone()),
            )
        }))
        .merge(indicator_select_menu_events.clone().map(|(ev, ..)| {
            (
                UiTarget::IndicatorList,
                OverlayEvent::SelectMenu(ev.clone()),
            )
        }))
        .inspect(|(ui_target, ev)| {
            debug!("overlay event: {:?}", (ui_target, ev));
        })
        .broadcast();

    event::queue_overlay_states_for_next_tick(overlay_events.clone(), overlay_state_queue.clone());

    let stock_symbols = stock_symbol_text_field_events
        .clone()
        .fold(args.symbol.clone(), |acc_symbol, ev| {
            if let TextFieldEvent::Accept(symbol) = ev {
                symbol.clone()
            } else {
                acc_symbol.clone()
            }
        })
        .distinct_until_changed()
        .broadcast();

    let time_frames = time_frame_select_menu_events
        .clone()
        .fold(args.time_frame, |acc_time_frame, (ev, ..)| {
            if let SelectMenuEvent::Accept(time_frame) = ev {
                time_frame.as_ref().unwrap().parse().unwrap()
            } else {
                *acc_time_frame
            }
        })
        .distinct_until_changed()
        .inspect(|time_frame| {
            debug!("selected time frame: {:?}", time_frame);
        })
        .broadcast();

    let date_ranges = app::to_date_ranges(
        non_overlay_user_input_events.clone(),
        stock_symbols.clone(),
        time_frames.clone(),
        args.symbol.clone(),
        args.time_frame,
        active_overlays.clone(),
    )
    .broadcast();

    let indicators = indicator_select_menu_events
        .clone()
        .fold(args.indicator, |acc_indicator, (ev, ..)| {
            if let SelectMenuEvent::Accept(indicator) = ev {
                indicator.as_ref().map(|s| s.parse().unwrap())
            } else {
                *acc_indicator
            }
        })
        .distinct_until_changed()
        .broadcast();

    let stock_profiles = stock::to_stock_profiles(stock_symbols.clone())
        .map(|stock_profile| Some(stock_profile.clone()))
        .broadcast();

    let stock_bar_sets = stock::to_stock_bar_sets(
        stock_symbols.clone(),
        time_frames.clone(),
        date_ranges.clone(),
        indicators.clone(),
    )
    .broadcast();

    let stocks = stock_symbols
        .clone()
        .combine_latest(stock_profiles.clone(), |(stock_symbol, stock_profile)| {
            (stock_symbol.clone(), stock_profile.clone())
        })
        .combine_latest(
            stock_bar_sets.clone(),
            |((stock_symbol, stock_profile), stock_bar_set)| Stock {
                bars: stock_bar_set.clone(),
                profile: stock_profile.clone(),
                symbol: stock_symbol.clone(),
                ..Stock::default()
            },
        )
        .broadcast();

    let stock_symbol_input_states =
        event::to_text_field_states(stock_symbol_text_field_events.clone()).broadcast();

    let time_frame_select_menu_states = time_frame_select_menu_events
        .clone()
        .map(|(_, select_menu_state)| select_menu_state.clone())
        .broadcast();

    let indicator_select_menu_states = indicator_select_menu_events
        .clone()
        .map(|(_, select_menu_state)| select_menu_state.clone())
        .broadcast();

    let init_ui_state = UiState {
        date_range: args.time_frame.now_date_range(),
        indicator: args.indicator,
        indicator_menu_state: Rc::new(RefCell::new(init_indicator_select_menu_state.clone())),
        time_frame: args.time_frame,
        time_frame_menu_state: Rc::new(RefCell::new(init_time_frame_select_menu_state.clone())),
        ..UiState::default()
    };

    let ui_states = time_frames
        .clone()
        .combine_latest(date_ranges.clone(), |(time_frame, date_range)| {
            (*time_frame, date_range.clone())
        })
        .combine_latest(
            indicators.clone(),
            |((time_frame, date_range), indicator)| (*time_frame, date_range.clone(), *indicator),
        )
        .combine_latest(
            stock_symbol_input_states.clone(),
            |((time_frame, date_range, indicator), stock_symbol_input_state)| {
                (
                    *time_frame,
                    date_range.clone(),
                    *indicator,
                    stock_symbol_input_state.clone(),
                )
            },
        )
        .combine_latest(
            time_frame_select_menu_states.clone(),
            |(
                (time_frame, date_range, indicator, stock_symbol_input_state),
                time_frame_select_menu_state,
            )| {
                (
                    *time_frame,
                    date_range.clone(),
                    *indicator,
                    stock_symbol_input_state.clone(),
                    time_frame_select_menu_state.clone(),
                )
            },
        )
        .combine_latest(
            indicator_select_menu_states.clone(),
            |(
                (
                    time_frame,
                    date_range,
                    indicator,
                    stock_symbol_input_state,
                    time_frame_select_menu_state,
                ),
                indicator_select_menu_state,
            )| {
                (
                    *time_frame,
                    date_range.clone(),
                    *indicator,
                    stock_symbol_input_state.clone(),
                    time_frame_select_menu_state.clone(),
                    indicator_select_menu_state.clone(),
                )
            },
        )
        .fold(init_ui_state.clone(), {
            let ui_target_areas = ui_target_areas.clone();
            move |acc_ui_state,
                  (
                time_frame,
                date_range,
                indicator,
                stock_symbol_input_state,
                time_frame_select_menu_state,
                indicator_select_menu_state,
            )| UiState {
                date_range: date_range.clone(),
                indicator: *indicator,
                indicator_menu_state: Rc::new(RefCell::new(indicator_select_menu_state.clone())),
                stock_symbol_input_state: stock_symbol_input_state.clone(),
                time_frame: *time_frame,
                time_frame_menu_state: Rc::new(RefCell::new(time_frame_select_menu_state.clone())),
                ui_target_areas: ui_target_areas.clone(),
                ..acc_ui_state.clone()
            }
        })
        .broadcast();

    tick_input_events
        .clone()
        .merge(non_overlay_user_input_events.clone())
        .with_latest_from(stocks.clone(), |(ev, stock)| (*ev, stock.clone()))
        .with_latest_from(ui_states.clone(), |((ev, stock), ui_state)| {
            (*ev, stock.clone(), ui_state.clone())
        })
        .subscribe(|(ev, stock, ui_state)| match ev {
            InputEvent::Key(KeyEvent { code, .. }) => match code {
                KeyCode::Char('q') => {
                    should_quit.store(true, atomic::Ordering::Relaxed);
                }
                KeyCode::Char(_) => {
                    execute!(terminal.backend_mut(), crossterm::style::Print("\x07"),).unwrap();
                }
                _ => {}
            },
            InputEvent::Tick => {
                let app = App {
                    stock: stock.clone(),
                    ui_state: ui_state.clone(),
                };
                terminal
                    .draw(|mut f| {
                        ui::draw(&mut f, &app).expect("draw failed");
                    })
                    .unwrap();
            }
            _ => {}
        });

    let input_event_stream = EventStream::new()
        .filter(|ev| match ev {
            Ok(Event::Key(_)) | Ok(Event::Mouse(_)) => true,
            _ => false,
        })
        .map(|ev| match ev {
            Ok(Event::Key(key_event)) => InputEvent::Key(key_event),
            Ok(Event::Mouse(mouse_event)) => InputEvent::Mouse(mouse_event),
            _ => unreachable!(),
        });
    let tick_stream = stream::interval(time::Duration::from_millis(TICK_RATE));
    let input_tick_stream = tick_stream.map(|()| InputEvent::Tick);
    let mut input_event_stream = input_event_stream.merge(input_tick_stream);

    // draw once before hitting the network, as it is blocking
    stocks.send(Stock {
        symbol: args.symbol.clone(),
        ..Stock::default()
    });
    ui_states.send(init_ui_state);
    input_events.send(InputEvent::Tick);

    // send the initial values
    date_ranges.send(args.time_frame.now_date_range());
    time_frames.send(args.time_frame);
    indicators.send(args.indicator);
    stock_symbols.send(args.symbol);
    stock_symbol_input_states.send(InputState::default());
    time_frame_select_menu_states.send(init_time_frame_select_menu_state);
    indicator_select_menu_states.send(init_indicator_select_menu_state);
    active_overlays.send(None);
    overlay_states.feed(
        vec![
            (UiTarget::StockSymbolInput, OverlayState::default()),
            (UiTarget::TimeFrameList, OverlayState::default()),
            (UiTarget::IndicatorList, OverlayState::default()),
        ]
        .iter(),
    );

    while !should_quit.load(atomic::Ordering::Relaxed) {
        let drained_overlay_states: VecDeque<_> =
            overlay_state_queue.borrow_mut().drain(..).collect();
        for (ui_target, overlay_state) in drained_overlay_states {
            debug!(
                "sending previously queued overlay state: {:?}",
                (ui_target, overlay_state)
            );
            overlay_states.send((ui_target, overlay_state));
        }
        input_events.send(input_event_stream.next().await.unwrap());
    }

    cleanup_terminal();

    Ok(())
}
