use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use time::{Date, OffsetDateTime, UtcOffset};

use crate::collector::FileEvents;
use crate::model::{Billing, SourceKind, TokenUsage, UsageEvent, UsageGranularity};
use crate::timestamp::to_local;

use super::CostStateRow;
use super::fork::discount_forks;
use super::transcript::{Call, transcripts};

/// A local date and hour.
type Slot = (Date, u8);

/// Each session-hour's share of the uncounted usage of each model.
type Buckets = BTreeMap<(String, String, Slot), UsageEvent>;

/// What each session's cost-state holds over its transcript, per model and
/// field, spread over the local hours the session ran in proportion to its
/// transcript volume in each. Spread by day and stamped at the day's last
/// call, it piled into the late hours of sessions that ran past midnight.
pub(in crate::collector::claude) fn uncounted_calls(
    per_file: &[(PathBuf, Option<FileEvents>)],
    local_offset: UtcOffset,
) -> Vec<UsageEvent> {
    let mut logs = log_per_session(per_file);
    if logs.is_empty() {
        return Vec::new();
    }
    let sessions: HashSet<&str> = logs.keys().copied().collect();
    let mut transcripts = transcripts(per_file, &sessions);
    // Judged after a fork's discount, a restart whose total outgrew what the
    // discount left of the fork's read as carried, and two runs merged.
    let restarts: HashMap<&str, Vec<bool>> = logs
        .iter()
        .map(|(&session, rows)| (session, restarts(rows)))
        .collect();
    discount_forks(&mut logs, &mut transcripts, &restarts);
    let mut buckets = Buckets::new();
    for (session, rows) in logs {
        let Some(calls) = transcripts.remove(session) else {
            continue;
        };
        let runs = runs(&rows, &restarts[session]);
        spread_session(session, &runs, calls, local_offset, &mut buckets);
    }
    buckets.into_values().collect()
}

/// Every session's rows from one file. A session whose rows are in more than
/// one file keeps the file that wrote last: the totals are cumulative, and
/// adding two files' would count their shared part twice.
fn log_per_session(per_file: &[(PathBuf, Option<FileEvents>)]) -> HashMap<&str, Vec<CostStateRow>> {
    let mut logs: HashMap<&str, Vec<&CostStateRow>> = HashMap::new();
    for (_, events) in per_file {
        let Some(events) = events else {
            continue;
        };
        let mut by_session: HashMap<&str, Vec<&CostStateRow>> = HashMap::new();
        for row in &events.claude_cost_state.rows {
            by_session
                .entry(row.session_id.as_str())
                .or_default()
                .push(row);
        }
        for (session, rows) in by_session {
            let rank = |rows: &[&CostStateRow]| {
                rows.last()
                    .map(|row| (row.before, row.volume()))
                    .unwrap_or_default()
            };
            match logs.get(session) {
                Some(kept) if rank(kept) >= rank(&rows) => {}
                _ => {
                    logs.insert(session, rows);
                }
            }
        }
    }
    logs.into_iter()
        .map(|(session, rows)| (session, rows.into_iter().cloned().collect()))
        .collect()
}

/// Whether each row begins a new run of a session's totals. A process that
/// resumes a session either carries the previous total forward or starts again
/// from zero; only a restart begins a new run. Splitting at every process
/// counted a carried-over total once per process. A carried total never
/// loses a counter. A restart can outgrow the previous total and still lack
/// the models it has not used yet; judged by volume alone, it read as carried
/// and their excess was lost.
fn restarts(rows: &[CostStateRow]) -> Vec<bool> {
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            index > 0
                && row.start_time != rows[index - 1].start_time
                && !carries(&rows[index - 1], row)
        })
        .collect()
}

/// The rows that close each run.
fn runs<'a>(rows: &'a [CostStateRow], restarts: &[bool]) -> Vec<&'a CostStateRow> {
    let mut runs: Vec<&CostStateRow> = Vec::new();
    for (row, &restarted) in rows.iter().zip(restarts) {
        match runs.last_mut() {
            Some(last) if !restarted => *last = row,
            _ => runs.push(row),
        }
    }
    runs
}

