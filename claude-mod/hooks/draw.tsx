import type { ElementTable, RenderChildren, RenderElement } from 'claude-code'

import type { Codename, SessionStats, Summary, TabResult } from '../types'
import type { Row } from './format'
import { activityRows, totalRows } from './format'
import type { DetailedRow } from './session-format'
import { cells, contextRow, fitDetails, sessionHeading, sessionRows } from './session-format'
import { AXIS, graphAnnotation, graphOf, graphSvg, turnColor } from './session-graph'

const LOADING = 'Loading…'
const BLANK = ' '
const RULE = '────'
const LABEL_WIDTH = 14
const VALUE_WIDTH = 12
const COLUMN_GAP = 4
const SESSION_TABLE_WIDTH = 52
const SESSION_GRAPH_WIDTH = 38
const SESSION_GAP = 2
const GRAPH_TITLE = 'TURN LENGTH'

type Table = Pick<ElementTable, 'Box' | 'Text'>
type DrawSvg = (source: string) => RenderElement

const textWidth = (text: string): number => [...text].length

const rankLine = (rank: string): string => `RANK ${rank}`

const badgeWidth = (codename: Codename): number =>
  Math.max(
    textWidth(`${codename.ops} ${codename.animal}`),
    textWidth(`${RULE}  ${rankLine(codename.rank)}  ${RULE}`),
    ...codename.braille.map(textWidth),
  )

const rowWidth = (row: Row): number =>
  LABEL_WIDTH + Math.max(VALUE_WIDTH, textWidth(row.value) + 1) + textWidth(row.detail)

const drawBadge = ({ Box, Text }: Table, codename: Codename): RenderElement => (
  <Box flexDirection="column" flexShrink={0}>
    <Text bold={true}>
      <Text color={codename.opsColor}>{codename.ops}</Text>
      {BLANK}
      {codename.animal}
    </Text>
    {codename.braille.map(row => (
      <Text color={codename.opsColor}>{row}</Text>
    ))}
    {codename.rankColor !== undefined && (
      <Text>
        <Text dimColor={true}>{`${RULE}  `}</Text>
        <Text bold={true} color={codename.rankColor}>
          {rankLine(codename.rank)}
        </Text>
        <Text dimColor={true}>{`  ${RULE}`}</Text>
      </Text>
    )}
  </Box>
)

const drawRow = ({ Box, Text }: Table, row: Row, isStacked: boolean): RenderElement => {
  const main = (
    <Box flexDirection="row">
      <Box width={LABEL_WIDTH} flexShrink={0}>
        <Text dimColor={true}>{row.label}</Text>
      </Box>
      <Box minWidth={VALUE_WIDTH} flexShrink={0} paddingRight={1}>
        <Text bold={true}>{row.value}</Text>
      </Box>
      {!isStacked && <Text dimColor={true}>{row.detail}</Text>}
    </Box>
  )
  if (!isStacked || row.detail === '') {
    return main
  }
  return (
    <Box flexDirection="column">
      {main}
      <Box paddingLeft={2}>
        <Text dimColor={true}>{row.detail}</Text>
      </Box>
    </Box>
  )
}

const drawSummary = (table: Table, summary: Summary, columns: number): RenderElement => {
  const { Box, Text } = table
  const totals = totalRows(summary)
  const activity = activityRows(summary)
  const rowsWidth = Math.max(...[...totals, ...activity].map(rowWidth))
  const badgeColumn = badgeWidth(summary.codename) + COLUMN_GAP
  const isBeside = columns >= badgeColumn + rowsWidth
  const isStacked = !isBeside && columns < rowsWidth
  const numbers = (
    <Box flexDirection="column" gap={1}>
      <Box flexDirection="column">{totals.map(row => drawRow(table, row, isStacked))}</Box>
      <Box flexDirection="column">{activity.map(row => drawRow(table, row, isStacked))}</Box>
      <Text dimColor={true}>{`${summary.start} – ${summary.end}`}</Text>
    </Box>
  )
  if (isBeside) {
    return (
      <Box flexDirection="row" gap={COLUMN_GAP}>
        {drawBadge(table, summary.codename)}
        {numbers}
      </Box>
    )
  }
  return (
    <Box flexDirection="column" gap={1}>
      {drawBadge(table, summary.codename)}
      {numbers}
    </Box>
  )
}

