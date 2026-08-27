//! Hybrid natural-language retrieval: lexical word-match channel + semantic
//! KNN channel, fused with RRF, grouped by session.

use std::collections::{HashMap, HashSet};

use crate::embed::{EmbedKind, Embedder};
use crate::hybrid::fuse;
use crate::model::format_ms;
use crate::search::{SearchFilters, search};
use crate::segment::SegmentSet;
use crate::store::RecallStore;
use crate::vector::VectorStore;

#[derive(Debug, Clone)]
pub struct AskHit {
    pub turn_id: i64,
    pub session_id: i64,
    pub seq: i64,
    pub agent: String,
    pub source_session_id: String,
    pub cwd: Option<String>,
    pub role: String,
    pub ts: Option<i64>,
    pub ts_source: String,
    pub snippet: String,
    pub score: f32,
    /// Which channels surfaced this turn.
    pub matched_lexical: bool,
    pub matched_semantic: bool,
}

#[derive(Debug, Clone)]
pub struct AskSessionGroup {
    pub best: AskHit,
    pub session_title: Option<String>,
    pub extra_hits: usize,
}

#[derive(Debug, Default)]
pub struct AskResult {
    pub groups: Vec<AskSessionGroup>,
    /// Present when the semantic channel could not run (no model, no
    /// vectors) — the answer is lexical-only, honestly labeled.
    pub notice: Option<String>,
}

const CHANNEL_DEPTH: usize = 200;
const SEM_SNIPPET_LEN: usize = 160;

/// Words worth matching lexically: 3+ chars, identifier-ish.
fn query_words(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for token in query.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-' || c == '.')) {
        let t = token.trim_matches(|c: char| c == '.' || c == '-');
        if t.len() < 3 {
            continue;
        }
        let key = t.to_lowercase();
        if seen.insert(key) && out.len() < 8 {
            out.push(t.to_string());
        }
    }
    out
}

/// Case-variant whole-word pattern for one query word (the trigram index is
/// case-sensitive; three variants cover the overwhelmingly common casings).
fn word_pattern(word: &str) -> String {
    let lower = word.to_lowercase();
    let upper = word.to_uppercase();
    let mut title = lower.clone();
    if let Some(first) = title.get_mut(0..1) {
        // Safe: ASCII-ish tokens dominate; non-ASCII falls back to lower.
        let up = first.to_uppercase();
        title.replace_range(0..1, &up);
    }
    let mut variants: Vec<String> = vec![word.to_string(), lower, upper, title]
        .into_iter()
        .collect::<HashSet<_>>()
        .into_iter()
        .map(|v| regex::escape(&v))
        .collect();
    variants.sort();
    format!(r"\b(?:{})\b", variants.join("|"))
}

