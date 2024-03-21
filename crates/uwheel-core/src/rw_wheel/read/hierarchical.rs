use core::{
    cmp,
    fmt::{self, Display},
};
use time::OffsetDateTime;

use super::{
    super::write::WriterWheel,
    aggregation::{
        conf::{DataLayout, RetentionPolicy},
        maybe::MaybeWheel,
        AggregationWheel,
    },
    plan::{ExecutionPlan, WheelAggregation, WheelRanges},
    window::WindowManager,
};

use crate::{
    aggregator::Aggregator,
    cfg_not_sync,
    cfg_sync,
    delta::DeltaState,
    rw_wheel::read::{
        aggregation::combine_or_insert,
        plan::{CombinedAggregation, WheelAggregations},
    },
    time_internal,
};

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

#[cfg(feature = "profiler")]
use super::stats::Stats;
#[cfg(feature = "profiler")]
use uwheel_stats::profile_scope;

crate::cfg_timer! {
    #[cfg(not(feature = "std"))]
    use alloc::{boxed::Box, rc::Rc};
    #[cfg(feature = "std")]
    use std::rc::Rc;
    use crate::rw_wheel::timer::{RawTimerWheel, TimerError, TimerAction};
    use core::cell::RefCell;
}
use super::aggregation::conf::WheelConf;

/// Type Alias for a Window result
pub type Window<T> = (u64, T);

/// Default Second tick represented in milliseconds
pub const SECOND_TICK_MS: u64 = time::Duration::SECOND.whole_milliseconds() as u64;
/// Default Minute tick represented in milliseconds
pub const MINUTE_TICK_MS: u64 = time::Duration::MINUTE.whole_milliseconds() as u64;
/// Default Hour tick represented in milliseconds
pub const HOUR_TICK_MS: u64 = time::Duration::HOUR.whole_milliseconds() as u64;
/// Default Day tick represented in milliseconds
pub const DAY_TICK_MS: u64 = time::Duration::DAY.whole_milliseconds() as u64;
/// Default Week tick represented in milliseconds
pub const WEEK_TICK_MS: u64 = time::Duration::WEEK.whole_milliseconds() as u64;
/// Default Year tick represented in milliseconds
pub const YEAR_TICK_MS: u64 = WEEK_TICK_MS * 52;

/// Configuration for a Hierarchical Aggregation Wheel
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug)]
pub struct HawConf {
    /// Initial watermark of the wheel
    pub watermark: u64,
    /// Config for the seconds wheel
    pub seconds: WheelConf,
    /// Config for the minutes wheel
    pub minutes: WheelConf,
    /// Config for the hours wheel
    pub hours: WheelConf,
    /// Config for the days wheel
    pub days: WheelConf,
    /// Config for the weeks wheel
    pub weeks: WheelConf,
    /// Config for the years wheel
    pub years: WheelConf,
    /// Optimizer configuration
    pub optimizer: Optimizer,
    /// Flag indicating whether to maintain deltas within the wheel
    pub generate_deltas: bool,
}

impl Default for HawConf {
    fn default() -> Self {
        Self {
            watermark: 0,
            seconds: WheelConf::new(SECOND_TICK_MS, SECONDS),
            minutes: WheelConf::new(MINUTE_TICK_MS, MINUTES),
            hours: WheelConf::new(HOUR_TICK_MS, HOURS),
            days: WheelConf::new(DAY_TICK_MS, DAYS),
            weeks: WheelConf::new(WEEK_TICK_MS, WEEKS),
            years: WheelConf::new(YEAR_TICK_MS, YEARS),
            optimizer: Default::default(),
            generate_deltas: false,
        }
    }
}

impl HawConf {
    /// Configures the initial watermark
    pub fn with_watermark(mut self, watermark: u64) -> Self {
        self.seconds.set_watermark(watermark);
        self.minutes.set_watermark(watermark);
        self.hours.set_watermark(watermark);
        self.days.set_watermark(watermark);
        self.weeks.set_watermark(watermark);
        self.years.set_watermark(watermark);

        self.watermark = watermark;
        self
    }

    /// Configures all wheels with prefix-sum enabled
    pub fn with_prefix_sum(mut self) -> Self {
        self.seconds.set_data_layout(DataLayout::Prefix);
        self.minutes.set_data_layout(DataLayout::Prefix);
        self.hours.set_data_layout(DataLayout::Prefix);
        self.days.set_data_layout(DataLayout::Prefix);
        self.weeks.set_data_layout(DataLayout::Prefix);
        self.years.set_data_layout(DataLayout::Prefix);

        self
    }

    /// Configures a global retention policy across all granularities
    pub fn with_retention_policy(mut self, policy: RetentionPolicy) -> Self {
        self.seconds.set_retention_policy(policy);
        self.minutes.set_retention_policy(policy);
        self.hours.set_retention_policy(policy);
        self.days.set_retention_policy(policy);
        self.weeks.set_retention_policy(policy);
        self.years.set_retention_policy(policy);

        self
    }

    /// Configures the seconds granularity
    pub fn with_seconds(mut self, seconds: WheelConf) -> Self {
        self.seconds = seconds;
        self
    }
    /// Configures the minutes granularity
    pub fn with_minutes(mut self, minutes: WheelConf) -> Self {
        self.minutes = minutes;
        self
    }
    /// Configures the hours granularity
    pub fn with_hours(mut self, hours: WheelConf) -> Self {
        self.hours = hours;
        self
    }
    /// Configures the days granularity
    pub fn with_days(mut self, days: WheelConf) -> Self {
        self.days = days;
        self
    }
    /// Configures the minutes granularity
    pub fn with_weeks(mut self, weeks: WheelConf) -> Self {
        self.weeks = weeks;
        self
    }
    /// Configures the years granularity
    pub fn with_years(mut self, years: WheelConf) -> Self {
        self.years = years;
        self
    }

    /// Configures the wheel to generate and maintain deltas
    pub fn with_deltas(mut self) -> Self {
        self.generate_deltas = true;
        self
    }
}

/// Default capacity of second slots
pub const SECONDS: usize = 60;
/// Default capacity of minute slots
pub const MINUTES: usize = 60;
/// Default capacity of hour slots
pub const HOURS: usize = 24;
/// Default capacity of day slots
pub const DAYS: usize = 7;
/// Default capacity of week slots
pub const WEEKS: usize = 52;
/// Default capacity of year slots
pub const YEARS: usize = 10;

/// A Wheel range representing a closed-open interval of [start, end)
#[derive(Debug, Copy, Hash, PartialEq, Eq, Clone)]
pub struct WheelRange {
    pub(crate) start: OffsetDateTime,
    pub(crate) end: OffsetDateTime,
}
impl WheelRange {
    /// Creates a new WheelRange given the start and end date
    pub fn new(start: OffsetDateTime, end: OffsetDateTime) -> Self {
        Self::from((start, end))
    }
}
impl From<(OffsetDateTime, OffsetDateTime)> for WheelRange {
    fn from(tuple: (OffsetDateTime, OffsetDateTime)) -> Self {
        WheelRange {
            start: tuple.0,
            end: tuple.1,
        }
    }
}

