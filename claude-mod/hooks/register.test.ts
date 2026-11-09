import type { On } from 'claude-code'
import { expect, mock, test } from 'claude-code/testing'

import { activityRows, formatDuration, formatHours, formatTokens, formatUsd, totalRows } from './format'
import { fitDetails } from './session-format'

type TestEngine = Parameters<Extract<Parameters<typeof test>[1], (...args: never[]) => unknown>>[0]

interface Answer {
  exitCode?: number
  stdout?: string
  stderr?: string
}

interface Setup {
  manifest?: string
  noSessionId?: boolean
  answer?: Answer
  env?: Record<string, string>
  run?: () => Promise<string> | undefined
}

const SUMMARY_DOC = {
  schema_version: 3,
  window: { days: 30, start: '2026-10-05', end: '2026-11-03' },
  total: {
    codename: {
      rank: 'A',
      ops: 'Eclipse',
      animal: 'Gull',
      ops_color: '#ba94ff',
      rank_color: '#6b9bd8',
      braille: ['⠀⠈⠓⢶⣶'],
    },
    tokens: { total: 9_142_911_082 },
    cost: { estimated_usd: 4092.36, unpriced_tokens: 0 },
    sessions: 330,
    active_days: 29,
    turn_length: { turns: 4093, p50_ms: 42_424, p90_ms: 357_379 },
    working_time: { active_ms: 602_763_155, tokens_per_minute: 906_279 },
    context: { cached_share: 0.973 },
    parallel: { avg_concurrency: 3.66, peak_concurrency: 13, four_plus_share: 0.48 },
  },
  card_svg: '<svg id="card"/>',
}

const MANIFEST_PATH = /\/\.claude-plugin\/plugin\.json$/u

const SESSION_DOC = {
  schema_version: 3,
  session: {
    id: '232f4aab-9dfd-4a3c-bbfb-3900229fcc57',
    first_at: '2026-11-03T17:42:06.898+09:00',
    last_at: '2026-11-06T00:53:11.978+09:00',
  },
  total: {
    cost: { estimated_usd: 106.31 },
    working_time: {
      active_ms: 18_538_917,
      human_wait_ms: 0,
      model_share: 0.329,
      pace: { p50_ms: 142_850, p90_ms: 750_462, mean_ms: 324_874 },
    },
    turn_length: {
      turns: 3,
      p50_ms: 129_309,
      p90_ms: 818_447,
      max_ms: 3_956_474,
      long_from_ms: 1_200_000,
      long_turns: 1,
      lengths_ms: [60_000, 3_956_474, 120_000],
    },
    tools: [
      { name: 'Bash', calls: 336 },
      { name: 'We\u202ebFetch\u001b[31m', calls: 140 },
    ],
    subagents: [{ name: 'code-evaluator\u001b]0;x', calls: 2 }],
    skills: [{ name: 'ref:ntn\u2028' }],
    context: { cached_share: 0.947, expired: { calls: 10 }, cold_start: { calls: 1 } },
    models: [{ label: 'Opus\u009b 5.5', share: 1 }],
  },
}

const paneAt = (bodyColumns: number, isFocused = true) =>
  ({
    plugin: 'agent-walker',
    component: 'Pane',
    requestId: 'agent-walker',
    props: {
      title: 'Agent Walker',
      isFocused,
      bodyColumns,
      placement: 'dock',
      scroll: { offset: 0, bodyRows: 30 },
      view: {},
    },
  }) as const

const open = ($: TestEngine, command = 'agent-walker') =>
  $.command.run({
    command,
    args: '',
    origin: { kind: 'composer' },
    presentation: { isFullscreen: true, columns: 140 },
  })

const openSession = async (
  $: TestEngine,
  clock: { advance: (ms: number) => Promise<void> },
  bodyColumns: number,
  surface: 'terminal' | 'desktop' | 'vscode' | 'mobile' = 'terminal',
) => {
  await open($)
  const ui = await $.ui.mount({ ...paneAt(bodyColumns), surface })
  await clock.advance(0)
  await ui.press({ key: 'tab:session' })
  await clock.advance(0)
  return ui
}

