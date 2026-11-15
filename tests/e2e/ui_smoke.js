// Browser smoke test of the web UI against a real `orbit server`.
//
// Started by the WebUI class in test_e2e.py, which runs the server, the
// example workers and the executions, and passes them in the environment:
//   ORBIT_URL, ORBIT_ADMIN_TOKEN, ORBIT_CLIENT_TOKEN, ORBIT_IDS (JSON)
// Uses the plain `playwright` library (Chromium).
'use strict';

const assert = require('assert');
const { chromium } = require('playwright');

const URL = process.env.ORBIT_URL;
const ADMIN = process.env.ORBIT_ADMIN_TOKEN;
const CLIENT = process.env.ORBIT_CLIENT_TOKEN;
const ids = JSON.parse(process.env.ORBIT_IDS);
const T = 15000;

function step(name) { console.log('ui: ' + name); }

async function signIn(page, token) {
  await page.goto(URL + '/ui/');
  await page.waitForSelector('[data-test=login]', { timeout: T });
  await page.fill('#token', token);
  await page.click('[data-test=login] button[type=submit]');
  await page.waitForSelector('#nav:not([hidden])', { timeout: T });
}

async function text(page, sel) { return (await page.textContent(sel, { timeout: T })) || ''; }

(async () => {
  const browser = await chromium.launch();
  const problems = [];
  const context = await browser.newContext({ viewport: { width: 1280, height: 900 }, deviceScaleFactor: 1 });
  const page = await context.newPage();
  page.on('pageerror', (e) => problems.push('page error: ' + e.message));
  page.on('console', (m) => { if (/Content.Security.Policy|Refused to/i.test(m.text())) problems.push('CSP: ' + m.text()); });
  try {
    step('/ redirects a browser to the UI; the login page loads without a token');
    const resp = await page.goto(URL + '/');
    assert.ok(resp.url().endsWith('/ui/'), 'redirected to /ui/: ' + resp.url());
    await page.waitForSelector('[data-test=login]', { timeout: T });

    step('a wrong token is rejected');
    await page.fill('#token', 'not-a-token');
    await page.click('[data-test=login] button[type=submit]');
    await page.waitForSelector('text=Token rejected', { timeout: T });

    step('admin sign-in; overview shows health and counts');
    await signIn(page, ADMIN);
    await page.waitForSelector('[data-test=health]', { timeout: T });
    assert.match(await text(page, '[data-test=health]'), /Rolesingle/);
    assert.match(await text(page, '#who'), /Admin token/);
    await page.waitForSelector('[data-test=counts] [data-count=RUNNING]', { timeout: T });
    const stored = await page.evaluate(() => [sessionStorage.getItem('orbit.ui.token'), localStorage.length]);
    assert.deepStrictEqual(stored, [ADMIN, 0], 'token in sessionStorage only');

    step('executions list, filtered by status');
    await page.click('#nav a[data-nav=executions]');
    await page.waitForSelector(`tr[data-execution="${ids.big}"]`, { timeout: T });
    await page.waitForSelector(`tr[data-execution="${ids.small}"]`, { timeout: T });
    await page.selectOption('[data-test=filter-status]', 'RUNNING');
    await page.waitForSelector(`tr[data-execution="${ids.small}"]`, { state: 'detached', timeout: T });
    await page.waitForSelector(`tr[data-execution="${ids.big}"]`, { timeout: T });
    assert.ok(page.url().includes('status=RUNNING'), 'filter kept in the URL');

    step('execution detail: steps, graph, history');
    await page.click(`tr[data-execution="${ids.big}"] a`);
    await page.waitForSelector('[data-test=graph]', { timeout: T });
    await page.waitForSelector('[data-test=graph] g.node[data-step=decision][data-status=WAITING]', { timeout: T });
    assert.strictEqual(await page.getAttribute('[data-test=graph] g.node[data-step=assess]:not([data-comp])', 'data-status'), 'SUCCEEDED');
    assert.strictEqual(await page.locator('[data-test=graph] g.node').count(), 5, 'four steps and one compensation');
    assert.strictEqual(await page.locator('[data-test=graph] g.node[data-comp=true][data-step=charge]').count(), 1);
    assert.ok(await page.locator('[data-test=graph] path.edge').count() >= 4, 'dependency edges drawn');
    await page.waitForSelector('[data-test=steps] tr[data-unit=decision] [data-status=WAITING]', { timeout: T });
    await page.waitForSelector('[data-test=history] details[data-seq="1"]', { timeout: T });
    await page.click('[data-test=history] details[data-seq="1"] summary');
    assert.match(await text(page, '[data-test=history] details[data-seq="1"] pre'), /"WorkflowCreated"/);

    step('run the status query');
    await page.click('[data-query=status]');
    await page.waitForSelector('[data-query-result=status] pre', { timeout: T });
    const q = JSON.parse(await text(page, '[data-query-result=status] pre'));
    assert.deepStrictEqual(q, { needs_approval: true, approved: null, reviewer: null, charged: false });
    assert.match(await text(page, '[data-query-result=status] .type'), /needs_approval: Bool\?/);

    step('an invalid signal is refused by the server');
    await page.click('form[data-signal=review] button[type=submit]');
    await page.waitForSelector('form[data-signal=review] .notice.error', { timeout: T });
    assert.match(await text(page, 'form[data-signal=review] .notice.error'), /SIGNAL_PAYLOAD_INVALID/);

    step('send the review signal through the form; the execution completes');
    await page.selectOption('form[data-signal=review] select[name=approved]', 'true');
    await page.fill('form[data-signal=review] input[name=reviewer]', 'dana');
    await page.fill('form[data-signal=review] input[name=comment]', 'looks fine');
    await page.click('form[data-signal=review] button[type=submit]');
    await page.waitForSelector('form[data-signal=review] >> text=Sent.', { timeout: T });
    await page.waitForSelector('.head [data-status=COMPLETED]', { timeout: 30000 });
    assert.match(await text(page, '[data-test=output] pre'), /"transaction_id": "tx-UI-1"/);
    await page.waitForSelector('[data-test=signals-received] >> text=review', { timeout: T });
    await page.waitForSelector('[data-test=history] >> text=SignalDelivered', { timeout: T });
    await page.waitForSelector('[data-test=graph] g.node[data-step=charge]:not([data-comp])[data-status=SUCCEEDED]', { timeout: T });
    assert.ok(await page.isDisabled('[data-test=cancel]'), 'cancel disabled once finished');

    step('cancel another execution, with confirmation');
    await page.goto(URL + '/ui/#/executions/' + encodeURIComponent(ids.cancel));
    await page.waitForSelector('[data-test=cancel]:not([disabled])', { timeout: T });
    await page.click('[data-test=cancel]');
    await page.fill('[data-test=cancel-reason]', 'smoke test');
    await page.click('[data-test=cancel-confirm]');
    await page.waitForSelector('.head [data-status=CANCELLED]', { timeout: 30000 });
    await page.waitForSelector('[data-test=history] >> text=CancelRequested', { timeout: T });

    step('workflow definition with its graph; workers');
    await page.goto(URL + '/ui/#/workflows');
    await page.click('tr[data-workflow=OrderApproval] a');
    await page.waitForSelector('[data-test=graph] g.node[data-step=decision]', { timeout: T });
    assert.match(await page.textContent('[data-view=workflows]'), /review/);
    await page.goto(URL + '/ui/#/workers');
    await page.waitForSelector('tr[data-worker=orders-python]', { timeout: T });

    step('an execution that continued as new: current run, run selector, continue_as_new node');
    await page.goto(URL + '/ui/#/executions/' + encodeURIComponent(ids.count));
    await page.waitForSelector('.head [data-status=COMPLETED]', { timeout: T });
    assert.strictEqual((await text(page, '.head [data-test=run]')).trim(), 'run 3');
    assert.deepStrictEqual(await page.$$eval('[data-test=run-select] option', (os) => os.map((o) => o.textContent)), ['Run 3 (current)', 'Run 2', 'Run 1']);
    assert.match(await text(page, '[data-view=executions] dl.kv'), /Run3 \(continue-as-new; previous run\)/);
    // The run completed: its continue_as_new was not taken.
    await page.waitForSelector('[data-test=graph] g.node[data-continue=true][data-status=SKIPPED]', { timeout: T });
    assert.match(await page.textContent('[data-test=graph] g.node[data-continue=true]'), /continue as newwhen tick\.left > 0/);
    assert.strictEqual(await page.locator('[data-test=graph] path[data-edge=next-run]').count(), 1, 'one edge back to the first step');
    assert.match(await text(page, '[data-test=history] h2'), /History of run 3 \(6 events\)/);

    step('an earlier run, from the run selector: CONTINUED_AS_NEW, its input and the next one');
    await page.selectOption('[data-test=run-select]', '1');
    await page.waitForSelector('.head [data-status=CONTINUED_AS_NEW]', { timeout: T });
    assert.ok(page.url().endsWith('?run=1'), 'run kept in the URL: ' + page.url());
    assert.strictEqual((await text(page, '.head [data-test=run]')).trim(), 'run 1 of 3');
    assert.strictEqual(await page.$eval('.head [data-status=CONTINUED_AS_NEW]', (e) => e.textContent), 'CONTINUED AS NEW');
    const contColour = await page.$eval('.head [data-status=CONTINUED_AS_NEW]', (e) => getComputedStyle(e).color);
    assert.deepStrictEqual(JSON.parse(await text(page, '[data-test=next-input] pre')), { n: 2 });
    await page.waitForSelector('[data-test=graph] g.node[data-continue=true][data-status=CONTINUED_AS_NEW]', { timeout: T });
    await page.waitForSelector('[data-test=graph] g.node[data-step=tick][data-status=SUCCEEDED]', { timeout: T });
    // A sleep's TimerFired finishes its step in the rebuilt run too.
    await page.waitForSelector('[data-test=graph] g.node[data-step=rest][data-status=SUCCEEDED]', { timeout: T });
    await page.waitForSelector('[data-test=steps] tr[data-unit=rest] [data-status=SUCCEEDED]', { timeout: T });
    const nodeColour = await page.$eval('[data-test=graph] g.node[data-continue=true] rect', (e) => getComputedStyle(e).stroke);
    const doneColour = await page.$eval('[data-test=graph] g.node[data-step=tick] rect', (e) => getComputedStyle(e).stroke);
    assert.strictEqual(contColour, nodeColour, 'the badge and the graph node share the continued-as-new colour');
    assert.notStrictEqual(contColour, doneColour, 'continued as new has its own colour');
    await page.waitForSelector('[data-test=history] .ev-cont >> text=WorkflowContinuedAsNew', { timeout: T });
    assert.strictEqual(await page.locator('[data-test=actions]').count(), 0, 'an earlier run is read-only');
    await page.click('[data-test=run-summary] a >> text=run 2');
    await page.waitForSelector('.head [data-test=run] >> text=run 2 of 3', { timeout: T });
    await page.selectOption('[data-test=run-select]', '3');
    await page.waitForSelector('.head [data-test=run] >> text=/^run 3$/', { timeout: T });
    assert.ok(!page.url().includes('run='), 'the current run has no run parameter');
    await page.goto(URL + '/ui/#/workflows/Countdown');
    await page.waitForSelector('[data-test=continue-as-new]', { timeout: T });
    assert.match(await text(page, '[data-test=continue-as-new]'), /tick\.left > 0/);
    assert.match(await text(page, '[data-test=continue-as-new] table'), /ntick\.left/);

    step('schedules: the schedule created through the API is listed');
    await page.click('#nav a[data-nav=schedules]');
    await page.waitForSelector('tr[data-schedule=ui-orders] [data-status=ACTIVE]', { timeout: T });
    const row = await text(page, 'tr[data-schedule=ui-orders]');
    assert.match(row, /OrderProcessing latest/);
    assert.match(row, /cron "0 3 \* \* \*" UTC/);
    await page.click('tr[data-schedule=ui-orders] a >> text=ui-orders');
    await page.waitForSelector('[data-test=schedule-state]', { timeout: T });
    assert.match(await text(page, '[data-test=schedule-state]'), /Fired0/);

    step('pause, resume and trigger it through the UI');
    await page.click('[data-test=schedule-pause]');
    await page.waitForSelector('.head [data-status=PAUSED]', { timeout: T });
    await page.waitForSelector('[data-test=schedule-result] >> text=Paused.', { timeout: T });
    assert.match(await text(page, '[data-test=schedule-state]'), /Next fire—/);
    await page.click('[data-test=schedule-resume]');
    await page.waitForSelector('.head [data-status=ACTIVE]', { timeout: T });
    await page.waitForSelector('[data-test=schedule-result] >> text=Resumed.', { timeout: T });
    await page.click('[data-test=schedule-trigger]');
    await page.waitForSelector('[data-test=schedule-result] >> text=Started.', { timeout: T });
    const started = (await text(page, '[data-test=schedule-result] a')).trim();
    assert.match(started, /^ui-orders:manual:/);
    await page.waitForSelector(`[data-test=schedule-executions] tr[data-execution="${started}"]`, { timeout: T });
    await page.waitForSelector('[data-test=schedule-state] >> text=Fired1', { timeout: T });

    step('the triggered execution links back to its schedule');
    await page.click('[data-test=schedule-result] a');
    await page.waitForSelector('.head [data-status=COMPLETED]', { timeout: 30000 });
    assert.strictEqual((await text(page, '.head [data-test=schedule-link]')).trim(), 'schedule ui-orders');
    await page.click('.head [data-test=schedule-link]');
    await page.waitForSelector(`[data-test=schedule-executions] tr[data-execution="${started}"] [data-status=COMPLETED]`, { timeout: T });

    step('create a schedule through the form (the server validates it), then delete it');
    await page.goto(URL + '/ui/#/schedules');
    await page.click('[data-test=schedule-new]');
    await page.waitForSelector('[data-test=schedule-form]', { timeout: T });
    await page.fill('#ns-id', 'ui-form');
    await page.selectOption('#ns-workflow', 'OrderProcessing');
    await page.fill('#ns-timing', '0 25 * * *');
    await page.fill('#ns-input', JSON.stringify({ order_id: 'UI-F', customer_id: 'c', amount: 1.0, items: [{ sku: 'BOOK-1', quantity: 1 }] }));
    await page.click('[data-test=schedule-form] button[type=submit]');
    await page.waitForSelector('[data-test=schedule-form] .notice.error', { timeout: T });
    assert.match(await text(page, '[data-test=schedule-form] .notice.error'), /INVALID_SCHEDULE.*out of range for the hour field/);
    await page.fill('#ns-timing', '0 4 * * *');
    await page.fill('#ns-tz', 'Europe/Berlin');
    await page.check('#ns-paused');
    await page.click('[data-test=schedule-form] button[type=submit]');
    await page.waitForSelector('.head [data-status=PAUSED]', { timeout: T });
    assert.ok(page.url().endsWith('#/schedules/ui-form'), page.url());
    assert.match(await text(page, '[data-test=schedule-spec]'), /cron "0 4 \* \* \*" Europe\/Berlin/);
    await page.click('[data-test=schedule-delete]');
    await page.click('[data-test=schedule-delete-confirm]');
    await page.waitForSelector('text=Deleted.', { timeout: T });
    await page.goto(URL + '/ui/#/schedules');
    await page.waitForSelector('tr[data-schedule=ui-orders]', { timeout: T });
    assert.strictEqual(await page.locator('tr[data-schedule=ui-form]').count(), 0);
    await page.goto(URL + '/ui/#/');
    await page.waitForSelector('[data-test=schedules-summary] .stat', { timeout: T });
    assert.match(await text(page, '[data-test=schedules-summary]'), /1active0paused/);

    step('narrow screen: no horizontal page scroll');
    await page.setViewportSize({ width: 360, height: 800 });
    await page.goto(URL + '/ui/#/executions/' + encodeURIComponent(ids.big));
    await page.waitForSelector('[data-test=graph]', { timeout: T });
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
    assert.ok(overflow <= 0, 'page overflows by ' + overflow + 'px at 360px');
    for (const hash of ['#/schedules', '#/schedules/ui-orders', '#/executions/' + encodeURIComponent(ids.count) + '?run=1']) {
      await page.goto(URL + '/ui/' + hash);
      await page.waitForSelector('[data-view] .head', { timeout: T });
      const over = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
      assert.ok(over <= 0, hash + ' overflows by ' + over + 'px at 360px');
    }
    await page.setViewportSize({ width: 1280, height: 900 });

    step('sign out and sign in as a client identity: restricted view');
    await page.click('#signout');
    await page.waitForSelector('[data-test=login]', { timeout: T });
    assert.strictEqual(await page.evaluate(() => sessionStorage.getItem('orbit.ui.token')), null);
    await signIn(page, CLIENT);
    assert.match(await text(page, '#who'), /Client billing \(restricted\)/);
    await page.goto(URL + '/ui/#/executions');
    await page.waitForSelector(`tr[data-execution="${ids.big}"]`, { timeout: T });
    assert.strictEqual(await page.locator(`tr[data-execution="${ids.other}"]`).count(), 0, 'other workflows are hidden');
    await page.goto(URL + '/ui/#/workers');
    await page.waitForSelector('.notice.denied', { timeout: T });
    assert.match(await text(page, '.notice.denied'), /Not permitted/);
    await page.goto(URL + '/ui/#/executions/' + encodeURIComponent(ids.other));
    await page.waitForSelector('.notice.denied', { timeout: T });
    await page.goto(URL + '/ui/#/executions/' + encodeURIComponent(ids.pending));
    await page.waitForSelector('[data-test=cancel]:not([disabled])', { timeout: T });
    await page.click('[data-test=cancel]');
    await page.click('[data-test=cancel-confirm]');
    await page.waitForSelector('[data-test=actions] .notice.denied', { timeout: T });
    assert.match(await text(page, '[data-test=actions] .notice.denied'), /may not cancel/);
    await page.click('[data-query=status]');
    await page.waitForSelector('[data-query-result=status] pre', { timeout: T });

    step('the client may not manage schedules: none listed, and a schedule page is refused');
    await page.goto(URL + '/ui/#/schedules');
    await page.waitForSelector('[data-view=schedules] td.empty', { timeout: T });
    assert.match(await text(page, '[data-view=schedules] td.empty'), /No schedules/);
    await page.goto(URL + '/ui/#/schedules/' + ids.schedule);
    await page.waitForSelector('.notice.denied', { timeout: T });
    assert.match(await text(page, '.notice.denied'), /may not manage schedules of `OrderProcessing`/);

    assert.deepStrictEqual(problems, [], 'no page errors or CSP violations');
    console.log('ui: ok');
  } catch (e) {
    try { await page.screenshot({ path: process.env.ORBIT_UI_FAILURE_SHOT || 'ui-failure.png', fullPage: true }); } catch (_) { /* ignore */ }
    console.error(e && e.stack ? e.stack : e);
    if (problems.length) console.error('problems:', problems);
    process.exitCode = 1;
  } finally {
    await browser.close();
  }
})();
