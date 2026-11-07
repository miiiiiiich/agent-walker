import type { EngineInterface, Hook, Register, UiFocusOrigin } from 'claude-code'
import { atom, read, update } from 'claude-code'

import type { TabResult } from '../types'
import { fetchSession, fetchSummary } from './cli'
import { drawResult } from './draw'
import { drawHeader, shownHint, tabKey } from './header'

interface Tab {
  id: string
  label: string
}

const COMMAND = 'agent-walker'
const PANE = 'agent-walker'
const TITLE = 'Agent Walker'
// `sdk` is how the desktop app's sessions receive what the person typed.
const PERSON_ORIGINS = ['composer', 'bridge', 'sdk']

const SUMMARY: Tab = { id: 'summary', label: 'Summary' }
const SESSION: Tab = { id: 'session', label: 'Session' }
const TABS: Tab[] = [SUMMARY, SESSION]

const selected = atom({ plugin: 'agent-walker', key: 'selected' } as const, '')
const results = atom({ plugin: 'agent-walker', key: 'results' } as const, {}, { shape: 'results-3' })

// Names the open, and leads each result's key: a load or a clean-up from one open can never touch another's results.
// Random because results outlive a reload, and a counter would start over and match the ones kept.
let generation = ''
// What this open has started. Kept until the next open: a redraw holding an older read would otherwise start it again.
const started = new Set<string>()
// Numbers the ring's moves: an older move can answer after a newer one that landed, and must not take the tab back.
let movesRaised = 0
let lastLanded = 0
// Whether this open has held the keys: until it has, not holding them says nothing.
let hadKeys = false

const closePane = ($: EngineInterface): void => {
  $.ui.close({ id: PANE }).catch((error: unknown) => {
    $.ui.log(`agent-walker: ${String(error)}`, { to: 'debug' })
  })
}

// The pane lives only while it holds the keys. Left open behind a conversation, Esc can't reach it: the prompt, the
// turn or a dialog takes that key first. A render hook may not close it, so the close goes through a timer.
const closeWhenKeysLeave = ($: EngineInterface, isFocused: boolean, drawnIn: string): void => {
  // A draw from before a reopen says nothing about the pane now open.
  if (drawnIn !== generation) {
    return
  }
  if (isFocused) {
    hadKeys = true
    return
  }
  if (hadKeys) {
    hadKeys = false
    $.clock.after(0, () => {
      if (drawnIn === generation) {
        closePane($)
      }
    })
  }
}

const resultKey = (tab: Tab): string => `${generation}:${tab.id}`

const currentResults = (all: Record<string, TabResult>): Record<string, TabResult> =>
  Object.fromEntries(Object.entries(all).filter(([key]) => key.startsWith(`${generation}:`)))

async function fetchFor($: EngineInterface, tab: Tab): Promise<TabResult> {
  if (tab.id !== SESSION.id) {
    return fetchSummary($)
  }
  try {
    return await fetchSession($, await $.session.id())
  } catch (error) {
    return { error: `Could not read this session's id: ${String(error)}` }
  }
}

const fetchAndStore = async ($: EngineInterface, tab: Tab, key: string, startedIn: string): Promise<void> => {
  const result = await fetchFor($, tab)
  await update($, results, all => {
    if (startedIn !== generation) {
      return all
    }
    return { ...currentResults(all), [key]: result }
  })
}

// Only the tab on screen: nobody may ever look at the other. A render hook may not write state, so it loads from a timer.
const loadIfMissing = ($: EngineInterface, tab: Tab, loaded: Record<string, TabResult>): void => {
  const key = resultKey(tab)
  if (loaded[key] === undefined && !started.has(key)) {
    started.add(key)
    const startedIn = generation
    $.clock.after(0, () => {
      fetchAndStore($, tab, key, startedIn).catch((error: unknown) => {
        $.ui.log(`agent-walker: ${String(error)}`, { to: 'debug' })
      })
    })
  }
}

const moveRing = async ($: EngineInterface, key: string): Promise<void> => {
  const moved = await $.ui.focus({ requestId: PANE, key }).catch((error: unknown) => ({ deny: String(error) }))
  if ('deny' in moved) {
    $.ui.log(`agent-walker: the focus stayed: ${String(moved.deny)}`, { to: 'debug' })
  }
}

