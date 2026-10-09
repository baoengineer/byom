// byoclaude's mod: panels as a native tool, a /panel command, a live pane and a band line.
// `byoclaude panel` does the work; this module starts it, follows its progress lines, and
// brings the verdict back into the session.

const PANE = 'byoclaude-panel'
const RISKY = /\brm\s+-[a-z]*(rf|fr)|\bgit\s+push\b.*(--force\b|\s-f\b)|\bgit\s+reset\s+--hard\b|\bgit\s+clean\s+-[a-z]*f|\bdrop\s+(table|database)\b|\bterraform\s+(apply|destroy)\b|\bkubectl\s+delete\b/i

// Panels started in this session, oldest first, and the one the pane shows.
const panels = []
let shown = -1
let ticker = null

/** A panel's state, built from `byoclaude panel` progress lines. */
function track(question, attempt) {
  const panel = {
    id: '',
    question,
    mode: attempt ? 'attempt' : 'opinion',
    phase: 'starting',
    members: [],
    judge: '',
    notes: [],
    verdict: '',
    error: '',
    started: 0,
    finished: 0,
    applied: '',
  }
  panels.push(panel)
  shown = panels.length - 1
  return panel
}

/** Apply one stderr line from `byoclaude panel` to a panel's state. */
export function progress(panel, line, now) {
  let m = line.match(/^panel: (\S+): (opinion|attempt) on (.+), judge (\S+); ledger /)
  if (m) {
    panel.id = m[1]
    panel.mode = m[2]
    panel.judge = m[4]
    panel.phase = 'running'
    panel.members = m[3].split(', ').map((model, i) => ({
      label: String.fromCharCode(65 + i),
      model,
      state: 'waiting',
      since: now,
      took: '',
    }))
    return
  }
  m = line.match(/^panel: ([A-Z]+) (\S+) (started|finished in (.+)|running `.*`|(\S+): (.*))$/)
  if (m) {
    const member = panel.members.find((x) => x.label === m[1])
    if (!member) return
    if (m[3] === 'started') {
      member.state = 'working'
      member.since = now
    } else if (m[4]) {
      member.state = 'done'
      member.took = m[4]
    } else if (m[3].startsWith('running')) {
      member.state = 'testing'
    } else {
      member.state = m[5]
      member.took = m[6]
    }
    return
  }
  m = line.match(/^panel: judge (\S+) comparing/)
  if (m) {
    panel.phase = 'judging'
    return
  }
  m = line.match(/^panel: (warning: .*|skipped .*)$/)
  if (m) panel.notes.push(m[1])
}

async function binary($) {
  return (await $.env.get('BYOCLAUDE_BIN')) || 'byoclaude'
}

function argv(bin, input) {
  const args = [bin, 'panel']
  if (input.attempt) args.push('--attempt')
  if (Array.isArray(input.models) && input.models.length) args.push('--models', input.models.map(String).join(','))
  if (input.judge) args.push('--judge', String(input.judge))
  if (input.size) args.push('--size', String(input.size))
  if (input.test) args.push('--test', String(input.test))
  args.push('-')
  return args
}

/** Whether anything draws: empty in `claude -p`, where a panel runs inline. */
async function interactive($) {
  return (await $.session.surfaces()).length > 0
}

/** Run one panel to its end, following its progress; resolves with the panel. */
async function execute($, panel, input) {
  const child = $.process.spawn({ argv: argv(await binary($), input), input: input.question })
  const pending = { stdout: '', stderr: '' }
  let out = ''
  let err = ''
  panel.started = await $.clock.now()
  for await (const chunk of child) {
    if (chunk.stream === 'stdout') {
      out += chunk.text
      continue
    }
    err += chunk.text
    pending.stderr += chunk.text
    const lines = pending.stderr.split('\n')
    pending.stderr = lines.pop()
    const now = await $.clock.now()
    for (const line of lines) progress(panel, line.trim(), now)
    $.ui.invalidate('ui.render')
  }
  const ended = await child.result
  panel.finished = await $.clock.now()
  if (ended.code === 0) {
    panel.phase = 'done'
    panel.verdict = out.trim()
  } else {
    panel.phase = 'failed'
    panel.verdict = out.trim()
    panel.error = err.trim().split('\n').slice(-8).join('\n') || 'exited with ' + (ended.signal || ended.code)
  }
  $.ui.invalidate('ui.render')
  return panel
}

/** What Claude reads when a panel ends. */
export function report(panel) {
  const head = 'byoclaude panel ' + (panel.id || '(no id)') + ' (' + panel.mode + ')'
  if (panel.phase === 'done') {
    const apply = panel.mode === 'attempt'
      ? '\n\nNothing was applied. Review a patch with `byoclaude panel show ' + panel.id + '` and apply one only with the user\'s go-ahead: `byoclaude panel apply ' + panel.id + ' [panelist]`.'
      : ''
    return head + ' finished.\n\n' + panel.verdict + apply
  }
  const partial = panel.verdict ? '\n\nWhat it printed:\n\n' + panel.verdict : ''
  return head + ' failed:\n\n' + panel.error + partial
}