const setup = (on: On, options: Setup = {}) => {
  const reads: string[] = []
  const argvs: string[][] = []
  const opens: unknown[] = []
  const closes: unknown[] = []
  const cwds: unknown[] = []
  const clock = mock.clock(on)
  mock.env(on, options.env ?? { HOME: '/home/me' })
  on('ui.open', (_, e) => {
    opens.push(e)
    return { value: { isPlaced: true } }
  })
  on('ui.close', (_, e) => {
    closes.push(e)
    return { value: undefined } as never
  })
  on('prompt.submit', (_, e) => ({ text: e.text }))
  on('ui.focus', () => ({}))
  on('ui.log', () => ({ value: undefined }) as never)
  on('session.id', () => {
    if (options.noSessionId === true) {
      throw new Error('no session')
    }
    return { value: '232f4aab-9dfd-4a3c-bbfb-3900229fcc57' }
  })
  on('fs.read', (_, e) => {
    reads.push(e.path)
    return { value: options.manifest ?? JSON.stringify({ name: 'agent-walker', version: '0.21.2' }) }
  })
  on('process.run', async (_, e) => {
    argvs.push([...e.argv])
    cwds.push(e.init?.cwd)
    const custom = await options.run?.()
    return {
      value: {
        exitCode: options.answer?.exitCode ?? 0,
        stdout:
          custom ?? options.answer?.stdout ?? JSON.stringify(e.argv.includes('session') ? SESSION_DOC : SUMMARY_DOC),
        stderr: options.answer?.stderr ?? '',
        isStdoutTruncated: false,
        isStderrTruncated: false,
      },
    }
  })
  return { argvs, opens, closes, cwds, clock, reads }
}

const hasText = async (ui: { findAll: (query: { type: 'Text' }) => Promise<{ text?: string }[]> }, words: string) =>
  (await ui.findAll({ type: 'Text' })).some(text => text.text?.includes(words))

test('opening asks for a pane that takes the keys and closes on Esc', async ($, on) => {
  const { opens } = setup(on)
  const result = await open($)
  expect(result.text).toBeUndefined()
  expect(opens[0]).toMatchObject({ id: 'agent-walker', focus: true, closeOnEscape: true })
})

test('the terminal asks for the summary, from the home directory, and draws it', async ($, on) => {
  const { argvs, cwds, clock } = setup(on)
  await open($)
  const ui = await $.ui.mount({ ...paneAt(100), surface: 'terminal' })
  expect(await hasText(ui, 'Loading')).toBe(true)

  await clock.advance(0)
  expect(argvs).toEqual([['npx', '-y', 'agent-walker@0.21.2', 'summary', '--json']])
  expect(cwds[0]).toBe('/home/me')
  const shown = (await ui.findAll({ type: 'Text' })).map(text => text.text).join('\n')
  for (const words of ['Eclipse', 'Gull', 'RANK A', 'Tokens', '9.1B', '$4,092', '2026-10-05 – 2026-11-03']) {
    expect(shown).toContain(words)
  }
  await ui.unmount()
})

test('npx runs the release this mod came with, read from its own manifest', async ($, on) => {
  const { argvs, clock, reads } = setup(on, { manifest: JSON.stringify({ version: '0.22.0' }) })
  await open($)
  const ui = await $.ui.mount({ ...paneAt(100), surface: 'terminal' })
  await clock.advance(0)
  expect(argvs[0]?.slice(0, 3)).toEqual(['npx', '-y', 'agent-walker@0.22.0'])
  expect(reads[0]).toMatch(MANIFEST_PATH)
  await ui.unmount()
})

test('a manifest without a release version runs nothing and says why', async ($, on) => {
  const { argvs, clock } = setup(on, { manifest: JSON.stringify({ version: 'latest' }) })
  await open($)
  const ui = await $.ui.mount({ ...paneAt(100), surface: 'terminal' })
  await clock.advance(0)
  expect(argvs).toHaveLength(0)
  expect(await hasText(ui, 'no version')).toBe(true)
  await ui.unmount()
})

test('AGENT_WALKER_BIN replaces npx', async ($, on) => {
  const { argvs, clock } = setup(on, { env: { AGENT_WALKER_BIN: '/bin/aw' } })
  await open($)
  const ui = await $.ui.mount({ ...paneAt(100), surface: 'terminal' })
  await clock.advance(0)
  expect(argvs[0]).toEqual(['/bin/aw', 'summary', '--json'])
  await ui.unmount()
})

