export const meta = {
  name: 'release-review',
  description: 'After a Flint release: usability, product-development and market-fit reviewers, then a critic merges their findings into one ranked list of proposed issues',
  whenToUse: 'Once per published release. args: {version: string (for example "0.3.0"), fileIssues?: boolean (default false)}.',
  phases: [
    { title: 'Review', detail: 'usability, product, market fit in parallel' },
    { title: 'Critique', detail: 'dedupe, challenge and rank the findings' },
  ],
}

const a = args || {}
if (!a.version) throw new Error('release-review needs args.version')
const v = a.version.replace(/^v/, '')
const out = `../flint-post-release/v${v}`
const intro = `Flint v${v} was just released (https://github.com/Sealos/flint/releases/tag/v${v}). Read README.md, CHANGELOG.md, docs/ARCHITECTURE.md and the roadmap (\`gh issue view 27 -R Sealos/flint\`). Write your report to ${out}/<your-file>.md (create the directory if needed). Do not edit the repo or write to GitHub. Each finding has: a title phrased like an issue, evidence, a recommended action and a priority (P0, P1 or P2). Give at most 10 findings. Reply with a 5-line summary.`

const REVIEWERS = [
  { key: 'usability', file: 'usability.md', prompt: `You are the usability tester.
Build the automation binary in a scratch target dir, and drive the critical user paths in docs/USER-PATHS.md against the local kind cluster through the automation channel, as a new user would: first launch, finding a pod, logs, shell, switching cluster, errors.
Take screenshots, shrink them before reading, and keep them in ${out}/shots/. Record friction, confusing wording, slow steps and dead ends.` },
  { key: 'product', file: 'product.md', prompt: `You are the product-development reviewer.
Compare what shipped with the roadmap and the milestone goals. Find gaps that block real daily use, features half-done across PRs, and what the next milestone should cut or add. Propose the next 5 issues in priority order.` },
  { key: 'market', file: 'market.md', prompt: `You are the market-fit reviewer.
Research the competitive landscape as of the release date with web search: k9s, Lens, OpenLens, Freelens, Headlamp, Aptakube, Seabird, Monokle, and the Kubernetes MCP servers. Cover pricing, licence, platform, native vs Electron, AI features and multi-cluster support. Cite URLs, and mark anything you couldn't verify. Search only product and market terms, never personal details.
Include a landscape table, Flint's sharpest wedge and a one-sentence pitch, the reasons someone would switch from k9s or Freelens, and the distribution and early-adopter channels.` },
]

phase('Review')
const reports = await parallel(REVIEWERS.map(r => () =>
  agent(`${r.prompt}\n\n${intro.replace('<your-file>.md', r.file)}`, { label: r.key, phase: 'Review' })))
const missing = REVIEWERS.filter((r, i) => !reports[i]).map(r => r.key)
if (missing.length) log(`No report from: ${missing.join(', ')}`)

phase('Critique')
const ISSUES = {
  type: 'object',
  properties: {
    issues: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          title: { type: 'string' }, priority: { type: 'string', enum: ['P0', 'P1', 'P2'] },
          body: { type: 'string' }, sources: { type: 'array', items: { type: 'string' } },
          duplicateOf: { type: 'integer', description: 'the existing issue number, if one already covers this' },
        },
        required: ['title', 'priority', 'body', 'sources'],
      },
    },
    summary: { type: 'string' },
  },
  required: ['issues', 'summary'],
}
const critique = await agent(
  `Critique the post-release reports in ${out}/ (usability.md, product.md and market.md). Challenge any claim without evidence, and check the claims against the code where that is cheap. Merge duplicates. Search the open issues (\`gh issue list -R Sealos/flint --state open --limit 200\`) and set duplicateOf when an issue already covers a finding.
Write ${out}/critique.md. Return a ranked list of proposed issues. Each body is ready to file, never calls the project owner "the user", and has no AI attribution.`,
  { label: 'critic', phase: 'Critique', schema: ISSUES })

let filed = []
if (a.fileIssues && critique) {
  const fresh = critique.issues.filter(i => !i.duplicateOf && i.priority !== 'P2')
  filed = await agent(
    `File these issues in Sealos/flint with \`gh issue create\`, using the label "from-release-review" (create it if missing), and return the issue URLs as a JSON array:\n${JSON.stringify(fresh, null, 1)}`,
    { label: 'file issues', phase: 'Critique' })
}
return { dir: out, missing, critique, filed }
