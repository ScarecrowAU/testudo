// Playwright reporter: streams test lifecycle to Testudo. Injected by Testudo via
// PW_TEST_REPORTER (appended after the user's configured reporters); users never
// import it.

import { stripVTControlCharacters as plain } from 'node:util';
import { connect } from './transport.mjs';

const STEP_CATEGORIES = new Set(['pw:api', 'test.step', 'expect']);

export default class TestudoPlaywrightReporter {
  constructor() {
    this.t = connect({ role: 'reporter', runner: 'playwright' });
    this.stepIds = new WeakMap();
    this.nextStep = 0;
  }

  printsToStdio() {
    return false;
  }

  onBegin(_config, suite) {
    this.t.send({ type: 'runBegin', totalTests: suite.allTests().length });
  }

  onTestBegin(test, result) {
    this.t.send({
      type: 'testBegin',
      testId: test.id,
      title: test.title,
      titlePath: test.titlePath().filter(Boolean),
      file: test.location?.file,
      line: test.location?.line,
      project: test.parent?.project()?.name || undefined,
      workerIndex: result.workerIndex,
    });
  }

  onStepBegin(test, _result, step) {
    if (!STEP_CATEGORIES.has(step.category)) return;
    const stepId = String(this.nextStep++);
    this.stepIds.set(step, stepId);
    this.t.send({ type: 'stepBegin', testId: test.id, stepId, title: step.title, category: step.category });
  }

  onStepEnd(test, _result, step) {
    const stepId = this.stepIds.get(step);
    if (stepId === undefined) return;
    this.t.send({
      type: 'stepEnd',
      testId: test.id,
      stepId,
      durationMs: step.duration,
      error: step.error?.message && plain(step.error.message),
    });
  }

  onTestEnd(test, result) {
    this.t.send({
      type: 'testEnd',
      testId: test.id,
      status: result.status,
      durationMs: result.duration,
      error: result.error?.message && plain(result.error.message),
    });
  }

  async onEnd(result) {
    this.t.send({ type: 'runEnd', status: result.status });
    await this.t.close();
  }
}
