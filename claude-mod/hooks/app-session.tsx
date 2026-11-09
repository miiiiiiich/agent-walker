import type { ElementTable, RenderChildren, RenderElement } from 'claude-code'

import type { SessionStats } from '../types'
import { SEPARATOR } from './format'
import type { DetailedRow } from './session-format'
import { contextRow, sessionHeading, sessionRows } from './session-format'
import { GRAPH_TITLE, graphAnnotation, graphSvg } from './session-graph'

// What a glance at the session is for, drawn large above the graph; the rest is listed below it.
const HEADLINE = new Set(['Cost', 'Working time', 'Turns'])
const LABEL_WIDTH = 14

type Table = Pick<ElementTable, 'Box' | 'Text'>
type DrawSvg = (source: string) => RenderElement

const rowsOf = (session: SessionStats): { headline: DetailedRow[]; rest: DetailedRow[] } => {
  const rows = sessionRows(session)
  const context = contextRow(session)
  if (context !== undefined) {
    rows.push(context)
  }
  return {
    headline: rows.filter(row => HEADLINE.has(row.label)),
    rest: rows.filter(row => !HEADLINE.has(row.label)),
  }
}

const drawTile = ({ Box, Text }: Table, row: DetailedRow): RenderElement => (
  <Box flexDirection="column">
    <Text size="caption" dimColor={true}>
      {row.label}
    </Text>
    <Text size="title" weight="semibold" tabular={true}>
      {row.value}
    </Text>
    {row.details.length > 0 && (
      <Text size="footnote" dimColor={true}>
        {row.details.join(SEPARATOR)}
      </Text>
    )}
  </Box>
)

// Wraps instead of cutting details: an app's width is not counted in cells.
const drawListRow = ({ Box, Text }: Table, row: DetailedRow): RenderElement => (
  <Box flexDirection="row" flexWrap="wrap" columnGap="md">
    <Box width={LABEL_WIDTH} flexShrink={0}>
      <Text dimColor={true}>{row.label}</Text>
    </Box>
    <Text weight="semibold" tabular={true}>
      {row.value}
    </Text>
    {row.details.length > 0 && <Text dimColor={true}>{row.details.join(SEPARATOR)}</Text>}
  </Box>
)

const drawGraph = ({ Box, Text }: Table, session: SessionStats, drawSvg: DrawSvg): RenderChildren => {
  if (session.lengthsMs.length === 0) {
    return undefined
  }
  return (
    <Box flexDirection="column" gap="xs">
      <Text>
        <Text weight="semibold">{GRAPH_TITLE}</Text>
        <Text dimColor={true}>{`  ${graphAnnotation(session)}`}</Text>
      </Text>
      {drawSvg(graphSvg(session))}
    </Box>
  )
}

const drawAppSession = (table: Table, session: SessionStats, drawSvg: DrawSvg): RenderElement => {
  const { Box, Text } = table
  const { id, span } = sessionHeading(session)
  const { headline, rest } = rowsOf(session)
  return (
    <Box flexDirection="column" gap="lg">
      <Box flexDirection="column">
        <Text size="heading" weight="semibold">{`Session ${id}`}</Text>
        <Text size="footnote" dimColor={true}>
          {span}
        </Text>
      </Box>
      <Box flexDirection="row" flexWrap="wrap" gap="xl">
        {headline.map(row => drawTile(table, row))}
      </Box>
      {drawGraph(table, session, drawSvg)}
      <Box flexDirection="column" gap="xs">
        {rest.map(row => drawListRow(table, row))}
      </Box>
    </Box>
  )
}

export { drawAppSession }
