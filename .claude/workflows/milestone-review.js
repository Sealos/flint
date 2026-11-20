export const meta = {
  name: 'milestone-review',
  description: 'Before cutting a release: five review axes plus an independent whole-diff reviewer over the milestone diff, deduped, then adversarially verified',
  whenToUse: 'Before merging a release PR. args: {milestone: string (for example "M2"), base: string (the previous release tag or SHA), head?: string (default origin/master), trackingIssue?: number}.',
  phases: [
    { title: 'Review', detail: '5 axes + independent' },
    { title: 'Verify', detail: 'one skeptic over the deduped list' },
  ],
}

const a = args || {}
if (!a.milestone || !a.base) throw new Error('milestone-review needs args.milestone and args.base')
const head = a.head || 'origin/master'
const scope = `Review the ${a.milestone} milestone of Flint: \`git diff ${a.base}..${head}\`, and the milestone's issues (\`gh issue list -R Sealos/flint --milestone ${a.milestone} --state all\`).${a.trackingIssue ? ` The tracking issue is #${a.trackingIssue}.` : ''} To build or run anything, use a detached worktree at ${head} under $TMPDIR, and remove it when done. You are read-only.`

const FINDINGS = {
  type: 'object',
  properties: {
    findings: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          title: { type: 'string' }, severity: { type: 'string', enum: ['blocker', 'major', 'minor'] },
          file: { type: 'string' }, line: { type: 'integer' }, scenario: { type: 'string' },
        },
        required: ['title', 'severity', 'scenario'],
      },
    },
  },
  required: ['findings'],
}
const AXES = [
  ['correctness', 'Concurrency, the watch engine, error paths, cancellation and resource leaks.'],
  ['feature-set', 'Does each milestone issue deliver what it promised? Are there half-done features or missing states?'],
  ['user-paths', 'For every UP-xx path the milestone touches, is a test covering it, and does the test actually fail when the path breaks?'],
  ['ui-ux', 'Drive the app through the automation channel on the kind cluster. Check keyboard flow, wording, themes, and the empty, loading and error states. Shrink screenshots before reading them.'],
  ['performance', 'UI-thread work, 50k-row lists, log volume, memory growth over a long session; measure where possible.'],
  ['independent', 'Ignore the other axes. Read the whole diff fresh, and report whatever worries you most.'],
]

phase('Review')
const raw = await parallel(AXES.map(([k, d]) => () =>
  agent(`${scope}\n\nYour axis is ${k}: ${d}\nReport only problems you confirmed, each with file:line and a scenario.`,
    { label: k, phase: 'Review', schema: FINDINGS })
    .then(r => r ? r.findings.map(f => ({ ...f, axis: k })) : null)))
const failed = AXES.filter((_, i) => !raw[i]).map(([k]) => k)
if (failed.length) log(`Axes with no result: ${failed.join(', ')}`)

// The barrier is justified: dedupe across all axes before verifying.
const seen = new Map()
for (const f of raw.filter(Boolean).flat()) {
  const key = `${f.file || ''}:${f.line || ''}:${f.title.toLowerCase().slice(0, 40)}`
  if (!seen.has(key)) seen.set(key, f)
  else seen.get(key).axis += `+${f.axis}`
}
const list = [...seen.values()]
if (!list.length) return { confirmed: [], refuted: [], failedAxes: failed }

phase('Verify')
const V = {
  type: 'object',
  properties: { results: { type: 'array', items: { type: 'object', properties: { index: { type: 'integer' }, confirmed: { type: 'boolean' }, reason: { type: 'string' } }, required: ['index', 'confirmed', 'reason'] } } },
  required: ['results'],
}
const v = await agent(
  `${scope}\n\nTry to REFUTE each finding below. Mark a finding confirmed only if you can trace it in the code or reproduce it. When unsure, set confirmed=false.\n\n` +
  list.map((f, i) => `[${i}] (${f.severity}, ${f.axis}) ${f.title}, ${f.file || '?'}:${f.line || '?'}. ${f.scenario}`).join('\n'),
  { label: 'skeptic', phase: 'Verify', schema: V })
const judged = list.map((f, i) => {
  const r = ((v && v.results) || []).find(x => x.index === i)
  return { ...f, confirmed: r ? r.confirmed : true, reason: r ? r.reason : 'no verdict; kept' }
})
return { confirmed: judged.filter(f => f.confirmed), refuted: judged.filter(f => !f.confirmed), failedAxes: failed }
