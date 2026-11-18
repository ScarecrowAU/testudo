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
    const tests = suite.allTests();
    this.t.send({ type: 'runBegin', totalTests: tests.length });
    // Announce every test up front so the list can show them as pending.
    for (const test of tests) {
      this.t.send({
        type: 'testQueued',
        testId: test.id,
        title: test.title,
        titlePath: test.titlePath().filter(Boolean),
        file: test.location?.file,
        line: test.location?.line,
        project: test.parent?.project()?.name || undefined,
      });
    }
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

  // Output printed while a test runs (console.log in the test, API logs):
  // its tile in Testudo shows it. Output outside a test stays run-level.
  onStdOut(chunk, test) {
    this.output(chunk, test, 'stdout');
  }

  onStdErr(chunk, test) {
    this.output(chunk, test, 'stderr');
  }

  output(chunk, test, stream) {
    if (!test) return;
    const text = plain(typeof chunk === 'string' ? chunk : chunk.toString('utf8'));
    if (text) this.t.send({ type: 'testOutput', testId: test.id, stream, text });
  }

  onTestEnd(test, result) {
    // test.skip(condition, reason) records the reason as a 'skip' annotation.
    const skip = [...(result.annotations ?? []), ...(test.annotations ?? [])].find(
      (a) => a.type === 'skip' && a.description,
    );
    this.t.send({
      type: 'testEnd',
      testId: test.id,
      status: result.status,
      durationMs: result.duration,
      error: result.error?.message && plain(result.error.message),
      note: result.status === 'skipped' && skip?.description ? plain(skip.description) : undefined,
    });
  }

  async onEnd(result) {
    this.t.send({ type: 'runEnd', status: result.status });
    await this.t.close();
  }
}
