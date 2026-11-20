export const meta = {
  name: 'implement-issue',
  description: 'Implement one Flint issue in its own worktree, run the gates, run the pr-review workflow, fix confirmed findings and re-review (at most 2 rounds)',
  whenToUse: 'One per issue, run in the background so the coordinator stays free. args: {issue: number, topic: string, notes?: string, rounds?: number}. Commits but does not push; the coordinator pushes, opens the PR and merges.',
  phases: [
    { title: 'Implement', detail: 'worktree, code, tests, gates, commit, PR body' },
    { title: 'Review', detail: 'pr-review workflow: two reviewers plus cross-check' },
    { title: 'Fix', detail: 'fix confirmed findings, then re-review the fix commit' },
  ],
}

const a = args || {}
if (!a.issue || !a.topic) throw new Error('implement-issue needs args.issue and args.topic (a branch and worktree slug)')
const wt = `../flint-${a.topic}`
const rounds = a.rounds || 2

const RESULT = {
  type: 'object',
  properties: {
    shas: { type: 'array', items: { type: 'string' } },
    gates: { type: 'string', enum: ['green', 'red'] },
    gateOutput: { type: 'string', description: 'the tail of any failing gate; empty when green' },
    prBodyPath: { type: 'string' },
    summary: { type: 'string' },
  },
  required: ['shas', 'gates', 'summary'],
}

phase('Implement')
const impl = await agent(
  `Implement issue #${a.issue} in Flint. Read it with \`gh issue view ${a.issue} -R Sealos/flint --comments\`, along with any issues and PRs it links.
Create the worktree: branch \`${a.topic}\`, path ${wt}, from origin/master. Follow the code style of the neighbouring modules, and add tests that name the UP-xx paths they cover.
${a.notes ? `Notes from the coordinator:\n${a.notes}\n` : ''}Run every gate listed in CLAUDE.md. Fix any failures until all gates are green, or stop and report red with the output.
Commit, write the PR body (with \`Closes #${a.issue}\`) to ${wt}-PR.md, and do not push.`,
  { label: `implement #${a.issue}`, phase: 'Implement', schema: RESULT })
if (!impl || impl.gates !== 'green') return { status: 'blocked', worktree: wt, impl }

phase('Review')
let review = await workflow('pr-review', { worktree: wt, context: `Implements #${a.issue}. ${impl.summary}` })
const history = [{ round: 0, verdict: review.verdict, confirmed: review.confirmed.length }]

phase('Fix')
let round = 0, lastFix = null
while (review.verdict === 'changes-needed' && round < rounds) {
  round++
  const list = review.confirmed.map((f, i) => `${i + 1}. (${f.severity}) ${f.title}, ${f.file || '?'}:${f.line || '?'}. ${f.scenario}`).join('\n')
  lastFix = await agent(
    `In the worktree ${wt} (branch ${a.topic}), fix these confirmed review findings for issue #${a.issue}. Add a test that fails without each fix wherever possible:\n${list}\nRun every gate in CLAUDE.md, commit one fix commit, and update ${wt}-PR.md if the behaviour changed. Do not push.`,
    { label: `fix round ${round}`, phase: 'Fix', schema: RESULT })
  if (!lastFix || lastFix.gates !== 'green') return { status: 'blocked', worktree: wt, impl, review, lastFix, history }
  review = await workflow('pr-review', {
    worktree: wt, commit: lastFix.shas[lastFix.shas.length - 1], fixedFindings: list,
    context: `Re-review round ${round} for #${a.issue}.`,
  })
  history.push({ round, verdict: review.verdict, confirmed: review.confirmed.length })
}
if (review.verdict === 'changes-needed') log(`Stopped after ${rounds} fix rounds with findings still open; the coordinator decides.`)

return {
  status: review.verdict === 'changes-needed' ? 'needs-decision' : 'ready-to-push',
  worktree: wt, branch: a.topic, prBodyPath: `${wt}-PR.md`,
  shas: [...impl.shas, ...((lastFix && lastFix.shas) || [])],
  openFindings: review.verdict === 'clean' ? [] : review.confirmed,
  reviewComment: review.comment, history,
}
