import { expect, mock, test } from 'claude-code/testing'
import { progress, summary } from '../hooks/byoclaude.js'

const STDERR = [
  'panel: 20261009-010203-abcd: attempt on openai/gpt-5.6-sol, kimi/k3, judge claude-opus-5-5; ledger /tmp/p',
  'panel: A openai/gpt-5.6-sol started',
  'panel: B kimi/k3 started',
  'panel: A openai/gpt-5.6-sol finished in 2m 3s',
  'panel: B kimi/k3 timeout: did not finish in 900s',
  'panel: judge claude-opus-5-5 comparing 2 reports',
].join('\n') + '\n'

const VERDICT = '# Panel 20261009-010203-abcd (attempt)\n\n## Summary\n\nA is right.'

function setup(on, surfaces: string[], calls: { argv?: readonly string[]; input?: string; prompts: string[]; runs: (readonly string[])[] }) {
  const clock = mock.clock(on)
  on('session.start', () => ({ cwd: '/work' }))
  on('tool.register', () => ({ value: undefined }))
  on('command.register', () => ({ value: undefined }))
  on('session.surfaces', () => ({ value: surfaces }))
  on('env.get', ($, e) => ({ value: e.name === 'BYOCLAUDE_BIN' ? '/bin/byoclaude' : null }))
  on('ui.open', () => ({ value: { isPlaced: true } }))
  on('ui.toast', () => ({ value: undefined }))
  on('ui.render', () => ({ type: 'Text', props: {}, children: ['engine'] }))
  on('process.spawn', async function* ($, e) {
    calls.argv = e.argv
    calls.input = e.input
    yield { stream: 'stderr', text: STDERR.slice(0, 40) }
    yield { stream: 'stderr', text: STDERR.slice(40) }
    yield { stream: 'stdout', text: VERDICT }
    return { value: { code: 0, signal: null } }
  })
  on('process.run', ($, e) => {
    calls.runs.push(e.argv)
    return { value: { exitCode: 0, stdout: 'applied', stderr: '' } }
  })
  on('prompt.submit', ($, e) => {
    calls.prompts.push(e.text)
    return { text: e.text }
  })
  return clock
}

test('progress lines build the panel state', () => {
  const panel = { id: '', mode: 'opinion', phase: 'starting', members: [], judge: '', notes: [], started: 1000 } as any
  for (const line of STDERR.trim().split('\n')) progress(panel, line, 1000)
  expect(panel.id).toBe('20261009-010203-abcd')
  expect(panel.mode).toBe('attempt')
  expect(panel.phase).toBe('judging')
  expect(panel.members.map((m) => m.state)).toEqual(['done', 'timeout'])
  expect(panel.members[0].took).toBe('2m 3s')
  expect(summary(panel, 62000)).toBe('panel 20261009-010203-abcd · attempt · judge claude-opus-5-5 comparing · 1m 1s')
})

test('headless: the tool runs the panel inline and returns the verdict', async ($, on) => {
  const calls = { prompts: [], runs: [] } as any
  setup(on, [], calls)
  await $.session.start({ surface: 'terminal', isInteractive: false, cwd: '/work' })
  const out = await $.tool.call({ tool: 'mcp__byoclaude__panel', question: 'Is it safe?', models: ['openai/gpt-5.6-sol', 'kimi/k3'], attempt: true })
  expect(calls.argv).toEqual(['/bin/byoclaude', 'panel', '--attempt', '--models', 'openai/gpt-5.6-sol,kimi/k3', '-'])
  expect(calls.input).toBe('Is it safe?')
  expect(out.result).toContain('byoclaude panel 20261009-010203-abcd (attempt) finished.')
  expect(out.result).toContain('A is right.')
  expect(out.result).toContain('byoclaude panel apply 20261009-010203-abcd')
  expect(calls.prompts).toEqual([])
})

test('interactive: the tool returns at once and the verdict arrives as a prompt; the pane applies a patch', async ($, on) => {
  const calls = { prompts: [], runs: [] } as any
  const clock = setup(on, ['terminal'], calls)
  await $.session.start({ surface: 'terminal', isInteractive: true, cwd: '/work' })
  const out = await $.tool.call({ tool: 'mcp__byoclaude__panel', question: 'Fix the race', attempt: true })
  expect(out.result).toContain('Panel started in the background')
  for (let i = 0; i < 50 && calls.prompts.length === 0; i++) await clock.advance(10)
  expect(calls.prompts.length).toBe(1)
  expect(calls.prompts[0]).toContain('A is right.')
  const ui = await $.ui.mount({
    plugin: 'byoclaude',
    component: 'Pane',
    requestId: 'byoclaude-panel',
    surface: 'terminal',
    viewport: { columns: 100, rows: 30 },
    props: { title: 'Panel', isFocused: true, bodyColumns: 80, placement: 'inline', scroll: { offset: 0, bodyRows: 20 }, view: {} },
  } as any)
  expect(await ui.find({ type: 'Text', text: /attempt · finished/ })).toBeDefined()
  expect(await ui.find({ key: 'apply-B' })).toBeUndefined()
  await ui.press({ key: 'apply-A' })
  expect(calls.runs).toEqual([['/bin/byoclaude', 'panel', 'apply', '20261009-010203-abcd', 'A']])
  await ui.unmount()
})

test('the gate holds a risky command and can refuse it', async ($, on) => {
  on('env.get', ($, e) => ({ value: e.name === 'BYOCLAUDE_PANEL_GATE' ? '1' : null }))
  on('session.surfaces', () => ({ value: ['terminal'] }))
  const ran: string[] = []
  on('tool.call', ($, e) => {
    if (e.tool === 'AskUserQuestion') return { result: { answers: { [e.questions[0].question]: 'Refuse' } } }
    ran.push(e.command)
    return { result: 'ran' }
  })
  const risky = await $.tool.call({ tool: 'Bash', command: 'git push --force origin main' })
  expect(risky.deny).toContain('refused')
  const safe = await $.tool.call({ tool: 'Bash', command: 'git status' })
  expect(safe.result).toBe('ran')
  expect(ran).toEqual(['git status'])
})
