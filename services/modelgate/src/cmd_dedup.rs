//! `modelgate dedup distribution` and `dedup sample`: the two readers of the comparison records
//! `embed run` writes. Neither sends, writes or judges: one prints counts per similarity band,
//! the other draws pairs for a person to label. `docs/design/architecture.md` section 3.5,
//! "Repeated releases".

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use canon::FileDigest;
use serde_json::{json, Value};

use crate::cmd_shadow::{admissible, invalid, unreadable, ShadowError};
use crate::embed_store::{Best, Candidate, EmbedStore, EMBED_DB_FILE, LIST_GUARDED, LIST_NEAREST};

pub const DISTRIBUTION_SCHEMA: &str = "modelgate.dedup_distribution/1";
pub const SAMPLE_SCHEMA: &str = "modelgate.dedup_sample/1";
/// Bands fixed in code before any similarity was read (2026-11-23): one below 0.50, then 25 of
/// width 0.02; the last holds 1.00.
const BAND_FLOOR_PPM: i64 = 500_000;
const BAND_WIDTH_PPM: i64 = 20_000;
const BANDS: usize = 26;
const AGE_EDGES: [(&str, i64); 5] = [
    ("1h", 3_600),
    ("6h", 21_600),
    ("24h", 86_400),
    ("72h", 259_200),
    ("7d", 604_800),
];
const HOSTS_LISTED: usize = 20;
/// An overlap over fewer than 3 tokens can only be 0, 0.5 or 1.
const FEW_NUMBERS: i64 = 3;
const EXCERPT_HEAD_CHARS: usize = 3_000;
const EXCERPT_TAIL_CHARS: usize = 1_000;
const EXCERPT_GAP: &str = "\n[...]\n";
const PAIR_ID_HEX: usize = 16;
const FIELD: char = '\u{1f}';

fn band(similarity_ppm: i64) -> usize {
    if similarity_ppm < BAND_FLOOR_PPM {
        return 0;
    }
    let above = (similarity_ppm - BAND_FLOOR_PPM) / BAND_WIDTH_PPM;
    (usize::try_from(above).unwrap_or(BANDS) + 1).min(BANDS - 1)
}

fn band_edges(index: usize) -> (i64, i64) {
    if index == 0 {
        return (-1_000_000, BAND_FLOOR_PPM);
    }
    let from = BAND_FLOOR_PPM + BAND_WIDTH_PPM * i64::try_from(index - 1).unwrap_or(0);
    (from, from + BAND_WIDTH_PPM)
}

fn present(store: &Path) -> Result<EmbedStore, ShadowError> {
    let file = store.join(EMBED_DB_FILE);
    if !file.is_file() {
        return Err(ShadowError::InputAbsent(format!(
            "{}: no embed run has written here",
            file.display()
        )));
    }
    Ok(EmbedStore::read_only(store)?)
}

#[derive(Default, Clone)]
struct BandCount {
    stories: u64,
    shares_host: u64,
    tickers_shared: u64,
    same_source: u64,
    age_within: [u64; AGE_EDGES.len()],
}

impl BandCount {
    fn add(&mut self, candidate: &Candidate) {
        self.stories += 1;
        self.shares_host += u64::from(!candidate.hosts_shared.is_empty());
        self.tickers_shared += u64::from(candidate.tickers_shared > 0);
        self.same_source += u64::from(candidate.same_source);
        for (slot, (_, seconds)) in self.age_within.iter_mut().zip(AGE_EDGES) {
            *slot += u64::from(candidate.age_seconds <= seconds);
        }
    }

    fn json(&self, index: usize) -> Value {
        let (from_ppm, to_ppm) = band_edges(index);
        let ages: BTreeMap<&str, u64> = AGE_EDGES
            .iter()
            .zip(self.age_within)
            .map(|((name, _), count)| (*name, count))
            .collect();
        json!({
            "from_ppm": from_ppm, "to_ppm": to_ppm, "stories": self.stories,
            "shares_host": self.shares_host, "tickers_shared": self.tickers_shared,
            "same_source": self.same_source, "age_within": ages,
        })
    }
}

#[derive(Default)]
struct Group {
    stories: u64,
    no_candidate: u64,
    unembedded: i64,
    no_visible_host: u64,
    few_numbers: u64,
    nearest: Vec<BandCount>,
    guarded: Vec<BandCount>,
    hosts: HashMap<String, u64>,
}

#[derive(Default)]
struct CanarySeries {
    own: Vec<i64>,
    pair: Vec<i64>,
    answers: u64,
}

/// `(hash of the seed and the pair, story, candidate)`.
type Drawn = (String, String, String);

fn series(values: &[i64]) -> Value {
    json!({
        "answers": values.len(), "min": values.iter().min(), "max": values.iter().max(),
        "last": values.last(),
    })
}

