//! What the interface reads: the latest value of everything, and a history of what is watched.
//!
//! The whole store sits behind one `RwLock` in [`xtce-gs-engine`]. The decode thread takes
//! the write lock once per batch of packets, the interface takes the read lock once per
//! frame, and neither holds it across anything that can block. That is deliberate: a
//! per-parameter lock would be 9 493 of them on CTIM, and a channel per plotted parameter
//! would move the fan-out problem into the interface instead of removing it.

use xtce_model::ParamId;

use crate::ring::RingBuffer;
use crate::sample::{Batch, Sample};
use crate::time::Utc;
use crate::value::Value;

/// One point of plottable history.
///
/// Time is seconds since the Unix epoch as an `f64` because that is the axis `egui_plot`
/// draws on, and the conversion belongs where the point is made, not in the draw loop. At a
/// timestamp of 1.8e9 an `f64` still resolves 0.2 µs, far below anything a downlink times.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Point {
    /// Seconds since the Unix epoch.
    pub t: f64,
    /// The engineering value.
    pub v: f64,
}

/// Latest values for every parameter, and a ring of history for the watched ones.
#[derive(Debug)]
pub struct ParameterStore {
    latest: Vec<Option<Sample>>,
    updates: Vec<u32>,
    history: Vec<Option<RingBuffer<Point>>>,
    depth: usize,
    generation: u64,
    ingested: u64,
}

impl ParameterStore {
    /// A store for a definition with `parameter_count` parameters, keeping `depth` points of
    /// history per watched parameter.
    #[must_use]
    pub fn new(parameter_count: usize, depth: usize) -> Self {
        let mut latest = Vec::new();
        latest.resize_with(parameter_count, || None);
        let mut history = Vec::new();
        history.resize_with(parameter_count, || None);
        Self {
            latest,
            updates: vec![0; parameter_count],
            history,
            depth,
            generation: 0,
            ingested: 0,
        }
    }

    /// How many parameters this store was built for.
    #[must_use]
    pub fn parameter_count(&self) -> usize {
        self.latest.len()
    }

    /// How many points of history a watched parameter keeps.
    #[must_use]
    pub const fn depth(&self) -> usize {
        self.depth
    }

    /// Bumped on every [`ParameterStore::ingest`].
    ///
    /// The interface compares it with what it drew last frame, and skips the frame when
    /// nothing has changed. That is what keeps an idle station off the processor.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// How many batches have been ingested since the store was made.
    #[must_use]
    pub const fn ingested(&self) -> u64 {
        self.ingested
    }

    /// Starts keeping history for a parameter. Returns whether it was not already watched.
    ///
    /// The ring is allocated here — 4 096 points is 64 KB — so opening a plot costs one
    /// allocation and closing it gives the memory back.
    pub fn watch(&mut self, parameter: ParamId) -> bool {
        let Some(slot) = self.history.get_mut(parameter.index()) else {
            return false;
        };
        if slot.is_some() {
            return false;
        }
        *slot = Some(RingBuffer::with_capacity(self.depth));
        true
    }

    /// Stops keeping history for a parameter and frees its ring.
    pub fn unwatch(&mut self, parameter: ParamId) {
        if let Some(slot) = self.history.get_mut(parameter.index()) {
            *slot = None;
        }
    }

    /// Whether history is being kept for a parameter.
    #[must_use]
    pub fn is_watched(&self, parameter: ParamId) -> bool {
        self.history
            .get(parameter.index())
            .is_some_and(Option::is_some)
    }

