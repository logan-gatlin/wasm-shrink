//! Memory packing: stop shipping the zero bytes of active data segments.
//!
//! A memory the module defines itself starts out zeroed, so an active segment
//! does not need to write zeros. This pass trims leading and trailing zeros off
//! every active segment and splits a segment where a run of zeros is longer than
//! the cost of a new segment header. A segment made only of zeros is removed.
//!
//! This is the part of binaryen's `memory-packing` that matters for Rust output.
//! Rust puts `.rodata` and `.data` in one segment each, and zero-initialised
//! statics land in `.data` as literal zeros.
//!
//! Safety conditions (otherwise the memory is left alone):
//! - the memory is not imported, so it really does start zeroed;
//! - every active segment on it has a constant offset;
//! - no two active segments on it overlap, so a later segment never relies on
//!   its zeros overwriting an earlier one;
//! - no instruction refers to the segment (`memory.init` / `data.drop`).

use std::collections::HashSet;
use walrus::ir::Value;
use walrus::{ConstExpr, DataId, DataKind, MemoryId, Module};

/// Bytes a new active segment costs: flags, `i32.const`, offset (signed LEB),
/// `end`, length LEB. A zero run must be longer than this to be worth a split.
fn header_len(offset: u64, len: usize) -> usize {
    let mut n = 1;
    let mut v = offset as i64;
    while !(-64..64).contains(&v) {
        v >>= 7;
        n += 1;
    }
    3 + n + crate::util::leb_len(len as u64)
}
/// Engines reject modules with more than 100,000 data segments.
const MAX_SEGMENTS: usize = 100_000;

#[derive(Default, Debug)]
pub struct Stats {
    pub zero_bytes_dropped: usize,
    pub segments_added: usize,
    pub segments_removed: usize,
}

fn offset_of(kind: &DataKind) -> Option<(MemoryId, u64)> {
    match kind {
        DataKind::Active {
            memory,
            offset: ConstExpr::Value(Value::I32(n)),
        } => Some((*memory, *n as u32 as u64)),
        DataKind::Active {
            memory,
            offset: ConstExpr::Value(Value::I64(n)),
        } => Some((*memory, *n as u64)),
        _ => None,
    }
}

/// Non-zero ranges `[start, end)` of `bytes`, merging ranges separated by a zero
/// run no longer than `threshold(start of the next range)`.
fn nonzero_ranges(bytes: &[u8], threshold: impl Fn(usize) -> usize) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0 {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i] != 0 {
            i += 1;
        }
        match out.last_mut() {
            Some(last) if start - last.1 <= threshold(start) => last.1 = i,
            _ => out.push((start, i)),
        }
    }
    out
}

pub fn run(m: &mut Module) -> Stats {
    let mut stats = Stats::default();

    // Segments referenced from code must keep their identity and contents.
    let mut referenced: HashSet<DataId> = HashSet::new();
    for (_, f) in m.funcs.iter_local() {
        referenced.extend(f.used_data_segments());
    }

    let memories: Vec<MemoryId> = m.memories.iter().map(|mem| mem.id()).collect();
    let mut total_segments = m.data.iter().count();
    for mem in memories {
        if m.memories.get(mem).import.is_some() {
            continue;
        }
        let mut segs: Vec<(DataId, u64, u64)> = Vec::new();
        let mut ok = true;
        for d in m.data.iter() {
            match &d.kind {
                DataKind::Active { memory, .. } if *memory == mem => match offset_of(&d.kind) {
                    Some((_, off)) => segs.push((d.id(), off, off + d.value.len() as u64)),
                    None => ok = false,
                },
                _ => {}
            }
        }
        segs.sort_by_key(|s| s.1);
        if !ok || segs.windows(2).any(|w| w[0].2 > w[1].1) {
            continue;
        }
        let memory64 = m.memories.get(mem).memory64;

        for (id, base, _) in segs {
            if referenced.contains(&id) {
                continue;
            }
            let value = std::mem::take(&mut m.data.get_mut(id).value);
            let budget = MAX_SEGMENTS.saturating_sub(total_segments);
            // A split pays when the zero run is longer than the new header
            // (its length LEB is estimated at 2 bytes).
            let mut ranges = nonzero_ranges(&value, |start| header_len(base + start as u64, 128));
            // Respect the segment limit by merging the closest ranges first.
            while ranges.len() > budget + 1 {
                let (k, _) = ranges
                    .windows(2)
                    .enumerate()
                    .min_by_key(|(_, w)| w[1].0 - w[0].1)
                    .unwrap();
                ranges[k].1 = ranges[k + 1].1;
                ranges.remove(k + 1);
            }
            if ranges.is_empty() {
                stats.zero_bytes_dropped += value.len();
                stats.segments_removed += 1;
                m.memories.get_mut(mem).data_segments.remove(&id);
                m.data.delete(id);
                total_segments -= 1;
                continue;
            }
            let kept: usize = ranges.iter().map(|r| r.1 - r.0).sum();
            stats.zero_bytes_dropped += value.len() - kept;
            let make_offset = |off: u64| {
                if memory64 {
                    ConstExpr::Value(Value::I64(off as i64))
                } else {
                    ConstExpr::Value(Value::I32(off as u32 as i32))
                }
            };
            // The first range stays in the original segment (keeps its name).
            let (s0, e0) = ranges[0];
            {
                let d = m.data.get_mut(id);
                d.value = value[s0..e0].to_vec();
                d.kind = DataKind::Active {
                    memory: mem,
                    offset: make_offset(base + s0 as u64),
                };
            }
            for &(s, e) in &ranges[1..] {
                let nid = m.data.add(
                    DataKind::Active {
                        memory: mem,
                        offset: make_offset(base + s as u64),
                    },
                    value[s..e].to_vec(),
                );
                m.memories.get_mut(mem).data_segments.insert(nid);
                stats.segments_added += 1;
                total_segments += 1;
            }
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::nonzero_ranges;

    #[test]
    fn ranges() {
        assert_eq!(nonzero_ranges(&[0, 0, 1, 2, 0, 0], |_| 3), vec![(2, 4)]);
        assert_eq!(nonzero_ranges(&[1, 0, 0, 1], |_| 2), vec![(0, 4)]);
        assert_eq!(
            nonzero_ranges(&[1, 0, 0, 0, 1], |_| 2),
            vec![(0, 1), (4, 5)]
        );
        assert!(nonzero_ranges(&[0; 10], |_| 2).is_empty());
    }

    #[test]
    fn header() {
        assert_eq!(super::header_len(1_048_600, 10), 8);
        assert_eq!(super::header_len(100, 10), 6);
    }
}
