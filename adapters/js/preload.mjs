// Testudo preload. Loaded into every Node process of a Testudo-started run via
// NODE_OPTIONS=--import. Instruments playwright-core so every page launched by
// Playwright Test workers or Vitest's Playwright browser provider streams:
//   - JPEG screencast frames (page.screencast, CDP fallback on Chromium)
//   - navigations, network requests/responses, console messages
//
// This patches playwright-core's client classes through their prototypes, which is
// version-sensitive. Everything is feature-detected and wrapped so a failure here
// degrades to "no live view" and never breaks the user's test run.

import { createRequire } from 'node:module';
import path from 'node:path';
import { connect } from './transport.mjs';

const PROJECT_ROOT = process.env.TESTUDO_PROJECT_ROOT ?? process.cwd();
// Recording resolution from Testudo's settings: frames fit within an N×N box.
const FRAME_MAX = Math.min(4096, Math.max(160, Number.parseInt(process.env.TESTUDO_FRAME_MAX ?? '', 10) || 1024));
const FRAME_SIZE = { width: FRAME_MAX, height: FRAME_MAX };
const FRAME_QUALITY = 70;

const PATCHED = Symbol.for('testudo.patched');
const instrumentedContexts = new WeakSet();
const instrumentedPages = new WeakSet();

let transport;
const t = () => (transport ??= connect({ role: 'preload', runner: runnerName() }));

function runnerName() {
  if (process.env.TEST_WORKER_INDEX !== undefined) return 'playwright';
  if (process.env.VITEST !== undefined) return 'vitest';
  return 'node';
}

function log(level, message) {
  try {
    t().send({ type: 'adapterLog', level, message: String(message) });
  } catch {}
}

// --- locating playwright-core -------------------------------------------------
//
// The copy we patch must be the copy the runner loads. With pnpm/monorepos
// playwright-core is not hoisted, so walk the dependency chains from the project
// root and patch every distinct copy found.

const ENTRY_CHAINS = [
  ['playwright-core'],
  ['@playwright/test', 'playwright', 'playwright-core'],
  ['playwright', 'playwright-core'],
  ['@vitest/browser-playwright', 'playwright', 'playwright-core'],
  ['@vitest/browser', 'playwright', 'playwright-core'],
  ['@storybook/addon-vitest', 'vitest', '@vitest/browser-playwright', 'playwright', 'playwright-core'],
];

function findPlaywrightCores() {
  const found = new Set();
  for (const chain of ENTRY_CHAINS) {
    let from = path.join(PROJECT_ROOT, 'package.json');
    try {
      for (const name of chain) {
        from = createRequire(from).resolve(`${name}/package.json`);
      }
      found.add(path.dirname(from));
    } catch {
      // chain not present in this project
    }
  }
  return [...found];
}

// --- patching -----------------------------------------------------------------

function wrapMethod(proto, name, after) {
  const orig = proto?.[name];
  if (typeof orig !== 'function' || orig[PATCHED]) return;
  const wrapped = async function (...args) {
    const result = await orig.apply(this, args);
    try {
      after(result, this);
    } catch (e) {
      log('warn', `instrumenting ${name} failed: ${e?.stack ?? e}`);
    }
    return result;
  };
  wrapped[PATCHED] = true;
  proto[name] = wrapped;
}

function patchCore(coreDir) {
  const req = createRequire(path.join(coreDir, 'package.json'));
  const pw = req(coreDir);

  // Preferred: the client instrumentation hook Playwright Test itself uses for
  // tracing. It fires for every context (launch, newContext, persistent, connect)
  // and keeps Testudo frames out of user-facing error stacks.
  const instrumentation = pw._instrumentation;
  if (typeof instrumentation?.addListener === 'function') {
    instrumentation.addListener({
      runAfterCreateBrowserContext: async (context) => {
        try {
          instrumentContext(context, browserNameOf(context));
        } catch (e) {
          log('warn', `instrumenting context failed: ${e?.stack ?? e}`);
        }
      },
    });
    return;
  }

  // Fallback for Playwright versions without client instrumentation. Note the
  // wrapper frames can show up in Playwright's error code frames.
  const browserTypeProto = Object.getPrototypeOf(pw.chromium);

  for (const m of ['launch', 'connect', 'connectOverCDP']) {
    wrapMethod(browserTypeProto, m, (browser, browserType) => instrumentBrowser(browser, browserType.name()));
  }
  wrapMethod(browserTypeProto, 'launchPersistentContext', (context, browserType) =>
    instrumentContext(context, browserType.name()),
  );
}

function instrumentBrowser(browser, browserName) {
  const proto = Object.getPrototypeOf(browser);
  // Playwright Test may reuse contexts through a private method; cover both.
  for (const m of ['newContext', '_newContextForReuse']) {
    wrapMethod(proto, m, (context) => instrumentContext(context, browserName));
  }
  for (const context of browser.contexts()) instrumentContext(context, browserName);
}

function browserNameOf(context) {
  try {
    return context.browser()?.browserType().name() ?? context._browserType?.name();
  } catch {
    return undefined;
  }
}