/// Counts of stories per band of their best candidate, per comparison version and space: a
/// similarity is never pooled across either.
pub fn distribution(store: &Path) -> Result<Value, ShadowError> {
    let store = present(store)?;
    let mut groups: BTreeMap<(String, String), Group> = BTreeMap::new();
    let of = |best: &Best| (best.compare_version.clone(), best.space.clone());
    for best in store.best()? {
        let group = groups.entry(of(&best)).or_default();
        group.nearest.resize(BANDS, BandCount::default());
        group.guarded.resize(BANDS, BandCount::default());
        group.stories += 1;
        group.unembedded += best.unembedded;
        group.no_visible_host += u64::from(best.own_hosts == 0);
        group.few_numbers += u64::from(best.own_numbers < FEW_NUMBERS);
        match &best.nearest {
            None => group.no_candidate += 1,
            Some(candidate) => {
                group.nearest[band(candidate.similarity_ppm)].add(candidate);
                for host in &candidate.hosts_shared {
                    *group.hosts.entry(host.clone()).or_default() += 1;
                }
            }
        }
    }
    for (best, candidate) in store.candidates(LIST_GUARDED)? {
        if candidate.rank == 1 {
            if let Some(group) = groups.get_mut(&of(&best)) {
                group.guarded[band(candidate.similarity_ppm)].add(&candidate);
            }
        }
    }
    let listed: Vec<Value> = groups
        .into_iter()
        .map(|((compare_version, space), group)| {
            let mut hosts: Vec<(String, u64)> = group.hosts.into_iter().collect();
            hosts.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
            hosts.truncate(HOSTS_LISTED);
            let bands = |counts: &[BandCount]| -> Vec<Value> {
                counts
                    .iter()
                    .enumerate()
                    .map(|(index, count)| count.json(index))
                    .collect()
            };
            json!({
                "compare_version": compare_version, "space": space,
                "stories": group.stories, "no_candidate": group.no_candidate,
                "candidates_without_vector": group.unembedded,
                "no_visible_host": group.no_visible_host,
                "fewer_than_three_numbers": group.few_numbers,
                "best_candidate": bands(&group.nearest),
                "best_structurally_alike_candidate": bands(&group.guarded),
                "hosts_shared_with_best_candidate": hosts
                    .iter()
                    .map(|(host, pairs)| json!({"host": host, "pairs": pairs}))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut canaries: BTreeMap<(String, String), CanarySeries> = BTreeMap::new();
    for row in store.canaries()? {
        let entry = canaries.entry((row.space, row.name)).or_default();
        entry.own.extend(row.self_similarity_ppm);
        entry.pair.extend(row.pair_similarity_ppm);
        entry.answers += 1;
    }
    let canaries: Vec<Value> = canaries
        .into_iter()
        .map(|((space, name), found)| {
            json!({
                "space": space, "name": name, "answers": found.answers,
                "similarity_to_first_answer_ppm": series(&found.own),
                "similarity_to_pair_ppm": series(&found.pair),
            })
        })
        .collect();
    Ok(json!({ "schema": DISTRIBUTION_SCHEMA, "groups": listed, "canaries": canaries }))
}

#[derive(Debug, Clone)]
pub struct SampleArgs {
    pub seed: u64,
    pub per_stratum: u64,
    pub snapshot: PathBuf,
    pub store: PathBuf,
    /// Needed only when the store holds comparisons of several spaces.
    pub space: Option<String>,
}

/// The whole body when short, else its head and its tail: the contact block sits at the end.
fn excerpt(body: &str) -> String {
    let characters: Vec<char> = body.chars().collect();
    if characters.len() <= EXCERPT_HEAD_CHARS + EXCERPT_TAIL_CHARS {
        return body.to_owned();
    }
    let head: String = characters[..EXCERPT_HEAD_CHARS].iter().collect();
    let tail: String = characters[characters.len() - EXCERPT_TAIL_CHARS..]
        .iter()
        .collect();
    format!("{head}{EXCERPT_GAP}{tail}")
}

/// Pairs for a person to label, drawn by a hash of the seed and the pair from each stratum (band
/// of the similarity x whether a visible host is shared). The file states no similarity and no
/// band, and its order is the hash's, so the labeller is shown nothing to anchor on.
pub fn sample(args: &SampleArgs) -> Result<Value, ShadowError> {
    let store = present(&args.store)?;
    admissible(&args.snapshot)?;
    let manifest = snapshot_contract::manifest(&args.snapshot).map_err(unreadable)?;
    let mut pairs: BTreeMap<(String, String), (Best, Candidate)> = BTreeMap::new();
    for list in [LIST_NEAREST, LIST_GUARDED] {
        for (best, candidate) in store.candidates(list)? {
            if args
                .space
                .as_ref()
                .is_some_and(|space| *space != best.space)
            {
                continue;
            }
            pairs
                .entry((best.story_id.clone(), candidate.story_id.clone()))
                .or_insert((best, candidate));
        }
    }
    let spaces: HashSet<(&str, &str)> = pairs
        .values()
        .map(|(best, _)| (best.compare_version.as_str(), best.space.as_str()))
        .collect();
    if spaces.len() > 1 {
        let mut named: Vec<String> = spaces
            .iter()
            .map(|(_, space)| (*space).to_owned())
            .collect();
        named.sort();
        named.dedup();
        return Err(invalid(format!(
            "the store holds comparisons of {} versions or spaces; name one with --space: {}",
            spaces.len(),
            named.join(" | ")
        )));
    }
    let (compare_version, space) = spaces
        .iter()
        .next()
        .map(|(version, space)| ((*version).to_owned(), (*space).to_owned()))
        .unzip();
    let population = pairs.len();
    let mut strata: BTreeMap<(usize, bool), Vec<Drawn>> = BTreeMap::new();
    for ((story_id, candidate_id), (_, candidate)) in pairs {
        let drawn = FileDigest::of(
            format!("{}{FIELD}{story_id}{FIELD}{candidate_id}", args.seed).as_bytes(),
        );
        strata
            .entry((
                band(candidate.similarity_ppm),
                !candidate.hosts_shared.is_empty(),
            ))
            .or_default()
            .push((drawn.hex().to_owned(), story_id, candidate_id));
    }
    let take = usize::try_from(args.per_stratum).unwrap_or(usize::MAX);
    let strata_drawn = strata.len();
    let mut chosen: Vec<Drawn> = Vec::new();
    for members in strata.values_mut() {
        members.sort();
        chosen.extend(members.iter().take(take).cloned());
    }
    chosen.sort();
    let wanted: HashSet<&str> = chosen
        .iter()
        .flat_map(|(_, story, candidate)| [story.as_str(), candidate.as_str()])
        .collect();
    let mut bodies: HashMap<String, ((String, i64), String)> = HashMap::new();
    for row in snapshot_contract::rows(&args.snapshot).map_err(unreadable)? {
        let row = row.map_err(unreadable)?;
        let Some(body) = row.body.filter(|_| wanted.contains(row.story_id.as_str())) else {
            continue;
        };
        let sighting = (row.observed_at, row.seq);
        match bodies.get(&row.story_id) {
            Some((kept, _)) if *kept <= sighting => {}
            _ => {
                bodies.insert(row.story_id, (sighting, body));
            }
        }
    }
    let mut without_body = 0_u64;
    let mut listed = Vec::new();
    for (drawn, story_id, candidate_id) in &chosen {
        let (Some((_, own)), Some((_, other))) = (bodies.get(story_id), bodies.get(candidate_id))
        else {
            without_body += 1;
            continue;
        };
        listed.push(json!({
            "pair_id": &drawn[..PAIR_ID_HEX], "story_id": story_id,
            "candidate_story_id": candidate_id, "excerpt_a": excerpt(own),
            "excerpt_b": excerpt(other), "label": null,
        }));
    }
    Ok(json!({
        "schema": SAMPLE_SCHEMA, "seed": args.seed, "per_stratum": args.per_stratum,
        "compare_version": compare_version, "space": space,
        "snapshot_id": manifest.snapshot_id, "population": population,
        "strata": strata_drawn, "pairs_without_body": without_body,
        "labels": ["duplicate", "same_matter", "different"], "pairs": listed,
    }))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::{band, band_edges, distribution, excerpt, sample, SampleArgs, BANDS, EXCERPT_GAP};
    use crate::cmd_embed::tests::compared;
    use crate::cmd_shadow::tests::{at, TestResult};

    /// Section 8 row MG22, the first reader. Mutations 2026-11-23, each alone: every candidate
    /// rank counted and not the first; the host count taken from the guarded list: red each.
    #[test]
    fn the_distribution_counts_each_story_in_the_band_of_its_best_candidate() -> TestResult {
        let (bench, _snapshot) = compared()?;
        let found = distribution(&bench.store())?;
        let group = at(&found, "/groups/0");
        assert_eq!(at(&found, "/groups/1"), serde_json::Value::Null);
        assert_eq!(at(&group, "/stories"), 5);
        assert_eq!(at(&group, "/no_candidate"), 1);
        assert_eq!(at(&group, "/candidates_without_vector"), 0);
        assert_eq!(at(&group, "/no_visible_host"), 0);
        // The copy at 0.6, the second firm at 0.8, and two stories whose best is at 0.
        let stories = |list: &str, index: usize| at(&group, &format!("/{list}/{index}/stories"));
        let best = [0, 6, 16].map(|index| stories("best_candidate", index));
        assert_eq!(best, [2, 1, 1]);
        assert_eq!(at(&group, "/best_candidate/6/from_ppm"), 600_000);
        assert_eq!(at(&group, "/best_candidate/6/shares_host"), 1);
        assert_eq!(at(&group, "/best_candidate/6/age_within/1h"), 1);
        assert_eq!(at(&group, "/best_candidate/16/shares_host"), 0);
        assert_eq!(at(&group, "/best_candidate/16/tickers_shared"), 1);
        assert_eq!(at(&group, "/best_candidate/16/age_within/1h"), 1);
        let alike = [0, 6, 16].map(|index| stories("best_structurally_alike_candidate", index));
        assert_eq!(alike, [0, 1, 0]);
        assert_eq!(
            at(&group, "/hosts_shared_with_best_candidate"),
            serde_json::json!([{"host": "harrowgate-robotics.example", "pairs": 1}])
        );
        assert_eq!(at(&found, "/canaries/3/name"), "short/1");
        assert_eq!(at(&found, "/canaries/1/similarity_to_pair_ppm/answers"), 1);
        Ok(())
    }

    /// A report over nothing is refused, never printed as zeros.
    #[test]
    fn a_store_no_embed_run_has_written_is_absent_and_not_an_empty_report() -> TestResult {
        let empty = TempDir::new()?;
        let refused = distribution(empty.path()).err();
        assert_eq!(refused.map(|why| why.code()), Some("input_absent"));
        Ok(())
    }

    /// Section 8 row MG22, the second reader. Mutations 2026-11-23, each alone: the similarity
    /// written into a pair; the strata dropped (one draw over all pairs): red each.
    #[test]
    fn a_sample_draws_the_same_pairs_for_a_seed_from_every_stratum_and_states_no_similarity(
    ) -> TestResult {
        let (bench, snapshot) = compared()?;
        let args = SampleArgs {
            seed: 7,
            per_stratum: 1,
            snapshot: snapshot.path().to_path_buf(),
            store: bench.store(),
            space: None,
        };
        let drawn = sample(&args)?;
        assert_eq!(drawn, sample(&args)?);
        assert_eq!(at(&drawn, "/population"), 10);
        assert_eq!(at(&drawn, "/strata"), 3);
        let pairs = at(&drawn, "/pairs");
        let pairs = pairs.as_array().map(Vec::as_slice).unwrap_or_default();
        let mut stories: Vec<&str> = pairs
            .iter()
            .filter_map(|pair| pair["story_id"].as_str())
            .collect();
        stories.sort_unstable();
        assert_eq!(stories.len(), 3);
        assert!(stories.contains(&"firm-two") && stories.contains(&"release-de"));
        let ids: Vec<&str> = pairs.iter().filter_map(|p| p["pair_id"].as_str()).collect();
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]), "{ids:?}");
        for pair in pairs {
            let mut keys: Vec<&str> = pair
                .as_object()
                .into_iter()
                .flat_map(|members| members.keys().map(String::as_str))
                .collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                [
                    "candidate_story_id",
                    "excerpt_a",
                    "excerpt_b",
                    "label",
                    "pair_id",
                    "story_id"
                ]
            );
            assert!(pair["label"].is_null());
        }
        let firm = pairs.iter().find(|pair| pair["story_id"] == "firm-two");
        let shown = firm.map(|pair| pair["excerpt_a"].as_str().unwrap_or_default());
        assert!(shown.is_some_and(|text| text.contains("Okonjo")));
        let elsewhere = SampleArgs {
            space: Some("another space".to_owned()),
            ..args
        };
        assert_eq!(at(&sample(&elsewhere)?, "/population"), 0);
        Ok(())
    }

    /// Mutation 2026-11-23, the last band's `min` removed: red (1.00 lands in a 27th band).
    #[test]
    fn a_similarity_falls_in_one_of_26_bands_and_one_point_zero_in_the_last() {
        let placed = [
            -200_000, 0, 499_999, 500_000, 519_999, 520_000, 979_999, 980_000, 1_000_000,
        ]
        .map(band);
        assert_eq!(placed, [0, 0, 0, 1, 1, 2, 24, 25, 25]);
        assert_eq!(band_edges(0), (-1_000_000, 500_000));
        assert_eq!(band_edges(1), (500_000, 520_000));
        assert_eq!(band_edges(BANDS - 1), (980_000, 1_000_000));
    }

    #[test]
    fn a_long_body_is_shown_as_its_head_and_its_tail() {
        assert_eq!(excerpt("short é"), "short é");
        let long = format!(
            "{}{}{}",
            "h".repeat(3_000),
            "m".repeat(500),
            "t".repeat(1_000)
        );
        let shown = excerpt(&long);
        assert_eq!(
            shown,
            format!("{}{EXCERPT_GAP}{}", "h".repeat(3_000), "t".repeat(1_000))
        );
    }
}