#[allow(clippy::too_many_arguments)]
pub fn ask(
    store: &RecallStore,
    segments: &SegmentSet,
    vectors: &VectorStore,
    mut embedder: Option<&mut (dyn Embedder + 'static)>,
    query: &str,
    filters: &SearchFilters,
    k: usize,
) -> Result<AskResult, String> {
    // --- lexical channel: rank turns by how many query words they contain.
    // Harness-injected "user" text (system reminders, global rules) repeats
    // in nearly every session and would drown discussion content, so the
    // ask channels always skip orchestrator turns — `recall search` remains
    // the raw view.
    let mut lexical_filters = filters.clone();
    lexical_filters.human_only = true;
    let words = query_words(query);
    // Words present in a large share of the corpus discriminate nothing
    // and cost seconds of fetch+verify — drop them from the lexical
    // channel (the semantic channel still sees the full query).
    const MAX_WORD_CANDIDATES: usize = 50_000;
    let words: Vec<String> = words
        .into_iter()
        .filter(|w| {
            crate::search::candidate_count(segments, &word_pattern(w))
                .is_some_and(|c| c <= MAX_WORD_CANDIDATES)
        })
        .collect();
    let mut word_hits: HashMap<i64, (usize, Option<i64>)> = HashMap::new();
    for word in &words {
        let result = search(
            store,
            segments,
            &word_pattern(word),
            false,
            &lexical_filters,
            0,
            usize::MAX,
        )?;
        for hit in result.hits {
            let entry = word_hits.entry(hit.turn_id).or_insert((0, hit.ts));
            entry.0 += 1;
        }
    }
    let mut lexical_ranked: Vec<(i64, usize, Option<i64>)> = word_hits
        .iter()
        .map(|(id, (count, ts))| (*id, *count, *ts))
        .collect();
    lexical_ranked.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)).then(b.0.cmp(&a.0)));
    let lexical: Vec<i64> = lexical_ranked
        .iter()
        .take(CHANNEL_DEPTH)
        .map(|(id, _, _)| *id)
        .collect();

    // --- semantic channel.
    let mut semantic: Vec<i64> = Vec::new();
    let mut chunk_start_by_turn: HashMap<i64, i64> = HashMap::new();
    let mut notice = None;
    match embedder.as_deref_mut() {
        None => {
            notice = Some(
                "semantic channel unavailable (no embedding model) — lexical-only answer"
                    .to_string(),
            );
        }
        Some(embedder) => {
            if vectors.meta.segments.is_empty() {
                notice = Some(
                    "semantic channel empty (run `gitpixel recall embed`) — lexical-only answer"
                        .to_string(),
                );
            } else {
                vectors.check_model(embedder.model_id(), embedder.dims())?;
                let qvec = embedder
                    .embed_batch(&[query], EmbedKind::Query)?
                    .into_iter()
                    .next()
                    .ok_or("empty query embedding")?;
                let has_filters = filters.agent.is_some()
                    || filters.repo_prefix.is_some()
                    || filters.since_ms.is_some()
                    || filters.until_ms.is_some()
                    || filters.role.is_some()
                    || filters.human_only
                    || filters.session_id.is_some();
                let allowed = if has_filters {
                    let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                    let sql = crate::search::filter_sql(filters, &mut args);
                    let arg_refs: Vec<&dyn rusqlite::types::ToSql> =
                        args.iter().map(|b| b.as_ref()).collect();
                    Some(
                        store
                            .allowed_chunk_ids(&sql, &arg_refs)
                            .map_err(|e| e.to_string())?,
                    )
                } else {
                    None
                };
                let chunk_hits = vectors.knn(&qvec, CHANNEL_DEPTH * 3, allowed.as_ref());
                let chunk_ids: Vec<i64> = chunk_hits.iter().map(|(id, _)| *id).collect();
                let mapping = store.chunk_turns(&chunk_ids).map_err(|e| e.to_string())?;
                let start_of: HashMap<i64, (i64, i64)> = mapping
                    .into_iter()
                    .map(|(chunk, turn, start)| (chunk, (turn, start)))
                    .collect();
                let mut seen_turns = HashSet::new();
                for (chunk_id, _score) in &chunk_hits {
                    if let Some((turn_id, start)) = start_of.get(chunk_id)
                        && seen_turns.insert(*turn_id)
                    {
                        semantic.push(*turn_id);
                        chunk_start_by_turn.insert(*turn_id, *start);
                        if semantic.len() >= CHANNEL_DEPTH {
                            break;
                        }
                    }
                }
            }
        }
    }

    // --- fuse and materialize.
    let fused = fuse(&lexical, &semantic);
    let lexical_set: HashSet<i64> = lexical.iter().copied().collect();
    let semantic_set: HashSet<i64> = semantic.iter().copied().collect();
    let word_re = if words.is_empty() {
        None
    } else {
        let alts: Vec<String> = words.iter().map(|w| regex::escape(w)).collect();
        regex::RegexBuilder::new(&format!(r"\b(?:{})\b", alts.join("|")))
            .case_insensitive(true)
            .build()
            .ok()
    };

    let mut groups: Vec<AskSessionGroup> = Vec::new();
    let mut per_session: HashMap<i64, usize> = HashMap::new();
    for (turn_id, score) in fused {
        if groups.len() >= k && per_session.len() >= k {
            break;
        }
        let Some(row) = fetch_turn(store, turn_id)? else {
            continue; // stale segment posting; the turn was re-ingested away
        };
        match per_session.get_mut(&row.0.session_id) {
            Some(slot) => {
                *slot += 1;
                if let Some(g) = groups
                    .iter_mut()
                    .find(|g| g.best.session_id == row.0.session_id)
                {
                    g.extra_hits += 1;
                }
                continue;
            }
            None => {
                if groups.len() >= k {
                    continue;
                }
                per_session.insert(row.0.session_id, 1);
            }
        }
        let (mut hit, text, title) = row;
        hit.score = score;
        hit.matched_lexical = lexical_set.contains(&turn_id);
        hit.matched_semantic = semantic_set.contains(&turn_id);
        hit.snippet = make_snippet(
            &text,
            word_re.as_ref(),
            chunk_start_by_turn.get(&turn_id).copied(),
        );
        groups.push(AskSessionGroup {
            best: hit,
            session_title: title,
            extra_hits: 0,
        });
    }
    Ok(AskResult { groups, notice })
}