function instrumentContext(context, browserName) {
  if (!context || instrumentedContexts.has(context)) return;
  instrumentedContexts.add(context);
  context.on('page', (page) => instrumentPage(page, browserName));
  for (const page of context.pages()) instrumentPage(page, browserName);
}

// --- per-page streaming -------------------------------------------------------

let nextPage = 0;
let nextRequest = 0;

function currentTest() {
  if (process.env.TEST_WORKER_INDEX === undefined) return undefined;
  try {
    const req = createRequire(path.join(PROJECT_ROOT, 'package.json'));
    const info = req('@playwright/test').test.info();
    return { testId: info.testId, workerIndex: info.workerIndex };
  } catch {
    return undefined; // page opened outside a test (e.g. beforeAll, worker fixture)
  }
}

function instrumentPage(page, browserName) {
  if (instrumentedPages.has(page)) return;
  instrumentedPages.add(page);

  const tr = t();
  if (!tr.enabled) return;

  const pageId = `${process.pid}-${nextPage++}`;
  const test = currentTest();
  tr.send({
    type: 'pageOpened',
    pageId,
    testId: test?.testId,
    workerIndex: test?.workerIndex ?? numberOrUndefined(process.env.TEST_WORKER_INDEX),
    browser: browserName,
    url: page.url(),
  });

  const stopScreencast = startScreencast(page, pageId, browserName);

  page.on('framenavigated', (frame) => {
    if (frame === page.mainFrame()) tr.send({ type: 'navigated', pageId, url: frame.url() });
  });

  const requestIds = new WeakMap();
  page.on('request', (r) => {
    const requestId = String(nextRequest++);
    requestIds.set(r, requestId);
    tr.send({
      type: 'network',
      pageId,
      requestId,
      phase: 'request',
      ts: Date.now(),
      method: r.method(),
      url: r.url(),
      resourceType: r.resourceType(),
    });
  });
  page.on('response', (res) => {
    const requestId = requestIds.get(res.request());
    if (requestId === undefined) return;
    tr.send({ type: 'network', pageId, requestId, phase: 'response', ts: Date.now(), status: res.status() });
  });
  page.on('requestfailed', (r) => {
    const requestId = requestIds.get(r);
    if (requestId === undefined) return;
    tr.send({ type: 'network', pageId, requestId, phase: 'failed', ts: Date.now(), error: r.failure()?.errorText });
  });

  page.on('console', (msg) => {
    tr.send({ type: 'console', pageId, level: msg.type(), text: msg.text(), ts: Date.now() });
  });

  page.once('close', () => {
    stopScreencast();
    tr.send({ type: 'pageClosed', pageId });
  });
}

function startScreencast(page, pageId, browserName) {
  const tr = t();
  const sendFrame = (data, ts, viewportWidth, viewportHeight) =>
    tr.frame({ pageId, ts, viewportWidth, viewportHeight }, data);

  // Playwright >= 1.59ish: cross-browser screencast API.
  if (typeof page.screencast?.start === 'function') {
    page.screencast
      .start({
        size: FRAME_SIZE,
        quality: FRAME_QUALITY,
        onFrame: ({ data, timestamp, viewportWidth, viewportHeight }) =>
          sendFrame(data, timestamp ?? Date.now(), viewportWidth, viewportHeight),
      })
      .catch((e) => log('warn', `screencast.start failed: ${e?.message ?? e}`));
    return () => {
      page.screencast.stop?.().catch(() => {});
    };
  }

  // Older Playwright on Chromium: raw CDP screencast.
  if (browserName === 'chromium') {
    let session;
    page
      .context()
      .newCDPSession(page)
      .then(async (s) => {
        session = s;
        s.on('Page.screencastFrame', ({ data, sessionId, metadata }) => {
          s.send('Page.screencastFrameAck', { sessionId }).catch(() => {});
          sendFrame(
            Buffer.from(data, 'base64'),
            (metadata?.timestamp ?? Date.now() / 1000) * 1000,
            metadata?.deviceWidth,
            metadata?.deviceHeight,
          );
        });
        await s.send('Page.startScreencast', {
          format: 'jpeg',
          quality: FRAME_QUALITY,
          maxWidth: FRAME_SIZE.width,
          maxHeight: FRAME_SIZE.height,
        });
      })
      .catch((e) => log('warn', `CDP screencast failed: ${e?.message ?? e}`));
    return () => {
      session?.detach().catch(() => {});
    };
  }

  log('info', `no live screencast available for ${browserName} on this Playwright version`);
  return () => {};
}

function numberOrUndefined(s) {
  const n = Number(s);
  return Number.isFinite(n) ? n : undefined;
}

// --- entry --------------------------------------------------------------------

if (process.env.TESTUDO_SOCKET) {
  for (const coreDir of findPlaywrightCores()) {
    try {
      patchCore(coreDir);
    } catch (e) {
      log('warn', `failed to patch playwright-core at ${coreDir}: ${e?.stack ?? e}`);
    }
  }
}