impl WheelRange {
    /// Returns the lowest granularity of the Wheel range
    pub(crate) fn lowest_granularity(&self) -> Granularity {
        let is_seconds = self.start.second() != 0 || self.end.second() != 0;
        let is_minutes = self.start.minute() != 0 || self.end.minute() != 0;
        let is_hours = self.start.hour() != 0 || self.end.hour() != 0;
        let is_days = self.start.day() != 0 || self.end.day() != 0;
        if is_seconds {
            Granularity::Second
        } else if is_minutes {
            Granularity::Minute
        } else if is_hours {
            Granularity::Hour
        } else if is_days {
            Granularity::Day
        } else {
            unimplemented!("Weeks and years not supported");
        }
    }
    /// Returns an estimation of the number of scans
    pub fn scan_estimation(&self) -> i64 {
        let dur = self.duration();
        match self.lowest_granularity() {
            Granularity::Second => dur.whole_seconds(),
            Granularity::Minute => dur.whole_minutes(),
            Granularity::Hour => dur.whole_hours(),
            Granularity::Day => dur.whole_days(),
        }
    }

    /// Returns a Duration object for the range
    #[inline]
    pub fn duration(&self) -> time_internal::Duration {
        time_internal::Duration::seconds((self.end - self.start).whole_seconds())
    }
}

#[derive(Debug, Copy, PartialEq, Clone)]
#[repr(usize)]
pub(crate) enum Granularity {
    Second,
    Minute,
    Hour,
    Day,
}
impl Granularity {
    fn from_usize(value: usize) -> Self {
        match value {
            0 => Granularity::Second,
            1 => Granularity::Minute,
            2 => Granularity::Hour,
            3 => Granularity::Day,
            _ => unreachable!(),
        }
    }
}

#[derive(Default, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
struct WheelFrequencies {
    // sum/count, can get avg slot scans then
    table: [Frequency; 6],
}
impl WheelFrequencies {
    /// Returns wheel granularities that are outliers in access frequency
    #[allow(dead_code)]
    fn outliers(&self) -> Vec<(Granularity, u64)> {
        let percentile = |data: &[Frequency; 6], p: f64| {
            let mut sorted: Vec<_> = data.iter().map(|m| m.avg()).collect();
            sorted.sort();
            let index = (p / 100.0 * sorted.len() as f64) as usize;
            sorted[index - 1] as f64
        };
        let q1 = percentile(&self.table, 25.0);
        let q3 = percentile(&self.table, 75.0);
        let iqr = q3 - q1;
        let lower_bound = q1 - 1.5 * iqr;
        let higher_bound = q3 + 1.5 * iqr;
        self.table
            .iter()
            .enumerate()
            .filter(|(_, value)| {
                value.avg() < lower_bound as u64 || value.avg() > higher_bound as u64
            })
            .map(|(pos, v)| (Granularity::from_usize(pos), v.avg()))
            .collect()
    }
    #[inline]
    fn add(&self, gran: Granularity, value: usize) {
        let slot = &self.table[gran as usize];
        slot.record(value as u64);
    }
}

cfg_not_sync! {
    use core::cell::Cell;
    #[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
    #[derive(Clone, Default)]
    #[doc(hidden)]
    pub struct Frequency {
        sum: Cell<u64>,
        count: Cell<u64>
    }

    impl Frequency {

        #[inline(always)]
        pub fn sum(&self) -> u64 {
            self.sum.get()
        }
        #[inline(always)]
        pub fn count(&self) -> u64 {
            self.count.get()
        }
        #[inline(always)]
        pub fn avg(&self) -> u64 {
            self.sum() / self.count()
        }

        #[inline(always)]
        pub fn record(&self, value: u64) {
            self.sum.set(self.sum.get() + value);
            self.count.set(self.count.get() + 1);
        }
    }
}

cfg_sync! {
    use std::sync::Arc;
    use core::sync::atomic::{AtomicU64, Ordering};
    #[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
    #[derive(Clone, Default)]
    #[doc(hidden)]
    // NOTE: optimize this..
    pub struct Frequency {
        sum: Arc<AtomicU64>,
        count: Arc<AtomicU64>,
    }

    impl Frequency {
        #[inline(always)]
        pub fn sum(&self) -> u64 {
            self.sum.load(Ordering::Relaxed)
        }

        #[inline(always)]
        pub fn count(&self) -> u64 {
            self.count.load(Ordering::Relaxed)
        }

        #[inline(always)]
        pub fn avg(&self) -> u64 {
            self.sum() / self.count()
        }

        #[inline(always)]
        pub fn record(&self, value: u64) {
            self.sum.fetch_add(value, Ordering::Relaxed);
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

}

/// Default threshold for SIMD-based Wheel Aggregations
pub const DEFAULT_SIMD_THRESHOLD: usize = 15000;

/// Optimizer Heuristics
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Debug, Clone, Copy)]
pub struct Heuristics {
    simd_threshold: usize,
}

impl Default for Heuristics {
    fn default() -> Self {
        Self {
            simd_threshold: DEFAULT_SIMD_THRESHOLD,
        }
    }
}

/// Optimization configuration
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Default, Debug, Clone, Copy)]
pub struct Optimizer {
    use_hints: bool,
    heuristics: Heuristics,
}
impl Optimizer {
    /// Sets the use hints flag
    pub fn use_hints(&mut self, use_hints: bool) {
        self.use_hints = use_hints;
    }
}

/// Hierarchical Aggregation Wheel
///
/// Similarly to Hierarchical Wheel Timers, HAW exploits the hierarchical nature of time and utilise several aggregation wheels,
/// each with a different time granularity. This enables a compact representation of aggregates across time
/// with a low memory footprint and makes it highly compressible and efficient to store on disk. HAWs are event-time driven and uses the notion of a Watermark which means that no timestamps are stored as they are implicit in the wheel slots. It is up to the user of the wheel to advance the watermark and thus roll up aggregates continously up the time hierarchy.
/// For instance, to store aggregates with second granularity up to 10 years, we would need the following aggregation wheels:

