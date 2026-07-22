//! Per-process ring buffers of ANSI-parsed styled lines, plus interleaving.
//!
//! Each incoming raw line is ANSI-parsed exactly once on arrival into styled
//! ratatui spans and stored in a bounded ring buffer (capacity from
//! [`Config::buffer_cap`](crate::types::Config)). The all-view is produced by
//! merging every process's buffer in global arrival order using each line's
//! [`Seq`]. This module is pure logic (no I/O, no rendering) and is the primary
//! unit-test surface.

use std::collections::VecDeque;

use ansi_to_tui::IntoText;
use ratatui::text::Line as TuiLine;

use crate::types::{Config, ProcId, Seq, StreamTag};

/// A single stored line: its provenance plus the pre-parsed styled content.
#[derive(Debug, Clone)]
pub struct StyledLine {
    pub proc: ProcId,
    pub stream: StreamTag,
    pub seq: Seq,
    /// ANSI-parsed styled content, owned (`'static`).
    pub content: TuiLine<'static>,
}

impl StyledLine {
    /// Parse a raw line (newline already stripped) into a styled line. ANSI
    /// escape sequences in `bytes` are converted to styled spans; invalid UTF-8
    /// is handled lossily.
    pub fn parse(proc: ProcId, stream: StreamTag, seq: Seq, bytes: &[u8]) -> StyledLine {
        let content = parse_line(bytes);
        StyledLine {
            proc,
            stream,
            seq,
            content,
        }
    }
}

/// Convert a single raw line's bytes into a styled ratatui [`TuiLine`].
///
/// `ansi-to-tui` parses into a multi-line `Text`; since a stored line has its
/// trailing newline already stripped we take the first produced line (any
/// embedded newlines from an oddly-framed chunk collapse into it). Invalid
/// escape sequences or UTF-8 fall back to a lossy plain-text rendering so
/// parsing never fails.
fn parse_line(bytes: &[u8]) -> TuiLine<'static> {
    match bytes.into_text() {
        Ok(text) => {
            // The input is a single logical line (newline already stripped), but
            // the parser may split oddly-framed bytes across several lines; flatten
            // every produced span back into one line, preserving styles.
            let spans: Vec<_> = text.lines.into_iter().flat_map(|l| l.spans).collect();
            if spans.is_empty() {
                TuiLine::from(String::from_utf8_lossy(bytes).into_owned())
            } else {
                TuiLine::from(spans)
            }
        }
        Err(_) => TuiLine::from(String::from_utf8_lossy(bytes).into_owned()),
    }
}

/// A bounded ring buffer of styled lines for one process.
#[derive(Debug)]
pub struct ProcBuffer {
    lines: VecDeque<StyledLine>,
    cap: usize,
}

impl ProcBuffer {
    /// Create an empty buffer with the given line capacity.
    ///
    /// A capacity of zero is clamped to one so the buffer always retains the
    /// most recent line.
    pub fn new(cap: usize) -> ProcBuffer {
        let cap = cap.max(1);
        ProcBuffer {
            lines: VecDeque::with_capacity(cap),
            cap,
        }
    }

    /// Append a line, evicting the oldest if at capacity.
    pub fn push(&mut self, line: StyledLine) {
        if self.lines.len() == self.cap {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// Number of lines currently retained.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// True if the buffer holds no lines.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Iterate retained lines from oldest to newest.
    pub fn iter(&self) -> impl Iterator<Item = &StyledLine> {
        self.lines.iter()
    }
}

/// Owns one [`ProcBuffer`] per process and produces interleaved views.
#[derive(Debug)]
pub struct BufferSet {
    buffers: Vec<ProcBuffer>,
}

impl BufferSet {
    /// Create a set of `count` empty buffers, each with capacity from `config`.
    pub fn new(count: usize, config: &Config) -> BufferSet {
        let buffers = (0..count)
            .map(|_| ProcBuffer::new(config.buffer_cap))
            .collect();
        BufferSet { buffers }
    }

    /// Number of process buffers held.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.buffers.len()
    }

    /// True if there are no process buffers.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.buffers.is_empty()
    }

    /// Append a line to the buffer of its originating process.
    ///
    /// # Panics
    /// Panics if `line.proc` is out of range for this set.
    pub fn push(&mut self, line: StyledLine) {
        let proc = line.proc;
        self.buffers[proc].push(line);
    }

    /// Access one process's buffer.
    ///
    /// # Panics
    /// Panics if `proc` is out of range for this set.
    pub fn buffer(&self, proc: ProcId) -> &ProcBuffer {
        &self.buffers[proc]
    }

