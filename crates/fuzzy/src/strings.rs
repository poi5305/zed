use crate::{
    CharBag,
    char_bag::simple_lowercase,
    matcher::{MatchCandidate, Matcher},
};
use gpui::BackgroundExecutor;
use std::{
    borrow::Borrow,
    cmp::{self, Ordering},
    iter,
    ops::Range,
    sync::atomic::{self, AtomicBool},
};

#[derive(Clone, Debug)]
pub struct StringMatchCandidate {
    pub id: usize,
    pub string: String,
    pub char_bag: CharBag,
}

impl StringMatchCandidate {
    pub fn new(id: usize, string: &str) -> Self {
        Self {
            id,
            string: string.into(),
            char_bag: string.into(),
        }
    }
}

impl MatchCandidate for &StringMatchCandidate {
    fn has_chars(&self, bag: CharBag) -> bool {
        self.char_bag.is_superset(bag)
    }

    fn candidate_chars(&self) -> impl Iterator<Item = char> {
        self.string.chars()
    }
}

#[derive(Clone, Debug)]
pub struct StringMatch {
    pub candidate_id: usize,
    pub score: f64,
    pub positions: Vec<usize>,
    pub string: String,
}

impl StringMatch {
    pub fn ranges(&self) -> impl '_ + Iterator<Item = Range<usize>> {
        let mut positions = self.positions.iter().peekable();
        iter::from_fn(move || {
            if let Some(start) = positions.next().copied() {
                let Some(char_len) = self.char_len_at_index(start) else {
                    log::error!(
                        "Invariant violation: Index {start} out of range or not on a utf-8 boundary in string {:?}",
                        self.string
                    );
                    return None;
                };
                let mut end = start + char_len;
                while let Some(next_start) = positions.peek() {
                    if end == **next_start {
                        let Some(char_len) = self.char_len_at_index(end) else {
                            log::error!(
                                "Invariant violation: Index {end} out of range or not on a utf-8 boundary in string {:?}",
                                self.string
                            );
                            return None;
                        };
                        end += char_len;
                        positions.next();
                    } else {
                        break;
                    }
                }

                return Some(start..end);
            }
            None
        })
    }

    /// Gets the byte length of the utf-8 character at a byte offset. If the index is out of range
    /// or not on a utf-8 boundary then None is returned.
    fn char_len_at_index(&self, ix: usize) -> Option<usize> {
        self.string
            .get(ix..)
            .and_then(|slice| slice.chars().next().map(|char| char.len_utf8()))
    }
}

