use gpui::BackgroundExecutor;
use path::{PathStyle, rel_path::RelPath};
use std::{
    cmp::{self, Ordering},
    sync::{
        Arc,
        atomic::{self, AtomicBool},
    },
};

use crate::{
    CharBag,
    char_bag::simple_lowercase,
    matcher::{MatchCandidate, Matcher},
};

#[derive(Clone, Debug)]
pub struct PathMatchCandidate<'a> {
    pub is_dir: bool,
    pub path: &'a RelPath,
    pub char_bag: CharBag,
}

#[derive(Clone, Debug)]
pub struct PathMatch {
    pub score: f64,
    pub positions: Vec<usize>,
    pub worktree_id: usize,
    pub path: Arc<RelPath>,
    pub path_prefix: Arc<RelPath>,
    pub is_dir: bool,
    /// Number of steps removed from a shared parent with the relative path
    /// Used to order closer paths first in the search list
    pub distance_to_relative_ancestor: usize,
}

pub trait PathMatchCandidateSet<'a>: Send + Sync {
    type Candidates: Iterator<Item = PathMatchCandidate<'a>>;
    fn id(&self) -> usize;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn root_is_file(&self) -> bool;
    fn prefix(&self) -> Arc<RelPath>;
    fn candidates(&'a self, start: usize) -> Self::Candidates;
    fn path_style(&self) -> PathStyle;
}

impl<'a> MatchCandidate for PathMatchCandidate<'a> {
    fn has_chars(&self, bag: CharBag) -> bool {
        self.char_bag.is_superset(bag)
    }

    fn candidate_chars(&self) -> impl Iterator<Item = char> {
        self.path.as_unix_str().chars()
    }
}