test('a new width lays out again without asking the CLI again', async ($, on) => {
  const { argvs, clock } = setup(on)
  await open($)
  const wide = await $.ui.mount({ ...paneAt(100), surface: 'terminal' })
  await clock.advance(0)
  await wide.unmount()
  const narrow = await $.ui.mount({ ...paneAt(40), surface: 'terminal' })
  await clock.advance(0)
  expect(await hasText(narrow, 'Tokens')).toBe(true)
  expect(argvs).toHaveLength(1)
  await narrow.unmount()
})

for (const surface of ['desktop', 'vscode', 'mobile'] as const) {
  test(`${surface} draws the card`, async ($, on) => {
    const { argvs, clock } = setup(on)
    await open($)
    const ui = await $.ui.mount({ ...paneAt(40), surface })
    await clock.advance(0)
    expect(await ui.find({ type: 'Svg' })).toBeDefined()
    expect(argvs).toEqual([['npx', '-y', 'agent-walker@0.21.2', 'summary', '--json']])
    await ui.unmount()
  })
}

test('opening again reads the logs again', async ($, on) => {
  const { argvs, clock } = setup(on)
  await open($)
  const first = await $.ui.mount({ ...paneAt(100), surface: 'terminal' })
  await clock.advance(0)
  await first.unmount()
  await open($)
  const second = await $.ui.mount({ ...paneAt(100), surface: 'terminal' })
  await clock.advance(0)
  await second.unmount()
  expect(argvs).toHaveLength(2)
})

test('a load from before a reopen does not land, and the reopen loads afresh', async ($, on) => {
  let release: (text: string) => void = () => undefined
  const held = new Promise<string>(resolve => {
    release = resolve
  })
  const state: { isFirstOpen: boolean } = { isFirstOpen: true }
  const { clock } = setup(on, {
    run: () => {
      if (state.isFirstOpen) {
        state.isFirstOpen = false
        return held
      }
    },
  })
  await open($)
  const ui = await $.ui.mount({ ...paneAt(100), surface: 'terminal' })
  await clock.advance(0)

  await open($)
  await clock.advance(0)
  expect(await hasText(ui, 'Gull')).toBe(true)

  const stale = { ...SUMMARY_DOC, total: { ...SUMMARY_DOC.total, sessions: 1 } }
  release(JSON.stringify(stale))
  await clock.advance(0)
  expect(await hasText(ui, '330')).toBe(true)
  await ui.unmount()
})

test('a window with no finished turns still draws, with dashes for the durations', async ($, on) => {
  const total = { ...SUMMARY_DOC.total, turn_length: { turns: 0, p50_ms: null, p90_ms: null } }
  const { clock } = setup(on, { answer: { stdout: JSON.stringify({ ...SUMMARY_DOC, total }) } })
  await open($)
  const ui = await $.ui.mount({ ...paneAt(100), surface: 'terminal' })
  await clock.advance(0)
  expect(await hasText(ui, 'p50 — · p90 —')).toBe(true)
  await ui.unmount()
})

test('the session tab asks for this session and draws it', async ($, on) => {
  const { argvs, clock } = setup(on)
  const ui = await openSession($, clock, 60)
  expect(argvs.at(-1)).toEqual([
    'npx',
    '-y',
    'agent-walker@0.21.2',
    'session',
    '232f4aab-9dfd-4a3c-bbfb-3900229fcc57',
    '--json',
  ])
  const shown = (await ui.findAll({ type: 'Text' })).map(text => text.text).join('\n')
  for (const words of [
    'Session 232f4aab',
    'Nov 3 17:42 – Nov 6 00:53',
    '$106',
    'Working time',
    'model 33%',
    'Your pace',
    'Bash 336',
    'code-evaluator]0;x 2',
    'ref:ntn',
    'Opus 5.5 100.0%',
    'TURN LENGTH',
    '3 turns · 1 ran ≥20m',
  ]) {
    expect(shown).toContain(words)
  }
  expect(shown).not.toContain('Tokens')
  await ui.unmount()
})

test('a name from the logs cannot hide text or move the cursor', async ($, on) => {
  const { clock } = setup(on)
  const ui = await openSession($, clock, 70)
  const shown = (await ui.findAll({ type: 'Text' })).map(text => text.text).join('\n')
  expect(shown).toContain('WebFetch[31m 140')
  expect(shown).toContain('code-evaluator]0;x 2')
  expect(shown).toContain('ref:ntn')
  expect(shown).toContain('Opus 5.5 100.0%')
  await ui.unmount()
})