/// * Seconds wheel with 60 slots
/// * Minutes wheel with 60 slots
/// * Hours wheel with 24 slots
/// * Days wheel with 7 slots
/// * Weeks wheel with 52 slots
/// * Years wheel with 10 slots
///
/// The above scheme results in a total of 213 wheel slots. This is the minimum number of slots
/// required to support rolling up aggregates across 10 years with second granularity.
#[repr(C)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(bound = "A: Default"))]
// #[derive(Clone)]
pub struct Haw<A>
where
    A: Aggregator,
{
    watermark: u64,
    frequencies: WheelFrequencies,
    seconds_wheel: MaybeWheel<A>,
    minutes_wheel: MaybeWheel<A>,
    hours_wheel: MaybeWheel<A>,
    days_wheel: MaybeWheel<A>,
    weeks_wheel: MaybeWheel<A>,
    years_wheel: MaybeWheel<A>,
    window_manager: Option<WindowManager<A>>,
    conf: HawConf,
    delta: DeltaState<A::PartialAggregate>,
    #[cfg(feature = "timer")]
    #[cfg_attr(feature = "serde", serde(skip))]
    timer: Rc<RefCell<RawTimerWheel<TimerAction<A>>>>,
    #[cfg(feature = "profiler")]
    stats: Stats,
}

