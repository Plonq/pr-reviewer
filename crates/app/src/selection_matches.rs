//! Passive, literal matches for the current text selection. Unlike Find, this
//! only inspects displayed source, never opens folds or moves the viewport.
#[cfg(test)]
mod tests;
use std::ops::Range;

use crate::minimap::MinimapMatch;
use crate::rows::{source_row, LineKind, Row};
use crate::selection::{row_side_text, selection_text, SelSide, Selection};

#[derive(Default)]
pub(crate) struct SelectionMatches {
    by_row: Vec<Vec<(SelSide, Range<usize>)>>,
    /// Cached horizontal positions on the minimap's character scale.
    pub(crate) marks: Vec<MinimapMatch>,
}

impl SelectionMatches {
    pub(crate) fn find(rows: &[Row], selection: Option<Selection>) -> Self {
        let mut result = Self::default();
        let Some(selection) = selection.filter(|s| !s.linewise && !rows.is_empty()) else {
            return result;
        };
        let query = selection_text(&selection, rows);
        if query.trim().is_empty() {
            return result;
        }
        result.by_row = vec![Vec::new(); rows.len()];
        // Unified view interleaves old and new lines. Search the two source
        // streams independently, then deduplicate shared context highlights.
        for (side, old) in [
            (SelSide::Unified, true),
            (SelSide::Unified, false),
            (SelSide::Left, true),
            (SelSide::Right, false),
        ] {
            let mut stream = Stream::default();
            for (ix, row) in rows.iter().enumerate() {
                if let Some(text) = row_side_text(row, side) {
                    let include = match source_row(row) {
                        Row::Line { kind, .. } => match kind {
                            LineKind::Context => true,
                            LineKind::Removed => old,
                            LineKind::Added => !old,
                        },
                        _ => true,
                    };
                    if include {
                        let no = match (source_row(row), side) {
                            (Row::Line { old_no, new_no, .. }, SelSide::Unified) => {
                                if old {
                                    *old_no
                                } else {
                                    *new_no
                                }
                            }
                            (Row::SplitLine { left, .. }, SelSide::Left) => {
                                left.as_ref().map(|c| c.no)
                            }
                            (Row::SplitLine { right, .. }, SelSide::Right) => {
                                right.as_ref().map(|c| c.no)
                            }
                            _ => None,
                        };
                        if !matches!(row, Row::Continuation(_)) {
                            if stream
                                .last_line
                                .zip(no)
                                .is_some_and(|(prev, no)| no != prev + 1)
                            {
                                stream.project(&query, side, &mut result);
                                stream = Stream::default();
                            }
                            if !stream.fragments.is_empty() {
                                stream.text.push('\n');
                            }
                            stream.last_line = no;
                        }
                        let start = stream.text.len();
                        stream.text.push_str(text);
                        stream.fragments.push((ix, start..stream.text.len()));
                    }
                } else if matches!(
                    source_row(row),
                    Row::FileHeader { .. } | Row::Fold { .. } | Row::LargeFile { .. } | Row::Binary
                ) {
                    // Never invent a multiline match through hidden source.
                    stream.project(&query, side, &mut result);
                    stream = Stream::default();
                }
                // Comments and absent split cells contribute no source text.
            }
            stream.project(&query, side, &mut result);
        }
        for (ix, spans) in result.by_row.iter_mut().enumerate() {
            spans.sort_by_key(|(side, range)| (side_order(*side), range.start, range.end));
            spans.dedup();
            for (side, range) in spans {
                let text = row_side_text(&rows[ix], *side).expect("match has source text");
                if let Some(range) = crate::minimap::text_range_frac(text, range) {
                    result.marks.push(MinimapMatch {
                        row: ix,
                        side: *side,
                        range,
                    });
                }
            }
        }
        result
    }

    pub(crate) fn ranges(&self, row: usize, side: SelSide) -> Vec<Range<usize>> {
        self.by_row
            .get(row)
            .into_iter()
            .flatten()
            .filter(|(s, _)| *s == side)
            .map(|(_, range)| range.clone())
            .collect()
    }
}

fn side_order(side: SelSide) -> u8 {
    match side {
        SelSide::Unified => 0,
        SelSide::Left => 1,
        SelSide::Right => 2,
    }
}

#[derive(Default)]
struct Stream {
    text: String,
    fragments: Vec<(usize, Range<usize>)>,
    last_line: Option<u32>,
}

impl Stream {
    fn project(&self, query: &str, side: SelSide, result: &mut SelectionMatches) {
        for (start, _) in self.text.match_indices(query) {
            let end = start + query.len();
            let first = self
                .fragments
                .partition_point(|(_, range)| range.end <= start);
            for (row, fragment) in &self.fragments[first..] {
                if fragment.start >= end {
                    break;
                }
                let lo = start.max(fragment.start);
                let hi = end.min(fragment.end);
                if lo < hi {
                    result.by_row[*row].push((side, lo - fragment.start..hi - fragment.start));
                }
            }
        }
    }
}
