use std::collections::{BTreeMap, HashMap, HashSet};

use crate::model::TokenUsage;

use super::CostStateRow;
use super::transcript::Call;

struct Discount<'a> {
    fork: &'a str,
    start: i64,
    carried: BTreeMap<String, TokenUsage>,
    copied: HashSet<&'a str>,
}

/// A session forked from another restores the other's total, `startTime`
/// included, and counts on from it, while it copies only part of the other's
/// calls. Compared whole, the other session's uncounted part counted again in
/// the fork. So the fork's totals lose the other's last total written before
/// the fork's first call of its own, and the copied calls that total covers
/// leave its side of the comparison; a copy of a later call stays, or it
/// would read as uncounted in the fork while the other session counts it.
/// Sessions that only share a `startTime`, with no call in common, are left
/// as they are.
pub(super) fn discount_forks<'a>(
    logs: &mut HashMap<&'a str, Vec<CostStateRow>>,
    transcripts: &mut HashMap<&'a str, Vec<Call<'a>>>,
) {
    let mut by_start: HashMap<i64, Vec<&'a str>> = HashMap::new();
    for (&session, rows) in logs.iter() {
        let starts: HashSet<i64> = rows.iter().filter_map(|row| row.start_time).collect();
        for start in starts {
            by_start.entry(start).or_default().push(session);
        }
    }
    let keys: HashMap<&'a str, HashSet<&'a str>> = transcripts
        .iter()
        .map(|(&session, calls)| (session, calls.iter().filter_map(|call| call.key).collect()))
        .collect();
    let mut discounts: Vec<Discount> = Vec::new();
    for (&start, sessions) in &by_start {
        for &fork in sessions {
            let (Some(fork_keys), Some(fork_calls)) = (keys.get(fork), transcripts.get(fork))
            else {
                continue;
            };
            let parent = sessions
                .iter()
                .filter(|&&other| other != fork)
                .filter_map(|&other| {
                    let other_keys = keys.get(other)?;
                    if fork_keys.is_disjoint(other_keys) {
                        return None;
                    }
                    let first_own = fork_calls
                        .iter()
                        .filter(|call| call.key.is_none_or(|key| !other_keys.contains(key)))
                        .filter_map(|call| call.timestamp)
                        .min()?;
                    // Taken when the stamp before it precedes the fork's
                    // first call, though it may be written a little later:
                    // a later total is larger, so the doubt subtracts more.
                    let base = logs.get(other)?.iter().rfind(|row| {
                        row.start_time == Some(start)
                            && row.before.is_none_or(|before| before < first_own)
                    })?;
                    Some((other, base))
                })
                .max_by_key(|(_, base)| base.volume());
            if let Some((parent, base)) = parent {
                discounts.push(Discount {
                    fork,
                    start,
                    carried: base.models.clone(),
                    copied: transcripts[parent]
                        .iter()
                        .filter(|call| call.timestamp.is_some_and(|at| base.covers(at)))
                        .filter_map(|call| call.key)
                        .collect(),
                });
            }
        }
    }
    for discount in discounts {
        if let Some(rows) = logs.get_mut(discount.fork) {
            for row in rows
                .iter_mut()
                .filter(|row| row.start_time == Some(discount.start))
            {
                for (model, usage) in &mut row.models {
                    if let Some(carried) = discount.carried.get(model) {
                        subtract(usage, carried);
                    }
                }
            }
        }
        if let Some(calls) = transcripts.get_mut(discount.fork) {
            calls.retain(|call| call.key.is_none_or(|key| !discount.copied.contains(key)));
        }
    }
}

fn subtract(usage: &mut TokenUsage, carried: &TokenUsage) {
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