const drawDetailedRow = (table: Table, row: DetailedRow, width: number): RenderElement => {
  const room = width - LABEL_WIDTH - Math.max(VALUE_WIDTH, cells(row.value) + 1)
  return drawRow(table, { ...row, detail: fitDetails(row.details, room) }, false)
}

const drawSessionTable = (table: Table, session: SessionStats, width: number, extra: DetailedRow[]): RenderElement => {
  const { Box, Text } = table
  const { id, span } = sessionHeading(session)
  return (
    <Box flexDirection="column">
      <Text>
        <Text bold={true}>{`Session ${id}`}</Text>
        <Text dimColor={true}>{`  ${span}`}</Text>
      </Text>
      {[...sessionRows(session), ...extra].map(row => drawDetailedRow(table, row, width))}
    </Box>
  )
}

const drawGraphTitle = ({ Text }: Table, session: SessionStats): RenderElement => (
  <Text>
    <Text bold={true}>{GRAPH_TITLE}</Text>
    <Text dimColor={true}>{`  ${graphAnnotation(session)}`}</Text>
  </Text>
)

const drawSessionGraph = (table: Table, session: SessionStats, width: number): RenderChildren => {
  const { Box, Text } = table
  const graph = graphOf(session, width)
  if (graph === undefined) {
    return undefined
  }
  return (
    <Box flexDirection="column">
      {drawGraphTitle(table, session)}
      {graph.rows.map(row => (
        <Text>
          <Text dimColor={true}>{`${row.label.padStart(graph.axisWidth)}${AXIS}`}</Text>
          {row.runs.map(run => (
            <Text color={turnColor(run.isLong)}>{run.text}</Text>
          ))}
        </Text>
      ))}
    </Box>
  )
}

const drawSession = (
  table: Table,
  session: SessionStats,
  columns: number,
  drawSvg: DrawSvg | undefined,
): RenderElement => {
  const { Box } = table
  const context = contextRow(session)
  const contextRows: DetailedRow[] = []
  if (context !== undefined) {
    contextRows.push(context)
  }
  if (drawSvg !== undefined) {
    return (
      <Box flexDirection="column" gap={1}>
        {drawSessionTable(table, session, columns, contextRows)}
        {session.lengthsMs.length > 0 && drawGraphTitle(table, session)}
        {session.lengthsMs.length > 0 && drawSvg(graphSvg(session))}
      </Box>
    )
  }
  if (columns >= SESSION_TABLE_WIDTH + SESSION_GAP + SESSION_GRAPH_WIDTH) {
    return (
      <Box flexDirection="row" gap={SESSION_GAP}>
        <Box width={SESSION_TABLE_WIDTH} flexShrink={0}>
          {drawSessionTable(table, session, SESSION_TABLE_WIDTH, [])}
        </Box>
        {/* Under the graph, where the left column is still running: below the table it would lengthen the pane. */}
        <Box flexDirection="column" gap={1}>
          {drawSessionGraph(table, session, SESSION_GRAPH_WIDTH)}
          {contextRows.map(row => drawDetailedRow(table, row, SESSION_GRAPH_WIDTH))}
        </Box>
      </Box>
    )
  }
  return (
    <Box flexDirection="column" gap={1}>
      {drawSessionTable(table, session, columns, contextRows)}
      {drawSessionGraph(table, session, columns)}
    </Box>
  )
}

const drawResult = (
  table: Table,
  result: TabResult | undefined,
  columns: number,
  drawSvg?: DrawSvg,
): RenderChildren => {
  const { Text } = table
  if (result === undefined) {
    return <Text dimColor={true}>{LOADING}</Text>
  }
  if ('error' in result) {
    return <Text color="error">{result.error}</Text>
  }
  if ('session' in result) {
    return drawSession(table, result.session, columns, drawSvg)
  }
  return drawSummary(table, result.summary, columns)
}

export { drawResult }