/** Keep the band's elapsed times moving while a panel runs. */
function tick($) {
  const running = panels.some((p) => p.phase !== 'done' && p.phase !== 'failed')
  if (running && !ticker) ticker = $.clock.every(1000, () => tickOnce($))
  if (!running && ticker) {
    ticker.cancel()
    ticker = null
  }
}

function tickOnce($) {
  $.ui.invalidate('ui.render')
  tick($)
}

/** Run a panel outside the event that started it, then hand the verdict to Claude. */
async function background($, panel, input) {
  try {
    await execute($, panel, input)
  } catch (error) {
    panel.phase = 'failed'
    panel.error = String(error && error.message ? error.message : error)
  }
  tick($)
  $.ui.toast('panel ' + (panel.id || '') + (panel.phase === 'done' ? ' finished' : ' failed'))
  await $.prompt.submit({ text: report(panel) })
}

/** Start a panel in the background; the verdict comes back as a prompt. */
function launch($, input) {
  const panel = track(input.question, input.attempt)
  tick($)
  $.clock.after(0, () => background($, panel, input))
  return panel
}

function elapsed(since, now) {
  const s = Math.max(0, Math.round((now - since) / 1000))
  return s < 60 ? s + 's' : Math.floor(s / 60) + 'm ' + (s % 60) + 's'
}

/** One line for the band above the prompt. */
export function summary(panel, now) {
  const done = panel.members.filter((m) => m.state === 'done').length
  const total = panel.members.length
  const where = panel.phase === 'starting'
    ? 'starting'
    : panel.phase === 'judging'
      ? 'judge ' + panel.judge + ' comparing'
      : done + '/' + total + ' reports'
  return 'panel ' + (panel.id || '') + ' · ' + panel.mode + ' · ' + where + ' · ' + elapsed(panel.started || now, now)
}

const TOOL_DESCRIPTION = [
  'Ask a byoclaude panel: several models from different providers answer the same question',
  'independently as Claude Code agents that can read this repository and run tests, and a judge',
  'from another provider compares their anonymized reports. Use it when being wrong is costly:',
  'reviews of risky changes (security, data loss, auth, billing, migrations), competing debugging',
  'hypotheses, design decisions, or when the user asks. Not for routine edits or quick questions.',
  'The question must stand alone: panelists do not see this conversation, so include the goal,',
  'paths, constraints and what a good answer contains. With attempt: true each panelist makes the',
  'change in its own git worktree and the verdict ranks the patches; nothing is applied.',
  'In an interactive session the call returns at once and the verdict arrives later as a message;',
  'keep working meanwhile. Panels take minutes and use several model runs.',
].join(' ')