const openPane = async ($: EngineInterface, wanted: string): Promise<{ text?: string }> => {
  // Each open reads the logs afresh: a pane left open is a look back, not a live meter.
  generation = crypto.randomUUID()
  hadKeys = false
  started.clear()
  await update($, selected, () => wanted)
  await update($, results, currentResults)
  const opened = await $.ui.open({ id: PANE, title: TITLE, focus: true, closeOnEscape: true })
  if (!opened.isPlaced) {
    return { text: `Could not open the pane: ${opened.reason}` }
  }
  await moveRing($, tabKey(wanted))
  return {}
}

const selectTab = async ($: EngineInterface, id: string): Promise<void> => {
  await update($, selected, () => id)
  await moveRing($, tabKey(id))
}

const selectTabSoon = ($: EngineInterface, id: string, move: number): void => {
  $.clock.after(0, () => {
    // A newer move made since was the person's latest word.
    if (move === movesRaised) {
      selectTab($, id).catch((error: unknown) => $.ui.log(`agent-walker: ${String(error)}`, { to: 'debug' }))
    }
  })
}

// Tab would otherwise stop on the pane's close mark, between the last tab and the first, where nothing changes.
const tabPastCloseMark = (current: string, origin: UiFocusOrigin): string | undefined => {
  const last = TABS.at(-1)
  if (origin.kind !== 'person' || last === undefined || last === SUMMARY) {
    return undefined
  }
  if (SUMMARY.id === current) {
    return last.id
  }
  return SUMMARY.id
}

const followMove = async ($: EngineInterface, element: string | undefined, move: number): Promise<void> => {
  const entry = TABS.find(tab => tabKey(tab.id) === element)
  if (entry !== undefined) {
    await update($, selected, current => {
      if (move < lastLanded) {
        return current
      }
      return entry.id
    })
  }
}

// For a pane that never held the keys: the focus refused at open, or a surface that does not report it.
const closeForPrompt = async ($: EngineInterface, origin: string, command: string | undefined): Promise<void> => {
  if (PERSON_ORIGINS.includes(origin) && command !== COMMAND) {
    await $.ui.close({ id: PANE })
  }
}

const registerCommand = async ($: EngineInterface): Promise<void> => {
  await $.command.register({
    name: COMMAND,
    description: "Show agent-walker's last 30 days in a pane",
    immediate: true,
  })
}

const moveTab: Hook<'ui.focus'> = async ($, e, next) => {
  movesRaised += 1
  const move = movesRaised
  const current = await read($, selected)
  const past = tabPastCloseMark(current, e.origin)
  // `next` may not turn a move onto the close mark into another: answering keeps the ring, and the timer moves it on.
  if (e.element === undefined && past !== undefined) {
    selectTabSoon($, past, move)
    return {}
  }
  const result = await next(e)
  if ('deny' in result || move < lastLanded) {
    return result
  }
  lastLanded = move
  await followMove($, e.element, move)
  return result
}

export const register: Register = on => {
  // Not only for an interactive session: the desktop app hosts its sessions as headless ones.
  on('session.start', async ($, e, next) => {
    await registerCommand($)
    return next(e)
  })

  on('command.run', { command: COMMAND }, $ => openPane($, SUMMARY.id))

  on('prompt.submit', async ($, e, next) => {
    await closeForPrompt($, e.origin.kind, e.command?.name)
    return next(e)
  })

  on('ui.focus', { requestId: PANE }, moveTab)

  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    const drawnIn = generation
    const [stored, loaded] = await Promise.all([read($, selected), read($, results)])
    const tab = TABS.find(entry => entry.id === stored) ?? SUMMARY
    loadIfMissing($, tab, loaded)
    if (e.surface === 'terminal') {
      closeWhenKeysLeave($, e.props.isFocused, drawnIn)
    }

    const table = $.ui.resolve(e)
    const { Box } = table
    const result = loaded[resultKey(tab)]
    let body = drawResult(table, result, e.props.bodyColumns)
    if (e.surface !== 'terminal' && result !== undefined) {
      const { Svg } = $.ui.resolve(e)
      if ('svg' in result && result.svg !== undefined) {
        body = <Svg source={result.svg} alt={tab.label} />
      } else if ('session' in result) {
        body = drawResult(table, result, e.props.bodyColumns, source => <Svg source={source} alt="Turn length" />)
      }
    }

    return (
      <Box flexDirection="column" gap={1}>
        {drawHeader(table, {
          tabs: TABS,
          current: tab.id,
          hint: shownHint(e.surface, e.props.isFocused, e.props.bodyColumns),
          onSelect: id => selectTab($, id),
        })}
        <Box flexDirection="column">{body}</Box>
      </Box>
    )
  })
}
