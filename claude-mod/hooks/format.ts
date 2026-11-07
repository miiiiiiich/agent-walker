import type { Summary } from '../types'

const BILLION = 1_000_000_000
const MILLION = 1_000_000
const THOUSAND = 1000
const MS_PER_SECOND = 1000
const SECONDS_PER_MINUTE = 60
const MINUTES_PER_HOUR = 60
const HOURS_PER_DAY = 24
const THOUSANDS = /\B(?=(?:\d{3})+$)/gu
const DASH = '—'
const SEPARATOR = ' · '
const PERCENT = 100
const COST_NO_CENTS_FROM = 100

interface Row {
  label: string
  value: string
  detail: string
}

const formatCount = (value: number): string => String(Math.trunc(value)).replace(THOUSANDS, ',')

const formatTokens = (tokens: number): string => {
  if (tokens >= BILLION) {
    return `${(tokens / BILLION).toFixed(1)}B`
  }
  if (tokens >= MILLION) {
    return `${(tokens / MILLION).toFixed(1)}M`
  }
  if (tokens >= THOUSAND) {
    return `${(tokens / THOUSAND).toFixed(1)}K`
  }
  return String(Math.trunc(tokens))
}

const formatUsd = (value: number): string => {
  if (value >= THOUSAND) {
    return `$${formatCount(Math.round(value))}`
  }
  if (value >= COST_NO_CENTS_FROM) {
    return `$${value.toFixed(0)}`
  }
  return `$${value.toFixed(2)}`
}

const formatHours = (ms: number): string => {
  const minutes = Math.floor(ms / (MS_PER_SECOND * SECONDS_PER_MINUTE))
  const remainder = String(minutes % MINUTES_PER_HOUR).padStart(2, '0')
  return `${String(Math.floor(minutes / MINUTES_PER_HOUR))}h ${remainder}m`
}

const formatDuration = (ms: number): string => {
  const seconds = Math.floor(ms / MS_PER_SECOND)
  const minutes = Math.floor(seconds / SECONDS_PER_MINUTE)
  const hours = Math.floor(minutes / MINUTES_PER_HOUR)
  const days = Math.floor(hours / HOURS_PER_DAY)
  const minutesPart = `${String(minutes % MINUTES_PER_HOUR)}m`
  if (days > 0) {
    return `${String(days)}d ${String(hours % HOURS_PER_DAY)}h ${minutesPart}`
  }
  if (hours > 0) {
    return `${String(hours)}h ${minutesPart}`
  }
  if (minutes > 0) {
    return minutesPart
  }
  return `${String(seconds)}s`
}

const formatPercent = (share: number): string => `${(share * PERCENT).toFixed(0)}%`

const perDay = (value: string): string => `${value}/day`

const orDash = <T>(value: T | undefined, format: (value: T) => string): string => {
  if (value === undefined) {
    return DASH
  }
  return format(value)
}

const orEmpty = <T>(value: T | undefined, format: (value: T) => string): string => {
  if (value === undefined) {
    return ''
  }
  return format(value)
}

const totalRows = (summary: Summary): Row[] => [
  {
    label: 'Tokens',
    value: formatTokens(summary.tokens),
    detail: perDay(formatTokens(summary.tokens / summary.days)),
  },
  {
    label: 'Working time',
    value: orDash(summary.workingMs, formatHours),
    detail: orEmpty(summary.workingMs, ms => perDay(formatHours(ms / summary.days))),
  },
  {
    label: 'Cost',
    value: orDash(summary.costUsd, formatUsd),
    detail: orEmpty(summary.costUsd, usd => perDay(formatUsd(usd / summary.days))),
  },
]

const activityRows = (summary: Summary): Row[] => [
  {
    label: 'Sessions',
    value: formatCount(summary.sessions),
    detail: `${String(summary.activeDays)}/${String(summary.days)} active`,
  },
  {
    label: 'Parallel',
    value: `avg ${summary.avgParallel.toFixed(1)}`,
    detail: `${orDash(summary.fourPlusShare, formatPercent)} at 4+${SEPARATOR}peak ${formatCount(summary.peakParallel)}`,
  },
  {
    label: 'Turns',
    value: formatCount(summary.turns),
    detail: `p50 ${orDash(summary.p50Ms, formatDuration)}${SEPARATOR}p90 ${orDash(summary.p90Ms, formatDuration)}`,
  },
  {
    label: 'Context',
    value: orDash(summary.tokensPerMinute, rate => `${formatTokens(rate)}/min`),
    detail: orEmpty(summary.cachedShare, share => `${formatPercent(share)} cached`),
  },
]

export type { Row }
export {
  activityRows,
  DASH,
  formatCount,
  formatDuration,
  formatHours,
  formatPercent,
  formatTokens,
  formatUsd,
  orDash,
  PERCENT,
  SEPARATOR,
  totalRows,
}