for (const [bodyColumns, layout] of [
  [100, 'beside the table'],
  [60, 'stacked'],
] as const) {
  test(`the cache shows ${layout}`, async ($, on) => {
    const { clock } = setup(on)
    const ui = await openSession($, clock, bodyColumns)
    const shown = (await ui.findAll({ type: 'Text' })).map(text => text.text).join('\n')
    expect(shown).toContain('95% cached')
    expect(shown).toContain('expired 10')
    await ui.unmount()
  })
}

test('a subagent with no calls to count is named without a zero', async ($, on) => {
  const doc = {
    ...SESSION_DOC,
    total: { ...SESSION_DOC.total, subagents: [{ name: 'default', calls: 0 }] },
  }
  const { clock } = setup(on, { answer: { stdout: JSON.stringify(doc) } })
  const ui = await openSession($, clock, 70)
  const shown = (await ui.findAll({ type: 'Text' })).map(text => text.text).join('\n')
  expect(shown).toContain('default')
  expect(shown).not.toContain('default 0')
  expect(shown).not.toContain('0 calls')
  await ui.unmount()
})

test('shares of a partly measured session say so, and an unpriced model shows', async ($, on) => {
  const doc = {
    ...SESSION_DOC,
    total: {
      ...SESSION_DOC.total,
      cost: { estimated_usd: 1, unpriced_tokens: 5000 },
      working_time: { active_ms: 36_000_000, human_wait_ms: 0, model_share: 1, measured_ms: 3_600_000 },
    },
  }
  const { clock } = setup(on, { answer: { stdout: JSON.stringify(doc) } })
  const ui = await openSession($, clock, 70)
  const shown = (await ui.findAll({ type: 'Text' })).map(text => text.text).join('\n')
  expect(shown).toContain('model 100% of 1h 00m')
  expect(shown).not.toContain('tools 0%')
  expect(shown).toContain('5.0K tokens unpriced')
  await ui.unmount()
})

test('details stop where the width does, counting wide names as two cells', () => {
  expect(fitDetails(['aaaa', 'bbbb'], 11)).toBe('aaaa · bbbb')
  expect(fitDetails(['aaaa', 'bbbb'], 10)).toBe('aaaa')
  expect(fitDetails(['日本語', 'bbbb'], 12)).toBe('日本語')
  expect(fitDetails(['日本語'], 5)).toBe('')
})

test('a session id that cannot be read shows in the pane instead of loading for ever', async ($, on) => {
  const { clock } = setup(on, { noSessionId: true })
  const ui = await openSession($, clock, 100)
  expect(await hasText(ui, 'Could not read')).toBe(true)
  await ui.unmount()
})

test('a session with no logs yet says so in the pane', async ($, on) => {
  const { clock } = setup(on, {
    answer: { exitCode: 1, stderr: 'Error: no session 232f in the last 30 days' },
  })
  const ui = await openSession($, clock, 100)
  expect(await hasText(ui, 'no session 232f')).toBe(true)
  await ui.unmount()
})

test('the other tab is read only when it is opened', async ($, on) => {
  const { argvs, clock } = setup(on)
  await open($)
  const ui = await $.ui.mount({ ...paneAt(100), surface: 'terminal' })
  await clock.advance(0)
  expect(argvs).toHaveLength(1)
  await ui.press({ key: 'tab:session' })
  await clock.advance(0)
  expect(argvs).toHaveLength(2)
  expect(argvs[1]).toContain('session')
  await ui.unmount()
})

for (const surface of ['desktop', 'vscode', 'mobile'] as const) {
  test(`${surface} draws the session's graph as an image`, async ($, on) => {
    const { clock } = setup(on)
    const ui = await openSession($, clock, 100, surface)
    const [image] = await ui.findAll({ type: 'Svg' })
    const source = String(image?.props.source)
    expect(source).toContain('#efc768')
    expect(source).toContain('#84a7ff')
    await ui.unmount()
  })

  test(`${surface} lays the session out for the app, its details whole at any width`, async ($, on) => {
    const { clock } = setup(on)
    const ui = await openSession($, clock, 40, surface)
    const [cost] = (await ui.findAll({ type: 'Text' })).filter(text => text.text === '$106')
    expect(cost?.props.size).toBe('title')
    expect(await hasText(ui, 'TURN LENGTH')).toBe(true)
    expect(await hasText(ui, 'expired 10 · cold start 1')).toBe(true)
    expect(await hasText(ui, 'p50 2m · p90 13m · max 1h 5m')).toBe(true)
    await ui.unmount()
  })
}

