import type { EngineInterface } from 'claude-code'

import type { TabResult } from '../types'
import { parseCard, parseJson, parseSession, parseSummary, versionOf } from './parse'

const NPX = ['npx', '-y']
const UNREADABLE = 'This agent-walker prints output this mod cannot read. Update the mod.'
// Long enough for a first run with no parse cache and an npx download.
const RUN_TIMEOUT_MS = 120_000
// A control character in any text costs the whole pane: the engine refuses the tree it is in.
const CONTROL = /[^\P{Cc}\n\t]/gu

interface Failed {
  error: string
}

// The CLI this mod was released with, from its own plugin.json: the JSON it reads is that release's, and npx would
// otherwise fetch the newest without asking.
async function cliCommand($: EngineInterface): Promise<string[]> {
  const bin = await $.env.get('AGENT_WALKER_BIN')
  if (bin !== undefined) {
    return [bin]
  }
  const version = versionOf(parseJson(await $.fs.read(`${$.plugin.root}/.claude-plugin/plugin.json`)))
  if (version === undefined) {
    throw new Error('the mod has no version to run agent-walker at')
  }
  return [...NPX, `agent-walker@${version}`]
}

async function runCli($: EngineInterface, args: readonly string[]): Promise<{ stdout: string } | Failed> {
  try {
    const command = await cliCommand($)
    // Not the session's directory: npx prefers a bin or registry the project there declares.
    const home = (await $.env.get('HOME')) ?? (await $.env.get('USERPROFILE'))
    const run = await $.process.run([...command, ...args], {
      timeoutMs: RUN_TIMEOUT_MS,
      ...(home !== undefined && { cwd: home }),
    })
    if (run.exitCode === 0) {
      return { stdout: run.stdout }
    }
    return {
      error: run.stderr.replace(CONTROL, '').trim() || `agent-walker exited with ${String(run.exitCode)}`,
    }
  } catch (error) {
    return { error: `Could not run agent-walker: ${String(error).replace(CONTROL, '')}` }
  }
}

async function fetchSummary($: EngineInterface): Promise<TabResult> {
  const run = await runCli($, ['summary', '--json'])
  if ('error' in run) {
    return run
  }
  const value = parseJson(run.stdout)
  const summary = parseSummary(value)
  if (summary === undefined) {
    return { error: UNREADABLE }
  }
  const svg = parseCard(value)
  return { summary, ...(svg !== undefined && { svg }) }
}

async function fetchSession($: EngineInterface, id: string): Promise<TabResult> {
  const run = await runCli($, ['session', id, '--json'])
  if ('error' in run) {
    return run
  }
  const session = parseSession(parseJson(run.stdout))
  if (session === undefined) {
    return { error: UNREADABLE }
  }
  return { session }
}

export { fetchSession, fetchSummary }
