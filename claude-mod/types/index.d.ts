declare module 'claude-code' {
  interface PluginState {
    'agent-walker': {
      selected: string
      results: Shaped<Record<string, TabResult>>
    }
  }
}

export interface Codename {
  ops: string
  animal: string
  rank: string
  opsColor: string
  rankColor?: string
  braille: string[]
}

export interface Summary {
  codename: Codename
  days: number
  start: string
  end: string
  tokens: number
  costUsd?: number
  sessions: number
  activeDays: number
  workingMs?: number
  tokensPerMinute?: number
  cachedShare?: number
  avgParallel: number
  peakParallel: number
  fourPlusShare?: number
  turns: number
  p50Ms?: number
  p90Ms?: number
}

export interface NameCalls {
  name: string
  calls: number
}

export interface ModelShare {
  label: string
  share: number
}

export interface SessionStats {
  id: string
  firstAt: string
  lastAt: string
  costUsd?: number
  unpricedTokens?: number
  workingMs?: number
  onYouMs?: number
  modelShare?: number
  measuredMs?: number
  paceP50Ms?: number
  paceP90Ms?: number
  paceAvgMs?: number
  turns: number
  p50Ms?: number
  p90Ms?: number
  maxMs?: number
  longFromMs: number
  longTurns: number
  cachedShare?: number
  expiredCalls?: number
  coldStartCalls?: number
  lengthsMs: number[]
  tools: NameCalls[]
  subagents: NameCalls[]
  skills: string[]
  models: ModelShare[]
}

export type TabResult = { summary: Summary; svg?: string } | { session: SessionStats } | { error: string }
