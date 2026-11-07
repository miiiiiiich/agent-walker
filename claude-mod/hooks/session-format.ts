import type { SessionStats } from '../types'
import {
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
} from './format'

const TOP_ITEMS = 3
const SHORT_ID_LENGTH = 8
const WIDE_CELL_WIDTH = 2
const WIDE =
  /[\u1100-\u115F\u2E80-\uA4CF\uAC00-\uD7A3\uF900-\uFAFF\uFE30-\uFE6F\uFF00-\uFF60\uFFE0-\uFFE6\u{1F300}-\u{1FAFF}\u{20000}-\u{3FFFD}]/u
const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec']
const ISO_MOMENT = /^\d{4}-(?<month>\d{2})-(?<day>\d{2})T(?<hour>\d{2}):(?<minute>\d{2})/u

interface DetailedRow {
  label: string
  value: string
  details: string[]
}

const cellWidth = (character: string): number => {
  if (WIDE.test(character)) {
    return WIDE_CELL_WIDTH
  }
  return 1
}

// Terminal cells, not characters: a name in Japanese takes two to each.
const cells = (text: string): number => [...text].reduce((sum, character) => sum + cellWidth(character), 0)

// Reads the local clock the CLI wrote into the timestamp, not this process's zone.
const formatMoment = (iso: string): string => {
  const parts = ISO_MOMENT.exec(iso)?.groups
  if (parts === undefined) {
    return iso
  }
  const { month = '', day = '', hour = '', minute = '' } = parts
  return `${MONTHS[Number(month) - 1] ?? month} ${String(Number(day))} ${hour}:${minute}`
}

const sessionHeading = (session: SessionStats): { id: string; span: string } => ({
  id: session.id.slice(0, SHORT_ID_LENGTH),
  span: `${formatMoment(session.firstAt)} – ${formatMoment(session.lastAt)}`,
})

const totalCalls = (items: { calls: number }[]): string =>
  `${formatCount(items.reduce((sum, item) => sum + item.calls, 0))} calls`

// When only part of the time was measured, the shares describe that part, as the TUI says: a model-only hour must not
// read as all of a ten-hour session.
const madeOf = ({ modelShare, measuredMs, workingMs }: SessionStats): string[] => {
  if (modelShare === undefined) {
    return []
  }
  if (measuredMs !== undefined && workingMs !== undefined && measuredMs < workingMs) {
    return [`model ${formatPercent(modelShare)} of ${formatHours(measuredMs)}`]
  }
  return [`model ${formatPercent(modelShare)}`, `tools ${formatPercent(1 - modelShare)}`]
}

const unpricedDetails = (unpricedTokens: number | undefined): string[] => {
  if (unpricedTokens === undefined || unpricedTokens <= 0) {
    return []
  }
  return [`${formatTokens(unpricedTokens)} tokens unpriced`]
}

const turnsRow = (session: SessionStats): DetailedRow => {
  if (session.turns <= 0) {
    return { label: 'Turns', value: DASH, details: [] }
  }
  return {
    label: 'Turns',
    value: formatCount(session.turns),
    details: [
      `p50 ${orDash(session.p50Ms, formatDuration)}`,
      `p90 ${orDash(session.p90Ms, formatDuration)}`,
      `max ${orDash(session.maxMs, formatDuration)}`,
    ],
  }
}

const timeRows = (session: SessionStats): DetailedRow[] => {
  const { costUsd, unpricedTokens, workingMs, onYouMs, paceP50Ms, paceP90Ms, paceAvgMs } = session
  const rows: DetailedRow[] = [
    { label: 'Cost', value: orDash(costUsd, formatUsd), details: unpricedDetails(unpricedTokens) },
    { label: 'Working time', value: orDash(workingMs, formatHours), details: madeOf(session) },
  ]
  if (onYouMs !== undefined && onYouMs > 0) {
    rows.push({ label: 'On you', value: formatHours(onYouMs), details: ['answering questions'] })
  }
  rows.push(turnsRow(session))
  if (paceP50Ms !== undefined) {
    rows.push({
      label: 'Your pace',
      value: `p50 ${formatDuration(paceP50Ms)}`,
      details: [`p90 ${orDash(paceP90Ms, formatDuration)}`, `avg ${orDash(paceAvgMs, formatDuration)}`],
    })
  }
  return rows
}

const nameWithCalls = ({ name, calls }: { name: string; calls: number }): string => {
  if (calls > 0) {
    return `${name} ${formatCount(calls)}`
  }
  return name
}

// Codex attributes usage to a subagent without any call to count: a zero there would say none ran.
const subagentsRow = (session: SessionStats): DetailedRow => {
  const { subagents } = session
  let value = formatCount(subagents.length)
  if (subagents.some(agent => agent.calls > 0)) {
    value = totalCalls(subagents)
  }
  return { label: 'Subagents', value, details: subagents.slice(0, TOP_ITEMS).map(nameWithCalls) }
}

const usedRows = (session: SessionStats): DetailedRow[] => {
  const rows: DetailedRow[] = [
    {
      label: 'Tools',
      value: totalCalls(session.tools),
      details: session.tools.slice(0, TOP_ITEMS).map(tool => `${tool.name} ${formatCount(tool.calls)}`),
    },
  ]
  if (session.subagents.length > 0) {
    rows.push(subagentsRow(session))
  }
  if (session.skills.length > 0) {
    rows.push({
      label: 'Skills',
      value: formatCount(session.skills.length),
      details: session.skills.slice(0, TOP_ITEMS),
    })
  }
  rows.push({
    label: 'Models',
    value: formatCount(session.models.length),
    details: session.models.slice(0, TOP_ITEMS).map(model => `${model.label} ${(model.share * PERCENT).toFixed(1)}%`),
  })
  return rows
}

const contextRow = ({ cachedShare, expiredCalls, coldStartCalls }: SessionStats): DetailedRow | undefined => {
  if (cachedShare === undefined) {
    return undefined
  }
  const details: string[] = []
  if (expiredCalls !== undefined && expiredCalls > 0) {
    details.push(`expired ${formatCount(expiredCalls)}`)
  }
  if (coldStartCalls !== undefined && coldStartCalls > 0) {
    details.push(`cold start ${formatCount(coldStartCalls)}`)
  }
  return { label: 'Context', value: `${formatPercent(cachedShare)} cached`, details }
}

const sessionRows = (session: SessionStats): DetailedRow[] => [...timeRows(session), ...usedRows(session)]

// As many details as fit, whole: a row that wraps breaks both columns.
const fitDetails = (details: string[], room: number): string => {
  let text = ''
  for (const detail of details) {
    let next = detail
    if (text !== '') {
      next = `${text}${SEPARATOR}${detail}`
    }
    if (cells(next) > room) {
      break
    }
    text = next
  }
  return text
}

export type { DetailedRow }
export { cells, contextRow, fitDetails, sessionHeading, sessionRows }
