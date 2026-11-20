export const meta = {
  name: 'pr-review',
  description: 'Two independent reviewers (A: correctness/safety/tests, B: UX/perf/feature set) cross-check each other; returns confirmed findings',
  whenToUse: 'Every Flint PR or local branch before merge. args: {pr?: number, worktree?: string, base?: string, commit?: string, context?: string, fixedFindings?: string}. Pass commit + fixedFindings to re-review a fix commit.',
  phases: [
    { title: 'Review', detail: 'reviewer A and reviewer B in parallel' },
    { title: 'Cross-check', detail: 'each reviewer tries to refute the other one\'s findings' },
  ],
}

const a = args || {}
if (!a.pr && !a.worktree) throw new Error('pr-review needs args.pr or args.worktree')

const FINDINGS = {
  type: 'object',
  properties: {
    verdict: { type: 'string', enum: ['clean', 'issues'] },
    findings: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          title: { type: 'string' },
          severity: { type: 'string', enum: ['blocker', 'major', 'minor'] },
          file: { type: 'string' },
          line: { type: 'integer' },
          scenario: { type: 'string', description: 'concrete inputs or state, and the wrong result' },
        },
        required: ['title', 'severity', 'scenario'],
      },
    },
    notes: { type: 'string', description: 'what you ran, including real-cluster checks and their output' },
  },
  required: ['verdict', 'findings'],
}
const VERDICTS = {
  type: 'object',
  properties: {
    results: {
      type: 'array',
      items: {
        type: 'object',
        properties: { index: { type: 'integer' }, confirmed: { type: 'boolean' }, reason: { type: 'string' } },
        required: ['index', 'confirmed', 'reason'],
      },
    },
  },
  required: ['results'],
}

const base = a.base || 'origin/master'
const target = a.commit
  ? `commit ${a.commit} (\`git -C ${a.worktree} show ${a.commit}\`). It claims to fix these findings:\n${a.fixedFindings || '(see the commit message)'}\nFor each finding, say whether it is fixed, and check that a test fails without the fix (revert the fix to prove it, then \`git checkout .\`). Then look for regressions the fix introduced.`
  : a.pr
    ? `PR #${a.pr} in Sealos/flint (\`gh pr view ${a.pr} -R Sealos/flint --comments\`, \`gh pr diff ${a.pr} -R Sealos/flint\`).` +
      (a.worktree ? ` The code is checked out in ${a.worktree}.` : ` To run code, fetch it into your own detached worktree: \`git fetch -q origin pull/${a.pr}/head && git worktree add --detach $TMPDIR/flint-review-${a.pr}-<you> FETCH_HEAD\`. Remove that worktree when you are done.`)
    : `the branch in ${a.worktree}, diffed against ${base} (\`git -C ${a.worktree} diff ${base}...HEAD\`).`
const ctx = a.context ? `\nContext from the coordinator:\n${a.context}\n` : ''
const common = `Review ${target}${ctx}
Read the linked issue and the user paths (docs/USER-PATHS.md) it touches. You are read-only: no commits, no pushes, no GitHub writes.
List only problems you confirmed by reading the code or running it. Return verdict "clean" if there are none.`

const LENSES = [
  { key: 'A', prompt: `${common}

You are reviewer A. Your lenses:
- correctness: edge cases, races, lock ordering, cancellation, error paths;
- safety: kubeconfig handling, MCP read-only guarantees, secret redaction;
- tests: does a test fail without the change? Are the UP-xx paths covered?
- regressions against master.` },
  { key: 'B', prompt: `${common}

You are reviewer B. Your lenses:
- UI/UX and design: keyboard flow, focus, wording, light and dark themes, empty, loading and error states;
- performance: UI-thread work, allocations per frame, behaviour at 50k rows, memory that is never freed;
- feature set: does it deliver what the issue asked for? What is missing?
Where it matters, check the behaviour on the local kind cluster, and on a scratch build of master for comparison.` },
]

phase('Review')
const reviews = await parallel(LENSES.map(l => () =>
  agent(l.prompt, { label: `reviewer ${l.key}`, phase: 'Review', schema: FINDINGS })))
const [ra, rb] = reviews
const all = [
  ...((ra && ra.findings) || []).map(f => ({ ...f, by: 'A' })),
  ...((rb && rb.findings) || []).map(f => ({ ...f, by: 'B' })),
]
if (!ra || !rb) log('A reviewer failed; its findings are missing from this run.')
if (all.length === 0) return { verdict: 'clean', confirmed: [], rejected: [], notes: [ra && ra.notes, rb && rb.notes] }

phase('Cross-check')
// Each reviewer tries to refute the other one's findings: an independent check without extra agents per finding.
const crossCheck = (by, list) => list.length === 0 ? Promise.resolve({ results: [] }) : agent(
  `${common}

Another reviewer reported the findings below. Try to REFUTE each one: read the code, and run it if that is cheap. Mark a finding confirmed only if you can reproduce the scenario or trace it in the code. When unsure, set confirmed=false.

${list.map((f, i) => `[${i}] (${f.severity}) ${f.title}, ${f.file || '?'}:${f.line || '?'}. ${f.scenario}`).join('\n')}`,
  { label: `cross-check ${by}'s findings`, phase: 'Cross-check', schema: VERDICTS })

const fa = all.filter(f => f.by === 'A'), fb = all.filter(f => f.by === 'B')
const [va, vb] = await parallel([() => crossCheck('A', fa), () => crossCheck('B', fb)])
const judge = (list, v) => list.map((f, i) => {
  const r = ((v && v.results) || []).find(x => x.index === i)
  return { ...f, confirmed: r ? r.confirmed : true, crossCheck: r ? r.reason : 'cross-check unavailable; kept' }
})
const judged = [...judge(fa, va), ...judge(fb, vb)]
const confirmed = judged.filter(f => f.confirmed)
const rejected = judged.filter(f => !f.confirmed)
log(`${confirmed.length} confirmed, ${rejected.length} refuted`)

const comment = confirmed.length === 0
  ? 'Two independent reviewers (correctness and safety; UX, performance and feature set) found no confirmed problems.'
  : `Two independent reviewers each checked the other's findings. Confirmed:\n\n` +
    confirmed.map(f => `- **${f.severity}**: ${f.title} (\`${f.file || '?'}:${f.line || '?'}\`). ${f.scenario}`).join('\n')

return {
  verdict: confirmed.some(f => f.severity !== 'minor') ? 'changes-needed' : confirmed.length ? 'minor-only' : 'clean',
  confirmed, rejected, comment,
  notes: [ra && ra.notes, rb && rb.notes].filter(Boolean),
}