fn fetch_turn(
    store: &RecallStore,
    turn_id: i64,
) -> Result<Option<(AskHit, String, Option<String>)>, String> {
    use rusqlite::OptionalExtension;
    store
        .connection()
        .query_row(
            "SELECT t.id, t.session_id, t.seq, s.agent, s.source_session_id, s.cwd,
                    t.role, t.ts, s.ts_source, t.text, s.title
             FROM turns t JOIN sessions s ON s.id = t.session_id WHERE t.id = ?1",
            [turn_id],
            |r| {
                Ok((
                    AskHit {
                        turn_id: r.get(0)?,
                        session_id: r.get(1)?,
                        seq: r.get(2)?,
                        agent: r.get(3)?,
                        source_session_id: r.get(4)?,
                        cwd: r.get(5)?,
                        role: r.get(6)?,
                        ts: r.get(7)?,
                        ts_source: r.get(8)?,
                        snippet: String::new(),
                        score: 0.0,
                        matched_lexical: false,
                        matched_semantic: false,
                    },
                    r.get::<_, String>(9)?,
                    r.get::<_, Option<String>>(10)?,
                ))
            },
        )
        .optional()
        .map_err(|e| e.to_string())
}

fn make_snippet(text: &str, word_re: Option<&regex::Regex>, chunk_start: Option<i64>) -> String {
    if let Some(re) = word_re
        && let Some(m) = re.find(text)
    {
        return crate::search::snippet_around(text, m.start(), m.end()).0;
    }
    // Semantic-only hit: show the head of the best-matching chunk.
    let mut start = chunk_start.unwrap_or(0).max(0) as usize;
    start = start.min(text.len());
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + SEM_SNIPPET_LEN).min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    let mut s = text[start..end].replace(['\n', '\r'], " ");
    if end < text.len() {
        s.push('…');
    }
    s
}

/// One line per session group, compact.
pub fn format_group(g: &AskSessionGroup) -> String {
    let h = &g.best;
    let ts = h.ts.map(format_ms).unwrap_or_else(|| "?".to_string());
    let cwd = h.cwd.as_deref().unwrap_or("-");
    let channels = match (h.matched_lexical, h.matched_semantic) {
        (true, true) => "l+s",
        (true, false) => "lex",
        (false, true) => "sem",
        (false, false) => "?",
    };
    let extra = if g.extra_hits > 0 {
        format!(" (+{} more turns)", g.extra_hits)
    } else {
        String::new()
    };
    format!(
        "{}:{} #{} t{} {} {} [{}] \"{}\"{}",
        h.agent,
        &h.source_session_id[..h.source_session_id.len().min(8)],
        h.session_id,
        h.seq,
        ts,
        cwd,
        channels,
        h.snippet,
        extra
    )
}