export function register(on) {
  on('session.start', async ($, e, next) => {
    const started = await next(e)
    await $.tool.register({
      name: 'panel',
      description: TOOL_DESCRIPTION,
      inputSchema: {
        type: 'object',
        properties: {
          question: { type: 'string', description: 'The self-contained question, or the change to attempt' },
          attempt: { type: 'boolean', description: 'Try the change in a worktree per panelist instead of answering' },
          models: { type: 'array', items: { type: 'string' }, description: 'Panel members by model ID; default picks ready models from distinct providers' },
          judge: { type: 'string', description: 'Judge model ID; default picks one from a provider not on the panel' },
          size: { type: 'integer', minimum: 2, description: 'Panel size when picking automatically' },
          test: { type: 'string', description: 'Test command run in each attempt worktree' },
        },
        required: ['question'],
      },
    })
    try {
      await $.command.register({
        name: 'panel',
        description: 'Ask a panel of models a question (no question: open the panel pane)',
        argumentHint: '[question]',
        immediate: true,
      })
    } catch (error) {
      $.ui.log('byoclaude: /panel is taken, use the panel tool or /byoclaude:panel', { to: 'debug' })
    }
    return started
  })

  on('tool.call', { tool: 'mcp__byoclaude__panel' }, async ($, e) => {
    const question = String(e.question || '').trim()
    if (!question) return { result: 'A panel needs a question.' }
    const input = { question, attempt: !!e.attempt, models: e.models, judge: e.judge, size: e.size, test: e.test }
    if (!(await interactive($))) {
      const panel = track(question, input.attempt)
      await execute($, panel, input)
      return { result: report(panel) }
    }
    launch($, input)
    await $.ui.open({ id: PANE, title: 'Panel' })
    return {
      result: 'Panel started in the background (' + (input.attempt ? 'attempt' : 'opinion') + ' mode). Its verdict will arrive as a message from byoclaude when the judge finishes, usually in a few minutes. Continue with other work; do not start the same panel again.',
    }
  })

  on('command.run', { command: 'panel' }, async ($, e) => {
    const question = String(e.args || '').trim()
    if (question) launch($, { question, attempt: false })
    if (!panels.length) return { text: 'No panels yet. Run /panel <question>, or ask Claude for a panel.' }
    await $.ui.open({ id: PANE, title: 'Panel', focus: true, closeOnEscape: true })
    return question ? { text: 'Panel started; the verdict will come back here as a message.' } : {}
  })

  on('tool.call', { tool: 'Bash' }, async ($, e, next) => {
    if ((await $.env.get('BYOCLAUDE_PANEL_GATE')) !== '1' || !RISKY.test(String(e.command || ''))) return next(e)
    if (!(await interactive($))) return next(e)
    let answer = 'Refuse'
    try {
      answer = await $.ui.ask('byoclaude: this command is risky. ' + e.command, ['Run it', 'Ask a panel first', 'Refuse'])
    } catch (error) {
      return next(e)
    }
    if (answer === 'Run it') return next(e)
    if (answer === 'Ask a panel first') {
      const cwd = await $.session.cwd()
      launch($, {
        question: 'Claude is about to run this shell command in ' + cwd + ':\n\n' + e.command + '\n\nIs it safe and correct to run now? Say what it would change and what could go wrong, check the repository state that matters, and say what to do instead if it should not run.',
        attempt: false,
      })
      return { deny: 'The user asked a byoclaude panel to review this command first. Wait for the panel verdict message, then follow it.' }
    }
    return { deny: 'The user refused this command. Ask before trying a different approach.' }
  }).catch(async ($, e, next) => (next.called ? next(e) : { deny: 'The byoclaude command guard failed, so this command was not run.' }))

  on('ui.render', { component: 'AbovePrompt' }, async ($, e, next) => {
    const running = panels.filter((p) => p.phase !== 'done' && p.phase !== 'failed')
    if (!running.length) return next(e)
    const { Box, Text } = $.ui.resolve(e)
    const now = await $.clock.now()
    const theirs = await next(e)
    const lines = running.map((p, i) => Text({ dimColor: true, children: [summary(p, now)] }))
    return Box({ flexDirection: 'column', children: theirs ? [theirs, ...lines] : lines })
  })

  on('ui.render', { component: 'Pane' }, async ($, e, next) => {
    if (e.requestId !== PANE) return next(e)
    const { Box, Text, Button, Markdown } = $.ui.resolve(e)
    const panel = panels[shown]
    if (!panel) return Text({ children: ['No panels in this session yet.'] })
    const now = await $.clock.now()
    const redraw = () => $.ui.invalidate('ui.render')
    const rows = []
    if (panels.length > 1) {
      rows.push(Box({
        flexDirection: 'row',
        columnGap: 2,
        children: panels.map((p, i) => Button({
          key: 'panel-' + i,
          label: p.id || 'panel ' + (i + 1),
          plain: true,
          dimColor: i !== shown,
          onPress: () => {
            shown = i
            redraw()
          },
        })),
      }))
    }
    const state = panel.phase === 'done' ? 'finished' : panel.phase === 'failed' ? 'failed' : panel.phase
    rows.push(Text({ bold: true, children: [(panel.id || 'panel') + ' · ' + panel.mode + ' · ' + state] }))
    rows.push(Text({ dimColor: true, wrap: 'truncate-end', children: [panel.question.split('\n')[0]] }))
    for (const m of panel.members) {
      const time = m.took || (m.state === 'working' || m.state === 'testing' ? elapsed(m.since, now) : '')
      rows.push(Text({ children: [m.label + '  ' + m.model + '  ' + m.state + (time ? '  ' + time : '')] }))
    }
    if (panel.judge) rows.push(Text({ dimColor: panel.phase !== 'judging', children: ['judge  ' + panel.judge + (panel.phase === 'judging' ? '  comparing' : '')] }))
    for (const note of panel.notes) rows.push(Text({ dimColor: true, children: [note] }))
    if (panel.phase === 'failed') rows.push(Text({ color: 'red', children: [panel.error] }))
    if (panel.verdict) rows.push(Markdown({ text: panel.verdict }))
    if (panel.phase === 'done' && panel.mode === 'attempt') {
      const patched = panel.members.filter((m) => m.state === 'done')
      rows.push(Box({
        flexDirection: 'row',
        columnGap: 2,
        children: patched.map((m) => Button({
          key: 'apply-' + m.label,
          label: 'Apply ' + m.label + ' ' + m.model,
          onPress: async () => {
            const r = await $.process.run([await binary($), 'panel', 'apply', panel.id, m.label], { timeoutMs: 120000 })
            panel.applied = r.exitCode === 0 ? m.label : ''
            $.ui.toast(r.exitCode === 0 ? 'Applied ' + m.model + "'s patch" : 'Apply failed: ' + (r.stderr.trim().split('\n').pop() || r.exitCode))
            redraw()
          },
        })),
      }))
      if (panel.applied) rows.push(Text({ dimColor: true, children: ['Applied ' + panel.applied + '. Review with git diff.'] }))
    }
    return Box({ flexDirection: 'column', children: rows })
  })
}
