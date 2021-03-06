use crate::{
    event::ChartEvent,
    reactive::StreamExt,
    stock::Stock,
    widgets::{SelectMenuState, TextFieldState},
};
use chrono::{DateTime, Datelike, Duration, Utc};
use derivative::Derivative;
use derive_more::{Display, From, Into};
use derive_new::new;
use math::round;
use once_cell::sync::Lazy;
use reactive_rs::{Broadcast, Stream};
use regex::Regex;
use shrinkwraprs::Shrinkwrap;
use std::{
    cell::RefCell, fmt, marker::PhantomData, num::ParseIntError, ops::Range, rc::Rc, str::FromStr,
};
use strum::IntoEnumIterator;
use strum_macros::EnumIter;
use thiserror::Error;
use tui::layout::Rect;
use typenum::{Unsigned, U2, U20, U50};
use yahoo_finance::Interval;

#[derive(Clone, Debug)]
pub struct App<'r> {
    pub stock: Stock,
    pub ui_state: UiState<'r>,
}

type DateRange = Range<DateTime<Utc>>;

#[derive(Clone, Derivative)]
#[derivative(Debug)]
pub struct UiState<'r> {
    pub date_range: Option<DateRange>,
    pub debug_draw: bool,
    pub frame_rate_counter: Rc<RefCell<FrameRateCounter>>,
    pub indicator: Option<Indicator>,
    pub indicator_menu_state: Rc<RefCell<SelectMenuState<Indicator>>>,
    pub stock_symbol_field_state: Rc<RefCell<TextFieldState>>,
    pub time_frame: TimeFrame,
    pub time_frame_menu_state: Rc<RefCell<SelectMenuState<TimeFrame>>>,
    #[derivative(Debug = "ignore")]
    pub ui_target_areas: Broadcast<'r, (), (UiTarget, Option<Rect>)>,
}

impl<'r> Default for UiState<'r> {
    fn default() -> Self {
        Self {
            date_range: TimeFrame::default().now_date_range(),
            debug_draw: false,
            frame_rate_counter: Rc::new(RefCell::new(FrameRateCounter::new(
                Duration::milliseconds(1_000),
            ))),
            indicator: None,
            indicator_menu_state: Rc::new(RefCell::new({
                let mut menu_state = SelectMenuState::new(Indicator::iter());
                menu_state.allow_empty_selection = true;
                menu_state.select(None).unwrap();
                menu_state
            })),
            stock_symbol_field_state: Rc::new(RefCell::new(TextFieldState::default())),
            time_frame: TimeFrame::default(),
            time_frame_menu_state: Rc::new(RefCell::new({
                let mut menu_state = SelectMenuState::new(TimeFrame::iter());
                menu_state.select(Some(TimeFrame::default())).unwrap();
                menu_state
            })),
            ui_target_areas: Broadcast::new(),
        }
    }
}

