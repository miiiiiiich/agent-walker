import type { SessionStats } from '../types'
import { formatCount, formatDuration, SEPARATOR } from './format'
import { cells } from './session-format'

const EIGHTHS = ' ▁▂▃▄▅▆▇█'
const EIGHTHS_PER_ROW = 8
const GRAPH_ROWS = 5
const AXIS_WIDTH = 6
const AXIS = ' ┤'
const SVG_WIDTH = 640
const SVG_HEIGHT = 120
const SVG_MAX_COLUMNS = 160
const SHORT_TURN_COLOR = '#84a7ff'
const LONG_TURN_COLOR = '#efc768'

interface GraphRun {
  text: string
  isLong: boolean
}

interface GraphRow {
  label: string
  runs: GraphRun[]
}

interface Graph {
  axisWidth: number
  rows: GraphRow[]
}

// One column per turn; past the width each column shows its longest, so the turn the graph is there to show does not
// vanish between two short ones.
const fitColumns = (lengths: number[], width: number): number[] => {
  if (lengths.length <= width) {
    return lengths
  }
  return Array.from({ length: width }, (_, column) => {
    const start = Math.floor((column * lengths.length) / width)
    const end = Math.max(start + 1, Math.floor(((column + 1) * lengths.length) / width))
    return Math.max(...lengths.slice(start, end))
  })
}

const axisLabel = (row: number, top: number): string => {
  if (row === 0) {
    return formatDuration(top)
  }
  if (row === GRAPH_ROWS - 1) {
    return '0'
  }
  return ''
}

const graphOf = (session: SessionStats, width: number): Graph | undefined => {
  const top = Math.max(...session.lengthsMs, 0)
  if (top <= 0) {
    return undefined
  }
  const axisWidth = Math.max(AXIS_WIDTH, cells(formatDuration(top)))
  const columns = fitColumns(session.lengthsMs, Math.max(1, width - axisWidth - AXIS.length))
  const filled = columns.map(length => Math.max(1, Math.round((length / top) * GRAPH_ROWS * EIGHTHS_PER_ROW)))
  const rows = Array.from({ length: GRAPH_ROWS }, (_, row) => {
    const floor = (GRAPH_ROWS - 1 - row) * EIGHTHS_PER_ROW
    const runs: GraphRun[] = []
    columns.forEach((length, column) => {
      const eighths = Math.min(EIGHTHS_PER_ROW, Math.max(0, (filled[column] ?? 0) - floor))
      const glyph = EIGHTHS.charAt(eighths)
      const isLong = length >= session.longFromMs
      const last = runs.at(-1)
      if (last?.isLong === isLong) {
        last.text += glyph
      } else {
        runs.push({ text: glyph, isLong })
      }
    })
    return { label: axisLabel(row, top), runs }
  })
  return { axisWidth, rows }
}

const turnColor = (isLong: boolean): string => {
  if (isLong) {
    return LONG_TURN_COLOR
  }
  return SHORT_TURN_COLOR
}

const graphSvg = (session: SessionStats): string => {
  const columns = fitColumns(session.lengthsMs, SVG_MAX_COLUMNS)
  const top = Math.max(...columns, 1)
  const step = SVG_WIDTH / Math.max(columns.length, 1)
  const bars = columns
    .map((length, column) => {
      const height = Math.max(1, (length / top) * SVG_HEIGHT)
      const x = (column * step).toFixed(1)
      const y = (SVG_HEIGHT - height).toFixed(1)
      const width = Math.max(1, step - 1).toFixed(1)
      const fill = turnColor(length >= session.longFromMs)
      return `<rect x="${x}" y="${y}" width="${width}" height="${height.toFixed(1)}" fill="${fill}"/>`
    })
    .join('')
  const size = `width="${String(SVG_WIDTH)}" height="${String(SVG_HEIGHT)}"`
  const box = `0 0 ${String(SVG_WIDTH)} ${String(SVG_HEIGHT)}`
  return `<svg xmlns="http://www.w3.org/2000/svg" ${size} viewBox="${box}">${bars}</svg>`
}

const graphAnnotation = (session: SessionStats): string =>
  `${formatCount(session.turns)} turns${SEPARATOR}${formatCount(session.longTurns)} ran ≥20m`

export type { Graph }
export { AXIS, graphAnnotation, graphOf, graphSvg, turnColor }