impl PartialEq for StringMatch {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for StringMatch {}

impl PartialOrd for StringMatch {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for StringMatch {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .partial_cmp(&other.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| self.candidate_id.cmp(&other.candidate_id))
    }
}

pub async fn match_strings<T>(
    candidates: &[T],
    query: &str,
    smart_case: bool,
    penalize_length: bool,
    max_results: usize,
    cancel_flag: &AtomicBool,
    executor: BackgroundExecutor,
) -> Vec<StringMatch>
where
    T: Borrow<StringMatchCandidate> + Sync,
{
    if candidates.is_empty() || max_results == 0 {
        return Default::default();
    }

    if query.is_empty() {
        return candidates
            .iter()
            .map(|candidate| StringMatch {
                candidate_id: candidate.borrow().id,
                score: 0.,
                positions: Default::default(),
                string: candidate.borrow().string.clone(),
            })
            .collect();
    }

    let lowercase_query = query.chars().map(simple_lowercase).collect::<Vec<_>>();
    let query = query.chars().collect::<Vec<_>>();

    let lowercase_query = &lowercase_query;
    let query = &query;
    let query_char_bag = CharBag::from(&lowercase_query[..]);

    // The browser's main thread has no other threads to fan out to, and `scoped` only
    // completes once its spawned tasks have been polled -- which cannot happen while a
    // caller is synchronously awaiting this future. Match inline instead: same results,
    // no parallelism, which is what a single-threaded dispatcher gives either way.
    #[cfg(target_family = "wasm")]
    {
        let _ = &executor;
        let mut results = Vec::with_capacity(max_results.min(candidates.len()));
        let mut matcher = Matcher::new(
            query,
            lowercase_query,
            query_char_bag,
            smart_case,
            penalize_length,
        );
        matcher.match_candidates(
            &[],
            &[],
            candidates.iter().map(|candidate| candidate.borrow()),
            &mut results,
            &cancel_flag,
            |candidate: &&StringMatchCandidate, score, positions| StringMatch {
                candidate_id: candidate.id,
                score,
                positions: positions.clone(),
                string: candidate.string.to_string(),
            },
        );
        if cancel_flag.load(atomic::Ordering::Acquire) {
            return Vec::new();
        }
        rank_matches(&mut results, max_results);
        return results;
    }

    #[cfg(not(target_family = "wasm"))]
    let num_cpus = executor.num_cpus().min(candidates.len());
    #[cfg(not(target_family = "wasm"))]
    let segment_size = candidates.len().div_ceil(num_cpus);
    #[cfg(not(target_family = "wasm"))]
    let mut segment_results = (0..num_cpus)
        .map(|_| Vec::with_capacity(max_results.min(candidates.len())))
        .collect::<Vec<_>>();

    #[cfg(not(target_family = "wasm"))]
    executor
        .scoped(|scope| {
            for (segment_idx, results) in segment_results.iter_mut().enumerate() {
                let cancel_flag = &cancel_flag;
                scope.spawn(async move {
                    let segment_start = cmp::min(segment_idx * segment_size, candidates.len());
                    let segment_end = cmp::min(segment_start + segment_size, candidates.len());
                    let mut matcher = Matcher::new(
                        query,
                        lowercase_query,
                        query_char_bag,
                        smart_case,
                        penalize_length,
                    );

                    matcher.match_candidates(
                        &[],
                        &[],
                        candidates[segment_start..segment_end]
                            .iter()
                            .map(|c| c.borrow()),
                        results,
                        cancel_flag,
                        |candidate: &&StringMatchCandidate, score, positions| StringMatch {
                            candidate_id: candidate.id,
                            score,
                            positions: positions.clone(),
                            string: candidate.string.to_string(),
                        },
                    );
                });
            }
        })
        .await;

    #[cfg(not(target_family = "wasm"))]
    {
        if cancel_flag.load(atomic::Ordering::Acquire) {
            return Vec::new();
        }

        let mut results = segment_results.concat();
        rank_matches(&mut results, max_results);
        results
    }
}

/// `match_strings` for callers that cannot await it.
///
/// wasm-only, and sound only there: the wasm path of [`match_strings`] matches inline and
/// never suspends, so polling it once always completes. The browser's main thread cannot
/// block, which is why those callers exist; on native they keep using `block_on`.
#[cfg(target_family = "wasm")]
pub fn match_strings_blocking<T>(
    candidates: &[T],
    query: &str,
    smart_case: bool,
    penalize_length: bool,
    max_results: usize,
    cancel_flag: &AtomicBool,
    executor: gpui::BackgroundExecutor,
) -> Vec<StringMatch>
where
    T: Borrow<StringMatchCandidate> + Sync,
{
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    // A waker that does nothing: the future below never registers interest in anything,
    // because the wasm path of `match_strings` returns without awaiting.
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(std::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };

    let future = match_strings(
        candidates,
        query,
        smart_case,
        penalize_length,
        max_results,
        cancel_flag,
        executor,
    );
    let mut future = std::pin::pin!(future);
    match future.as_mut().poll(&mut Context::from_waker(&waker)) {
        Poll::Ready(matches) => matches,
        Poll::Pending => {
            unreachable!("the wasm match_strings path matches inline and cannot suspend")
        }
    }
}

#[inline(never)]
fn rank_matches(results: &mut Vec<StringMatch>, max_results: usize) {
    gpui_util::truncate_to_bottom_n_sorted_by(results, max_results, &|a, b| b.cmp(a));
}