impl<A> Haw<A>
where
    A: Aggregator,
{
    const MINUTES_AS_SECS: u64 = time::Duration::MINUTE.whole_seconds() as u64;
    const HOURS_AS_SECS: u64 = time::Duration::HOUR.whole_seconds() as u64;
    const DAYS_AS_SECS: u64 = time::Duration::DAY.whole_seconds() as u64;
    const WEEK_AS_SECS: u64 = time::Duration::WEEK.whole_seconds() as u64;
    const YEAR_AS_SECS: u64 = Self::WEEK_AS_SECS * WEEKS as u64;
    const CYCLE_LENGTH_SECS: u64 = Self::CYCLE_LENGTH.whole_seconds() as u64;

    const SECOND_AS_MS: u64 = time::Duration::SECOND.whole_milliseconds() as u64;

    const TOTAL_SECS_IN_WHEEL: u64 = Self::YEAR_AS_SECS * YEARS as u64;
    /// Duration of a full wheel cycle
    pub const CYCLE_LENGTH: time::Duration =
        time::Duration::seconds((Self::YEAR_AS_SECS * (YEARS as u64 + 1)) as i64); // need 1 extra to force full cycle rotation
    /// Total number of wheel slots across all granularities
    pub const TOTAL_WHEEL_SLOTS: usize = SECONDS + MINUTES + HOURS + DAYS + WEEKS + YEARS;

    /// Creates a new Wheel starting from the given time and configuration
    ///
    /// Time is represented as milliseconds
    pub fn new(time: u64, conf: HawConf) -> Self {
        let mut seconds = conf.seconds;
        seconds.set_watermark(time);

        let mut minutes = conf.minutes;
        minutes.set_watermark(time);

        let mut hours = conf.hours;
        hours.set_watermark(time);

        let mut days = conf.days;
        days.set_watermark(time);

        let mut weeks = conf.weeks;
        weeks.set_watermark(time);

        let mut years = conf.years;
        years.set_watermark(time);

        Self {
            watermark: time,
            frequencies: Default::default(),
            seconds_wheel: MaybeWheel::new(seconds),
            minutes_wheel: MaybeWheel::new(minutes),
            hours_wheel: MaybeWheel::new(hours),
            days_wheel: MaybeWheel::new(days),
            weeks_wheel: MaybeWheel::new(weeks),
            years_wheel: MaybeWheel::new(years),
            conf,
            delta: DeltaState::new(time, Vec::new()),
            window_manager: None,
            #[cfg(feature = "timer")]
            timer: Rc::new(RefCell::new(RawTimerWheel::default())),
            #[cfg(feature = "profiler")]
            stats: Stats::default(),
        }
    }

    #[doc(hidden)]
    pub fn set_optimizer_hints(&mut self, hints: bool) {
        self.conf.optimizer.use_hints = hints;
    }

    /// Returns the current DeltaState object
    pub fn delta_state(&self) -> DeltaState<A::PartialAggregate> {
        self.delta.clone()
    }

    /// Returns how many wheel slots are utilised
    pub fn len(&self) -> usize {
        self.seconds_wheel.len()
            + self.minutes_wheel.len()
            + self.hours_wheel.len()
            + self.days_wheel.len()
            + self.weeks_wheel.len()
            + self.years_wheel.len()
    }

    /// Returns true if the internal wheel time has never been advanced
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns true if all slots in the hierarchy are utilised
    pub fn is_full(&self) -> bool {
        self.len() == Self::TOTAL_WHEEL_SLOTS
    }
    /// Returns the current low watermark as [OffsetDateTime]
    pub fn now(&self) -> OffsetDateTime {
        Self::to_offset_date(self.watermark)
    }
    /// Returns memory used in bytes for all levels
    pub fn size_bytes(&self) -> usize {
        let secs = self.seconds_wheel.size_bytes();
        let min = self.minutes_wheel.size_bytes();
        let hr = self.hours_wheel.size_bytes();
        let day = self.days_wheel.size_bytes();
        let week = self.weeks_wheel.size_bytes();
        let year = self.years_wheel.size_bytes();

        secs + min + hr + day + week + year
    }

    /// Returns how many ticks (seconds) are left until the wheel is fully utilised
    pub fn remaining_ticks(&self) -> u64 {
        Self::TOTAL_SECS_IN_WHEEL - self.current_time_in_cycle().whole_seconds() as u64
    }

    /// Returns Duration that represents where the wheel currently is in its cycle
    #[inline]
    pub fn current_time_in_cycle(&self) -> time_internal::Duration {
        let secs = self.seconds_wheel.rotation_count() as u64;
        let min_secs = self.minutes_wheel.rotation_count() as u64 * Self::MINUTES_AS_SECS;
        let hr_secs = self.hours_wheel.rotation_count() as u64 * Self::HOURS_AS_SECS;
        let day_secs = self.days_wheel.rotation_count() as u64 * Self::DAYS_AS_SECS;
        let week_secs = self.weeks_wheel.rotation_count() as u64 * Self::WEEK_AS_SECS;
        let year_secs = self.years_wheel.rotation_count() as u64 * Self::YEAR_AS_SECS;
        let cycle_time = secs + min_secs + hr_secs + day_secs + week_secs + year_secs;
        time_internal::Duration::seconds(cycle_time as i64)
    }

    #[inline]
    const fn to_ms(ts: u64) -> u64 {
        ts * 1000
    }
    #[inline]
    fn to_offset_date(ts: u64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(ts as i64 / 1000).unwrap()
    }

    /// Installs a periodic window
    pub fn window(&mut self, range: time_internal::Duration, slide: time_internal::Duration) {
        assert!(
            range >= slide,
            "Window range must be larger or equal to slide"
        );
        self.window_manager = Some(WindowManager::new(
            self.watermark,
            range.whole_milliseconds() as usize,
            slide.whole_milliseconds() as usize,
        ));
    }

    /// Advances the time of the wheel aligned by the lowest unit (Second)
    #[inline]
    pub(crate) fn advance_to(
        &mut self,
        watermark: u64,
        waw: &mut WriterWheel<A>,
    ) -> Vec<Window<A::PartialAggregate>> {
        let diff = watermark.saturating_sub(self.watermark());
        self.advance(time_internal::Duration::milliseconds(diff as i64), waw)
    }

    /// Advances the wheel by applying a set of deltas where each delta represents the lowest unit of time
    ///
    /// Note that deltas are processed in the order of the iterator. If you have the following deltas
    /// [Some(10),  Some(20)], it will first insert Some(10) into the wheel and then Some(20).
    #[inline]
    pub fn delta_advance(&mut self, deltas: impl IntoIterator<Item = Option<A::PartialAggregate>>) {
        for delta in deltas {
            // TODO: does not new deltas and does not return possible windows
            self.tick(delta);
        }
    }

    /// Advance the watermark of the wheel by the given [time::Duration]
    #[inline(always)]
    pub fn advance(
        &mut self,
        duration: time_internal::Duration,
        waw: &mut WriterWheel<A>,
    ) -> Vec<Window<A::PartialAggregate>> {
        let ticks: usize = duration.whole_seconds() as usize;
        let mut windows = Vec::new();

        if ticks <= Self::CYCLE_LENGTH_SECS as usize {
            for _ in 0..ticks {
                // tick the write wheel and freeze mutable aggregate
                let delta = waw.tick().map(A::freeze);

                // Store delta if configured to
                if self.conf.generate_deltas {
                    self.delta.push(delta);
                }

                // Tick the HAW
                self.tick(delta);

                // maybe handle window if there is any configured
                self.handle_window_maybe(&mut windows);
            }
        } else {
            // Exceeds full cycle length, clear all!
            self.clear();
        }
        windows
    }
    fn handle_window_maybe(&mut self, windows: &mut Vec<Window<A::PartialAggregate>>) {
        // TODO: refactor this..

        let (pairs_remaining, current_pair_len) =
            if let Some(manager) = self.window_manager.as_mut() {
                manager.state.pair_ticks_remaining -= 1;
                (
                    manager.state.pair_ticks_remaining,
                    manager.state.current_pair_len,
                )
            } else {
                return;
            };

        if pairs_remaining == 0 {
            let from = Self::to_offset_date(self.watermark - current_pair_len as u64);
            let to = Self::to_offset_date(self.watermark);
            let pair = self.combine_range(WheelRange::new(from, to));

            let manager = self.window_manager.as_mut().unwrap();
            let state = &mut manager.state;
            let window = &mut manager.window;

            // insert pair into window aggregator
            window.push(pair.unwrap_or(A::IDENTITY));

            // Update pair metadata
            state.update_pair_len();
            state.next_pair_end = self.watermark + state.current_pair_len as u64;
            state.pair_ticks_remaining = state.current_pair_duration().whole_seconds() as usize;

            if self.watermark == state.next_window_end {
                // insert window result
                windows.push((self.watermark, window.query()));
                // clean up
                window.pop();
                // adjust next window end
                state.next_window_end += state.slide as u64;
            }
        }
    }

    /// Clears the state of all wheels
    pub fn clear(&mut self) {
        self.seconds_wheel.clear();
        self.minutes_wheel.clear();
        self.hours_wheel.clear();
        self.days_wheel.clear();
        self.weeks_wheel.clear();
        self.years_wheel.clear();
    }

    /// Return the current watermark as milliseconds for this wheel
    #[inline]
    pub fn watermark(&self) -> u64 {
        self.watermark
    }
    /// Returns the aggregate in the given time interval
    pub fn interval_and_lower(&self, dur: time_internal::Duration) -> Option<A::Aggregate> {
        self.interval(dur).map(|partial| A::lower(partial))
    }

    /// Returns the execution plan for a given combine range query
    #[inline]
    pub fn explain_combine_range(&self, range: impl Into<WheelRange>) -> Option<ExecutionPlan> {
        Some(self.create_exec_plan(range.into()))
    }

    /// Combines partial aggregates within the given date range and lowers it to a final aggregate
    #[inline]
    pub fn combine_range_and_lower(&self, range: impl Into<WheelRange>) -> Option<A::Aggregate> {
        self.combine_range(range).map(A::lower)
    }

    /// Combines partial aggregates within the given date range [start, end) into a final partial aggregate
    #[inline]
    pub fn combine_range(&self, range: impl Into<WheelRange>) -> Option<A::PartialAggregate> {
        self.combine_range_inner(range).0
    }

    /// Executes a combine range query and returns the result + cost (combine ops) of executing it
    #[inline]
    pub fn analyze_combine_range(
        &self,
        range: impl Into<WheelRange>,
    ) -> (Option<A::PartialAggregate>, usize) {
        self.combine_range_inner(range)
    }

    // for benching purposes right now
    #[doc(hidden)]
    pub fn convert_all_to_prefix(&mut self) {
        self.seconds_wheel.as_mut().unwrap().to_prefix_array();
        self.minutes_wheel.as_mut().unwrap().to_prefix_array();
        self.hours_wheel.as_mut().unwrap().to_prefix_array();
        self.days_wheel.as_mut().unwrap().to_prefix_array();
    }

    // for benching purposes right now
    #[doc(hidden)]
    pub fn convert_all_to_array(&mut self) {
        self.seconds_wheel.as_mut().unwrap().to_array();
        self.minutes_wheel.as_mut().unwrap().to_array();
        self.hours_wheel.as_mut().unwrap().to_array();
        self.days_wheel.as_mut().unwrap().to_array();
    }

    /// Combines partial aggregates within the given date range [start, end) into a final partial aggregate
    #[inline]
    fn combine_range_inner(
        &self,
        range: impl Into<WheelRange>,
    ) -> (Option<A::PartialAggregate>, usize) {
        #[cfg(feature = "profiler")]
        profile_scope!(&self.stats.combine_range);

        let range = range.into();
        let WheelRange { start, end } = range;

        assert!(end > start, "End date needs to be larger than start date");

        // create the best possible execution plan
        let exec = self.create_exec_plan(range);

        match exec {
            ExecutionPlan::WheelAggregation(wheel_agg) => {
                (self.wheel_aggregation(wheel_agg.range), wheel_agg.cost())
            }
            ExecutionPlan::CombinedAggregation(combined) => self.combined_aggregation(combined),
            ExecutionPlan::LandmarkAggregation => self.analyze_landmark(),
            ExecutionPlan::InverseLandmarkAggregation(wheel_aggs) => {
                let (result, cost) = self.inverse_landmark_aggregation(wheel_aggs);
                (Some(result), cost)
            }
        }
    }

    /// Performs a given Combined Aggregation and returns the partial aggregate and the cost of the operation
    fn inverse_landmark_aggregation(
        &self,
        wheel_aggregations: WheelAggregations,
    ) -> (A::PartialAggregate, usize) {
        #[cfg(feature = "profiler")]
        profile_scope!(&self.stats.inverse_landmark);

        // get landmark partial
        let (landmark, lcost) = self.analyze_landmark();
        let combine_inverse = A::combine_inverse().unwrap(); // assumed to be safe as it has been verified by the plan generation

        wheel_aggregations
            .iter()
            .fold((landmark.unwrap_or(A::IDENTITY), lcost), |mut acc, plan| {
                let cost = plan.cost();
                let agg = self.wheel_aggregation(plan.range).unwrap_or(A::IDENTITY);
                acc.0 = combine_inverse(acc.0, agg);
                acc.1 += cost;
                acc
            })
    }

    /// Performs a given Combined Aggregation and returns the partial aggregate and the cost of the operation
    #[inline]
    fn combined_aggregation(
        &self,
        combined: CombinedAggregation,
    ) -> (Option<A::PartialAggregate>, usize) {
        #[cfg(feature = "profiler")]
        profile_scope!(&self.stats.combined_aggregation);

        let cost = combined.cost();
        let agg = combined
            .aggregations
            .into_iter()
            .fold(None, |mut acc, wheel_agg| {
                match self.wheel_aggregation(wheel_agg.range) {
                    Some(agg) => {
                        combine_or_insert::<A>(&mut acc, agg);
                        acc
                    }
                    None => acc,
                }
            });
        (agg, cost)
    }

    /// Returns the best logical plan possible for a given wheel range
    #[inline]
    fn create_exec_plan(&self, range: WheelRange) -> ExecutionPlan {
        #[cfg(feature = "profiler")]
        profile_scope!(&self.stats.logical_plan);

        let mut best_plan = ExecutionPlan::WheelAggregation(self.wheel_aggregation_plan(range));

        let wheel_start = self
            .watermark()
            .saturating_sub(self.current_time_in_cycle().whole_milliseconds() as u64);

        let end_ms = Self::to_ms(range.end.unix_timestamp() as u64);
        let start_ms = Self::to_ms(range.start.unix_timestamp() as u64);

        // Landmark optimization: landmark covers the whole range
        if start_ms <= wheel_start && end_ms >= self.watermark() {
            best_plan = ExecutionPlan::LandmarkAggregation;
        }

        // Early return if plan supports single-wheel prefix scan or if landmark aggregation is possible (both O(1) complexity)
        // Or if the range duration is lower than the lowest granularity, then we cannot execute a combined aggreation
        if best_plan.is_prefix_or_landmark() || range.duration().whole_seconds() < SECONDS as i64 {
            return best_plan;
        }

        if A::invertible() {
            let mut aggregations = WheelAggregations::default();

            // always include: [wheel_start, start]
            aggregations.push(self.wheel_aggregation_plan(WheelRange::new(
                Self::to_offset_date(wheel_start),
                range.start,
            )));

            // if range.end != current watermark also include [end, watermark_now]
            if end_ms < self.watermark() {
                aggregations.push(self.wheel_aggregation_plan(WheelRange::new(
                    range.end,
                    Self::to_offset_date(self.watermark()),
                )));
            }

            let inverse_plan = ExecutionPlan::InverseLandmarkAggregation(aggregations);

            best_plan = cmp::min(best_plan, inverse_plan);
        }

        // Check whether it is worth to create a combined plan as it comes with some overhead
        let combined_aggregation = {
            let scan_estimation = best_plan.cost();

            if self.conf.optimizer.use_hints && A::simd_support() {
                scan_estimation > self.conf.optimizer.heuristics.simd_threshold
            } else {
                true // always generate a combined aggregation plan
            }
        };

        // if the range can be split into multiple non-overlapping ranges
        if combined_aggregation {
            // NOTE: could create multiple combinations of combined aggregations to check
            let combined_plan = ExecutionPlan::CombinedAggregation(
                self.combined_aggregation_plan(Self::split_wheel_ranges(range)),
            );
            best_plan = cmp::min(best_plan, combined_plan);
        }

        best_plan
    }

    /// Given a [start, end) interval, returns the cost (number of ⊕ operations) for a given wheel aggregation
    #[inline]
    fn wheel_aggregation_plan(&self, range: WheelRange) -> WheelAggregation {
        let plan = match range.lowest_granularity() {
            Granularity::Second => self
                .seconds_wheel
                .aggregate_plan(&range, Granularity::Second),
            Granularity::Minute => self
                .minutes_wheel
                .aggregate_plan(&range, Granularity::Minute),
            Granularity::Hour => self.hours_wheel.aggregate_plan(&range, Granularity::Hour),
            Granularity::Day => self.days_wheel.aggregate_plan(&range, Granularity::Day),
        };
        WheelAggregation::new(range, plan)
    }

    /// Logically splits the wheel range into multiple non-overlapping ranges to execute using Combined Aggregation
    #[inline]
    #[doc(hidden)]
    pub fn split_wheel_ranges(range: WheelRange) -> WheelRanges {
        let mut ranges = WheelRanges::default();
        let WheelRange { start, end } = range;

        // initial starting point
        let mut current_start = start;

        // while we have not reached the end date, keep building wheel rangess.
        while current_start < end {
            let current_end = Self::new_curr_end(current_start, end);
            let range = WheelRange {
                start: current_start,
                end: current_end,
            };
            ranges.push(range);
            current_start = current_end;
        }
        ranges
    }

    // helper fn to calculate the next aligned end date
    #[inline]
    fn new_curr_end(current_start: OffsetDateTime, end: OffsetDateTime) -> OffsetDateTime {
        let second = current_start.second();
        let minute = current_start.minute();
        let hour = current_start.hour();
        let day = current_start.day();

        // based on the lowest granularity figure out the duration to next alignment point
        let next = if second > 0 {
            time::Duration::seconds(SECONDS as i64 - second as i64)
        } else if minute > 0 {
            time::Duration::minutes(MINUTES as i64 - minute as i64)
        } else if hour > 0 {
            time::Duration::hours(HOURS as i64 - hour as i64)
        } else if day > 0 {
            // NOTE: fix deadlock issue with certain dates
            // example: start = 2018-08-31 0:00:00, end = 2018-08-31 0:05:00
            time::Duration::days(31 - day as i64) // Needs to be checked
        } else {
            unimplemented!("Weeks and Years not supported yet");
        };

        // TODO: time::checked_add is somewhat expensive.
        // let s = current_start.unix_timestamp() * 1000 + next.whole_milliseconds() as i64;
        // let next_aligned = OffsetDateTime::from_unix_timestamp(s / 1000).unwrap();
        let next_aligned = current_start + next;

        if next_aligned > end {
            // if jumping too far: check the remaining duration between current_start and end
            let rem = end - current_start;
            let rem_secs = rem.whole_seconds();
            let rem_mins = rem.whole_minutes();
            let rem_hours = rem.whole_hours();
            let rem_days = rem.whole_days();

            let remains = [
                (rem_secs, time::Duration::seconds(rem_secs)),
                (rem_mins, time::Duration::minutes(rem_mins)),
                (rem_hours, time::Duration::hours(rem_hours)),
                (rem_days, time::Duration::days(rem_days)),
            ];

            // Jump with lowest remaining duration > 0
            let idx = remains
                .iter()
                .rposition(|&(rem, _)| rem > 0)
                .unwrap_or(remains.len());

            current_start + remains[idx].1
        } else {
            next_aligned
        }
    }

    /// Takes a set of wheel ranges and creates a CombinedAggregation plan
    #[doc(hidden)]
    #[inline]
    pub fn combined_aggregation_plan(&self, ranges: WheelRanges) -> CombinedAggregation {
        #[cfg(feature = "profiler")]
        profile_scope!(&self.stats.combined_aggregation_plan);

        let mut aggregations =
            ranges
                .into_iter()
                .fold(WheelAggregations::default(), |mut acc, range| {
                    acc.push(self.wheel_aggregation_plan(range));
                    acc
                });

        // function that returns a score of the wheel range which is used during sorting
        let granularity_score = |start: &OffsetDateTime, end: &OffsetDateTime| {
            let (sh, sm, ss) = start.time().as_hms();
            let (eh, em, es) = end.time().as_hms();
            if ss > 0 || es > 0 {
                // second rank
                0
            } else if sm > 0 || em > 0 {
                // minute rank
                1
            } else if sh > 0 || eh > 0 {
                // hour rank
                2
            } else {
                // day rank (00:00:00 - 00:00:00)
                3
            }
        };

        // Sort ranges based on lowest granularity in order to execute ranges in order of granularity
        // so that the execution visits wheels sequentially and not randomly.
        aggregations.sort_unstable_by(|a, b| {
            let a_score = granularity_score(&a.range.start, &a.range.end);
            let b_score = granularity_score(&b.range.start, &b.range.end);
            a_score.cmp(&b_score)
        });

        CombinedAggregation::from(aggregations)
    }

    /// Combines partial aggregates within [start, end) using the lowest time granularity wheel
    ///
    /// Note that start date is inclusive while end date is exclusive.
    /// For instance, the range [16:00:50, 16:01:00) refers to the 10 remaining seconds of the minute
    #[inline]
    pub fn wheel_aggregation(&self, range: impl Into<WheelRange>) -> Option<A::PartialAggregate> {
        #[cfg(feature = "profiler")]
        profile_scope!(&self.stats.wheel_aggregation);

        let range = range.into();
        let start = range.start;
        let end = range.end;
        // NOTE: naivé version, still needs checks and can do more optimizations
        assert!(
            end >= start,
            "End date needs to be equal or larger than start date"
        );
        let gran = range.lowest_granularity();

        let (result, scans) = match gran {
            Granularity::Second => {
                let seconds = (end - start).whole_seconds() as usize;
                (
                    self.seconds_wheel
                        .aggregate(start, seconds, Granularity::Second),
                    seconds,
                )
            }
            Granularity::Minute => {
                let minutes = (end - start).whole_minutes() as usize;
                self.frequencies.add(gran, minutes);
                (
                    self.minutes_wheel
                        .aggregate(start, minutes, Granularity::Minute),
                    minutes,
                )
            }
            Granularity::Hour => {
                let hours = (end - start).whole_hours() as usize;
                self.frequencies.add(gran, hours);
                (
                    self.hours_wheel.aggregate(start, hours, Granularity::Hour),
                    hours,
                )
            }
            Granularity::Day => {
                let days = (end - start).whole_days() as usize;
                self.frequencies.add(gran, days);
                (
                    self.days_wheel.aggregate(start, days, Granularity::Day),
                    days,
                )
            }
        };
        // record stats about wheel accesses
        self.frequencies.add(gran, scans);

        result
    }

    /// Schedules a timer to fire once the given time has been reached
    #[cfg(feature = "timer")]
    pub(crate) fn schedule_once(
        &self,
        time: u64,
        f: impl Fn(&Haw<A>) + 'static,
    ) -> Result<(), TimerError<TimerAction<A>>> {
        self.timer
            .borrow_mut()
            .schedule_at(time, TimerAction::Oneshot(Box::new(f)))
    }
    /// Schedules a timer to fire repeatedly
    #[cfg(feature = "timer")]
    pub(crate) fn schedule_repeat(
        &self,
        at: u64,
        interval: time_internal::Duration,
        f: impl Fn(&Haw<A>) + 'static,
    ) -> Result<(), TimerError<TimerAction<A>>> {
        self.timer
            .borrow_mut()
            .schedule_at(at, TimerAction::Repeat((at, interval, Box::new(f))))
    }

    /// Returns the partial aggregate in the given time interval
    pub fn interval(&self, dur: time_internal::Duration) -> Option<A::PartialAggregate> {
        self.interval_with_stats(dur).0
    }

    /// Returns the partial aggregate in the given time interval
    #[inline]
    pub fn interval_with_stats(
        &self,
        dur: time_internal::Duration,
    ) -> (Option<A::PartialAggregate>, usize) {
        #[cfg(feature = "profiler")]
        profile_scope!(&self.stats.interval);

        let to = self.now();
        let from = to - time::Duration::seconds(dur.whole_seconds());
        self.analyze_combine_range(WheelRange::new(from, to))
    }

    /// Executes a Landmark Window that combines total partial aggregates across all wheels
    #[inline]
    pub fn landmark(&self) -> Option<A::PartialAggregate> {
        self.analyze_landmark().0
    }

    /// Executes a Landmark Window that combines total partial aggregates across all wheels and lowers the result
    #[inline]
    pub fn landmark_and_lower(&self) -> Option<A::Aggregate> {
        self.landmark().map(|partial| A::lower(partial))
    }

    /// Executes a Landmark Window that combines total partial aggregates across all wheels and returns the cost
    #[inline]
    pub fn analyze_landmark(&self) -> (Option<A::PartialAggregate>, usize) {
        #[cfg(feature = "profiler")]
        profile_scope!(&self.stats.landmark);

        let wheels = [
            self.seconds_wheel.total(),
            self.minutes_wheel.total(),
            self.hours_wheel.total(),
            self.days_wheel.total(),
            self.weeks_wheel.total(),
            self.years_wheel.total(),
        ];
        Self::reduce(wheels)
    }

    #[inline]
    fn reduce(
        partial_aggs: impl IntoIterator<Item = Option<A::PartialAggregate>>,
    ) -> (Option<A::PartialAggregate>, usize) {
        let mut combines = 0;
        let agg = partial_aggs
            .into_iter()
            .reduce(|acc, b| match (acc, b) {
                (Some(curr), Some(agg)) => {
                    combines += 1;
                    Some(A::combine(curr, agg))
                }
                (None, Some(_)) => b,
                _ => acc,
            })
            .flatten();
        (agg, combines)
    }

    /// Tick the wheel by a single unit (second)
    ///
    /// In the worst case, a tick may cause a rotation of all the wheels in the hierarchy.
    #[inline]
    fn tick(&mut self, partial_opt: Option<A::PartialAggregate>) {
        #[cfg(feature = "profiler")]
        profile_scope!(&self.stats.tick);

        self.watermark += Self::SECOND_AS_MS;

        // if 'None', insert the Identity value
        let partial = partial_opt.unwrap_or(A::IDENTITY);
        let seconds = self.seconds_wheel.get_or_insert();

        seconds.insert_head(partial);

        // full rotation of seconds wheel
        if let Some(rot_data) = seconds.tick() {
            // insert 60 seconds worth of partial aggregates into minute wheel and then tick it
            let minutes = self.minutes_wheel.get_or_insert();

            minutes.insert_slot(rot_data);

            // full rotation of minutes wheel
            if let Some(rot_data) = minutes.tick() {
                // insert 60 minutes worth of partial aggregates into hours wheel and then tick it
                let hours = self.hours_wheel.get_or_insert();

                hours.insert_slot(rot_data);

                // full rotation of hours wheel
                if let Some(rot_data) = hours.tick() {
                    // insert 24 hours worth of partial aggregates into days wheel and then tick it
                    let days = self.days_wheel.get_or_insert();
                    days.insert_slot(rot_data);

                    // full rotation of days wheel
                    if let Some(rot_data) = days.tick() {
                        // insert 7 days worth of partial aggregates into weeks wheel and then tick it
                        let weeks = self.weeks_wheel.get_or_insert();

                        weeks.insert_slot(rot_data);

                        // full rotation of weeks wheel
                        if let Some(rot_data) = weeks.tick() {
                            // insert 1 years worth of partial aggregates into year wheel and then tick it
                            let years = self.years_wheel.get_or_insert();
                            years.insert_slot(rot_data);

                            // tick but ignore full rotations as this is the last hierarchy
                            let _ = years.tick();
                        }
                    }
                }
            }
        }

        // Fire any outgoing timers
        #[cfg(feature = "timer")]
        {
            let mut timer = self.timer.borrow_mut();

            for action in timer.advance_to(self.watermark) {
                match action {
                    TimerAction::Oneshot(udf) => {
                        udf(self);
                    }
                    TimerAction::Repeat((at, interval, udf)) => {
                        udf(self);
                        let new_at = at + interval.whole_milliseconds() as u64;
                        let _ =
                            timer.schedule_at(new_at, TimerAction::Repeat((new_at, interval, udf)));
                    }
                }
            }
        }
    }

    /// Returns a reference to the seconds wheel
    pub fn seconds(&self) -> Option<&AggregationWheel<A>> {
        self.seconds_wheel.as_ref()
    }
    /// Returns an unchecked reference to the seconds wheel
    pub fn seconds_unchecked(&self) -> &AggregationWheel<A> {
        self.seconds_wheel.as_ref().unwrap()
    }

    /// Returns a reference to the minutes wheel
    pub fn minutes(&self) -> Option<&AggregationWheel<A>> {
        self.minutes_wheel.as_ref()
    }
    /// Returns an unchecked reference to the minutes wheel
    pub fn minutes_unchecked(&self) -> &AggregationWheel<A> {
        self.minutes_wheel.as_ref().unwrap()
    }
    /// Returns a reference to the hours wheel
    pub fn hours(&self) -> Option<&AggregationWheel<A>> {
        self.hours_wheel.as_ref()
    }
    /// Returns an unchecked reference to the hours wheel
    pub fn hours_unchecked(&self) -> &AggregationWheel<A> {
        self.hours_wheel.as_ref().unwrap()
    }
    /// Returns a reference to the days wheel
    pub fn days(&self) -> Option<&AggregationWheel<A>> {
        self.days_wheel.as_ref()
    }
    /// Returns an unchecked reference to the days wheel
    pub fn days_unchecked(&self) -> &AggregationWheel<A> {
        self.days_wheel.as_ref().unwrap()
    }

    /// Returns a reference to the weeks wheel
    pub fn weeks(&self) -> Option<&AggregationWheel<A>> {
        self.weeks_wheel.as_ref()
    }

    /// Returns an unchecked reference to the weeks wheel
    pub fn weeks_unchecked(&self) -> &AggregationWheel<A> {
        self.weeks_wheel.as_ref().unwrap()
    }

    /// Returns a reference to the years wheel
    pub fn years(&self) -> Option<&AggregationWheel<A>> {
        self.years_wheel.as_ref()
    }
    /// Returns a reference to the years wheel
    pub fn years_unchecked(&self) -> &AggregationWheel<A> {
        self.years_wheel.as_ref().unwrap()
    }

    /// Merges two wheels
    ///
    /// Note that the time in `other` may be advanced and thus change state
    pub(crate) fn merge(&mut self, other: &mut Self) {
        let other_watermark = other.watermark();

        // make sure both wheels are aligned by time
        if self.watermark() > other_watermark {
            other.advance_to(self.watermark(), &mut WriterWheel::default());
        } else {
            self.advance_to(other_watermark, &mut WriterWheel::default());
        }

        // merge all aggregation wheels
        self.seconds_wheel.merge(&other.seconds_wheel);
        self.minutes_wheel.merge(&other.minutes_wheel);
        self.hours_wheel.merge(&other.hours_wheel);
        self.days_wheel.merge(&other.days_wheel);
        self.weeks_wheel.merge(&other.weeks_wheel);
        self.years_wheel.merge(&other.years_wheel);
    }
    #[cfg(feature = "profiler")]
    /// Returns a reference to the stats of the [HAW]
    pub fn stats(&self) -> &Stats {
        &self.stats
    }
}