    /// Merge every process buffer into a single sequence ordered by global
    /// [`Seq`] (arrival order). Used to render the all-view.
    ///
    /// Each per-process buffer is already sorted ascending by `seq`, so this is
    /// a k-way merge. Older lines evicted from a ring buffer simply do not
    /// appear.
    pub fn interleaved(&self) -> Vec<&StyledLine> {
        // Cursor per buffer into its ordered lines.
        let mut cursors: Vec<_> = self.buffers.iter().map(|b| b.lines.iter().peekable()).collect();
        let total: usize = self.buffers.iter().map(|b| b.len()).sum();
        let mut out: Vec<&StyledLine> = Vec::with_capacity(total);

        loop {
            // Find the cursor whose next line has the smallest seq.
            let mut best: Option<(usize, Seq)> = None;
            for (i, cur) in cursors.iter_mut().enumerate() {
                if let Some(line) = cur.peek() {
                    match best {
                        Some((_, best_seq)) if line.seq >= best_seq => {}
                        _ => best = Some((i, line.seq)),
                    }
                }
            }
            match best {
                Some((i, _)) => out.push(cursors[i].next().unwrap()),
                None => break,
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn cfg(cap: usize) -> Config {
        Config {
            buffer_cap: cap,
            ..Config::default()
        }
    }

    fn line(proc: ProcId, seq: Seq, text: &str) -> StyledLine {
        StyledLine::parse(proc, StreamTag::Stdout, seq, text.as_bytes())
    }

    // --- ring buffer behavior -------------------------------------------

    #[test]
    fn ring_buffer_retains_up_to_capacity() {
        let mut buf = ProcBuffer::new(3);
        assert!(buf.is_empty());
        for i in 0..3 {
            buf.push(line(0, i, &format!("l{i}")));
        }
        assert_eq!(buf.len(), 3);
        assert!(!buf.is_empty());
    }

    #[test]
    fn ring_buffer_evicts_oldest_over_capacity() {
        let mut buf = ProcBuffer::new(3);
        for i in 0..5 {
            buf.push(line(0, i, &format!("l{i}")));
        }
        assert_eq!(buf.len(), 3);
        let seqs: Vec<Seq> = buf.iter().map(|l| l.seq).collect();
        // Oldest two (seq 0,1) evicted, keeping 2,3,4 in order.
        assert_eq!(seqs, vec![2, 3, 4]);
    }

    #[test]
    fn ring_buffer_zero_cap_clamped_to_one() {
        let mut buf = ProcBuffer::new(0);
        buf.push(line(0, 0, "a"));
        buf.push(line(0, 1, "b"));
        assert_eq!(buf.len(), 1);
        assert_eq!(buf.iter().next().unwrap().seq, 1);
    }

    // --- ANSI parse-to-spans --------------------------------------------

    #[test]
    fn plain_text_parses_to_single_span() {
        let sl = line(0, 0, "hello world");
        let text: String = sl
            .content
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text, "hello world");
    }

    #[test]
    fn ansi_color_parses_to_styled_spans() {
        // Red "err" then reset.
        let raw = b"\x1b[31merr\x1b[0m done";
        let sl = StyledLine::parse(0, StreamTag::Stderr, 0, raw);
        // Reconstructed text drops escape codes.
        let text: String = sl
            .content
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text, "err done");
        // At least one span carries red foreground.
        let has_red = sl
            .content
            .spans
            .iter()
            .any(|s| s.style.fg == Some(Color::Red));
        assert!(has_red, "expected a red-styled span");
    }

    #[test]
    fn invalid_utf8_is_lossy_not_panic() {
        let raw = &[0xff, 0xfe, b'h', b'i'];
        let sl = StyledLine::parse(0, StreamTag::Stdout, 0, raw);
        let text: String = sl
            .content
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("hi"));
    }

    // --- interleave ordering --------------------------------------------

    #[test]
    fn interleaved_merges_by_global_seq() {
        let mut set = BufferSet::new(3, &cfg(100));
        // Push out of proc order but with a coherent global seq timeline.
        set.push(line(0, 0, "a0"));
        set.push(line(1, 1, "b1"));
        set.push(line(0, 2, "a2"));
        set.push(line(2, 3, "c3"));
        set.push(line(1, 4, "b4"));

        let merged = set.interleaved();
        let seqs: Vec<Seq> = merged.iter().map(|l| l.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn interleaved_preserves_order_after_eviction() {
        let mut set = BufferSet::new(2, &cfg(2));
        // proc 0 gets seq 0,2,4 ; proc 1 gets 1,3,5. cap 2 each.
        for &(p, s) in &[(0, 0u64), (1, 1), (0, 2), (1, 3), (0, 4), (1, 5)] {
            set.push(line(p, s, "x"));
        }
        // Each buffer retains its last two: proc0=[2,4], proc1=[3,5].
        let merged = set.interleaved();
        let seqs: Vec<Seq> = merged.iter().map(|l| l.seq).collect();
        assert_eq!(seqs, vec![2, 3, 4, 5]);
    }

    #[test]
    fn interleaved_empty_set_is_empty() {
        let set = BufferSet::new(0, &cfg(10));
        assert!(set.interleaved().is_empty());
        assert!(set.is_empty());
    }

    #[test]
    fn buffer_accessor_returns_process_buffer() {
        let mut set = BufferSet::new(2, &cfg(10));
        set.push(line(1, 0, "only proc1"));
        assert!(set.buffer(0).is_empty());
        assert_eq!(set.buffer(1).len(), 1);
    }
}