fn carries(earlier: &CostStateRow, later: &CostStateRow) -> bool {
    earlier.models.iter().all(|(model, before)| {
        later
            .models
            .get(model)
            .is_some_and(|after| at_least(after, before))
    })
}

/// Whether every counter of `usage` is at least that of `floor`.
pub(super) fn at_least(usage: &TokenUsage, floor: &TokenUsage) -> bool {
    usage.input_tokens >= floor.input_tokens
        && usage.output_tokens >= floor.output_tokens
        && usage.cache_creation_input_tokens >= floor.cache_creation_input_tokens
        && usage.cache_read_input_tokens >= floor.cache_read_input_tokens
        && usage.cache_creation_ephemeral_5m_input_tokens
            >= floor.cache_creation_ephemeral_5m_input_tokens
        && usage.cache_creation_ephemeral_1h_input_tokens
            >= floor.cache_creation_ephemeral_1h_input_tokens
}

pub(super) fn subtract(usage: &mut TokenUsage, carried: &TokenUsage) {
    let less = |field: &mut u64, by: u64| *field = field.saturating_sub(by);
    less(&mut usage.input_tokens, carried.input_tokens);
    less(&mut usage.output_tokens, carried.output_tokens);
    less(
        &mut usage.cache_creation_input_tokens,
        carried.cache_creation_input_tokens,
    );
    less(
        &mut usage.cache_read_input_tokens,
        carried.cache_read_input_tokens,
    );
    less(
        &mut usage.cache_creation_ephemeral_5m_input_tokens,
        carried.cache_creation_ephemeral_5m_input_tokens,
    );
    less(
        &mut usage.cache_creation_ephemeral_1h_input_tokens,
        carried.cache_creation_ephemeral_1h_input_tokens,
    );
}

/// Each call belongs to the first run whose total was written after it. The
/// last run takes the rest, unless later calls were never totalled.
fn spread_session(
    session: &str,
    runs: &[&CostStateRow],
    calls: Vec<Call>,
    local_offset: UtcOffset,
    buckets: &mut Buckets,
) {
    let mut per_run: Vec<Vec<Call>> = runs.iter().map(|_| Vec::new()).collect();
    let last = runs.len() - 1;
    for call in calls {
        let run = match call.timestamp {
            None => Some(last),
            Some(at) => (0..runs.len())
                .find(|&index| (index == last && !runs[index].stale) || runs[index].covers(at)),
        };
        if let Some(run) = run {
            per_run[run].push(call);
        }
    }
    for (row, calls) in runs.iter().zip(per_run) {
        spread_run(session, row, &calls, local_offset, buckets);
    }
}

fn spread_run(
    session: &str,
    total: &CostStateRow,
    calls: &[Call],
    local_offset: UtcOffset,
    buckets: &mut Buckets,
) {
    let mut counted: HashMap<&str, TokenUsage> = HashMap::new();
    let mut slots: BTreeMap<Slot, (u64, OffsetDateTime, &Call)> = BTreeMap::new();
    for call in calls {
        counted
            .entry(call.model())
            .or_default()
            .add_assign(&call.usage);
        let Some((at, local)) = call
            .timestamp
            .and_then(|at| Some((at, to_local(at, local_offset)?)))
        else {
            continue;
        };
        let slot = slots
            .entry((local.date(), local.hour()))
            .or_insert((0, at, call));
        slot.0 = slot.0.saturating_add(call.usage.token_volume());
        if at > slot.1 {
            (slot.1, slot.2) = (at, call);
        }
    }
    let weights: Vec<u64> = slots.values().map(|(volume, _, _)| *volume).collect();
    if weights.iter().all(|weight| *weight == 0) {
        return;
    }
    for (model, recorded) in &total.models {
        let excess = excess(recorded, counted.get(model.as_str()));
        if excess.token_volume() == 0 {
            continue;
        }
        let shares = split_by_weight(&excess, &weights);
        for ((slot, &(_, at, latest)), usage) in slots.iter().zip(shares) {
            if usage.token_volume() == 0 {
                continue;
            }
            let event = buckets
                .entry((session.to_owned(), model.clone(), *slot))
                .or_insert_with(|| UsageEvent {
                    timestamp: Some(at),
                    session_id: Some(session.to_owned()),
                    model: Some(model.clone()),
                    source_kind: if latest.source_kind == SourceKind::Background {
                        SourceKind::Background
                    } else {
                        SourceKind::Main
                    },
                    attribution_agent: None,
                    attribution_skill: None,
                    project: latest.project.clone(),
                    usage: TokenUsage::default(),
                    granularity: UsageGranularity::Remainder,
                    billing: Billing::default(),
                });
            event.usage.add_assign(&usage);
            if Some(at) > event.timestamp {
                event.timestamp = Some(at);
                event.project = latest.project.clone().or(event.project.take());
            }
        }
    }
}

