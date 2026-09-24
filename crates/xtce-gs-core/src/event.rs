//! The event log: what the station wants to tell the operator, in order, bounded.
//!
//! Bounded matters. A station pointed at a definition that does not describe the stream will
//! produce one event per packet, and an unbounded log is then a memory leak with a scrollbar.

use crate::ring::RingBuffer;
use crate::time::Utc;

/// How much attention a line deserves.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Severity {
    /// Something happened that was expected.
    Info,
    /// Something happened that the operator should know about, and the station continued.
    Warning,
    /// Something failed.
    Error,
}

impl Severity {
    /// A single letter for a narrow column.
    #[must_use]
    pub const fn letter(self) -> char {
        match self {
            Self::Info => 'i',
            Self::Warning => '!',
            Self::Error => 'x',
        }
    }
}

/// One line of the log.
#[derive(Clone, Debug)]
pub struct Event {
    /// When it happened.
    pub time: Utc,
    /// How much it matters.
    pub severity: Severity,
    /// Which part of the station said it: `link`, `frame`, `decode`, `session`.
    pub source: &'static str,
    /// What it said.
    pub message: String,
}

impl Event {
    /// An informational line, stamped now.
    #[must_use]
    pub fn info(source: &'static str, message: impl Into<String>) -> Self {
        Self::at(Utc::now(), Severity::Info, source, message)
    }

    /// A warning, stamped now.
    #[must_use]
    pub fn warning(source: &'static str, message: impl Into<String>) -> Self {
        Self::at(Utc::now(), Severity::Warning, source, message)
    }

    /// An error, stamped now.
    #[must_use]
    pub fn error(source: &'static str, message: impl Into<String>) -> Self {
        Self::at(Utc::now(), Severity::Error, source, message)
    }

    /// A line at a given instant.
    #[must_use]
    pub fn at(
        time: Utc,
        severity: Severity,
        source: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            time,
            severity,
            source,
            message: message.into(),
        }
    }
}

/// A bounded log that also collapses a line repeated back to back.
///
/// Collapsing is not cosmetic: a link that drops every frame produces thousands of identical
/// lines a second, and without this the one line that says something else scrolls past before
/// it can be read.
#[derive(Debug)]
pub struct EventLog {
    entries: RingBuffer<(Event, u32)>,
    total: u64,
}

impl EventLog {
    /// A log holding at most `capacity` distinct lines.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: RingBuffer::with_capacity(capacity),
            total: 0,
        }
    }

    /// Appends a line, or bumps the repeat count if it equals the last one.
    pub fn push(&mut self, event: Event) {
        self.total = self.total.saturating_add(1);
        if let Some((last, count)) = self.entries.last_mut()
            && last.severity == event.severity
            && last.source == event.source
            && last.message == event.message
        {
            // The line is the same; only the time it was last seen and the count move.
            last.time = event.time;
            *count = count.saturating_add(1);
            return;
        }
        self.entries.push((event, 1));
    }

    /// Every line held, oldest first, with how many times each was repeated.
    pub fn iter(&self) -> impl Iterator<Item = (&Event, u32)> + '_ {
        self.entries.iter().map(|(event, count)| (event, *count))
    }

    /// The most recent line.
    #[must_use]
    pub fn last(&self) -> Option<&Event> {
        self.entries.last().map(|(event, _)| event)
    }

    /// How many lines are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the log holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many lines have ever been pushed, repeats included.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// Drops every line.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeated_line_is_counted_not_appended() {
        let mut log = EventLog::with_capacity(8);
        for _ in 0..5 {
            log.push(Event::warning("frame", "CRC failed"));
        }
        assert_eq!(log.len(), 1);
        assert_eq!(log.iter().next().map(|(_, count)| count), Some(5));
        assert_eq!(log.total(), 5);
    }

    #[test]
    fn a_different_line_breaks_the_run() {
        let mut log = EventLog::with_capacity(8);
        log.push(Event::warning("frame", "CRC failed"));
        log.push(Event::info("link", "locked"));
        log.push(Event::warning("frame", "CRC failed"));
        assert_eq!(log.len(), 3);
    }

    #[test]
    fn it_is_bounded() {
        let mut log = EventLog::with_capacity(2);
        for i in 0..10 {
            log.push(Event::info("link", format!("line {i}")));
        }
        assert_eq!(log.len(), 2);
        assert_eq!(log.last().map(|e| e.message.clone()), Some("line 9".into()));
    }
}