test('the terminal keeps the session in cells, not in app type sizes', async ($, on) => {
  const { clock } = setup(on)
  const ui = await openSession($, clock, 100)
  const [cost] = (await ui.findAll({ type: 'Text' })).filter(text => text.text === '$106')
  expect(cost?.props.size).toBeUndefined()
  await ui.unmount()
})

test('a session document with no id is refused rather than misdrawn', async ($, on) => {
  const { clock } = setup(on, {
    answer: { stdout: JSON.stringify({ ...SESSION_DOC, session: {} }) },
  })
  const ui = await openSession($, clock, 100)
  expect(await hasText(ui, 'cannot read')).toBe(true)
  await ui.unmount()
})

test('what the CLI says on failure shows in the pane', async ($, on) => {
  const { clock } = setup(on, {
    answer: { exitCode: 2, stdout: '', stderr: "error: invalid value 'summary'\n" },
  })
  await open($)
  const ui = await $.ui.mount({ ...paneAt(100), surface: 'terminal' })
  await clock.advance(0)
  expect(await hasText(ui, "error: invalid value 'summary'")).toBe(true)
  await ui.unmount()
})

test('an escape code in what the CLI prints does not cost the pane', async ($, on) => {
  const { clock } = setup(on, {
    answer: { exitCode: 1, stdout: '', stderr: '\u001b[33mWARN\u001b[0m no logs\n' },
  })
  await open($)
  const ui = await $.ui.mount({ ...paneAt(100), surface: 'terminal' })
  await clock.advance(0)
  expect(await hasText(ui, '[33mWARN[0m no logs')).toBe(true)
  await ui.unmount()
})

test('a run that throws shows its error, and a reopen tries again', async ($, on) => {
  const state: { shouldFail: boolean } = { shouldFail: true }
  const { clock } = setup(on, {
    run: () => {
      if (state.shouldFail) {
        throw new Error('spawn failed')
      }
    },
  })
  await open($)
  const ui = await $.ui.mount({ ...paneAt(100), surface: 'terminal' })
  await clock.advance(0)
  expect(await hasText(ui, 'Could not run agent-walker')).toBe(true)

  state.shouldFail = false
  await open($)
  await clock.advance(0)
  expect(await hasText(ui, 'Gull')).toBe(true)
  await ui.unmount()
})

for (const [name, doc] of [
  ['an older schema', { ...SUMMARY_DOC, schema_version: 2 }],
  ['a missing field', { ...SUMMARY_DOC, total: { ...SUMMARY_DOC.total, tokens: {} } }],
  ['a control character in a name', { ...SUMMARY_DOC, total: { ...SUMMARY_DOC.total, codename: { ops: 'a\u001bb' } } }],
] as const) {
  test(`${name} is refused rather than misdrawn`, async ($, on) => {
    const { clock } = setup(on, { answer: { stdout: JSON.stringify(doc) } })
    await open($)
    const ui = await $.ui.mount({ ...paneAt(100), surface: 'terminal' })
    await clock.advance(0)
    expect(await hasText(ui, 'Update the mod')).toBe(true)
    await ui.unmount()
  })
}

test('a newer schema is read as long as the fields are there', async ($, on) => {
  const { clock } = setup(on, {
    answer: { stdout: JSON.stringify({ ...SUMMARY_DOC, schema_version: 4 }) },
  })
  await open($)
  const ui = await $.ui.mount({ ...paneAt(100), surface: 'terminal' })
  await clock.advance(0)
  expect(await hasText(ui, 'Gull')).toBe(true)
  await ui.unmount()
})

for (const [name, svg] of [
  ['no card', undefined],
  ['an empty card', ''],
  ['a card past what the element takes', `<svg>${'x'.repeat(131_073)}</svg>`],
] as const) {
  test(`with ${name}, an app draws the numbers instead`, async ($, on) => {
    const { clock } = setup(on, {
      answer: { stdout: JSON.stringify({ ...SUMMARY_DOC, card_svg: svg }) },
    })
    await open($)
    const ui = await $.ui.mount({ ...paneAt(40), surface: 'desktop' })
    await clock.advance(0)
    expect(await ui.find({ type: 'Svg' })).toBeUndefined()
    expect(await hasText(ui, 'Tokens')).toBe(true)
    await ui.unmount()
  })
}