/// A type containing error variants that may arise when querying a wheel
#[derive(Copy, Clone, Debug)]
pub enum QueryError {
    /// Invalid Date Range
    Invalid,
}
impl Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryError::Invalid => write!(f, "Invalid Date Range"),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        aggregator::sum::U64SumAggregator,
        rw_wheel::read::plan::Aggregation,
        time_internal::NumericalDuration,
    };
    use time::macros::datetime;

    use super::*;

    #[test]
    fn delta_advance_test() {
        let mut haw: Haw<U64SumAggregator> = Haw::new(0, Default::default());
        // oldest to newest
        let deltas = vec![Some(10), None, Some(50), None];

        // Visualization:
        // 10
        // 0, 10
        // 50, 0, 10
        // 0, 50, 0, 10
        haw.delta_advance(deltas);

        assert_eq!(haw.interval(1.seconds()), Some(0));
        assert_eq!(haw.interval(2.seconds()), Some(50));
        assert_eq!(haw.interval(3.seconds()), Some(50));
        assert_eq!(haw.interval(4.seconds()), Some(60));
    }

    #[test]
    fn range_query_sec_test() {
        // 2023-11-09 00:00:00
        let watermark = 1699488000000;
        let conf = HawConf::default().with_watermark(watermark);
        let mut haw: Haw<U64SumAggregator> = Haw::new(watermark, conf);
        // oldest to newest
        let deltas = vec![Some(10), None, Some(50), None];

        haw.delta_advance(deltas);

        assert_eq!(haw.interval(1.seconds()), Some(0));
        assert_eq!(haw.interval(2.seconds()), Some(50));
        assert_eq!(haw.interval(3.seconds()), Some(50));
        assert_eq!(haw.interval(4.seconds()), Some(60));

        let start = datetime!(2023-11-09 00:00:00 UTC);
        let end = datetime!(2023-11-09 00:00:02 UTC);

        let agg = haw.combine_range((start, end));
        assert_eq!(agg, Some(10));

        let start = datetime!(2023-11-09 00:00:00 UTC);
        let end = datetime!(2023-11-09 00:00:04 UTC);
        let agg = haw.combine_range((start, end));
        assert_eq!(agg, Some(60));
    }

    #[test]
    fn range_query_min_test() {
        // 2023-11-09 00:00:00
        let watermark = 1699488000000;
        let conf = HawConf::default().with_watermark(watermark);
        let mut haw: Haw<U64SumAggregator> = Haw::new(watermark, conf);

        let deltas: Vec<Option<u64>> = (0..180).map(|_| Some(1)).collect();
        haw.delta_advance(deltas);
        assert_eq!(haw.watermark(), 1699488180000);

        // time is 2023-11-09 00:03:00
        // queryable minutes are 00:00 - 00:03
        let start = datetime!(2023-11-09 00:00 UTC);
        let end = datetime!(2023-11-09 00:03 UTC);

        let agg = haw.combine_range((start, end));
        // 1 for each second rolled-up over 3 minutes > 180
        assert_eq!(agg, Some(180));
    }

    #[test]
    fn range_query_hour_test() {
        // 2023-11-09 00:00:00
        let watermark = 1699488000000;
        let conf = HawConf::default().with_watermark(watermark);
        let mut haw: Haw<U64SumAggregator> = Haw::new(watermark, conf);

        let hours_as_secs = 3600;
        let hours = hours_as_secs * 3;
        let deltas: Vec<Option<u64>> = (0..hours).map(|_| Some(1)).collect();
        // advance wheel by 3 hours
        haw.delta_advance(deltas);

        let start = datetime!(2023-11-09 00:00 UTC);
        let end = datetime!(2023-11-09 01:00 UTC);

        assert_eq!(haw.combine_range((start, end)), Some(3600));

        let start = datetime!(2023-11-09 00:00 UTC);
        let end = datetime!(2023-11-09 03:00 UTC);

        assert_eq!(haw.combine_range((start, end)), Some(10800));
    }

    #[test]
    fn range_query_day_test() {
        // 2023-11-09 00:00:00
        let watermark = 1699488000000;
        let conf = HawConf::default()
            .with_watermark(watermark)
            .with_retention_policy(RetentionPolicy::Keep);
        let mut haw: Haw<U64SumAggregator> = Haw::new(watermark, conf);

        let days_as_secs = 3600 * 24;
        let days = days_as_secs * 3;
        let deltas: Vec<Option<u64>> = (0..days).map(|_| Some(1)).collect();
        // advance wheel by 3 days
        haw.delta_advance(deltas);

        let start = datetime!(2023 - 11 - 09 00:00:00 UTC);
        let end = datetime!(2023 - 11 - 10 00:00:00 UTC);

        assert_eq!(haw.combine_range((start, end)), Some(3600 * 24));

        let start = datetime!(2023 - 11 - 09 00:00:00 UTC);
        let end = datetime!(2023 - 11 - 12 00:00:00 UTC);

        assert_eq!(haw.combine_range((start, end)), Some(259200));

        let start = datetime!(2023 - 11 - 09 15:50:50 UTC);
        let end = datetime!(2023 - 11 - 11 12:30:45 UTC);

        // verify that both wheel aggregation using seconds and combine range using multiple wheel aggregations end up the same
        assert_eq!(haw.wheel_aggregation((start, end)), Some(160795));
        assert_eq!(haw.combine_range((start, end)), Some(160795));

        let plan = haw
            .explain_combine_range(WheelRange::new(start, end))
            .unwrap();
        dbg!(&plan);
        let combined_plan = match plan {
            ExecutionPlan::CombinedAggregation(plan) => plan,
            _ => panic!("not supposed to happen"),
        };

        let mut expected = WheelAggregations::default();

        expected.push(WheelAggregation {
            range: WheelRange {
                start: datetime!(2023 - 11 - 09 15:50:50 UTC),
                end: datetime!(2023 - 11 - 09 15:51:00 UTC),
            },
            plan: Aggregation::Scan(10),
        });

        expected.push(WheelAggregation {
            range: WheelRange {
                start: datetime!(2023 - 11 - 11 12:30:00 UTC),
                end: datetime!(2023 - 11 - 11 12:30:45 UTC),
            },
            plan: Aggregation::Scan(45),
        });

        expected.push(WheelAggregation {
            range: WheelRange {
                start: datetime!(2023 - 11 - 09 15:51:00 UTC),
                end: datetime!(2023 - 11 - 09 16:00:00 UTC),
            },
            plan: Aggregation::Scan(9),
        });

        expected.push(WheelAggregation {
            range: WheelRange {
                start: datetime!(2023 - 11 - 11 12:00:00 UTC),
                end: datetime!(2023 - 11 - 11 12:30:00 UTC),
            },
            plan: Aggregation::Scan(30),
        });

        expected.push(WheelAggregation {
            range: WheelRange {
                start: datetime!(2023 - 11 - 09 16:00:00 UTC),
                end: datetime!(2023 - 11 - 10 00:00:00 UTC),
            },
            plan: Aggregation::Scan(8),
        });
        expected.push(WheelAggregation {
            range: WheelRange {
                start: datetime!(2023 - 11 - 11 00:00:00 UTC),
                end: datetime!(2023 - 11 - 11 12:00:00 UTC),
            },
            plan: Aggregation::Scan(12),
        });
        expected.push(WheelAggregation {
            range: WheelRange {
                start: datetime!(2023 - 11 - 10 00:00:00 UTC),
                end: datetime!(2023 - 11 - 11 00:00:00 UTC),
            },
            plan: Aggregation::Scan(1),
        });

        assert_eq!(combined_plan.aggregations, expected);

        let start = datetime!(2023 - 11 - 09 05:00:00 UTC);
        let end = datetime!(2023 - 11 - 12 00:00:00 UTC);

        // Runs a Inverse Landmark Execution
        let result = haw.combine_range(WheelRange::new(start, end));
        let plan = haw.create_exec_plan(WheelRange::new(start, end));
        assert!(matches!(plan, ExecutionPlan::InverseLandmarkAggregation(_)));
        assert_eq!(result, Some(241200));
    }
}
