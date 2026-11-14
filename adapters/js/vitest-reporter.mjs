// Vitest reporter (Vitest 3+ reporter API): streams test lifecycle to Testudo.
// Injected by Testudo with `--reporter=<path>`; covers Storybook's addon-vitest.

import { stripVTControlCharacters as plain } from 'node:util';
import { connect } from './transport.mjs';

const STATUS = { passed: 'passed', failed: 'failed', skipped: 'skipped', pending: 'skipped' };

export default class TestudoVitestReporter {
  constructor() {
    this.t = connect({ role: 'reporter', runner: 'vitest' });
  }

  onTestRunStart() {
    this.t.send({ type: 'runBegin' });
  }

  onTestCaseReady(testCase) {
    this.t.send({
      type: 'testBegin',
      testId: testCase.id,
      title: testCase.name,
      titlePath: testCase.fullName.split(' > '),
      file: testCase.module.moduleId,
      line: testCase.location?.line,
      project: testCase.project?.name || undefined,
    });
  }

  onTestCaseResult(testCase) {
    const result = testCase.result();
    this.t.send({
      type: 'testEnd',
      testId: testCase.id,
      status: STATUS[result.state] ?? 'unknown',
      durationMs: testCase.diagnostic()?.duration,
      error: result.errors?.[0]?.message && plain(result.errors[0].message),
    });
  }

  async onTestRunEnd(_modules, _errors, reason) {
    this.t.send({ type: 'runEnd', status: reason });
    // Flush, don't close: in watch mode another run follows on the same connection.
    await this.t.flush();
  }
}