/// Per field, never below zero: a total that started after some of the
/// session's calls, or a call logged twice, leaves the transcript ahead.
/// The cache-write split is clamped to the write excess so that a row's
/// 5m and 1h parts never exceed its writes.
fn excess(recorded: &TokenUsage, counted: Option<&TokenUsage>) -> TokenUsage {
    let mut excess = recorded.clone();
    if let Some(counted) = counted {
        subtract(&mut excess, counted);
    }
    let writes = excess.cache_creation_input_tokens;
    let long = excess.cache_creation_ephemeral_1h_input_tokens.min(writes);
    excess.cache_creation_ephemeral_1h_input_tokens = long;
    excess.cache_creation_ephemeral_5m_input_tokens = excess
        .cache_creation_ephemeral_5m_input_tokens
        .min(writes - long);
    excess
}

/// Splits every field by largest remainder so the parts add up to the whole.
/// The 5m, 1h and unsplit writes are split apart and added back, so every
/// part keeps its 5m and 1h within its writes.
fn split_by_weight(usage: &TokenUsage, weights: &[u64]) -> Vec<TokenUsage> {
    let unsplit = usage.cache_creation_input_tokens
        - usage.cache_creation_ephemeral_5m_input_tokens
        - usage.cache_creation_ephemeral_1h_input_tokens;
    let input = apportion(usage.input_tokens, weights);
    let output = apportion(usage.output_tokens, weights);
    let reads = apportion(usage.cache_read_input_tokens, weights);
    let short = apportion(usage.cache_creation_ephemeral_5m_input_tokens, weights);
    let long = apportion(usage.cache_creation_ephemeral_1h_input_tokens, weights);
    let rest = apportion(unsplit, weights);
    (0..weights.len())
        .map(|index| TokenUsage {
            input_tokens: input[index],
            output_tokens: output[index],
            cache_creation_input_tokens: short[index] + long[index] + rest[index],
            cache_read_input_tokens: reads[index],
            cache_creation_ephemeral_5m_input_tokens: short[index],
            cache_creation_ephemeral_1h_input_tokens: long[index],
            ..TokenUsage::default()
        })
        .collect()
}

pub(super) fn apportion(total: u64, weights: &[u64]) -> Vec<u64> {
    let weight_sum: u128 = weights.iter().map(|weight| u128::from(*weight)).sum();
    let mut shares = Vec::with_capacity(weights.len());
    let mut remainders = Vec::with_capacity(weights.len());
    let mut given = 0_u64;
    for (index, weight) in weights.iter().enumerate() {
        let exact = u128::from(total) * u128::from(*weight);
        let share = u64::try_from(exact / weight_sum).unwrap_or(total);
        given += share;
        shares.push(share);
        remainders.push((exact % weight_sum, index));
    }
    remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (_, index) in remainders
        .into_iter()
        .take(usize::try_from(total - given).unwrap_or(0))
    {
        shares[index] += 1;
    }
    shares
}
