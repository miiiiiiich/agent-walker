import type { Codename, ModelShare, NameCalls, SessionStats, Summary } from '../types'

// The oldest shape this mod can read. The fields it needs are checked one by one, so a newer one is read as long as
// they are there.
const MIN_SCHEMA_VERSION = 3
// What `Svg` takes; past it the engine refuses the whole tree, tabs included.
const MAX_SVG_CHARS = 131_072

// A control character in any text costs the whole pane: the engine refuses the tree it is in.
const HAS_CONTROL = /[^\P{Cc}\n\t]/u
// Names come from logs a cloned repo can write to: nothing in them may reorder what is read or hide text.
const UNSEEN = /[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/gu

const record = (value: unknown): Record<string, unknown> => {
  if (typeof value === 'object' && value !== null) {
    return { ...value }
  }
  return {}
}

const numberOr = (value: unknown): number | undefined => {
  if (typeof value === 'number' && Number.isFinite(value)) {
    return value
  }
  return undefined
}

const isPlainText = (value: unknown): value is string => typeof value === 'string' && !HAS_CONTROL.test(value)

const isSupported = (value: unknown): boolean => (numberOr(record(value).schema_version) ?? 0) >= MIN_SCHEMA_VERSION

const parseJson = (text: string): unknown => {
  try {
    return JSON.parse(text)
  } catch {
    return undefined
  }
}

const parseCodename = (value: unknown): Codename | undefined => {
  const { ops, animal, rank, ops_color: opsColor, rank_color: rankColor, braille } = record(value)
  const hasNames = isPlainText(ops) && isPlainText(animal) && isPlainText(rank)
  const hasColor = isPlainText(opsColor) && opsColor !== ''
  if (!(hasNames && hasColor && Array.isArray(braille) && braille.every(isPlainText))) {
    return undefined
  }
  return {
    ops,
    animal,
    rank,
    opsColor,
    braille,
    ...(isPlainText(rankColor) && rankColor !== '' && { rankColor }),
  }
}

const parseOptionalNumbers = (total: Record<string, unknown>): Partial<Summary> => {
  const working = record(total.working_time)
  const costUsd = numberOr(record(total.cost).estimated_usd)
  const workingMs = numberOr(working.active_ms)
  const tokensPerMinute = numberOr(working.tokens_per_minute)
  const cachedShare = numberOr(record(total.context).cached_share)
  const fourPlusShare = numberOr(record(total.parallel).four_plus_share)
  const p50Ms = numberOr(record(total.turn_length).p50_ms)
  const p90Ms = numberOr(record(total.turn_length).p90_ms)
  return {
    ...(costUsd !== undefined && { costUsd }),
    ...(workingMs !== undefined && { workingMs }),
    ...(tokensPerMinute !== undefined && { tokensPerMinute }),
    ...(cachedShare !== undefined && { cachedShare }),
    ...(fourPlusShare !== undefined && { fourPlusShare }),
    ...(p50Ms !== undefined && { p50Ms }),
    ...(p90Ms !== undefined && { p90Ms }),
  }
}

const parseSummary = (value: unknown): Summary | undefined => {
  if (!isSupported(value)) {
    return undefined
  }
  const { window, total: rawTotal } = record(value)
  const { days, start, end } = record(window)
  const total = record(rawTotal)
  const parallel = record(total.parallel)
  const turnLength = record(total.turn_length)
  const required = {
    days: numberOr(days),
    tokens: numberOr(record(total.tokens).total),
    sessions: numberOr(total.sessions),
    activeDays: numberOr(total.active_days),
    avgParallel: numberOr(parallel.avg_concurrency),
    peakParallel: numberOr(parallel.peak_concurrency),
    turns: numberOr(turnLength.turns),
  }
  const codename = parseCodename(total.codename)
  const { days: windowDays, tokens, sessions, activeDays, avgParallel, peakParallel, turns } = required
  if (
    codename === undefined ||
    !(isPlainText(start) && isPlainText(end)) ||
    windowDays === undefined ||
    tokens === undefined ||
    sessions === undefined ||
    activeDays === undefined ||
    avgParallel === undefined ||
    peakParallel === undefined ||
    turns === undefined
  ) {
    return undefined
  }
  return {
    codename,
    days: windowDays,
    start,
    end,
    tokens,
    sessions,
    activeDays,
    avgParallel,
    peakParallel,
    turns,
    ...parseOptionalNumbers(total),
  }
}

// An unusable card is no reason to lose the numbers: a surface that cannot draw it draws those instead.
const parseCard = (value: unknown): string | undefined => {
  const { card_svg: svg } = record(value)
  if (typeof svg === 'string' && svg.startsWith('<svg') && svg.length <= MAX_SVG_CHARS) {
    return svg
  }
  return undefined
}

const cleanName = (value: unknown): string | undefined => {
  if (typeof value !== 'string') {
    return undefined
  }
  return value.replace(UNSEEN, '') || undefined
}

const list = (value: unknown): unknown[] => {
  if (Array.isArray(value)) {
    return value
  }
  return []
}

const parseNameCalls = (value: unknown): NameCalls[] =>
  list(value).flatMap(item => {
    const { name: rawName, calls: rawCalls } = record(item)
    const name = cleanName(rawName)
    const calls = numberOr(rawCalls)
    if (name === undefined || calls === undefined) {
      return []
    }
    return [{ name, calls }]
  })

const parseModels = (value: unknown): ModelShare[] =>
  list(value).flatMap(item => {
    const { label: rawLabel, share: rawShare } = record(item)
    const label = cleanName(rawLabel)
    const share = numberOr(rawShare)
    if (label === undefined || share === undefined) {
      return []
    }
    return [{ label, share }]
  })

const parseSkills = (value: unknown): string[] =>
  list(value).flatMap(item => {
    const name = cleanName(record(item).name)
    if (name === undefined) {
      return []
    }
    return [name]
  })

const parseSessionTimes = (total: Record<string, unknown>): Partial<SessionStats> => {
  const working = record(total.working_time)
  const pace = record(working.pace)
  const length = record(total.turn_length)
  const costUsd = numberOr(record(total.cost).estimated_usd)
  const unpricedTokens = numberOr(record(total.cost).unpriced_tokens)
  const workingMs = numberOr(working.active_ms)
  const onYouMs = numberOr(working.human_wait_ms)
  const modelShare = numberOr(working.model_share)
  const measuredMs = numberOr(working.measured_ms)
  const paceP50Ms = numberOr(pace.p50_ms)
  const paceP90Ms = numberOr(pace.p90_ms)
  const paceAvgMs = numberOr(pace.mean_ms)
  const p50Ms = numberOr(length.p50_ms)
  const p90Ms = numberOr(length.p90_ms)
  const maxMs = numberOr(length.max_ms)
  return {
    ...(costUsd !== undefined && { costUsd }),
    ...(unpricedTokens !== undefined && { unpricedTokens }),
    ...(workingMs !== undefined && { workingMs }),
    ...(onYouMs !== undefined && { onYouMs }),
    ...(modelShare !== undefined && { modelShare }),
    ...(measuredMs !== undefined && { measuredMs }),
    ...(paceP50Ms !== undefined && { paceP50Ms }),
    ...(paceP90Ms !== undefined && { paceP90Ms }),
    ...(paceAvgMs !== undefined && { paceAvgMs }),
    ...(p50Ms !== undefined && { p50Ms }),
    ...(p90Ms !== undefined && { p90Ms }),
    ...(maxMs !== undefined && { maxMs }),
  }
}

const parseSessionContext = (total: Record<string, unknown>): Partial<SessionStats> => {
  const context = record(total.context)
  const cachedShare = numberOr(context.cached_share)
  const expiredCalls = numberOr(record(context.expired).calls)
  const coldStartCalls = numberOr(record(context.cold_start).calls)
  return {
    ...(cachedShare !== undefined && { cachedShare }),
    ...(expiredCalls !== undefined && { expiredCalls }),
    ...(coldStartCalls !== undefined && { coldStartCalls }),
  }
}

const parseSession = (value: unknown): SessionStats | undefined => {
  if (!isSupported(value)) {
    return undefined
  }
  const { session, total: rawTotal } = record(value)
  const { id, first_at: firstAt, last_at: lastAt } = record(session)
  const total = record(rawTotal)
  const length = record(total.turn_length)
  const turns = numberOr(length.turns)
  const longFromMs = numberOr(length.long_from_ms)
  const longTurns = numberOr(length.long_turns)
  if (
    !(isPlainText(id) && isPlainText(firstAt) && isPlainText(lastAt)) ||
    turns === undefined ||
    longFromMs === undefined ||
    longTurns === undefined
  ) {
    return undefined
  }
  return {
    id,
    firstAt,
    lastAt,
    turns,
    longFromMs,
    longTurns,
    lengthsMs: list(length.lengths_ms).flatMap(item => numberOr(item) ?? []),
    tools: parseNameCalls(total.tools),
    subagents: parseNameCalls(total.subagents),
    skills: parseSkills(total.skills),
    models: parseModels(total.models),
    ...parseSessionTimes(total),
    ...parseSessionContext(total),
  }
}

const RELEASE = /^\d+\.\d+\.\d+$/u

const versionOf = (manifest: unknown): string | undefined => {
  const { version } = record(manifest)
  if (typeof version === 'string' && RELEASE.test(version)) {
    return version
  }
  return undefined
}

export { parseCard, parseJson, parseSession, parseSummary, versionOf }
