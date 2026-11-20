// Classifies a run of the http-cache-tests suite with the suite's own rules
// (test-engine/lib/results.mjs, determineTestResult), so tests/conformance.rs
// judges each test exactly as https://cache-tests.fyi/ would show it.
//
//   node classify.mjs SUITE_DIR RESULTS_JSON
//
// Prints a JSON array, one entry per test the suite's command line runs (every
// test but the browser-only ones): its id, suite, kind (required, optimal or
// check), result (a key of the suite's resultTypes: pass, fail,
// optional_fail, yes, no, setup_fail, harness_fail, dependency_fail, retry or
// untested) and the suite's message for anything but a pass. Each test is
// judged on its own (dependencies not honoured), so a test that fails only
// because another did shows its own result.
//
// Runs only in the conformance CI job, against the suite checked out at the
// commit pinned in .github/workflows/conformance.yml.
import fs from 'node:fs'
import path from 'node:path'
import process from 'node:process'
import { pathToFileURL } from 'node:url'

const [suiteDir, resultsFile] = process.argv.slice(2)
if (!suiteDir || !resultsFile) {
  console.error('usage: node classify.mjs SUITE_DIR RESULTS_JSON')
  process.exit(2)
}
const load = rel => import(pathToFileURL(path.join(path.resolve(suiteDir), rel)).href)
const { determineTestResult, parseResults, resultTypes } = await load('test-engine/lib/results.mjs')
const { default: suites } = await load('tests/index.mjs')

const { results } = parseResults(JSON.parse(fs.readFileSync(resultsFile, 'utf8')))
const names = new Map(Object.entries(resultTypes).map(([name, type]) => [type, name]))
const out = []
for (const suite of suites) {
  for (const test of suite.tests) {
    if (test.browser_only === true) continue
    const type = determineTestResult(suites, test.id, results, false)
    const name = names.get(type)
    if (name === undefined) throw new Error(`${test.id}: unknown result ${JSON.stringify(type)}`)
    const raw = results[test.id]
    out.push({
      id: test.id,
      suite: suite.id,
      kind: test.kind ?? 'required',
      result: name,
      detail: Array.isArray(raw) ? raw.map(String).join(': ') : ''
    })
  }
}
process.stdout.write(JSON.stringify(out, null, 1) + '\n')