test('the pane closes once the keys leave it', async ($, on) => {
  const { closes, clock } = setup(on)
  await open($)
  const focused = await $.ui.mount({ ...paneAt(100), surface: 'terminal' })
  await clock.advance(0)
  expect(closes).toHaveLength(0)
  await focused.unmount()

  const left = await $.ui.mount({ ...paneAt(100, false), surface: 'terminal' })
  await clock.advance(0)
  expect(closes).toHaveLength(1)
  await left.unmount()
})

test('a pane that has not held the keys yet stays open', async ($, on) => {
  const { closes, clock } = setup(on)
  await open($)
  const ui = await $.ui.mount({ ...paneAt(100, false), surface: 'terminal' })
  await clock.advance(0)
  expect(closes).toHaveLength(0)
  await ui.unmount()
})

test('a draw from before a reopen does not close the pane', async ($, on) => {
  const { closes, clock } = setup(on)
  await open($)
  const focused = await $.ui.mount({ ...paneAt(100), surface: 'terminal' })
  await clock.advance(0)
  await focused.unmount()
  const left = await $.ui.mount({ ...paneAt(100, false), surface: 'terminal' })
  await open($)
  await clock.advance(0)
  expect(closes).toHaveLength(0)
  await left.unmount()
})

test('an app keeps its pane when it reports no keys', async ($, on) => {
  const { closes, clock } = setup(on)
  await open($)
  const first = await $.ui.mount({ ...paneAt(40), surface: 'desktop' })
  await clock.advance(0)
  await first.unmount()
  const second = await $.ui.mount({ ...paneAt(40, false), surface: 'desktop' })
  await clock.advance(0)
  expect(closes).toHaveLength(0)
  await second.unmount()
})

for (const kind of ['composer', 'bridge', 'sdk']) {
  test(`a prompt from the person through ${kind} closes the pane`, async ($, on) => {
    const { closes } = setup(on)
    await $.prompt.submit({ text: 'next question', origin: { kind } } as never)
    expect(closes).toHaveLength(1)
  })
}

test('a background prompt leaves the pane alone', async ($, on) => {
  const { closes } = setup(on)
  await $.prompt.submit({ text: 'tick', origin: { kind: 'scheduled-trigger' } } as never)
  expect(closes).toHaveLength(0)
})

test('opening the pane with its own command does not close it', async ($, on) => {
  const { closes } = setup(on)
  await $.prompt.submit({
    text: '/agent-walker',
    origin: { kind: 'composer' },
    command: { name: 'agent-walker', args: '' },
  } as never)
  expect(closes).toHaveLength(0)
})

for (const isInteractive of [true, false]) {
  test(`the command is there when isInteractive is ${String(isInteractive)}`, async ($, on) => {
    const registered: string[] = []
    on('command.register', (_, e) => {
      registered.push(e.name)
      return { value: { command: e.name } }
    })
    on('session.start', (_, e) => ({ cwd: e.cwd }))
    await $.session.start({ cwd: '/', surface: 'desktop', isInteractive })
    expect(registered).toEqual(['agent-walker'])
  })
}

test('numbers read as they do on the card', () => {
  expect(formatTokens(9_142_911_082)).toBe('9.1B')
  expect(formatTokens(304_763_702)).toBe('304.8M')
  expect(formatTokens(906_279)).toBe('906.3K')
  expect(formatTokens(42)).toBe('42')
  expect(formatUsd(4092.36)).toBe('$4,092')
  expect(formatUsd(136.4)).toBe('$136')
  expect(formatUsd(9.5)).toBe('$9.50')
  expect(formatHours(602_763_155)).toBe('167h 26m')
  expect(formatDuration(42_424)).toBe('42s')
  expect(formatDuration(357_379)).toBe('5m')
  expect(formatDuration(8_561_552)).toBe('2h 22m')
})

test('an unknown value shows a dash, never a zero', () => {
  const summary = {
    codename: { ops: 'Eclipse', animal: 'Gull', rank: 'A', opsColor: '#fff', braille: [] },
    days: 30,
    start: '2026-10-05',
    end: '2026-11-03',
    tokens: 0,
    sessions: 0,
    activeDays: 0,
    avgParallel: 0,
    peakParallel: 0,
    turns: 0,
    p50Ms: 0,
    p90Ms: 0,
  }
  const rows = [...totalRows(summary), ...activityRows(summary)]
  for (const label of ['Working time', 'Cost', 'Context']) {
    expect(rows.find(row => row.label === label)?.value).toBe('—')
  }
})