impl PartialEq for PathMatch {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for PathMatch {}

impl PartialOrd for PathMatch {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PathMatch {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .partial_cmp(&other.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| self.worktree_id.cmp(&other.worktree_id))
            .then_with(|| {
                other
                    .distance_to_relative_ancestor
                    .cmp(&self.distance_to_relative_ancestor)
            })
            .then_with(|| self.path.cmp(&other.path))
    }
}

pub fn match_fixed_path_set(
    candidates: Vec<PathMatchCandidate>,
    worktree_id: usize,
    worktree_root_name: Option<Arc<RelPath>>,
    query: &str,
    smart_case: bool,
    max_results: usize,
    path_style: PathStyle,
) -> Vec<PathMatch> {
    let lowercase_query = query.chars().map(simple_lowercase).collect::<Vec<_>>();
    let query = query.chars().collect::<Vec<_>>();
    let query_char_bag = CharBag::from(&lowercase_query[..]);

    let mut matcher = Matcher::new(&query, &lowercase_query, query_char_bag, smart_case, true);

    let mut results = Vec::with_capacity(candidates.len());
    let (path_prefix, path_prefix_chars, lowercase_prefix) = match worktree_root_name {
        Some(worktree_root_name) => {
            let mut path_prefix_chars = worktree_root_name
                .display(path_style)
                .chars()
                .collect::<Vec<_>>();
            path_prefix_chars.extend(path_style.primary_separator().chars());
            let lowercase_pfx = path_prefix_chars
                .iter()
                .map(|c| simple_lowercase(*c))
                .collect::<Vec<_>>();

            (worktree_root_name, path_prefix_chars, lowercase_pfx)
        }
        None => (RelPath::empty_arc(), Default::default(), Default::default()),
    };

    matcher.match_candidates(
        &path_prefix_chars,
        &lowercase_prefix,
        candidates.into_iter(),
        &mut results,
        &AtomicBool::new(false),
        |candidate, score, positions| PathMatch {
            score,
            worktree_id,
            positions: positions.clone(),
            is_dir: candidate.is_dir,
            path: candidate.path.into(),
            path_prefix: path_prefix.clone(),
            distance_to_relative_ancestor: usize::MAX,
        },
    );
    gpui_util::truncate_to_bottom_n_sorted_by(&mut results, max_results, &|a, b| b.cmp(a));
    results
}

/// Always compiled for wasm and for native tests so results can be pinned
/// without a wasm runtime. The browser path of [`match_path_sets`] calls this
/// instead of `executor.scoped`.
#[cfg(any(target_family = "wasm", test))]
fn match_path_sets_inline<'a, Set: PathMatchCandidateSet<'a>>(
    candidate_sets: &'a [Set],
    query: &str,
    relative_to: &Option<Arc<RelPath>>,
    smart_case: bool,
    max_results: usize,
    cancel_flag: &AtomicBool,
) -> Vec<PathMatch> {
    let path_count: usize = candidate_sets.iter().map(|s| s.len()).sum();
    if path_count == 0 {
        return Vec::new();
    }

    let path_style = candidate_sets[0].path_style();

    let query = query
        .chars()
        .map(|char| {
            if path_style.is_windows() && char == '\\' {
                '/'
            } else {
                char
            }
        })
        .collect::<Vec<_>>();

    let lowercase_query = query
        .iter()
        .map(|query| simple_lowercase(*query))
        .collect::<Vec<_>>();

    let query_char_bag = CharBag::from_iter(lowercase_query.iter().copied());
    let mut results = Vec::with_capacity(max_results.min(path_count));
    let mut matcher = Matcher::new(&query, &lowercase_query, query_char_bag, smart_case, true);

    for candidate_set in candidate_sets {
        if cancel_flag.load(atomic::Ordering::Acquire) {
            break;
        }

        let worktree_id = candidate_set.id();
        let mut prefix = candidate_set
            .prefix()
            .as_unix_str()
            .chars()
            .collect::<Vec<_>>();
        if !candidate_set.root_is_file() && !prefix.is_empty() {
            prefix.push('/');
        }
        let lowercase_prefix = prefix
            .iter()
            .map(|c| simple_lowercase(*c))
            .collect::<Vec<_>>();
        matcher.match_candidates(
            &prefix,
            &lowercase_prefix,
            candidate_set.candidates(0),
            &mut results,
            cancel_flag,
            |candidate, score, positions| PathMatch {
                score,
                worktree_id,
                positions: positions.clone(),
                path: Arc::from(candidate.path),
                is_dir: candidate.is_dir,
                path_prefix: candidate_set.prefix(),
                distance_to_relative_ancestor: relative_to
                    .as_ref()
                    .map_or(usize::MAX, |relative_to| {
                        distance_between_paths(candidate.path, relative_to.as_ref())
                    }),
            },
        );
    }

    if cancel_flag.load(atomic::Ordering::Acquire) {
        return Vec::new();
    }

    gpui_util::truncate_to_bottom_n_sorted_by(&mut results, max_results, &|a, b| b.cmp(a));
    results
}

pub async fn match_path_sets<'a, Set: PathMatchCandidateSet<'a>>(
    candidate_sets: &'a [Set],
    query: &str,
    relative_to: &Option<Arc<RelPath>>,
    smart_case: bool,
    max_results: usize,
    cancel_flag: &AtomicBool,
    executor: BackgroundExecutor,
) -> Vec<PathMatch> {
    let path_count: usize = candidate_sets.iter().map(|s| s.len()).sum();
    if path_count == 0 {
        return Vec::new();
    }

    // The browser's main thread has no other threads to fan out to, and `scoped` only
    // completes once its spawned tasks have been polled -- which cannot happen while a
    // caller is synchronously awaiting this future. Match inline instead: same results,
    // no parallelism, which is what a single-threaded dispatcher gives either way.
    #[cfg(target_family = "wasm")]
    {
        let _ = &executor;
        return match_path_sets_inline(
            candidate_sets,
            query,
            relative_to,
            smart_case,
            max_results,
            cancel_flag,
        );
    }

    #[cfg(not(target_family = "wasm"))]
    let path_style = candidate_sets[0].path_style();

    #[cfg(not(target_family = "wasm"))]
    let query = query
        .chars()
        .map(|char| {
            if path_style.is_windows() && char == '\\' {
                '/'
            } else {
                char
            }
        })
        .collect::<Vec<_>>();

    #[cfg(not(target_family = "wasm"))]
    let lowercase_query = query
        .iter()
        .map(|query| simple_lowercase(*query))
        .collect::<Vec<_>>();

    #[cfg(not(target_family = "wasm"))]
    let query = &query;
    #[cfg(not(target_family = "wasm"))]
    let lowercase_query = &lowercase_query;
    #[cfg(not(target_family = "wasm"))]
    let query_char_bag = CharBag::from_iter(lowercase_query.iter().copied());

    #[cfg(not(target_family = "wasm"))]
    let num_cpus = executor.num_cpus().min(path_count);
    #[cfg(not(target_family = "wasm"))]
    let segment_size = path_count.div_ceil(num_cpus);
    #[cfg(not(target_family = "wasm"))]
    let mut segment_results = (0..num_cpus)
        .map(|_| Vec::with_capacity(max_results))
        .collect::<Vec<_>>();

    #[cfg(not(target_family = "wasm"))]
    executor
        .scoped(|scope| {
            for (segment_idx, results) in segment_results.iter_mut().enumerate() {
                scope.spawn(async move {
                    let segment_start = segment_idx * segment_size;
                    let segment_end = segment_start + segment_size;
                    let mut matcher =
                        Matcher::new(query, lowercase_query, query_char_bag, smart_case, true);

                    let mut tree_start = 0;
                    for candidate_set in candidate_sets {
                        if cancel_flag.load(atomic::Ordering::Acquire) {
                            break;
                        }

                        let tree_end = tree_start + candidate_set.len();

                        if tree_start < segment_end && segment_start < tree_end {
                            let start = cmp::max(tree_start, segment_start) - tree_start;
                            let end = cmp::min(tree_end, segment_end) - tree_start;
                            let candidates = candidate_set.candidates(start).take(end - start);

                            let worktree_id = candidate_set.id();
                            let mut prefix = candidate_set
                                .prefix()
                                .as_unix_str()
                                .chars()
                                .collect::<Vec<_>>();
                            if !candidate_set.root_is_file() && !prefix.is_empty() {
                                prefix.push('/');
                            }
                            let lowercase_prefix = prefix
                                .iter()
                                .map(|c| simple_lowercase(*c))
                                .collect::<Vec<_>>();
                            matcher.match_candidates(
                                &prefix,
                                &lowercase_prefix,
                                candidates,
                                results,
                                cancel_flag,
                                |candidate, score, positions| PathMatch {
                                    score,
                                    worktree_id,
                                    positions: positions.clone(),
                                    path: Arc::from(candidate.path),
                                    is_dir: candidate.is_dir,
                                    path_prefix: candidate_set.prefix(),
                                    distance_to_relative_ancestor: relative_to.as_ref().map_or(
                                        usize::MAX,
                                        |relative_to| {
                                            distance_between_paths(
                                                candidate.path,
                                                relative_to.as_ref(),
                                            )
                                        },
                                    ),
                                },
                            );
                        }
                        if tree_end >= segment_end {
                            break;
                        }
                        tree_start = tree_end;
                    }
                })
            }
        })
        .await;

    #[cfg(not(target_family = "wasm"))]
    {
        if cancel_flag.load(atomic::Ordering::Acquire) {
            return Vec::new();
        }

        let mut results = segment_results.concat();
        gpui_util::truncate_to_bottom_n_sorted_by(&mut results, max_results, &|a, b| b.cmp(a));
        results
    }
}

/// Compute the distance from a given path to some other path
/// If there is no shared path, returns usize::MAX
fn distance_between_paths(path: &RelPath, relative_to: &RelPath) -> usize {
    let mut path_components = path.components();
    let mut relative_components = relative_to.components();

    while path_components
        .next()
        .zip(relative_components.next())
        .map(|(path_component, relative_component)| path_component == relative_component)
        .unwrap_or_default()
    {}
    path_components.count() + relative_components.count() + 1
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::AtomicBool};