pub fn to_date_ranges<'a, S, U, R, C>(
    chart_events: S,
    stock_symbols: U,
    init_stock_symbol: String,
    time_frames: R,
    init_time_frame: TimeFrame,
) -> impl Stream<'a, Item = Option<DateRange>, Context = C>
where
    S: Stream<'a, Item = ChartEvent, Context = C>,
    U: Stream<'a, Item = String>,
    R: Stream<'a, Item = TimeFrame>,
    C: 'a + Clone,
{
    chart_events
        .combine_latest(
            stock_symbols.distinct_until_changed(),
            |(ev, stock_symbol)| (*ev, stock_symbol.clone()),
        )
        .combine_latest(
            time_frames.distinct_until_changed(),
            |((ev, stock_symbol), time_frame)| (*ev, stock_symbol.clone(), *time_frame),
        )
        .fold(
            (
                init_time_frame.now_date_range(),
                init_stock_symbol,
                init_time_frame,
            ),
            |(acc_date_range, acc_stock_symbol, acc_time_frame), (ev, stock_symbol, time_frame)| {
                let noop = || {
                    (
                        acc_date_range.clone(),
                        acc_stock_symbol.clone(),
                        *acc_time_frame,
                    )
                };
                let reset = || {
                    (
                        time_frame.now_date_range(),
                        stock_symbol.clone(),
                        *time_frame,
                    )
                };

                let stock_symbol_changed = acc_stock_symbol != stock_symbol;
                let time_frame_changed = acc_time_frame != time_frame;
                if stock_symbol_changed || time_frame_changed {
                    return reset();
                }

                match ev {
                    ChartEvent::PanBackward if time_frame != &TimeFrame::YearToDate => {
                        let date_range = time_frame.duration().map(|duration| {
                            acc_date_range
                                .as_ref()
                                .map(|acc_date_range| {
                                    let end_date = acc_date_range.start;
                                    (end_date - duration)..end_date
                                })
                                .unwrap()
                        });
                        (date_range, stock_symbol.clone(), *time_frame)
                    }
                    ChartEvent::PanForward if time_frame != &TimeFrame::YearToDate => {
                        let date_range = time_frame.duration().map(|duration| {
                            acc_date_range
                                .as_ref()
                                .map(|acc_date_range| {
                                    let start_date = acc_date_range.end;
                                    start_date..(start_date + duration)
                                })
                                .map(|date_range| {
                                    let max_date_range = time_frame.now_date_range().unwrap();
                                    if date_range.end > max_date_range.end {
                                        max_date_range
                                    } else {
                                        date_range
                                    }
                                })
                                .unwrap()
                        });
                        (date_range, stock_symbol.clone(), *time_frame)
                    }
                    ChartEvent::Reset => reset(),
                    _ => noop(),
                }
            },
        )
        .map(|(date_range, ..)| date_range.clone())
        .distinct_until_changed()
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum UiTarget {
    IndicatorBox,
    IndicatorMenu,
    StockNameButton,
    StockSymbolButton,
    StockSymbolField,
    TimeFrameBox,
    TimeFrameMenu,
}

#[derive(Debug)]
pub struct FrameRateCounter {
    frame_time: u16,
    frames: u16,
    last_interval: DateTime<Utc>,
    update_interval: Duration,
}

impl FrameRateCounter {
    pub fn new(update_interval: Duration) -> Self {
        Self {
            frame_time: 0,
            frames: 0,
            last_interval: Utc::now(),
            update_interval,
        }
    }

    /// Increments the counter. Returns the frame time if the update interval has elapsed.
    pub fn incr(&mut self) -> Option<Duration> {
        self.frames += 1;

        let now = Utc::now();

        if now >= self.last_interval + self.update_interval {
            let frame_time =
                (now - self.last_interval).num_milliseconds() as f64 / self.frames as f64;
            let frame_time = round::floor(frame_time, 0) as u16;
            self.frame_time = frame_time;

            self.frames = 0;

            self.last_interval = now;

            return Some(Duration::milliseconds(frame_time as i64));
        }

        None
    }

    pub fn frame_time(&self) -> Option<Duration> {
        match self.frame_time {
            0 => None,
            frame_time => Some(Duration::milliseconds(frame_time as i64)),
        }
    }
}

#[derive(Clone, Copy, Debug, EnumIter, Eq, PartialEq)]
pub enum Indicator {
    BollingerBands(Period<U20>, StdDevMultiplier<U2>),
    ExponentialMovingAverage(Period<U50>),
    // MovingAverageConvergenceDivergence,
    // RelativeStrengthIndex,
    SimpleMovingAverage(Period<U50>),
}

#[derive(
    Clone, Copy, Debug, Display, Eq, From, Into, new, Ord, PartialEq, PartialOrd, Shrinkwrap,
)]
#[display(fmt = "{}", _0)]
pub struct Period<D: Unsigned>(#[shrinkwrap(main_field)] u16, PhantomData<*const D>);

impl<D> Default for Period<D>
where
    D: Unsigned,
{
    fn default() -> Self {
        Self::new(D::to_u16())
    }
}

impl<D> FromStr for Period<D>
where
    D: Unsigned,
{
    type Err = <u16 as FromStr>::Err;

    fn from_str(src: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(u16::from_str(src)?))
    }
}

#[derive(
    Clone, Copy, Debug, Display, Eq, From, Into, new, Ord, PartialEq, PartialOrd, Shrinkwrap,
)]
#[display(fmt = "{}", _0)]
pub struct StdDevMultiplier<D: Unsigned>(#[shrinkwrap(main_field)] u8, PhantomData<*const D>);

impl<D> Default for StdDevMultiplier<D>
where
    D: Unsigned,
{
    fn default() -> Self {
        Self::new(D::to_u8())
    }
}

impl<D> FromStr for StdDevMultiplier<D>
where
    D: Unsigned,
{
    type Err = <u8 as FromStr>::Err;

    fn from_str(src: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(u8::from_str(src)?))
    }
}

impl FromStr for Indicator {
    type Err = ParseIndicatorError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const BB_PATTERN: &str = r"BB\s*\(\s*(?P<n>\d+)\s*,\s*(?P<k>\d+)\s*\)";
        const EMA_PATTERN: &str = r"EMA\s*\(\s*(?P<n>\d+)\s*\)";
        const SMA_PATTERN: &str = r"SMA\s*\(\s*(?P<n>\d+)\s*\)";

        static BB_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(BB_PATTERN).unwrap());
        static EMA_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(EMA_PATTERN).unwrap());
        static SMA_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(SMA_PATTERN).unwrap());

        if let Some(caps) = BB_REGEX.captures(s) {
            let n = &caps["n"];
            let n = n.parse().map_err(|err| ParseIndicatorError::ParseInt {
                name: "n".to_owned(),
                source: err,
                value: n.to_owned(),
            })?;
            let k = &caps["k"];
            let k = k.parse().map_err(|err| ParseIndicatorError::ParseInt {
                name: "k".to_owned(),
                source: err,
                value: k.to_owned(),
            })?;
            Ok(Indicator::BollingerBands(n, k))
        } else if let Some(caps) = EMA_REGEX.captures(s) {
            let n = &caps["n"];
            let n = n.parse().map_err(|err| ParseIndicatorError::ParseInt {
                name: "n".to_owned(),
                source: err,
                value: n.to_owned(),
            })?;
            Ok(Indicator::ExponentialMovingAverage(n))
        } else if let Some(caps) = SMA_REGEX.captures(s) {
            let n = &caps["n"];
            let n = n.parse().map_err(|err| ParseIndicatorError::ParseInt {
                name: "n".to_owned(),
                source: err,
                value: n.to_owned(),
            })?;
            Ok(Indicator::SimpleMovingAverage(n))
        } else if s == "" {
            Err(ParseIndicatorError::Empty)
        } else {
            Err(ParseIndicatorError::Invalid)
        }
    }
}

