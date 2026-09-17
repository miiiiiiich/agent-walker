use serde::Serialize;
use time::Date;

use crate::model::{CreditsHistory, LimitDay, LimitsHistory, ModesSummary};

use super::panels::CountDto;
use super::ymd;

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::format::json) struct ModesDto {
    assistant_turns: usize,
    thinking_turns: usize,
    fast_turns: usize,
    efforts: Vec<CountDto>,
    permissions: Vec<CountDto>,
}

impl ModesDto {
    pub(in crate::format::json) fn new(modes: &ModesSummary) -> Self {
        let counts = |rows: &[(String, usize)]| {
            rows.iter()
                .map(|(label, turns)| CountDto {
                    label: label.clone(),
                    turns: *turns,
                })
                .collect()
        };
        Self {
            assistant_turns: modes.assistant_turns,
            thinking_turns: modes.thinking_turns,
            fast_turns: modes.fast_turns,
            efforts: counts(&modes.efforts),
            permissions: counts(&modes.permissions),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::format::json) struct LimitsDto {
    peak: Option<LimitPeakDto>,
    days: Vec<LimitDayDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct LimitPeakDto {
    #[serde(with = "ymd")]
    date: Date,
    used_percent: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct LimitDayDto {
    #[serde(with = "ymd")]
    date: Date,
    used_percent: Option<f64>,
    state: &'static str,
}

impl LimitsDto {
    pub(in crate::format::json) fn new(limits: &LimitsHistory) -> Self {
        Self {
            peak: limits
                .peak
                .map(|(date, used_percent)| LimitPeakDto { date, used_percent }),
            days: limits
                .days
                .iter()
                .map(|&(date, day)| {
                    let (used_percent, state) = match day {
                        LimitDay::Measured(value) => (Some(value), "measured"),
                        LimitDay::NoSample => (None, "no_sample"),
                        LimitDay::NoUse => (None, "no_use"),
                    };
                    LimitDayDto {
                        date,
                        used_percent,
                        state,
                    }
                })
                .collect(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::format::json) struct CreditsDto {
    total: f64,
    peak: Option<CreditDayDto>,
    days: Vec<CreditDayDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct CreditDayDto {
    #[serde(with = "ymd")]
    date: Date,
    credits: f64,
}

impl CreditsDto {
    pub(in crate::format::json) fn new(credits: &CreditsHistory) -> Self {
        Self {
            total: credits.total,
            peak: credits
                .peak
                .map(|(date, credits)| CreditDayDto { date, credits }),
            days: credits
                .days
                .iter()
                .map(|&(date, credits)| CreditDayDto { date, credits })
                .collect(),
        }
    }
}