    /// Every parameter currently watched.
    pub fn watched(&self) -> impl Iterator<Item = ParamId> + '_ {
        self.history
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.as_ref().map(|_| ParamId::new(index as u32)))
    }

    /// Changes how much history each watched parameter keeps, preserving the newest points.
    pub fn set_depth(&mut self, depth: usize) {
        self.depth = depth;
        for ring in self.history.iter_mut().flatten() {
            ring.resize(depth);
        }
    }

    /// Files one decoded packet.
    ///
    /// Every sample updates the parameter's latest value; a watched parameter whose value is
    /// numeric also gets a point of history. A sample for a parameter index this store was
    /// not built for is dropped — that means the definition was reloaded underneath it, which
    /// the engine handles by building a new store, not by growing this one.
    pub fn ingest(&mut self, batch: &Batch) {
        let time = batch.time().unix_secs_f64();
        for sample in &batch.samples {
            let index = sample.parameter.index();
            if let Some(slot) = self.latest.get_mut(index) {
                *slot = Some(sample.clone());
            } else {
                continue;
            }
            if let Some(count) = self.updates.get_mut(index) {
                *count = count.saturating_add(1);
            }
            if let Some(Some(ring)) = self.history.get_mut(index)
                && let Some(v) = sample.eng.as_f64()
            {
                ring.push(Point { t: time, v });
            }
        }
        self.generation = self.generation.wrapping_add(1);
        self.ingested = self.ingested.saturating_add(1);
    }

    /// Files one sample outside a batch — used by tests and by synthetic sources.
    pub fn push(&mut self, sample: Sample) {
        let index = sample.parameter.index();
        if let Some(Some(ring)) = self.history.get_mut(index)
            && let Some(v) = sample.eng.as_f64()
        {
            ring.push(Point {
                t: sample.time.unix_secs_f64(),
                v,
            });
        }
        if let Some(count) = self.updates.get_mut(index) {
            *count = count.saturating_add(1);
        }
        if let Some(slot) = self.latest.get_mut(index) {
            *slot = Some(sample);
        }
        self.generation = self.generation.wrapping_add(1);
    }

    /// The most recent sample for a parameter, if one has ever arrived.
    #[must_use]
    pub fn latest(&self, parameter: ParamId) -> Option<&Sample> {
        self.latest.get(parameter.index())?.as_ref()
    }

    /// The most recent engineering value for a parameter.
    #[must_use]
    pub fn latest_value(&self, parameter: ParamId) -> Option<&Value> {
        self.latest(parameter).map(|sample| &sample.eng)
    }

    /// When a parameter last arrived.
    #[must_use]
    pub fn latest_time(&self, parameter: ParamId) -> Option<Utc> {
        self.latest(parameter).map(|sample| sample.time)
    }

    /// How many times a parameter has been seen.
    #[must_use]
    pub fn updates(&self, parameter: ParamId) -> u32 {
        self.updates.get(parameter.index()).copied().unwrap_or(0)
    }

    /// The history of a watched parameter.
    #[must_use]
    pub fn history(&self, parameter: ParamId) -> Option<&RingBuffer<Point>> {
        self.history.get(parameter.index())?.as_ref()
    }

    /// Every parameter that has ever arrived.
    pub fn seen(&self) -> impl Iterator<Item = ParamId> + '_ {
        self.latest
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.as_ref().map(|_| ParamId::new(index as u32)))
    }

    /// How many distinct parameters have ever arrived.
    #[must_use]
    pub fn seen_count(&self) -> usize {
        self.latest.iter().filter(|slot| slot.is_some()).count()
    }

    /// Drops every value and every history, keeping what is watched.
    pub fn clear(&mut self) {
        self.latest.fill(None);
        self.updates.fill(0);
        for ring in self.history.iter_mut().flatten() {
            ring.clear();
        }
        self.generation = self.generation.wrapping_add(1);
        self.ingested = 0;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use xtce_model::ContainerId;

    use super::*;

    fn batch(param: u32, value: f64, at: f64) -> Batch {
        Batch {
            container: ContainerId::new(0),
            received: Utc::from_unix_nanos((at * 1e9) as i64),
            spacecraft: None,
            apid: 1,
            sequence: 0,
            sequence_gap: false,
            samples: vec![Sample {
                parameter: ParamId::new(param),
                time: Utc::from_unix_nanos((at * 1e9) as i64),
                raw: Value::Unsigned(value as u64),
                eng: Value::Float(value),
            }],
        }
    }

    #[test]
    fn history_is_kept_only_for_watched_parameters() {
        let mut store = ParameterStore::new(8, 16);
        store.watch(ParamId::new(1));
        store.ingest(&batch(1, 10.0, 100.0));
        store.ingest(&batch(2, 20.0, 100.0));

        assert_eq!(store.history(ParamId::new(1)).map(RingBuffer::len), Some(1));
        assert!(store.history(ParamId::new(2)).is_none());
        // The latest value is kept for both, watched or not.
        assert!(store.latest(ParamId::new(2)).is_some());
    }

    #[test]
    fn unwatching_frees_the_ring() {
        let mut store = ParameterStore::new(4, 16);
        assert!(store.watch(ParamId::new(0)));
        assert!(!store.watch(ParamId::new(0)));
        store.unwatch(ParamId::new(0));
        assert!(!store.is_watched(ParamId::new(0)));
    }

    #[test]
    fn a_label_has_no_history_but_has_a_value() {
        let mut store = ParameterStore::new(2, 8);
        store.watch(ParamId::new(0));
        let mut b = batch(0, 0.0, 1.0);
        if let Some(sample) = b.samples.get_mut(0) {
            sample.eng = Value::Label(Arc::from("SAFE"));
        }
        store.ingest(&b);
        assert_eq!(store.history(ParamId::new(0)).map(RingBuffer::len), Some(0));
        assert_eq!(
            store.latest_value(ParamId::new(0)).and_then(Value::as_str),
            Some("SAFE")
        );
    }

    #[test]
    fn a_sample_for_an_unknown_parameter_is_dropped_not_fatal() {
        let mut store = ParameterStore::new(1, 8);
        store.ingest(&batch(9, 1.0, 1.0));
        assert_eq!(store.seen_count(), 0);
    }

    #[test]
    fn the_generation_moves_when_something_arrives() {
        let mut store = ParameterStore::new(2, 8);
        let before = store.generation();
        store.ingest(&batch(0, 1.0, 1.0));
        assert_ne!(before, store.generation());
    }
}