#[derive(Debug, Error)]
pub enum ParseIndicatorError {
    #[error("cannot parse indicator from empty string")]
    Empty,
    #[error("invalid indicator literal")]
    Invalid,
    #[error("invalid indicator parameter {}: {}", .name, .value)]
    ParseInt {
        name: String,
        source: ParseIntError,
        value: String,
    },
}

impl fmt::Display for Indicator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BollingerBands(n, k) => write!(f, "BB({}, {})", n, k),
            Self::ExponentialMovingAverage(n) => write!(f, "EMA({})", n),
            // Self::MovingAverageConvergenceDivergence => write!(f, "MACD"),
            // Self::RelativeStrengthIndex => write!(f, "RSI"),
            Self::SimpleMovingAverage(n) => write!(f, "SMA({})", n),
        }
    }
}

#[derive(Clone, Copy, Debug, Derivative, EnumIter, Eq, PartialEq)]
#[derivative(Default)]
pub enum TimeFrame {
    FiveDays,
    #[derivative(Default)]
    OneMonth,
    ThreeMonths,
    SixMonths,
    YearToDate,
    OneYear,
    TwoYears,
    FiveYears,
    TenYears,
    Max,
}

impl TimeFrame {
    pub fn duration(self) -> Option<Duration> {
        match self {
            Self::FiveDays => Some(Duration::days(5)),
            Self::OneMonth => Some(Duration::days(30)),
            Self::ThreeMonths => Some(Duration::days(30 * 3)),
            Self::SixMonths => Some(Duration::days(30 * 6)),
            Self::OneYear => Some(Duration::days(30 * 12)),
            Self::TwoYears => Some(Duration::days(30 * 12 * 2)),
            Self::FiveYears => Some(Duration::days(30 * 12 * 5)),
            Self::TenYears => Some(Duration::days(30 * 12 * 10)),
            _ => None,
        }
    }

    pub fn interval(self) -> Interval {
        match self {
            Self::FiveDays => Interval::_5d,
            Self::OneMonth => Interval::_1mo,
            Self::ThreeMonths => Interval::_3mo,
            Self::SixMonths => Interval::_6mo,
            Self::YearToDate => Interval::_ytd,
            Self::OneYear => Interval::_1y,
            Self::TwoYears => Interval::_2y,
            Self::FiveYears => Interval::_5y,
            Self::TenYears => Interval::_10y,
            Self::Max => Interval::_max,
        }
    }

    pub fn now_date_range(self) -> Option<DateRange> {
        let end_date = Utc::now().date().and_hms(0, 0, 0) + Duration::days(1);

        if self == Self::YearToDate {
            return Some(end_date.with_ordinal(1).unwrap()..end_date);
        }

        self.duration()
            .map(|duration| (end_date - duration)..end_date)
    }
}

impl FromStr for TimeFrame {
    type Err = ParseTimeFrameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "5D" | "5d" => Ok(Self::FiveDays),
            "1M" | "1mo" => Ok(Self::OneMonth),
            "3M" | "3mo" => Ok(Self::ThreeMonths),
            "6M" | "6mo" => Ok(Self::SixMonths),
            "YTD" | "ytd" => Ok(Self::YearToDate),
            "1Y" | "1y" => Ok(Self::OneYear),
            "2Y" | "2y" => Ok(Self::TwoYears),
            "5Y" | "5y" => Ok(Self::FiveYears),
            "10Y" | "10y" => Ok(Self::TenYears),
            "Max" | "max" => Ok(Self::Max),
            "" => Err(ParseTimeFrameError::Empty),
            _ => Err(ParseTimeFrameError::Invalid),
        }
    }
}

#[derive(Debug, Error)]
pub enum ParseTimeFrameError {
    #[error("cannot parse time frame from empty string")]
    Empty,
    #[error("invalid time frame literal")]
    Invalid,
}

impl fmt::Display for TimeFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FiveDays => write!(f, "5D"),
            Self::OneMonth => write!(f, "1M"),
            Self::ThreeMonths => write!(f, "3M"),
            Self::SixMonths => write!(f, "6M"),
            Self::YearToDate => write!(f, "YTD"),
            Self::OneYear => write!(f, "1Y"),
            Self::TwoYears => write!(f, "2Y"),
            Self::FiveYears => write!(f, "5Y"),
            Self::TenYears => write!(f, "10Y"),
            Self::Max => write!(f, "Max"),
        }
    }
}