    use path::{
        PathStyle,
        rel_path::{RelPath, RelPathBuf},
    };

    use super::{
        PathMatchCandidate, PathMatchCandidateSet, distance_between_paths, match_path_sets_inline,
    };
    use crate::CharBag;

    struct TestCandidateSet {
        id: usize,
        prefix: Arc<RelPath>,
        root_is_file: bool,
        entries: Vec<(bool, RelPathBuf, CharBag)>,
    }

    struct TestCandidateIter<'a> {
        entries: std::slice::Iter<'a, (bool, RelPathBuf, CharBag)>,
    }

    impl<'a> Iterator for TestCandidateIter<'a> {
        type Item = PathMatchCandidate<'a>;

        fn next(&mut self) -> Option<Self::Item> {
            self.entries
                .next()
                .map(|(is_dir, path, char_bag)| PathMatchCandidate {
                    is_dir: *is_dir,
                    path: path.as_rel_path(),
                    char_bag: *char_bag,
                })
        }
    }

    impl<'a> PathMatchCandidateSet<'a> for TestCandidateSet {
        type Candidates = TestCandidateIter<'a>;

        fn id(&self) -> usize {
            self.id
        }

        fn len(&self) -> usize {
            self.entries.len()
        }

        fn root_is_file(&self) -> bool {
            self.root_is_file
        }

        fn prefix(&self) -> Arc<RelPath> {
            self.prefix.clone()
        }

        fn candidates(&'a self, start: usize) -> Self::Candidates {
            TestCandidateIter {
                entries: self.entries[start..].iter(),
            }
        }

        fn path_style(&self) -> PathStyle {
            PathStyle::Unix
        }
    }

    fn rel(path: &str) -> RelPathBuf {
        RelPath::new_test(path).into_owned()
    }

    fn entry(path: &str, is_dir: bool) -> (bool, RelPathBuf, CharBag) {
        let path = rel(path);
        let char_bag = CharBag::from(path.as_rel_path().as_unix_str());
        (is_dir, path, char_bag)
    }

    fn sample_set() -> TestCandidateSet {
        TestCandidateSet {
            id: 7,
            prefix: RelPath::empty_arc(),
            root_is_file: false,
            entries: vec![
                entry("src/main.rs", false),
                entry("src/lib.rs", false),
                entry("README.md", false),
            ],
        }
    }

    fn matched_paths(query: &str, max_results: usize, cancel: bool) -> Vec<String> {
        let set = sample_set();
        let cancel_flag = AtomicBool::new(cancel);
        match_path_sets_inline(
            std::slice::from_ref(&set),
            query,
            &None,
            false,
            max_results,
            &cancel_flag,
        )
        .into_iter()
        .map(|path_match| path_match.path.as_unix_str().to_string())
        .collect()
    }

    #[test]
    fn test_distance_between_paths_empty() {
        distance_between_paths(RelPath::empty(), RelPath::empty());
    }

    #[test]
    fn inline_match_main_returns_only_main_rs() {
        let actual = matched_paths("main", 10, false);
        let correct = vec!["src/main.rs".to_string()];
        assert_eq!(
            actual, correct,
            "actual vs correct for query 'main' (max_results=10)"
        );
    }

    #[test]
    fn inline_match_rs_returns_rust_sources_not_readme() {
        let actual = matched_paths("rs", 10, false);
        let correct_count = 2;
        assert_eq!(
            actual.len(),
            correct_count,
            "actual len {} vs correct {correct_count} for query 'rs'; actual={actual:?}",
            actual.len()
        );
        assert!(
            actual.contains(&"src/main.rs".to_string()),
            "actual {actual:?} vs correct containing src/main.rs"
        );
        assert!(
            actual.contains(&"src/lib.rs".to_string()),
            "actual {actual:?} vs correct containing src/lib.rs"
        );
        assert!(
            !actual.contains(&"README.md".to_string()),
            "actual {actual:?} vs correct not containing README.md"
        );
    }

    #[test]
    fn inline_match_respects_max_results() {
        let actual = matched_paths("rs", 1, false);
        assert_eq!(
            actual.len(),
            1,
            "actual len {} vs correct len 1 for query 'rs' max_results=1; actual={actual:?}",
            actual.len()
        );
        assert!(
            actual[0] == "src/main.rs" || actual[0] == "src/lib.rs",
            "actual {:?} vs correct one of [src/main.rs, src/lib.rs]",
            actual
        );
    }

    #[test]
    fn inline_match_cancel_returns_empty() {
        let actual = matched_paths("main", 10, true);
        let correct: Vec<String> = Vec::new();
        assert_eq!(
            actual, correct,
            "actual vs correct when cancel_flag is already set"
        );
    }

    #[test]
    fn inline_match_empty_sets_returns_empty() {
        let actual = match_path_sets_inline(
            &[] as &[TestCandidateSet],
            "main",
            &None,
            false,
            10,
            &AtomicBool::new(false),
        );
        assert_eq!(
            actual.len(),
            0,
            "actual len {} vs correct 0 for empty candidate_sets",
            actual.len()
        );
    }
}
