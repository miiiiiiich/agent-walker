import type { ElementTable, RenderElement } from 'claude-code'

const TAB_KEY = 'tab:'
const MAX_HOTKEYS = 9
const HINT = 'Tab to switch · Esc to close'
const SHORT_HINT = 'Esc to close'
// The tab row and the full hint side by side; narrower, the hint would wrap and push the body down.
const FULL_HINT_FROM_COLUMNS = 56

interface TabLabel {
  id: string
  label: string
}

interface Header {
  tabs: TabLabel[]
  current: string
  hint: string | undefined
  onSelect: (id: string) => Promise<void>
}

type Table = Pick<ElementTable, 'Box' | 'Button' | 'Text'>

const tabKey = (id: string): string => `${TAB_KEY}${id}`

const drawTabs = (
  { Box, Button, Text }: Table,
  tabs: TabLabel[],
  current: string,
  onSelect: (id: string) => Promise<void>,
): RenderElement => (
  <Box flexDirection="row" gap={2}>
    {tabs.map((tab, index) => {
      const isActive = tab.id === current
      return (
        <Button
          key={tabKey(tab.id)}
          plain={true}
          {...(index < MAX_HOTKEYS && { hotkey: String(index + 1) })}
          {...(isActive && { autoFocus: true })}
          onPress={(): Promise<void> => onSelect(tab.id)}
        >
          <Text bold={isActive} dimColor={!isActive}>
            {tab.label}
          </Text>
        </Button>
      )
    })}
  </Box>
)

const hintFor = (columns: number): string => {
  if (columns >= FULL_HINT_FROM_COLUMNS) {
    return HINT
  }
  return SHORT_HINT
}

const shownHint = (surface: string, isFocused: boolean, columns: number): string | undefined => {
  if (surface === 'terminal' && isFocused) {
    return hintFor(columns)
  }
  return undefined
}

const drawHeader = (table: Table, { tabs, current, hint, onSelect }: Header): RenderElement => {
  const { Box, Text } = table
  return (
    <Box flexDirection="row" justifyContent="space-between" gap={2}>
      {drawTabs(table, tabs, current, onSelect)}
      {hint !== undefined && <Text dimColor={true}>{hint}</Text>}
    </Box>
  )
}

export { drawHeader, shownHint, tabKey }
