// ORBIT web UI. Plain JavaScript, no dependencies, no build step.
//
// Every piece of data shown here comes from the HTTP API (/v1/*), called with
// the token the user entered; the API decides what each credential may see.
// All text is inserted as text nodes; no markup is ever built from strings.
'use strict';

(() => {
  const TOKEN_KEY = 'orbit.ui.token';
  const MODE_KEY = 'orbit.ui.mode';
  const WHO_KEY = 'orbit.ui.who';
  const AUTO_KEY = 'orbit.ui.autorefresh';
  const PAGE_SIZE = 25;
  const LIST_MAX = 1000; // the API returns at most 1000 executions per request
  const STATUSES = ['RUNNING', 'COMPENSATING', 'COMPLETED', 'FAILED', 'CANCELLED', 'TIMED_OUT'];
  const TERMINAL = new Set(['COMPLETED', 'FAILED', 'CANCELLED', 'TIMED_OUT']);
  // The status of a run that ended with `continue_as_new`. The API never
  // lists a current execution with it (the execution goes on in its next
  // run); it is the status of every earlier run.
  const CONTINUED = 'CONTINUED_AS_NEW';
  // The UI is served at <base>/ui/; the API lives at <base>/v1/.
  const BASE = location.pathname.replace(/\/ui(\/.*)?$/, '');

  // ---------------------------------------------------------------- storage

  const session = {
    get(k) { try { return sessionStorage.getItem(k); } catch (_) { return null; } },
    set(k, v) { try { sessionStorage.setItem(k, v); } catch (_) { /* unavailable */ } },
    del(k) { try { sessionStorage.removeItem(k); } catch (_) { /* unavailable */ } },
  };

  // ------------------------------------------------------------------- API

  class ApiError extends Error {
    constructor(status, code, message) {
      super(message);
      this.status = status;
      this.code = code;
    }
  }

  async function api(path, opts = {}) {
    const headers = { accept: 'application/json' };
    const token = opts.token !== undefined ? opts.token : session.get(TOKEN_KEY);
    if (token) headers.authorization = 'Bearer ' + token;
    const init = { method: opts.method || 'GET', headers, cache: 'no-store', credentials: 'omit', referrerPolicy: 'no-referrer' };
    if (opts.body !== undefined) {
      headers['content-type'] = 'application/json';
      init.body = JSON.stringify(opts.body);
    }
    let resp;
    try {
      resp = await fetch(BASE + path, init);
    } catch (e) {
      throw new ApiError(0, 'NETWORK', 'cannot reach the server (' + e.message + ')');
    }
    const text = await resp.text();
    let data = null;
    if (text) {
      try { data = JSON.parse(text); } catch (_) { data = text; }
    }
    if (!resp.ok) {
      const err = (data && typeof data === 'object' && data.error) || {};
      const message = err.message || (typeof data === 'string' && data) || resp.statusText || 'request failed';
      throw new ApiError(resp.status, err.code || 'HTTP_' + resp.status, message);
    }
    return data;
  }

  const enc = encodeURIComponent;

  // ------------------------------------------------------------------- DOM

  function append(el, ...kids) {
    for (const k of [kids].flat(Infinity)) {
      if (k === null || k === undefined || k === false) continue;
      el.append(k instanceof Node ? k : document.createTextNode(String(k)));
    }
    return el;
  }

  function setAttrs(el, attrs) {
    let value;
    for (const [k, v] of Object.entries(attrs || {})) {
      if (v === null || v === undefined || v === false) continue;
      if (k === 'class') el.setAttribute('class', v);
      else if (k === 'value') value = v;
      else if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2), v);
      else el.setAttribute(k, v === true ? '' : String(v));
    }
    return value;
  }

  function h(tag, attrs, ...kids) {
    const el = document.createElement(tag);
    const value = setAttrs(el, attrs);
    append(el, kids);
    if (value !== undefined) el.value = value;
    return el;
  }

  const SVG = 'http://www.w3.org/2000/svg';
  function s(tag, attrs, ...kids) {
    const el = document.createElementNS(SVG, tag);
    setAttrs(el, attrs);
    return append(el, kids);
  }

  function replace(el, ...kids) {
    el.replaceChildren();
    return append(el, kids);
  }

  const link = (href, ...kids) => h('a', { href }, ...kids);
  const execHref = (id, run) => '#/executions/' + enc(id) + (run ? '?run=' + run : '');
  const schedHref = (id) => '#/schedules/' + enc(id);
  const wfHref = (name, version) => '#/workflows/' + enc(name) + (version ? '/' + enc(version) : '');

  // ---------------------------------------------------------- formatting

  const pad = (n, w = 2) => String(n).padStart(w, '0');

  function fmtTime(ms) {
    if (ms === null || ms === undefined) return '—';
    const d = new Date(ms);
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
  }

  function fmtDur(ms) {
    if (ms === null || ms === undefined || !Number.isFinite(ms) || ms < 0) return '—';
    if (ms < 1000) return Math.round(ms) + 'ms';
    const sec = ms / 1000;
    if (sec < 60) return (sec < 10 ? sec.toFixed(2) : sec.toFixed(1)) + 's';
    const m = Math.floor(sec / 60);
    if (m < 60) return `${m}m ${pad(Math.floor(sec % 60))}s`;
    const hrs = Math.floor(m / 60);
    if (hrs < 48) return `${hrs}h ${pad(m % 60)}m`;
    return `${Math.floor(hrs / 24)}d ${hrs % 24}h`;
  }

  function ago(ms) {
    if (ms === null || ms === undefined) return '—';
    const d = Date.now() - ms;
    if (d < 0) return 'in ' + fmtDur(-d);
    if (d < 1000) return 'just now';
    return fmtDur(d).replace(/\.\d+s$/, 's') + ' ago';
  }

  function timeCell(ms) {
    return h('td', { class: 'nowrap', title: ms ? new Date(ms).toISOString() : null }, fmtTime(ms));
  }

  /** An RFC 3339 time from the API (schedules) as epoch milliseconds, or null. */
  function isoMs(s) {
    if (!s) return null;
    const t = Date.parse(s);
    return Number.isFinite(t) ? t : null;
  }

  function json(v) {
    return h('pre', { class: 'json' }, v === undefined ? '—' : JSON.stringify(v, null, 2));
  }

  function typeStr(t) {
    if (!t || typeof t !== 'object') return '?';
    switch (t.kind) {
      case 'string': return 'String';
      case 'int': return 'Int';
      case 'float': return 'Float';
      case 'bool': return 'Bool';
      case 'bytes': return 'Bytes';
      case 'duration': return 'Duration';
      case 'timestamp': return 'Timestamp';
      case 'uuid': return 'UUID';
      case 'json': return 'JSON';
      case 'null': return 'null';
      case 'list': return `List<${typeStr(t.item)}>`;
      case 'map': return `Map<${typeStr(t.key)}, ${typeStr(t.value)}>`;
      case 'optional': return typeStr(t.inner) + '?';
      case 'record': {
        const body = '{ ' + (t.fields || []).map((f) => f.name + ': ' + typeStr(f.type) +
          (f.default !== undefined ? ' = ' + JSON.stringify(f.default) : '')).join(', ') + ' }';
        return t.name ? t.name + ' ' + body : body;
      }
      default: return String(t.kind);
    }
  }

  // ORBIT source text of an IR expression (as `orbit graph` and `orbit docs`
  // print it), for `continue_as_new` conditions and inputs.
  const BINOPS = {
    or: ['||', 1], and: ['&&', 2], eq: ['==', 3], ne: ['!=', 3], lt: ['<', 4], le: ['<=', 4], gt: ['>', 4], ge: ['>=', 4],
    add: ['+', 5], sub: ['-', 5], mul: ['*', 6], div: ['/', 6], rem: ['%', 6],
  };

  function exprPrec(e) {
    if (!e || typeof e !== 'object') return 8;
    if (e.op === 'binary') return (BINOPS[e.operator] || ['', 5])[1];
    if (e.op === 'if' || e.op === 'lambda') return 0;
    if (e.op === 'unary') return 7;
    return 8;
  }

  function literalStr(v) {
    if (v === null || v === undefined) return 'null';
    if (Array.isArray(v)) return '[' + v.map(literalStr).join(', ') + ']';
    if (typeof v === 'object') {
      const keys = Object.keys(v);
      return keys.length ? '{ ' + keys.map((k) => k + ': ' + literalStr(v[k])).join(', ') + ' }' : '{}';
    }
    return typeof v === 'string' ? JSON.stringify(v) : String(v);
  }

  function exprStr(e) {
    if (!e || typeof e !== 'object') return '';
    const path = (root, p) => [root].concat(p || []).join('.');
    const operand = (x, min) => (exprPrec(x) < min ? '(' + exprStr(x) + ')' : exprStr(x));
    switch (e.op) {
      case 'literal': return literalStr(e.value);
      case 'input': return path('input', e.path);
      case 'step': return path(e.step, e.path);
      case 'execution': return 'execution.' + e.field;
      case 'local': return path(e.name, e.path);
      case 'index': return operand(e.target, 8) + '[' + exprStr(e.index) + ']';
      case 'object': return (e.fields || []).length ? '{ ' + e.fields.map((f) => f.name + ': ' + exprStr(f.value)).join(', ') + ' }' : '{}';
      case 'list': return '[' + (e.items || []).map(exprStr).join(', ') + ']';
      case 'binary': {
        const [sym, p] = BINOPS[e.operator] || [e.operator, 5];
        return operand(e.left, p) + ' ' + sym + ' ' + operand(e.right, p + 1);
      }
      case 'unary': return (e.operator === 'not' ? '!' : '-') + operand(e.operand, 7);
      case 'if': return 'if ' + exprStr(e.condition) + ' then ' + exprStr(e.then) + ' else ' + exprStr(e.otherwise);
      case 'call': return e.function + '(' + (e.args || []).map(exprStr).join(', ') + ')';
      case 'lambda': return e.param + ' => ' + exprStr(e.body);
      default: return '…';
    }
  }

  /** `when <condition>`, or `always` for a `continue_as_new` without `when`. */
  function continueWhen(can) { return can.condition ? 'when ' + exprStr(can.condition) : 'always'; }

  const WF_CLASS = { RUNNING: 's-run', COMPENSATING: 's-warn', COMPLETED: 's-ok', FAILED: 's-bad', CANCELLED: 's-idle', TIMED_OUT: 's-bad', CONTINUED_AS_NEW: 's-cont' };
  const SCHED_CLASS = { ACTIVE: 's-ok', PAUSED: 's-warn', ENDED: 's-idle' };
  const UNIT_CLASS = {
    PENDING: 's-idle', RUNNING: 's-run', SCHEDULED: 's-run', DISPATCHED: 's-run', ATTEMPT_FAILED: 's-warn',
    UNCERTAIN: 's-warn', RETRY_WAIT: 's-warn', WAITING: 's-run', SUCCEEDED: 's-ok', SKIPPED: 's-idle',
    FAILED: 's-bad', CANCELLED: 's-idle',
  };

  function badge(status, map = WF_CLASS) {
    return h('span', { class: 'badge ' + (map[status] || 's-idle'), 'data-status': status || '' }, (status || 'not started').replace(/_/g, ' '));
  }

  function durationOf(ex) {
    const end = ex.finished_at_ms || (TERMINAL.has(ex.status) ? ex.updated_at_ms : Date.now());
    return end - ex.created_at_ms;
  }

  // ------------------------------------------------------------ messages

  function notice(kind, title, text) {
    return h('div', { class: 'notice ' + kind, role: kind === 'error' ? 'alert' : 'status' },
      title ? h('strong', null, title + ' ') : null, text);
  }

  function errorNotice(e) {
    if (e instanceof ApiError && e.status === 403) {
      return notice('denied', 'Not permitted.', e.message + '. The API refused this request for the signed-in credential.');
    }
    if (e instanceof ApiError && e.status === 404) return notice('error', 'Not found.', e.message);
    if (e instanceof ApiError) return notice('error', `${e.code} (${e.status || 'network'}).`, e.message);
    return notice('error', 'Error.', String(e && e.message ? e.message : e));
  }

  // --------------------------------------------------------------- views

  let view = { gen: 0, timers: [] };
  const main = () => document.getElementById('main');

  function every(ms, fn) {
    const t = setInterval(fn, ms);
    view.timers.push(t);
    return t;
  }

  function autoRefresh() { return session.get(AUTO_KEY) !== 'off'; }

  function autoToggle(onChange) {
    const box = h('input', { type: 'checkbox', 'data-test': 'autorefresh' });
    box.checked = autoRefresh();
    box.addEventListener('change', () => { session.set(AUTO_KEY, box.checked ? 'on' : 'off'); if (onChange) onChange(box.checked); });
    return h('label', { class: 'check' }, box, 'Auto-refresh');
  }

  function header(title, sub, ...right) {
    return h('div', { class: 'head' }, h('div', null, h('h1', null, title), sub ? h('div', { class: 'sub' }, sub) : null),
      right.length ? h('div', { class: 'row' }, right) : null);
  }

  function table(columns, rows, empty = 'Nothing to show.') {
    const head = h('tr', null, columns.map((c) => h('th', { class: c.cls || null }, c.label)));
    const body = rows.length ? rows : [h('tr', null, h('td', { class: 'empty', colspan: columns.length }, empty))];
    return h('div', { class: 'table-wrap' }, h('table', null, h('thead', null, head), h('tbody', null, body)));
  }

  // ---------------------------------------------------------------- login

  function viewLogin(message) {
    setChrome(false);
    const input = h('input', { type: 'password', id: 'token', name: 'token', autocomplete: 'off', spellcheck: 'false', placeholder: 'Bearer token', required: true });
    const status = h('div');
    if (message) append(status, notice('error', null, message));
    const form = h('form', { class: 'panel login', 'data-test': 'login' },
      h('h1', null, 'Sign in'),
      h('p', null, 'Enter an admin token or a client identity token for this server. ' +
        'It is kept in this browser tab’s session storage only, and sent as a bearer token with each API request.'),
      h('label', { for: 'token' }, 'Token'), input,
      status,
      h('button', { type: 'submit', class: 'btn btn-primary' }, 'Sign in'));
    form.addEventListener('submit', async (ev) => {
      ev.preventDefault();
      replace(status);
      const token = input.value.trim();
      if (!token) return;
      try {
        await signIn(token);
        if (!location.hash || location.hash === '#/login') location.hash = '#/';
        else route();
      } catch (e) {
        replace(status, e instanceof ApiError && e.status === 401
          ? notice('error', 'Token rejected.', 'The server did not accept this token.')
          : errorNotice(e));
      }
    });
    replace(main(), form);
    input.focus();
  }

  /** Establish what kind of credential `token` is; throws if it cannot read executions. */
  async function probe(token) {
    await api('/v1/executions?limit=1', { token });
    try {
      await api('/v1/workers', { token });
      return { mode: token ? 'admin' : 'open', who: token ? 'Admin token' : 'No authentication configured' };
    } catch (e) {
      if (e instanceof ApiError && e.status === 403) {
        const m = /client `([^`]+)`/.exec(e.message);
        return { mode: 'client', who: 'Client ' + (m ? m[1] : 'identity') + ' (restricted)' };
      }
      throw e;
    }
  }

  async function signIn(token) {
    const p = await probe(token);
    if (token) session.set(TOKEN_KEY, token); else session.del(TOKEN_KEY);
    session.set(MODE_KEY, p.mode);
    session.set(WHO_KEY, p.who);
  }

  function signOut(message) {
    session.del(TOKEN_KEY);
    session.del(MODE_KEY);
    session.del(WHO_KEY);
    stopView();
    viewLogin(message);
  }

  function setChrome(signedIn) {
    document.getElementById('nav').hidden = !signedIn;
    document.getElementById('session').hidden = !signedIn;
    const who = document.getElementById('who');
    who.textContent = signedIn ? session.get(WHO_KEY) || '' : '';
    document.getElementById('signout').hidden = session.get(MODE_KEY) === 'open';
    document.body.dataset.mode = signedIn ? session.get(MODE_KEY) || '' : '';
  }

  // ------------------------------------------------------------- overview

  async function viewOverview(root, gen) {
    const body = h('div', { class: 'stack' });
    append(root, header('Overview', null, autoToggle()), body);
    const load = async () => {
      const [health, lists, workers, schedules] = await Promise.all([
        api('/v1/health').catch((e) => e),
        Promise.all(STATUSES.map((st) => api(`/v1/executions?status=${st}&limit=${LIST_MAX}`).then((r) => r.executions).catch((e) => e))),
        api('/v1/workers').catch((e) => e),
        api('/v1/schedules').catch((e) => e),
      ]);
      if (gen !== view.gen) return;
      const failures = [];
      lists.forEach((l, i) => { if (Array.isArray(l) && (STATUSES[i] === 'FAILED' || STATUSES[i] === 'TIMED_OUT')) failures.push(...l); });
      failures.sort((a, b) => b.updated_at_ms - a.updated_at_ms);
      const recent = failures.slice(0, 8);
      const details = await Promise.all(recent.map((f) => api('/v1/executions/' + enc(f.execution_id)).catch(() => null)));
      if (gen !== view.gen) return;
      replace(body,
        h('div', { class: 'grid' }, healthPanel(health), h('div', { class: 'stack' }, workersSummary(workers), schedulesSummary(schedules))),
        countsPanel(lists),
        failuresPanel(recent, details));
    };
    await load();
    every(5000, () => { if (autoRefresh()) load().catch(() => {}); });
  }

  function leaderText(l) {
    if (l === null || l === undefined) return '—';
    if (typeof l !== 'object') return String(l);
    return Object.entries(l).filter(([, v]) => v !== null && v !== undefined).map(([k, v]) => `${k}: ${v}`).join(', ') || '—';
  }

  function healthPanel(hl) {
    if (hl instanceof Error) return h('section', { class: 'panel' }, h('h2', null, 'Server'), errorNotice(hl));
    const kv = [
      ['Status', hl.status], ['Runtime', hl.runtime_version], ['API', hl.api_version],
      ['Role', hl.role || 'single'],
    ];
    if (hl.node_id) kv.push(['Node', hl.node_id]);
    if (hl.term !== null && hl.term !== undefined) kv.push(['Term', hl.term]);
    if (hl.role === 'leader' || hl.role === 'standby') kv.push(['Leader', leaderText(hl.leader)]);
    kv.push(['Active executions', hl.active_executions], ['Worker sessions', hl.worker_sessions]);
    if (hl.owned_executions !== undefined) kv.push(['Owned here', hl.owned_executions]);
    if (hl.tasks_claimed !== undefined) kv.push(['Tasks claimed here', hl.tasks_claimed]);
    const panel = h('section', { class: 'panel', 'data-test': 'health' }, h('h2', null, 'Server'),
      h('dl', { class: 'kv' }, kv.map(([k, v]) => [h('dt', null, k), h('dd', null, v === undefined || v === null ? '—' : String(v))])));
    if (Array.isArray(hl.nodes) && hl.nodes.length) {
      const keys = [...new Set(hl.nodes.flatMap((n) => Object.keys(n)))];
      append(panel, h('h3', null, 'Cluster nodes'), table(keys.map((k) => ({ label: k.replace(/_/g, ' ') })),
        hl.nodes.map((n) => h('tr', null, keys.map((k) => h('td', null, cellText(k, n[k])))))));
    }
    return panel;
  }

  function cellText(key, v) {
    if (v === null || v === undefined) return '—';
    if (/_ms$/.test(key) && typeof v === 'number') return fmtTime(v);
    if (typeof v === 'object') return JSON.stringify(v);
    return String(v);
  }

  function workersSummary(w) {
    const panel = h('section', { class: 'panel' }, h('h2', null, 'Workers'));
    if (w instanceof Error) return append(panel, errorNotice(w));
    const online = w.workers.filter((x) => x.online);
    const inflight = w.workers.reduce((a, x) => a + (x.inflight || 0), 0);
    append(panel, h('div', { class: 'stats' },
      stat(online.length, 'online', '#/workers'),
      stat(w.workers.length - online.length, 'offline', '#/workers'),
      stat(inflight, 'tasks in flight', '#/workers')));
    const tasks = new Set(online.flatMap((x) => x.contracts.map((c) => c.task_name)));
    append(panel, h('p', { class: 'small muted' }, tasks.size ? `Online workers serve ${tasks.size} task${tasks.size === 1 ? '' : 's'}: ` : 'No online worker serves any task.',
      [...tasks].sort().map((t) => h('span', { class: 'tag' }, t))));
    return panel;
  }

  function schedulesSummary(r) {
    const panel = h('section', { class: 'panel', 'data-test': 'schedules-summary' }, h('h2', null, 'Schedules'));
    if (r instanceof Error) return append(panel, errorNotice(r));
    const all = r.schedules || [];
    const n = (state) => all.filter((sc) => scheduleState(sc) === state).length;
    append(panel, h('div', { class: 'stats' },
      stat(n('ACTIVE'), 'active', '#/schedules'),
      stat(n('PAUSED'), 'paused', '#/schedules'),
      stat(all.filter((sc) => sc.last_error).length, 'failing to start', '#/schedules')));
    const next = all.map((sc) => isoMs(sc.next_fire_at)).filter((t) => t !== null).sort((a, b) => a - b)[0];
    append(panel, h('p', { class: 'small muted' }, all.length ? (next ? 'Next fire ' + fmtTime(next) + ' (' + ago(next) + ').' : 'No schedule is due to fire.') : 'No schedules this credential may manage.'));
    return panel;
  }

  function stat(n, label, href) {
    return h(href ? 'a' : 'div', { class: 'stat', href: href || null }, h('div', { class: 'n' }, n), h('div', { class: 'l' }, label));
  }

  function countsPanel(lists) {
    const denied = lists.find((l) => l instanceof Error);
    return h('section', { class: 'panel', 'data-test': 'counts' }, h('h2', null, 'Executions by status'),
      denied ? errorNotice(denied) : null,
      h('div', { class: 'stats' }, STATUSES.map((st, i) => {
        const l = lists[i];
        const n = Array.isArray(l) ? (l.length >= LIST_MAX ? LIST_MAX + '+' : l.length) : '—';
        return h('a', { class: 'stat', href: '#/executions?status=' + st, 'data-count': st },
          h('div', { class: 'n' }, n), h('div', { class: 'l' }, st.replace('_', ' ').toLowerCase()));
      })),
      h('p', { class: 'small faint' }, 'Counts cover the executions this credential may read, up to ' + LIST_MAX + ' per status.'));
  }

  function failuresPanel(recent, details) {
    const rows = recent.map((f, i) => {
      const d = details[i];
      const err = d && d.error ? `${d.error.code}: ${d.error.message}` : '';
      return h('tr', null, h('td', { class: 'id' }, link(execHref(f.execution_id), f.execution_id)),
        h('td', null, link(wfHref(f.workflow, f.version), f.workflow), h('span', { class: 'faint' }, ' ' + f.version)),
        h('td', null, badge(f.status)), timeCell(f.updated_at_ms), h('td', { class: 'wrap-any' }, err));
    });
    return h('section', { class: 'panel' }, h('h2', null, 'Recent failures'),
      table([{ label: 'Execution' }, { label: 'Workflow' }, { label: 'Status' }, { label: 'Finished' }, { label: 'Error' }], rows, 'No failed or timed-out executions.'));
  }

  // ----------------------------------------------------------- executions

  async function viewExecutions(root, gen, params) {
    const state = { status: params.get('status') || '', workflow: params.get('workflow') || '', q: params.get('q') || '', page: Math.max(0, parseInt(params.get('page') || '0', 10) || 0) };
    const statusSel = h('select', { id: 'f-status', 'data-test': 'filter-status' }, h('option', { value: '' }, 'All statuses'),
      STATUSES.map((st) => h('option', { value: st }, st.replace('_', ' ').toLowerCase())));
    statusSel.value = state.status;
    const wfSel = h('select', { id: 'f-workflow', 'data-test': 'filter-workflow' }, h('option', { value: '' }, 'All workflows'));
    const search = h('input', { type: 'search', id: 'f-q', placeholder: 'Execution id', 'data-test': 'search', value: state.q });
    const results = h('div');
    append(root, header('Executions', null, autoToggle()),
      h('div', { class: 'filters' },
        h('div', { class: 'field' }, h('label', { for: 'f-status' }, 'Status'), statusSel),
        h('div', { class: 'field' }, h('label', { for: 'f-workflow' }, 'Workflow'), wfSel),
        h('div', { class: 'field' }, h('label', { for: 'f-q' }, 'Search'), search)),
      results);

    api('/v1/workflows').then((r) => {
      if (gen !== view.gen) return;
      const names = [...new Set(r.workflows.map((w) => w.name))].sort();
      if (state.workflow && !names.includes(state.workflow)) names.push(state.workflow);
      append(wfSel, names.map((n) => h('option', { value: n }, n)));
      wfSel.value = state.workflow;
    }).catch(() => {
      if (state.workflow) { append(wfSel, h('option', { value: state.workflow }, state.workflow)); wfSel.value = state.workflow; }
    });

    const sync = () => {
      const p = new URLSearchParams();
      for (const k of ['status', 'workflow', 'q']) if (state[k]) p.set(k, state[k]);
      if (state.page) p.set('page', String(state.page));
      const qs = p.toString();
      history.replaceState(null, '', '#/executions' + (qs ? '?' + qs : ''));
    };

    const load = async () => {
      const q = new URLSearchParams();
      if (state.status) q.set('status', state.status);
      if (state.workflow) q.set('workflow', state.workflow);
      const wanted = state.q ? LIST_MAX : Math.min(LIST_MAX, (state.page + 1) * PAGE_SIZE + 1);
      q.set('limit', String(wanted));
      let list;
      try {
        list = (await api('/v1/executions?' + q.toString())).executions;
      } catch (e) {
        if (gen === view.gen) replace(results, errorNotice(e));
        return;
      }
      const needle = state.q.trim().toLowerCase();
      if (needle) {
        list = list.filter((e) => e.execution_id.toLowerCase().includes(needle));
        if (!list.length) {
          // Older than the newest 1000, or a different filter: look it up directly.
          const one = await api('/v1/executions/' + enc(state.q.trim())).catch(() => null);
          if (one) list = [{ execution_id: one.id, workflow: one.workflow, version: one.version, status: one.status, created_at_ms: one.created_at_ms, updated_at_ms: one.updated_at_ms }];
        }
      }
      if (gen !== view.gen) return;
      const start = state.page * PAGE_SIZE;
      const page = list.slice(start, start + PAGE_SIZE);
      const more = list.length > start + PAGE_SIZE;
      const rows = page.map((e) => h('tr', { 'data-execution': e.execution_id },
        h('td', { class: 'id' }, link(execHref(e.execution_id), e.execution_id)),
        h('td', null, link(wfHref(e.workflow, e.version), e.workflow), h('span', { class: 'faint' }, ' ' + e.version)),
        h('td', null, badge(e.status)),
        timeCell(e.created_at_ms),
        h('td', { class: 'nowrap hide-sm' }, ago(e.updated_at_ms)),
        h('td', { class: 'num nowrap' }, fmtDur(durationOf(e)))));
      const prev = h('button', { type: 'button', class: 'btn btn-small', disabled: state.page === 0 }, 'Previous');
      const next = h('button', { type: 'button', class: 'btn btn-small', disabled: !more, 'data-test': 'next-page' }, 'Next');
      prev.addEventListener('click', () => { state.page = Math.max(0, state.page - 1); sync(); load(); });
      next.addEventListener('click', () => { state.page += 1; sync(); load(); });
      const shown = page.length ? `${start + 1}–${start + page.length}` : '0';
      replace(results,
        table([{ label: 'Execution' }, { label: 'Workflow' }, { label: 'Status' }, { label: 'Started' }, { label: 'Updated', cls: 'hide-sm' }, { label: 'Duration', cls: 'num' }],
          rows, 'No executions match.'),
        h('div', { class: 'pager' }, h('span', null, 'Showing ' + shown), prev, next));
    };

    const reload = () => { state.page = 0; sync(); load(); };
    statusSel.addEventListener('change', () => { state.status = statusSel.value; reload(); });
    wfSel.addEventListener('change', () => { state.workflow = wfSel.value; reload(); });
    let debounce;
    search.addEventListener('input', () => { clearTimeout(debounce); debounce = setTimeout(() => { state.q = search.value.trim(); reload(); }, 250); });
    await load();
    every(3000, () => { if (autoRefresh()) load(); });
  }

  // ---------------------------------------------------- units and graphs

  /** Decompose a unit key: `[compensate:][on_<kind>:|on_event:<signal>@<seq>:]step[[i]]`. */
  function parseUnit(key) {
    let k = key;
    let comp = false;
    if (k.startsWith('compensate:')) { comp = true; k = k.slice('compensate:'.length); }
    let group = 'main';
    let groupName = null;
    let instance = null;
    let m = /^on_event:([^@:]+)@(\d+)(?::(.+))?$/.exec(k);
    if (m && m[3] === undefined) return { comp, group: 'event', groupName: m[1], instance: Number(m[2]), step: null, index: null };
    if (m) { group = 'event'; groupName = m[1]; instance = Number(m[2]); k = m[3]; } else {
      m = /^on_(failure|cancel|timeout):(.+)$/.exec(k);
      if (m) { group = 'handler'; groupName = m[1]; k = m[2]; }
    }
    if (k.startsWith('compensate:')) { comp = true; k = k.slice('compensate:'.length); }
    let index = null;
    m = /^(.*)\[(\d+)\]$/.exec(k);
    if (m) { k = m[1]; index = Number(m[2]); }
    return { comp, group, groupName, instance, step: k, index };
  }

  function groupSteps(def, group, name) {
    if (!def) return [];
    if (group === 'main') return def.steps || [];
    if (group === 'handler') return (def.handlers || {})[name] || [];
    return (def.events || {})[name] || [];
  }

  function stepDef(def, p) {
    return groupSteps(def, p.group, p.groupName).find((x) => x.id === p.step) || null;
  }

  function fmtMs(ms) { return ms === undefined || ms === null ? '' : fmtDur(ms).replace(/\.0+s$/, 's'); }

  function actionText(step) {
    if (!step) return '';
    const a = step.action || {};
    let t;
    switch (a.type) {
      case 'task': t = a.task; break;
      case 'workflow': t = 'workflow ' + a.task.replace(/^workflow:/, ''); break;
      case 'compute': t = 'compute'; break;
      case 'await': t = 'await ' + a.signal; break;
      case 'sleep': t = 'sleep ' + fmtMs(a.duration_ms); break;
      default: t = a.type || '';
    }
    return step.for_each ? 'for ' + step.for_each.variable + ': ' + t : t;
  }

  /** Child execution id of a `workflow` unit: `<id>.<unit>`, or `<id>.run<N>.<unit>` in run N > 1. */
  function childId(execId, key, run) {
    return execId + (run > 1 ? '.run' + run : '') + '.' + key.replaceAll('compensate:', 'compensate.').replaceAll('[', '.').replaceAll(']', '');
  }

  /** Status of a graph node: the step's own unit, or the latest instance of an event step. */
  function nodeStatus(units, group, groupName, step, comp) {
    if (!units) return { status: null, count: 0 };
    const prefix = comp ? 'compensate:' : '';
    if (group === 'event') {
      const keys = Object.keys(units).filter((k) => {
        const p = parseUnit(k);
        return p.group === 'event' && p.groupName === groupName && p.step === step && p.comp === comp && p.index === null;
      }).sort((a, b) => parseUnit(a).instance - parseUnit(b).instance);
      const last = keys[keys.length - 1];
      return { status: last ? units[last].status : null, count: keys.length, unit: last ? units[last] : null };
    }
    const key = prefix + (group === 'handler' ? `on_${groupName}:` : '') + step;
    const u = units[key];
    if (u) return { status: u.status, count: 1, unit: u };
    // A `for` step's elements without a parent unit.
    const els = Object.keys(units).filter((k) => k.startsWith(key + '['));
    if (!els.length) return { status: null, count: 0 };
    const sts = els.map((k) => units[k].status);
    const pick = ['FAILED', 'RUNNING', 'DISPATCHED', 'SCHEDULED', 'RETRY_WAIT', 'WAITING', 'CANCELLED', 'SUCCEEDED', 'SKIPPED'].find((x) => sts.includes(x));
    return { status: pick || sts[0], count: els.length };
  }

  const NODE_W = 172;
  const NODE_H = 44;
  const GAP_X = 46;
  const GAP_Y = 16;
  const PAD = 16;
  const LANE_LABEL = 22;

  /** Column (longest dependency chain within the group) and row of each step. */
  function layers(steps) {
    const ids = new Set(steps.map((x) => x.id));
    const layer = {};
    steps.forEach((x) => { layer[x.id] = 0; });
    for (let pass = 0; pass < steps.length; pass++) {
      let changed = false;
      for (const x of steps) {
        for (const d of x.dependencies || []) {
          if (ids.has(d) && layer[d] + 1 > layer[x.id]) { layer[x.id] = layer[d] + 1; changed = true; }
        }
      }
      if (!changed) break;
    }
    const rows = {};
    const pos = {};
    for (const x of steps) {
      const c = layer[x.id];
      rows[c] = (rows[c] || 0) + 1;
      pos[x.id] = { col: c, row: rows[c] - 1 };
    }
    return { pos, cols: Math.max(0, ...Object.values(layer)) + 1, rows: Math.max(0, ...Object.values(rows)) };
  }

  let graphSeq = 0;

  /**
   * SVG of a definition's steps: dependencies, compensations, outcome and
   * event handlers, and `continue_as_new` (a node after the final main steps
   * with a dashed edge back to the first ones). `cont` is the status of the
   * `continue_as_new` node: CONTINUED_AS_NEW (taken), SKIPPED (the run
   * completed instead) or null.
   */
  function graph(def, units, cont = null) {
    const lanes = [{ group: 'main', name: null, title: null, steps: def.steps || [] }];
    for (const [kind, steps] of Object.entries(def.handlers || {})) lanes.push({ group: 'handler', name: kind, title: 'on ' + kind, steps });
    for (const [sig, steps] of Object.entries(def.events || {})) lanes.push({ group: 'event', name: sig, title: 'on event ' + sig, steps });
    const nodes = [];
    const edges = [];
    let y = PAD;
    let width = 0;
    for (const lane of lanes) {
      if (!lane.steps.length) continue;
      if (lane.title) y += LANE_LABEL;
      const top = y;
      const L = layers(lane.steps);
      const at = {};
      for (const st of lane.steps) {
        const p = L.pos[st.id];
        const n = { lane, step: st, comp: false, x: PAD + p.col * (NODE_W + GAP_X), y: top + p.row * (NODE_H + GAP_Y) };
        at[st.id] = n;
        nodes.push(n);
      }
      for (const st of lane.steps) {
        for (const d of st.dependencies || []) if (at[d]) edges.push({ from: at[d], to: at[st.id], comp: false });
      }
      let cols = L.cols;
      if (lane.group === 'main' && def.continue_as_new) {
        // Evaluated when every step has finished: after the steps nothing
        // depends on; the next run starts again at the steps without dependencies.
        const n = { lane, step: { id: 'continue_as_new' }, cont: def.continue_as_new, comp: false, x: PAD + L.cols * (NODE_W + GAP_X), y: top };
        nodes.push(n);
        for (const st of lane.steps) {
          if (!lane.steps.some((o) => (o.dependencies || []).includes(st.id))) edges.push({ from: at[st.id], to: n, comp: false });
          if (!(st.dependencies || []).length) edges.push({ from: n, to: at[st.id], back: true, top });
        }
        cols += 1;
      }
      y = top + L.rows * (NODE_H + GAP_Y);
      // Compensations sit under the lane, in their step's column.
      const compRows = {};
      let maxComp = 0;
      for (const st of lane.steps) {
        if (!st.compensation) continue;
        const col = L.pos[st.id].col;
        const r = compRows[col] || 0;
        compRows[col] = r + 1;
        maxComp = Math.max(maxComp, r + 1);
        const n = { lane, step: st, comp: true, x: at[st.id].x, y: y + 6 + r * (NODE_H + GAP_Y) };
        nodes.push(n);
        edges.push({ from: at[st.id], to: n, comp: true, slot: r });
      }
      if (maxComp) y += 6 + maxComp * (NODE_H + GAP_Y);
      lane.top = top;
      lane.bottom = y - GAP_Y;
      width = Math.max(width, PAD * 2 + cols * (NODE_W + GAP_X) - GAP_X);
      y += 8;
    }
    const height = y + PAD - 8 - GAP_Y + 8;
    width = Math.max(width, 240);
    const mid = 'arrow-' + (++graphSeq);
    const svg = s('svg', { class: 'graph', width, height, viewBox: `0 0 ${width} ${height}`, role: 'img', 'aria-label': `Steps of ${def.name}`, 'data-test': 'graph' },
      s('defs', null, s('marker', { id: mid, viewBox: '0 0 8 8', refX: 7, refY: 4, markerWidth: 7, markerHeight: 7, orient: 'auto-start-reverse' },
        s('path', { d: 'M0,0 L8,4 L0,8 z', class: 'arrow' }))));
    for (const lane of lanes) {
      if (!lane.title || !lane.steps.length) continue;
      append(svg, s('text', { class: 'lane-label', x: PAD, y: lane.top - 8 }, lane.title));
    }
    for (const e of edges) {
      let d;
      if (e.back) {
        // `next run`: up from the continue_as_new node, along the top, down
        // the left margin and into the first step.
        const x1 = e.from.x + NODE_W / 2;
        const yt = e.top - 8;
        const xl = PAD - 8;
        const y2 = e.to.y + NODE_H / 2;
        d = `M${x1},${e.from.y} L${x1},${yt} L${xl},${yt} L${xl},${y2} L${e.to.x - 1},${y2}`;
      } else if (e.comp) {
        // Down the left edge of the column, clear of the steps in between.
        const x = e.from.x - 6 - 3 * (e.slot % 3);
        const y1 = e.from.y + NODE_H / 2;
        const y2 = e.to.y + NODE_H / 2;
        d = `M${e.from.x},${y1} L${x},${y1} L${x},${y2} L${e.to.x - 1},${y2}`;
      } else {
        const x1 = e.from.x + NODE_W;
        const y1 = e.from.y + NODE_H / 2;
        const x2 = e.to.x;
        const y2 = e.to.y + NODE_H / 2;
        const c = Math.max(18, (x2 - x1) / 2);
        d = `M${x1},${y1} C${x1 + c},${y1} ${x2 - c},${y2} ${x2 - 1},${y2}`;
      }
      append(svg, s('path', { d, class: 'edge' + (e.comp ? ' comp' : '') + (e.back ? ' back' : ''), 'marker-end': `url(#${mid})`, 'data-edge': e.back ? 'next-run' : null }));
    }
    for (const n of nodes) {
      if (n.cont) {
        const when = continueWhen(n.cont);
        const label = cont === CONTINUED ? 'continued as new' : cont === 'SKIPPED' ? 'not taken: the run completed' : null;
        const tip = ['continue as new', when, 'next input: ' + exprStr(n.cont.input), label ? 'this run: ' + label : null].filter(Boolean).join('\n');
        append(svg, s('g', { class: 'node cont' + (cont ? ' ' + (WF_CLASS[cont] || 's-idle').replace('s-', 'st-') : ''), transform: `translate(${n.x},${n.y})`,
          'data-step': 'continue_as_new', 'data-continue': 'true', 'data-lane': 'main', 'data-status': cont || '' },
        s('title', null, tip),
        s('rect', { width: NODE_W, height: NODE_H, rx: 5 }),
        s('rect', { class: 'inner', x: 3, y: 3, width: NODE_W - 6, height: NODE_H - 6, rx: 3 }),
        s('text', { x: 10, y: 18 }, 'continue as new'),
        s('text', { x: 10, y: 34, class: 'kind' }, clip(when, 26))));
        continue;
      }
      const st = nodeStatus(units, n.lane.group, n.lane.name, n.step.id, n.comp);
      const cls = st.status ? (UNIT_CLASS[st.status] || 's-idle').replace('s-', 'st-') : '';
      const name = n.comp ? 'compensate ' + n.step.id : n.step.id;
      const kind = n.comp ? n.step.compensation.task : actionText(n.step);
      const tip = [name, kind, units ? 'status: ' + (st.status || 'not started') : null,
        st.count > 1 ? `${st.count} ${n.lane.group === 'event' ? 'instances' : 'elements'}` : null,
        st.unit && st.unit.attempt ? 'attempt ' + st.unit.attempt : null].filter(Boolean).join('\n');
      append(svg, s('g', { class: `node ${n.comp ? 'comp ' : ''}${cls}`, transform: `translate(${n.x},${n.y})`,
        'data-step': n.step.id, 'data-lane': n.lane.title || 'main', 'data-comp': n.comp ? 'true' : null, 'data-status': st.status || '' },
      s('title', null, tip),
      s('rect', { width: NODE_W, height: NODE_H, rx: 5 }),
      s('text', { x: 10, y: 18 }, clip(name, 22) + (st.count > 1 ? ` ×${st.count}` : '')),
      s('text', { x: 10, y: 34, class: 'kind' }, clip(kind, 26))));
    }
    return h('div', { class: 'graph-wrap' }, svg);
  }

  function clip(t, n) { t = String(t || ''); return t.length > n ? t.slice(0, n - 1) + '…' : t; }

  function legend(def) {
    const cont = def && def.continue_as_new;
    return h('div', { class: 'legend' }, [['SUCCEEDED', 'succeeded'], ['RUNNING', 'running or waiting'], ['RETRY_WAIT', 'retrying'], ['FAILED', 'failed'], ['SKIPPED', 'skipped or cancelled']]
      .map(([st, label]) => h('span', null, h('span', { class: 'swatch ' + UNIT_CLASS[st] }), label)),
    cont ? h('span', null, h('span', { class: 'swatch s-cont' }), 'continued as new') : null,
    h('span', null, 'Dashed: compensation' + (cont ? '; next run' : '') + '. Unfilled: not started.' + (cont ? ' Double border: continue-as-new.' : '')));
  }

  // ------------------------------------------------------ execution detail

  /** The run selector of an execution that continued as new; `shown` is the run on the page. */
  function runSelect(ex, shown) {
    const current = ex.run || 1;
    if (current <= 1) return null;
    const sel = h('select', { id: 'run-select', 'data-test': 'run-select', 'aria-label': 'Run' });
    for (let r = current; r >= 1; r--) append(sel, h('option', { value: String(r) }, r === current ? `Run ${r} (current)` : `Run ${r}`));
    sel.value = String(shown);
    sel.addEventListener('change', () => {
      const r = Number(sel.value);
      location.hash = execHref(ex.id, r === current ? null : r);
    });
    return h('label', { class: 'check', for: 'run-select' }, 'Run', sel);
  }

  function scheduleLink(id) {
    return link(schedHref(id), h('span', { class: 'tag', 'data-test': 'schedule-link' }, 'schedule ' + id));
  }

  async function viewExecution(root, gen, id, params) {
    let ex = await api('/v1/executions/' + enc(id));
    if (gen !== view.gen) return;
    let def = null;
    let defError = null;
    try { def = await api(`/v1/workflows/${enc(ex.workflow)}/${enc(ex.version)}`); } catch (e) { defError = e; }
    if (gen !== view.gen) return;
    const current = ex.run || 1;
    const wanted = parseInt(params.get('run') || '', 10);
    if (Number.isFinite(wanted) && wanted !== current) {
      await viewRun(root, gen, ex, def, defError, wanted);
      return;
    }
    const openEvents = new Set();
    let history = [];
    let lastSeq = -1;

    const badgeSlot = h('span');
    const sub = h('span', null, link(wfHref(ex.workflow, ex.version), ex.workflow), ' ', h('span', { class: 'faint' }, ex.version), ' ', badgeSlot,
      current > 1 ? [' ', h('span', { class: 'tag', 'data-test': 'run' }, 'run ' + current)] : null,
      ex.schedule_id ? [' ', scheduleLink(ex.schedule_id)] : null);
    const summary = h('section', { class: 'panel' });
    const io = h('section', { class: 'panel' });
    const graphPanel = h('section', { class: 'panel' });
    const steps = h('section', { class: 'panel', 'data-test': 'steps' });
    const children = h('section', { class: 'panel', hidden: true });
    const signals = h('section', { class: 'panel', 'data-test': 'signals-received' });
    const historyPanel = h('section', { class: 'panel', 'data-test': 'history' });
    const actions = h('section', { class: 'panel', 'data-test': 'actions' });

    append(root, header(ex.id, sub, runSelect(ex, current), autoToggle()),
      defError ? errorNotice(defError) : null,
      h('div', { class: 'grid' }, summary, actions),
      io, graphPanel, steps, children, signals, historyPanel);

    const refresh = async () => {
      const [e2, hist] = await Promise.all([api('/v1/executions/' + enc(id)), api(`/v1/executions/${enc(id)}/history`)]);
      if (gen !== view.gen) return;
      // The execution went on to its next run: show that one.
      if ((e2.run || 1) !== current) { route(); return; }
      ex = e2;
      history = hist.events;
      if (ex.last_seq === lastSeq) return;
      lastSeq = ex.last_seq;
      render();
    };

    const render = () => {
      replace(badgeSlot, badge(ex.status));
      renderSummary(summary, ex);
      renderIo(io, ex);
      replace(graphPanel, h('h2', null, 'Graph'), def ? [graph(def, ex.units, ex.status === 'COMPLETED' ? 'SKIPPED' : null), legend(def)] : h('p', { class: 'muted' }, 'The definition is not available.'));
      renderSteps(steps, ex, def);
      renderChildren(children, ex, def, gen);
      renderSignals(signals, ex);
      renderHistory(historyPanel, history, openEvents, current);
      updateActions();
    };

    const updateActions = buildActions(actions, def, () => ex, () => refresh().catch(() => {}));
    await refresh();
    every(2000, () => {
      if (autoRefresh() && !TERMINAL.has(ex.status)) refresh().catch(() => {});
    });
  }

  /** Unit state rebuilt from a run's history (an earlier run has no live state). */
  function unitsFromHistory(events) {
    const units = {};
    const u = (k) => (units[k] = units[k] || { attempt: 0, dispatch_count: 0 });
    for (const e of events) {
      if (!e.step) continue;
      const x = u(e.step);
      switch (e.type) {
        case 'StepFanOut': x.status = 'RUNNING'; x.input = e.items; break;
        case 'StepScheduled': Object.assign(x, { status: 'SCHEDULED', attempt: e.attempt, input: e.input, idempotency_key: e.idempotency_key, scheduled_at_ms: e.timestamp_ms }); break;
        case 'StepDispatched': Object.assign(x, { status: 'DISPATCHED', attempt: e.attempt, worker_id: e.worker_id, dispatched_at_ms: e.timestamp_ms }); x.dispatch_count += 1; break;
        case 'StepSucceeded': Object.assign(x, { status: 'SUCCEEDED', output: e.output, finished_at_ms: e.timestamp_ms }); break;
        case 'StepAttemptFailed': Object.assign(x, { status: 'ATTEMPT_FAILED', last_error: e.error }); break;
        case 'StepAttemptUncertain': x.status = 'UNCERTAIN'; break;
        case 'RetryScheduled': Object.assign(x, { status: 'RETRY_WAIT', wake_at_ms: e.fire_at_ms }); break;
        case 'StepFailed': Object.assign(x, { status: 'FAILED', error: e.error, finished_at_ms: e.timestamp_ms }); break;
        case 'StepSkipped': Object.assign(x, { status: 'SKIPPED', finished_at_ms: e.timestamp_ms }); break;
        case 'StepCancelled': Object.assign(x, { status: 'CANCELLED', finished_at_ms: e.timestamp_ms }); break;
        case 'TimerStarted': Object.assign(x, { status: 'WAITING', wake_at_ms: e.fire_at_ms, scheduled_at_ms: x.scheduled_at_ms || e.timestamp_ms }); break;
        case 'SignalAwaitStarted': Object.assign(x, { status: 'WAITING', wake_at_ms: e.deadline_ms, scheduled_at_ms: x.scheduled_at_ms || e.timestamp_ms }); break;
        // A timer that fired and a signal that was taken finish their step.
        case 'TimerFired': Object.assign(x, { status: 'SUCCEEDED', finished_at_ms: e.timestamp_ms, wake_at_ms: undefined }); break;
        case 'SignalDelivered': Object.assign(x, { status: 'SUCCEEDED', finished_at_ms: e.timestamp_ms, wake_at_ms: undefined }); break;
        default: break;
      }
    }
    return units;
  }

  /** What an earlier run's history says: status, input, next input, units, signals. */
  function runState(events) {
    const first = events[0] || {};
    const last = events[events.length - 1] || {};
    const ends = { WorkflowContinuedAsNew: CONTINUED, WorkflowCompleted: 'COMPLETED', WorkflowFailed: 'FAILED', WorkflowCancelled: 'CANCELLED', WorkflowTimedOut: 'TIMED_OUT' };
    const signals = [];
    for (const e of events) {
      if (e.type === 'SignalReceived') signals.push({ seq: e.seq, name: e.name, payload: e.payload, received_at_ms: e.timestamp_ms });
      if (e.type === 'SignalDelivered') { const sg = signals.find((x) => x.seq === e.signal_seq); if (sg) sg.delivered_to = e.step; }
    }
    return {
      status: ends[last.type] || 'RUNNING',
      input: first.input,
      next: last.type === 'WorkflowContinuedAsNew' ? last : null,
      created_at_ms: first.timestamp_ms,
      finished_at_ms: ends[last.type] ? last.timestamp_ms : null,
      definition_hash: first.definition_hash,
      units: unitsFromHistory(events),
      signals,
    };
  }

  /** An earlier run of an execution that continued as new: read-only, from `history?run=N`. */
  async function viewRun(root, gen, ex, def, defError, run) {
    const current = ex.run || 1;
    const hist = await api(`/v1/executions/${enc(ex.id)}/history?run=${run}`);
    if (gen !== view.gen) return;
    const events = hist.events || [];
    const st = runState(events);
    const sub = h('span', null, link(wfHref(ex.workflow, ex.version), ex.workflow), ' ', h('span', { class: 'faint' }, ex.version), ' ', badge(st.status),
      ' ', h('span', { class: 'tag', 'data-test': 'run' }, `run ${run} of ${current}`), ex.schedule_id ? [' ', scheduleLink(ex.schedule_id)] : null);
    const kv = [
      ['Run', `${run} of ${current}`],
      ['Status', badge(st.status)],
      ['Workflow', [link(wfHref(ex.workflow, ex.version), ex.workflow), ' ', ex.version]],
      ['Definition', h('span', { class: 'mono' }, st.definition_hash || ex.definition_hash)],
      ['Started', fmtTime(st.created_at_ms)],
      ['Ended', st.finished_at_ms ? fmtTime(st.finished_at_ms) : '—'],
      ['Duration', st.finished_at_ms ? fmtDur(st.finished_at_ms - st.created_at_ms) : '—'],
      ['History events', String(events.length)],
    ];
    if (st.next) kv.push(['Continued as', link(execHref(ex.id, st.next.next_run === current ? null : st.next.next_run), 'run ' + st.next.next_run)]);
    if (ex.schedule_id) kv.push(['Schedule', link(schedHref(ex.schedule_id), ex.schedule_id)]);
    const steps = h('section', { class: 'panel', 'data-test': 'steps' });
    const signals = h('section', { class: 'panel', 'data-test': 'signals-received' });
    const historyPanel = h('section', { class: 'panel', 'data-test': 'history' });
    const pseudo = { id: ex.id, run, units: st.units, signals: st.signals };
    append(root, header(ex.id, sub, runSelect(ex, run)),
      notice('', `Run ${run} of ${current}.`, ['An earlier run of this execution, rebuilt from its archived history. It is read-only: ',
        link(execHref(ex.id), 'open the current run'), ' to query, signal or cancel the execution.']),
      defError ? errorNotice(defError) : null,
      h('div', { class: 'grid' },
        h('section', { class: 'panel', 'data-test': 'run-summary' }, h('h2', null, 'Run'), h('dl', { class: 'kv' }, kv.map(([k, v]) => [h('dt', null, k), h('dd', null, v)]))),
        h('section', { class: 'panel' }, h('h2', null, 'Input and next input'), h('div', { class: 'io' },
          h('div', null, h('h3', null, 'Input'), json(st.input)),
          h('div', { 'data-test': 'next-input' }, h('h3', null, st.next ? `Input of run ${st.next.next_run}` : 'Next input'),
            st.next ? json(st.next.input) : h('p', { class: 'muted' }, 'This run did not continue as new.'))))),
      h('section', { class: 'panel' }, h('h2', null, 'Graph'), def ? [graph(def, st.units, st.status === CONTINUED ? CONTINUED : st.status === 'COMPLETED' ? 'SKIPPED' : null), legend(def)] : h('p', { class: 'muted' }, 'The definition is not available.')),
      steps, signals, historyPanel);
    if (def) renderSteps(steps, pseudo, def); else steps.hidden = true;
    renderSignals(signals, pseudo);
    renderHistory(historyPanel, events, new Set(), run);
  }

  function renderSummary(panel, ex) {
    const kv = [
      ['Status', badge(ex.status)],
      ['Workflow', [link(wfHref(ex.workflow, ex.version), ex.workflow), ' ', ex.version]],
      ['Definition', h('span', { class: 'mono' }, ex.definition_hash)],
      ['Started', fmtTime(ex.created_at_ms)],
      ['Finished', ex.finished_at_ms ? fmtTime(ex.finished_at_ms) : '—'],
      ['Duration', fmtDur(durationOf(ex))],
      ['Updated', ago(ex.updated_at_ms)],
      ['History events', String(ex.last_seq)],
    ];
    if ((ex.run || 1) > 1) {
      kv.splice(1, 0, ['Run', [String(ex.run), ' (continue-as-new; ', link(execHref(ex.id, ex.run - 1), 'previous run'), ')']]);
    }
    if (ex.schedule_id) kv.push(['Schedule', link(schedHref(ex.schedule_id), ex.schedule_id)]);
    if (ex.parent) kv.push(['Parent', [link(execHref(ex.parent.execution_id), ex.parent.execution_id), ' step ', h('span', { class: 'mono' }, ex.parent.step)]]);
    if (ex.abort) kv.push(['Winding down', ex.abort.kind + (ex.abort.reason ? ': ' + ex.abort.reason : '')]);
    if (ex.compensation) kv.push(['Compensation', String(ex.compensation)]);
    if (ex.handlers) kv.push(['Handlers', `on ${ex.handlers.kind} (${ex.handlers.compensation})`]);
    replace(panel, h('h2', null, 'Execution'), h('dl', { class: 'kv' }, kv.map(([k, v]) => [h('dt', null, k), h('dd', null, v)])));
  }

  function renderIo(panel, ex) {
    replace(panel, h('h2', null, 'Input and result'), h('div', { class: 'io' },
      h('div', null, h('h3', null, 'Input'), json(ex.input)),
      h('div', { 'data-test': 'output' }, h('h3', null, 'Output'), ex.output !== undefined ? json(ex.output) : h('p', { class: 'muted' }, TERMINAL.has(ex.status) ? 'None.' : 'Not finished.')),
      ex.error ? h('div', { 'data-test': 'error' }, h('h3', null, 'Error'), json(ex.error)) : null));
  }

  /** Unit keys in definition order: steps, elements, compensations, handlers, event instances. */
  function orderedUnits(ex, def) {
    const present = Object.keys(ex.units);
    // Main steps that have not started yet still get a row.
    const missing = def ? (def.steps || []).filter((st) => !(st.id in ex.units) && !present.some((k) => k.startsWith(st.id + '['))).map((st) => st.id) : [];
    const keys = present.concat(missing);
    const order = new Map();
    const rank = (k) => {
      const p = parseUnit(k);
      const groupRank = p.group === 'main' ? 0 : p.group === 'handler' ? 1 : 2;
      const steps = groupSteps(def, p.group, p.groupName);
      const idx = p.step === null ? -1 : steps.findIndex((x) => x.id === p.step);
      return [groupRank, p.groupName || '', p.instance || 0, p.comp ? 1 : 0, idx < 0 ? 1e6 : idx, p.index === null ? -1 : p.index, k];
    };
    keys.forEach((k) => order.set(k, rank(k)));
    const cmp = (a, b) => {
      const x = order.get(a);
      const y = order.get(b);
      for (let i = 0; i < x.length; i++) {
        if (x[i] < y[i]) return -1;
        if (x[i] > y[i]) return 1;
      }
      return 0;
    };
    return keys.sort(cmp);
  }

  function unitKind(def, p) {
    if (p.group === 'event' && p.step === null) return 'event instance (signal #' + p.instance + ')';
    const st = stepDef(def, p);
    if (!st) return '';
    if (p.comp) return 'compensate: ' + (st.compensation ? st.compensation.task : '');
    return actionText(st);
  }

  function renderSteps(panel, ex, def) {
    const rows = orderedUnits(ex, def).map((k) => {
      const u = ex.units[k];
      if (!u) {
        const st = (def.steps || []).find((x) => x.id === k);
        return h('tr', { 'data-unit': k }, h('td', { class: 'id' }, k), h('td', { class: 'small' }, actionText(st)),
          h('td', null, badge(null, UNIT_CLASS)), h('td', { class: 'num' }, '—'), h('td', null, '—'), h('td', null, '—'), h('td', null, '—'), h('td', { class: 'num' }, '—'), h('td', null, ''));
      }
      const p = parseUnit(k);
      const st = stepDef(def, p);
      const started = u.dispatched_at_ms || u.scheduled_at_ms;
      const err = u.error || u.last_error;
      let note = '';
      if (err) note = `${err.code}: ${err.message}`;
      else if (u.status === 'WAITING' && u.wake_at_ms) note = (st && st.action.type === 'await' ? 'times out ' : 'wakes ') + fmtTime(u.wake_at_ms);
      else if (u.status === 'RETRY_WAIT' && u.wake_at_ms) note = 'retries ' + fmtTime(u.wake_at_ms);
      const isChild = st && st.action.type === 'workflow' && !p.comp && u.status && u.status !== 'SKIPPED' && u.status !== 'PENDING';
      const data = {};
      if (u.input !== undefined) data.input = u.input;
      if (u.output !== undefined) data.output = u.output;
      if (u.idempotency_key) data.idempotency_key = u.idempotency_key;
      return h('tr', { 'data-unit': k },
        h('td', { class: 'id' }, k),
        h('td', { class: 'small' }, unitKind(def, p), isChild ? [' ', link(execHref(childId(ex.id, k, ex.run)), 'child')] : null),
        h('td', null, badge(u.status, UNIT_CLASS)),
        h('td', { class: 'num' }, u.attempt ? String(u.attempt) + (u.dispatch_count > u.attempt ? ` (${u.dispatch_count} deliveries)` : '') : '—'),
        h('td', { class: 'small' }, u.worker_id || '—'),
        timeCell(started),
        timeCell(u.finished_at_ms),
        h('td', { class: 'num nowrap' }, u.finished_at_ms && started ? fmtDur(u.finished_at_ms - started) : '—'),
        h('td', { class: 'small wrap-any' }, note,
          Object.keys(data).length ? h('details', null, h('summary', { class: 'faint' }, Object.keys(data).join(', ')), json(data)) : null));
    });
    replace(panel, h('h2', null, 'Steps'),
      table([{ label: 'Unit' }, { label: 'Action' }, { label: 'Status' }, { label: 'Attempt', cls: 'num' }, { label: 'Worker' }, { label: 'Started' }, { label: 'Finished' }, { label: 'Took', cls: 'num' }, { label: 'Details' }], rows, 'No units yet.'));
  }

  function renderChildren(panel, ex, def, gen) {
    const kids = Object.keys(ex.units).filter((k) => {
      const p = parseUnit(k);
      const st = stepDef(def, p);
      const u = ex.units[k];
      return st && st.action.type === 'workflow' && !p.comp && u.status && !['SKIPPED', 'PENDING', 'RUNNING'].includes(u.status) && !(u.status === 'SCHEDULED' && !u.dispatched_at_ms && !u.attempt);
    });
    if (!kids.length) { panel.hidden = true; return; }
    panel.hidden = false;
    const body = h('tbody');
    replace(panel, h('h2', null, 'Child executions'), h('div', { class: 'table-wrap' }, h('table', null,
      h('thead', null, h('tr', null, h('th', null, 'Execution'), h('th', null, 'Unit'), h('th', null, 'Workflow'), h('th', null, 'Status'))), body)));
    for (const k of kids.slice(0, 100)) {
      const cid = childId(ex.id, k, ex.run);
      const cell = h('td', null, '…');
      const wf = h('td', null, '');
      append(body, h('tr', { 'data-child': cid }, h('td', { class: 'id' }, link(execHref(cid), cid)), h('td', { class: 'id' }, k), wf, cell));
      api('/v1/executions/' + enc(cid)).then((c) => {
        if (gen !== view.gen) return;
        replace(cell, badge(c.status));
        replace(wf, link(wfHref(c.workflow, c.version), c.workflow), h('span', { class: 'faint' }, ' ' + c.version));
      }).catch((e) => replace(cell, h('span', { class: 'muted small' }, e.status === 404 ? 'not started' : e.message)));
    }
  }

  function renderSignals(panel, ex) {
    const rows = (ex.signals || []).map((sg) => h('tr', null,
      h('td', { class: 'num' }, String(sg.seq)), h('td', { class: 'mono' }, sg.name), timeCell(sg.received_at_ms),
      h('td', { class: 'id' }, sg.delivered_to || '—'), h('td', null, h('details', null, h('summary', { class: 'faint' }, 'payload'), json(sg.payload)))));
    replace(panel, h('h2', null, 'Signals received'),
      table([{ label: 'Seq', cls: 'num' }, { label: 'Signal' }, { label: 'Received' }, { label: 'Delivered to' }, { label: 'Payload' }], rows, 'No signals received.'));
  }

  function eventClass(type) {
    if (type === 'WorkflowContinuedAsNew') return 'ev-cont';
    if (/Failed|TimedOut|LimitExceeded|TimeoutReached/.test(type)) return 'ev-bad';
    if (/Succeeded|Completed|Delivered/.test(type)) return 'ev-ok';
    if (/Retry|Uncertain|Cancel|Compensation|Stale|HandlersStarted/.test(type)) return 'ev-warn';
    return '';
  }

  function renderHistory(panel, events, openEvents, run = 1) {
    const t0 = events.length ? events[0].timestamp_ms : 0;
    const items = events.map((e) => {
      const d = h('details', { 'data-seq': e.seq }, h('summary', null,
        h('span', { class: 'num faint' }, '#' + e.seq),
        h('span', { class: 'time faint mono' }, '+' + fmtDur(e.timestamp_ms - t0)),
        h('span', { class: 'ev ' + eventClass(e.type) }, e.type, e.step ? h('span', { class: 'unit' }, e.step) : e.name ? h('span', { class: 'unit' }, e.name) : null),
        h('span', { class: 'faint small nowrap' }, fmtTime(e.timestamp_ms))),
      json(e));
      if (openEvents.has(e.seq)) d.open = true;
      d.addEventListener('toggle', () => { if (d.open) openEvents.add(e.seq); else openEvents.delete(e.seq); });
      return h('li', null, d);
    });
    replace(panel, h('div', { class: 'row spread' }, h('h2', null, run > 1 ? `History of run ${run} (${events.length} events)` : `History (${events.length} events)`)),
      items.length ? h('ol', { class: 'timeline' }, items) : h('p', { class: 'muted' }, 'No events.'));
  }

  /** Queries, signals and cancel. Built once (form state survives refreshes). */
  function buildActions(panel, def, getEx, refresh) {
    const list = h('div', { class: 'actions-list' });
    replace(panel, h('h2', null, 'Actions'), list);
    const signalButtons = [];
    const queries = Object.entries((def && def.queries) || {});
    for (const [name, q] of queries) {
      const out = h('div', { class: 'result', 'data-query-result': name });
      const btn = h('button', { type: 'button', class: 'btn btn-small', 'data-query': name }, 'Run');
      btn.addEventListener('click', async () => {
        btn.disabled = true;
        try {
          const r = await api(`/v1/executions/${enc(getEx().id)}/queries/${enc(name)}`);
          replace(out, h('div', { class: 'type' }, typeStr(q.output_type)), json(r.value));
        } catch (e) {
          replace(out, errorNotice(e));
        } finally { btn.disabled = false; }
      });
      append(list, h('div', null, h('div', { class: 'row spread' }, h('span', null, 'Query ', h('strong', { class: 'mono' }, name)), btn),
        h('div', { class: 'small faint mono wrap-any' }, '→ ' + typeStr(q.output_type)), out));
    }
    for (const [name, type] of Object.entries((def && def.signals) || {})) {
      const form = signalForm(name, type, async (payload, out) => {
        try {
          const r = await api(`/v1/executions/${enc(getEx().id)}/signals/${enc(name)}`, { method: 'POST', body: payload });
          replace(out, notice('', 'Sent.', `Signal ${name} accepted as history event #${r.seq}.`));
          refresh();
        } catch (e) {
          replace(out, errorNotice(e));
        }
      });
      signalButtons.push(form.querySelector('button[type="submit"]'));
      append(list, h('div', null, form));
    }
    // Cancel, with confirmation.
    const cancelOut = h('div');
    const cancelBtn = h('button', { type: 'button', class: 'btn btn-danger btn-small', 'data-test': 'cancel' }, 'Cancel execution…');
    const confirmBox = h('div', { class: 'confirm', hidden: true });
    const reason = h('input', { type: 'text', id: 'cancel-reason', placeholder: 'Reason (recorded in history)', 'data-test': 'cancel-reason' });
    const yes = h('button', { type: 'button', class: 'btn btn-danger btn-small', 'data-test': 'cancel-confirm' }, 'Confirm cancel');
    const no = h('button', { type: 'button', class: 'btn btn-small' }, 'Keep running');
    append(confirmBox, h('div', null, 'Cancel this execution? Running steps are abandoned and completed steps are compensated. This cannot be undone.'),
      reason, h('div', { class: 'row' }, yes, no));
    cancelBtn.addEventListener('click', () => { confirmBox.hidden = false; cancelBtn.hidden = true; reason.focus(); });
    no.addEventListener('click', () => { confirmBox.hidden = true; cancelBtn.hidden = false; });
    yes.addEventListener('click', async () => {
      yes.disabled = true;
      try {
        const body = reason.value.trim() ? { reason: reason.value.trim() } : {};
        await api(`/v1/executions/${enc(getEx().id)}/cancel`, { method: 'POST', body });
        confirmBox.hidden = true;
        replace(cancelOut, notice('', 'Cancellation requested.', ''));
        refresh();
      } catch (e) {
        replace(cancelOut, errorNotice(e));
      } finally { yes.disabled = false; }
    });
    const cancelRow = h('div', null, h('div', { class: 'row spread' }, h('span', null, 'Cancel'), cancelBtn), confirmBox, cancelOut);
    append(list, cancelRow);
    return () => {
      const done = TERMINAL.has(getEx().status);
      cancelBtn.disabled = done;
      if (done) confirmBox.hidden = true;
      signalButtons.forEach((b) => { b.disabled = done; });
      if (done && !cancelBtn.hidden) cancelBtn.title = 'The execution has finished.';
    };
  }

  /** A form for a signal's payload type; values are checked by the server. */
  function signalForm(name, type, send) {
    const out = h('div', { class: 'result' });
    const fields = type && type.kind === 'record' ? type.fields || [] : null;
    const controls = [];
    const grid = h('div', { class: 'form-grid' });
    const fid = (f) => `sig-${name}-${f}`;
    if (fields) {
      for (const f of fields) {
        const opt = f.type.kind === 'optional';
        const base = opt ? f.type.inner : f.type;
        const hasDefault = f.default !== undefined;
        let el;
        let read;
        if (base.kind === 'bool') {
          el = h('select', { id: fid(f.name), name: f.name },
            (opt || hasDefault) ? h('option', { value: '' }, hasDefault ? `default (${JSON.stringify(f.default)})` : 'null') : h('option', { value: '' }, 'choose…'),
            h('option', { value: 'true' }, 'true'), h('option', { value: 'false' }, 'false'));
          read = () => (el.value === '' ? undefined : el.value === 'true');
        } else if (['int', 'float', 'duration'].includes(base.kind)) {
          el = h('input', { type: 'number', id: fid(f.name), name: f.name, step: base.kind === 'float' ? 'any' : '1', placeholder: hasDefault ? 'default ' + JSON.stringify(f.default) : opt ? 'null' : '' });
          read = () => {
            const v = el.value.trim();
            if (v === '') return undefined;
            const n = Number(v);
            return Number.isFinite(n) ? n : v;
          };
        } else if (['string', 'uuid', 'timestamp', 'bytes'].includes(base.kind)) {
          el = h('input', { type: 'text', id: fid(f.name), name: f.name, autocomplete: 'off',
            placeholder: hasDefault ? 'default ' + JSON.stringify(f.default) : base.kind === 'timestamp' ? '2026-01-01T00:00:00Z' : opt ? 'null' : '' });
          read = () => (el.value === '' ? undefined : el.value);
        } else {
          el = h('textarea', { id: fid(f.name), name: f.name, rows: 3, spellcheck: 'false', placeholder: 'JSON' + (hasDefault ? ', default ' + JSON.stringify(f.default) : '') });
          read = () => {
            const v = el.value.trim();
            if (v === '') return undefined;
            try { return JSON.parse(v); } catch (e) { throw new Error(`${f.name}: not valid JSON (${e.message})`); }
          };
        }
        controls.push({ f, read });
        append(grid, h('label', { for: fid(f.name), class: 'field-label' }, f.name, (!opt && !hasDefault) ? h('span', { class: 'faint' }, ' *') : null),
          h('div', { class: 'field-input' }, el, h('span', { class: 'type' }, typeStr(f.type))));
      }
    } else {
      const el = h('textarea', { id: fid('payload'), rows: 3, spellcheck: 'false', placeholder: 'JSON payload' });
      controls.push({ f: null, read: () => JSON.parse(el.value || 'null') });
      append(grid, h('label', { for: fid('payload'), class: 'field-label' }, 'payload'), h('div', { class: 'field-input' }, el, h('span', { class: 'type' }, typeStr(type))));
    }
    const submit = h('button', { type: 'submit', class: 'btn btn-primary btn-small' }, 'Send signal');
    const form = h('form', { 'data-signal': name, novalidate: true },
      h('div', { class: 'row spread' }, h('span', null, 'Signal ', h('strong', { class: 'mono' }, name)), submit),
      h('div', { class: 'small faint mono wrap-any' }, typeStr(type)),
      grid, out);
    form.addEventListener('submit', async (ev) => {
      ev.preventDefault();
      let payload;
      try {
        if (fields) {
          payload = {};
          for (const c of controls) { const v = c.read(); if (v !== undefined) payload[c.f.name] = v; }
        } else payload = controls[0].read();
      } catch (e) {
        replace(out, notice('error', 'Invalid input.', e.message));
        return;
      }
      submit.disabled = true;
      try { await send(payload, out); } finally { submit.disabled = false; }
    });
    return form;
  }

  // -------------------------------------------------------------- workflows

  async function viewWorkflows(root, gen) {
    const r = await api('/v1/workflows');
    if (gen !== view.gen) return;
    const rows = r.workflows.slice().sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version, undefined, { numeric: true }))
      .map((w) => h('tr', { 'data-workflow': w.name },
        h('td', null, link(wfHref(w.name, w.version), w.name)),
        h('td', { class: 'mono' }, w.version),
        h('td', { class: 'small' }, w.description || ''),
        h('td', { class: 'num' }, String(w.steps.length)),
        h('td', { class: 'small' }, w.tasks.map((t) => h('span', { class: 'tag' }, t))),
        h('td', { class: 'mono small faint hide-sm' }, (w.definition_hash || '').replace('sha256:', '').slice(0, 12)),
        h('td', null, link('#/executions?workflow=' + enc(w.name), 'executions'))));
    append(root, header('Workflows', `${r.workflows.length} registered definition${r.workflows.length === 1 ? '' : 's'}`),
      table([{ label: 'Workflow' }, { label: 'Version' }, { label: 'Description' }, { label: 'Steps', cls: 'num' }, { label: 'Tasks' }, { label: 'Hash', cls: 'hide-sm' }, { label: '' }], rows, 'No workflows registered.'));
  }

  function fieldsTable(type) {
    if (!type || type.kind !== 'record') return h('p', { class: 'mono' }, typeStr(type));
    return table([{ label: 'Field' }, { label: 'Type' }, { label: 'Default' }],
      (type.fields || []).map((f) => h('tr', null, h('td', { class: 'mono' }, f.name), h('td', { class: 'mono small wrap-any' }, typeStr(f.type)),
        h('td', { class: 'mono small' }, f.default !== undefined ? JSON.stringify(f.default) : f.type.kind === 'optional' ? 'null' : 'required'))), 'No fields.');
  }

  function stepRows(def) {
    const all = [['', def.steps || []]];
    for (const [k, st] of Object.entries(def.handlers || {})) all.push(['on ' + k, st]);
    for (const [k, st] of Object.entries(def.events || {})) all.push(['on event ' + k, st]);
    const rows = [];
    for (const [lane, steps] of all) {
      for (const st of steps) {
        const inv = st.action && (st.action.type === 'task' || st.action.type === 'workflow') ? st.action : null;
        const retry = inv && inv.retry ? `${inv.retry.max_attempts}× ${inv.retry.backoff}` : '';
        const timeout = inv && inv.timeout_ms ? fmtMs(inv.timeout_ms) : st.action && st.action.timeout_ms ? fmtMs(st.action.timeout_ms) : '';
        rows.push(h('tr', null, h('td', { class: 'id' }, st.id), h('td', { class: 'small faint' }, lane || 'main'), h('td', { class: 'small' }, actionText(st)),
          h('td', { class: 'small mono' }, (st.dependencies || []).join(', ') || '—'),
          h('td', { class: 'small' }, st.condition ? 'yes' : ''),
          h('td', { class: 'small' }, retry), h('td', { class: 'small' }, timeout),
          h('td', { class: 'small mono' }, st.compensation ? st.compensation.task : '')));
      }
    }
    return table([{ label: 'Step' }, { label: 'Block' }, { label: 'Action' }, { label: 'After' }, { label: 'When' }, { label: 'Retry' }, { label: 'Timeout' }, { label: 'Compensation' }], rows);
  }

  /** A definition's `continue_as_new`: when it applies and the next run's input. */
  function continuePanel(def) {
    const can = def.continue_as_new;
    const declared = def.input && def.input.kind === 'record' ? def.input.fields || [] : [];
    let input;
    if (can.input && can.input.op === 'object') {
      const given = can.input.fields.map((f) => f.name);
      input = table([{ label: 'Next run’s input' }, { label: 'Value' }],
        can.input.fields.map((f) => h('tr', null, h('td', { class: 'mono' }, f.name), h('td', { class: 'mono small wrap-any' }, exprStr(f.value))))
          .concat(declared.filter((f) => !given.includes(f.name)).map((f) => h('tr', null, h('td', { class: 'mono' }, f.name),
            h('td', { class: 'small muted' }, f.default !== undefined ? 'default ' + JSON.stringify(f.default) : 'null')))));
    } else {
      input = h('p', { class: 'mono' }, 'with ' + exprStr(can.input));
    }
    return h('section', { class: 'panel', 'data-test': 'continue-as-new' }, h('h2', null, 'Continue-as-new'),
      h('p', null, can.condition
        ? ['When every step has finished and ', h('code', null, exprStr(can.condition)), ' is true, the run ends with status ', badge(CONTINUED),
          ' and the execution continues as a new run, under the same id, with the input below and an empty history. Otherwise the run completes.']
        : ['When every step has finished, the run ends with status ', badge(CONTINUED),
          ' and the execution always continues as a new run with the input below: it never completes by itself.']),
      input);
  }

  async function viewWorkflow(root, gen, name, version) {
    const [def, all] = await Promise.all([
      api('/v1/workflows/' + enc(name) + (version ? '/' + enc(version) : '')),
      api('/v1/workflows').catch(() => ({ workflows: [] })),
    ]);
    if (gen !== view.gen) return;
    const versions = all.workflows.filter((w) => w.name === def.name).map((w) => w.version).sort((a, b) => a.localeCompare(b, undefined, { numeric: true }));
    const kv = [['Version', def.version], ['Description', def.description || '—'], ['Definition hash', h('span', { class: 'mono' }, def.definition_hash)],
      ['IR version', def.ir_version], ['Timeout', def.timeout_ms ? fmtMs(def.timeout_ms) : '—']];
    if (def.limits) kv.push(['Limits', Object.entries(def.limits).map(([k, v]) => `${k} = ${v}`).join(', ')]);
    if (versions.length > 1) kv.push(['Versions', versions.map((v) => [v === def.version ? h('strong', null, v) : link(wfHref(def.name, v), v), ' '])]);
    const signals = Object.entries(def.signals || {});
    const queries = Object.entries(def.queries || {});
    const contracts = Object.values(def.contracts || {});
    append(root,
      header(def.name, [def.version, def.description ? ' · ' + def.description : ''], link('#/executions?workflow=' + enc(def.name), 'Executions')),
      h('div', { class: 'grid' },
        h('section', { class: 'panel' }, h('h2', null, 'Definition'), h('dl', { class: 'kv' }, kv.map(([k, v]) => [h('dt', null, k), h('dd', null, v)]))),
        h('section', { class: 'panel' }, h('h2', null, 'Signals and queries'),
          signals.length || queries.length ? table([{ label: 'Kind' }, { label: 'Name' }, { label: 'Type' }],
            [...signals.map(([n, t]) => h('tr', null, h('td', null, 'signal'), h('td', { class: 'mono' }, n), h('td', { class: 'mono small wrap-any' }, typeStr(t)))),
              ...queries.map(([n, q]) => h('tr', null, h('td', null, 'query'), h('td', { class: 'mono' }, n), h('td', { class: 'mono small wrap-any' }, typeStr(q.output_type))))])
            : h('p', { class: 'muted' }, 'None declared.'))),
      h('div', { class: 'grid' },
        h('section', { class: 'panel' }, h('h2', null, 'Input'), fieldsTable(def.input)),
        h('section', { class: 'panel' }, h('h2', null, 'Output'), fieldsTable(def.output))),
      h('section', { class: 'panel' }, h('h2', null, 'Graph'), graph(def, null), def.continue_as_new ? legend(def) : null),
      h('section', { class: 'panel' }, h('h2', null, 'Steps'), stepRows(def)),
      def.continue_as_new ? continuePanel(def) : null,
      h('section', { class: 'panel' }, h('h2', null, 'Task contracts'),
        table([{ label: 'Task' }, { label: 'Version' }, { label: 'Input' }, { label: 'Output' }, { label: 'Notes' }],
          contracts.map((c) => h('tr', null, h('td', { class: 'mono' }, c.name), h('td', { class: 'mono' }, c.version),
            h('td', { class: 'mono small wrap-any' }, typeStr(c.input)), h('td', { class: 'mono small wrap-any' }, typeStr(c.output)),
            h('td', { class: 'small' }, [c.builtin ? 'built in' : '', c.compensated_by ? 'compensated by ' + c.compensated_by : '', c.workflow_hash ? 'child workflow' : ''].filter(Boolean).join('; '))))))
    );
  }

  // -------------------------------------------------------------- schedules

  function scheduleTiming(sc) {
    if (sc.cron) return `cron "${sc.cron}"` + (sc.timezone ? ' ' + sc.timezone : ' UTC');
    if (sc.interval) return 'every ' + sc.interval;
    return '—';
  }

  /** ACTIVE, PAUSED or ENDED (past `end_at`: no next occurrence). */
  function scheduleState(sc) {
    if (sc.paused) return 'PAUSED';
    return sc.next_fire_at ? 'ACTIVE' : 'ENDED';
  }

  function isoCell(s) { return timeCell(isoMs(s)); }

  async function viewSchedules(root, gen) {
    const body = h('div');
    append(root, header('Schedules', null, h('a', { class: 'btn btn-small', href: '#/schedules?new=1', 'data-test': 'schedule-new' }, 'New schedule'), autoToggle()), body);
    const load = async () => {
      let r;
      try { r = await api('/v1/schedules'); } catch (e) { if (gen === view.gen) replace(body, errorNotice(e)); return; }
      if (gen !== view.gen) return;
      const rows = r.schedules.map((sc) => h('tr', { 'data-schedule': sc.schedule_id },
        h('td', { class: 'id nowrap' }, link(schedHref(sc.schedule_id), sc.schedule_id)),
        h('td', null, link(wfHref(sc.workflow, sc.version), sc.workflow), h('span', { class: 'faint' }, ' ' + (sc.version || 'latest'))),
        h('td', { class: 'mono small' }, scheduleTiming(sc)),
        h('td', null, badge(scheduleState(sc), SCHED_CLASS)),
        isoCell(sc.next_fire_at),
        isoCell(sc.last_fire_at),
        h('td', { class: 'id' }, sc.last_execution_id ? link(execHref(sc.last_execution_id), sc.last_execution_id) : '—'),
        h('td', { class: 'num' }, String(sc.fired)),
        h('td', { class: 'num' }, String(sc.skipped)),
        h('td', { class: 'small wrap-any' }, sc.last_error ? h('span', { class: 'ev-bad' }, sc.last_error) : '')));
      replace(body,
        table([{ label: 'Schedule' }, { label: 'Workflow' }, { label: 'Timing' }, { label: 'State' }, { label: 'Next fire' }, { label: 'Last fire' }, { label: 'Last execution' }, { label: 'Fired', cls: 'num' }, { label: 'Skipped', cls: 'num' }, { label: 'Last error' }],
          rows, 'No schedules. Create one with New schedule, orbit schedule create or POST /v1/schedules.'),
        h('p', { class: 'small faint' }, 'Times are in this browser’s time zone. A client identity sees the schedules of the workflows it may schedule.'));
    };
    await load();
    every(5000, () => { if (autoRefresh()) load(); });
  }

  async function viewSchedule(root, gen, id) {
    let sc = await api('/v1/schedules/' + enc(id));
    if (gen !== view.gen) return;
    const badgeSlot = h('span');
    const spec = h('section', { class: 'panel', 'data-test': 'schedule-spec' });
    const state = h('section', { class: 'panel', 'data-test': 'schedule-state' });
    const inputPanel = h('section', { class: 'panel' });
    const actions = h('section', { class: 'panel', 'data-test': 'schedule-actions' });
    const runs = h('section', { class: 'panel', 'data-test': 'schedule-executions' });
    append(root, header(id, h('span', null, link(wfHref(sc.workflow, sc.version), sc.workflow), ' ', h('span', { class: 'faint' }, sc.version || 'latest version'), ' ', badgeSlot), autoToggle()),
      h('div', { class: 'grid' }, spec, actions),
      h('div', { class: 'grid' }, state, inputPanel),
      runs);

    const out = h('div', { class: 'result', 'data-test': 'schedule-result' });
    const pauseBtn = h('button', { type: 'button', class: 'btn btn-small', 'data-test': 'schedule-pause' }, 'Pause');
    const resumeBtn = h('button', { type: 'button', class: 'btn btn-small', 'data-test': 'schedule-resume' }, 'Resume');
    const triggerBtn = h('button', { type: 'button', class: 'btn btn-primary btn-small', 'data-test': 'schedule-trigger' }, 'Trigger now');
    const deleteBtn = h('button', { type: 'button', class: 'btn btn-danger btn-small', 'data-test': 'schedule-delete' }, 'Delete schedule…');
    const confirmBox = h('div', { class: 'confirm', hidden: true });
    const yes = h('button', { type: 'button', class: 'btn btn-danger btn-small', 'data-test': 'schedule-delete-confirm' }, 'Confirm delete');
    const no = h('button', { type: 'button', class: 'btn btn-small' }, 'Keep it');
    append(confirmBox, h('div', null, `Delete schedule ${id}? It stops firing at once. Executions it started are not affected. This cannot be undone.`), h('div', { class: 'row' }, yes, no));
    replace(actions, h('h2', null, 'Actions'), h('div', { class: 'actions-list' },
      h('div', null, h('div', { class: 'row spread' }, h('span', null, 'Pause or resume firing'), h('span', { class: 'row' }, pauseBtn, resumeBtn)),
        h('div', { class: 'small faint' }, 'Resuming continues with the next occurrence after now; occurrences missed while paused are not started.')),
      h('div', null, h('div', { class: 'row spread' }, h('span', null, 'Start an execution now'), triggerBtn),
        h('div', { class: 'small faint' }, 'With the schedule’s workflow, version and input, also while paused. The overlap policy applies.')),
      h('div', null, h('div', { class: 'row spread' }, h('span', null, 'Delete'), deleteBtn), confirmBox),
      out));

    const render = () => {
      replace(badgeSlot, badge(scheduleState(sc), SCHED_CLASS));
      pauseBtn.hidden = sc.paused;
      resumeBtn.hidden = !sc.paused;
      const kv = [
        ['Workflow', [link(wfHref(sc.workflow, sc.version), sc.workflow), ' ', sc.version || '(latest version at each fire)']],
        ['Timing', h('span', { class: 'mono' }, scheduleTiming(sc))],
        ['Overlap', sc.overlap], ['Catch-up', sc.catchup],
      ];
      if (sc.start_at) kv.push(['Start at', fmtTime(isoMs(sc.start_at))]);
      if (sc.end_at) kv.push(['End at', fmtTime(isoMs(sc.end_at))]);
      if (sc.jitter) kv.push(['Jitter', sc.jitter]);
      if (sc.description) kv.push(['Description', sc.description]);
      kv.push(['Created', fmtTime(isoMs(sc.created_at))], ['Updated', fmtTime(isoMs(sc.updated_at))]);
      replace(spec, h('h2', null, 'Schedule'), h('dl', { class: 'kv' }, kv.map(([k, v]) => [h('dt', null, k), h('dd', null, v)])));
      const next = isoMs(sc.next_fire_at);
      const kv2 = [
        ['State', badge(scheduleState(sc), SCHED_CLASS)],
        ['Next fire', next ? [fmtTime(next), h('span', { class: 'faint' }, ' (' + ago(next) + ')')] : '—'],
        ['Next occurrences', (sc.next_fire_times || []).length ? h('div', { class: 'small mono' }, sc.next_fire_times.map((t) => h('div', null, fmtTime(isoMs(t))))) : '—'],
        ['Last fire', fmtTime(isoMs(sc.last_fire_at))],
        ['Last execution', sc.last_execution_id ? link(execHref(sc.last_execution_id), sc.last_execution_id) : '—'],
        ['Fired', String(sc.fired)], ['Skipped', String(sc.skipped)],
        ['Last error', sc.last_error ? h('span', { class: 'ev-bad', 'data-test': 'schedule-error' }, sc.last_error) : '—'],
      ];
      replace(state, h('h2', null, 'State'), h('dl', { class: 'kv' }, kv2.map(([k, v]) => [h('dt', null, k), h('dd', null, v)])));
      replace(inputPanel, h('h2', null, 'Input'), json(sc.input));
    };

    const loadRuns = async () => {
      let list;
      try {
        list = (await api(`/v1/executions?workflow=${enc(sc.workflow)}&limit=${LIST_MAX}`)).executions;
      } catch (e) {
        if (gen === view.gen) replace(runs, h('h2', null, 'Executions started'), errorNotice(e));
        return;
      }
      if (gen !== view.gen) return;
      const mine = list.filter((e) => e.execution_id.startsWith(id + ':'));
      replace(runs, h('h2', null, 'Executions started'),
        table([{ label: 'Execution' }, { label: 'Status' }, { label: 'Started' }, { label: 'Duration', cls: 'num' }],
          mine.slice(0, PAGE_SIZE).map((e) => h('tr', { 'data-execution': e.execution_id },
            h('td', { class: 'id' }, link(execHref(e.execution_id), e.execution_id)), h('td', null, badge(e.status)), timeCell(e.created_at_ms),
            h('td', { class: 'num nowrap' }, fmtDur(durationOf(e))))), 'No executions of this schedule among the newest ' + LIST_MAX + ' of ' + sc.workflow + '.'),
        h('p', { class: 'small faint' }, 'Executions of ', sc.workflow, ' whose id starts with ', h('code', null, id + ':'), ' (occurrences and triggers), newest first.'));
    };

    const reload = async () => {
      const r = await api('/v1/schedules/' + enc(id));
      if (gen !== view.gen) return;
      sc = r;
      render();
      await loadRuns();
    };

    const act = async (btn, fn) => {
      btn.disabled = true;
      try { await fn(); } catch (e) { replace(out, errorNotice(e)); } finally { btn.disabled = false; }
    };
    pauseBtn.addEventListener('click', () => act(pauseBtn, async () => {
      sc = await api(`/v1/schedules/${enc(id)}/pause`, { method: 'POST' });
      replace(out, notice('', 'Paused.', 'The schedule does not fire until it is resumed.'));
      render();
    }));
    resumeBtn.addEventListener('click', () => act(resumeBtn, async () => {
      sc = await api(`/v1/schedules/${enc(id)}/resume`, { method: 'POST' });
      replace(out, notice('', 'Resumed.', sc.next_fire_at ? 'Next fire ' + fmtTime(isoMs(sc.next_fire_at)) + '.' : 'There is no next occurrence.'));
      render();
    }));
    triggerBtn.addEventListener('click', () => act(triggerBtn, async () => {
      const r = await api(`/v1/schedules/${enc(id)}/trigger`, { method: 'POST' });
      replace(out, r.started
        ? notice('', 'Started.', ['Execution ', link(execHref(r.execution_id), r.execution_id), '.'])
        : notice('denied', 'Not started.', `The previous execution of ${id} is still running (overlap: ${sc.overlap}).`));
      await reload();
    }));
    deleteBtn.addEventListener('click', () => { confirmBox.hidden = false; deleteBtn.hidden = true; });
    no.addEventListener('click', () => { confirmBox.hidden = true; deleteBtn.hidden = false; });
    yes.addEventListener('click', () => act(yes, async () => {
      await api(`/v1/schedules/${enc(id)}`, { method: 'DELETE' });
      stopView();
      replace(root, header(id, 'Deleted'), notice('', 'Deleted.', ['Schedule ', id, ' was deleted. ', link('#/schedules', 'All schedules')]));
    }));

    render();
    await loadRuns();
    every(3000, () => { if (autoRefresh()) reload().catch(() => {}); });
  }

  /** Create a schedule. The server validates everything (timing, time zone, input). */
  async function viewNewSchedule(root, gen) {
    const names = await api('/v1/workflows').then((r) => [...new Set(r.workflows.map((w) => w.name))].sort()).catch(() => []);
    if (gen !== view.gen) return;
    const f = (id, el, label, note) => [h('label', { for: id, class: 'field-label' }, label), h('div', { class: 'field-input' }, el, note ? h('span', { class: 'type' }, note) : null)];
    const sid = h('input', { type: 'text', id: 'ns-id', autocomplete: 'off', spellcheck: 'false', placeholder: 'nightly-report' });
    const wf = names.length ? h('select', { id: 'ns-workflow' }, names.map((n) => h('option', { value: n }, n))) : h('input', { type: 'text', id: 'ns-workflow', autocomplete: 'off' });
    const version = h('input', { type: 'text', id: 'ns-version', autocomplete: 'off', placeholder: 'latest' });
    const kind = h('select', { id: 'ns-kind' }, h('option', { value: 'cron' }, 'cron'), h('option', { value: 'interval' }, 'interval'));
    const timing = h('input', { type: 'text', id: 'ns-timing', autocomplete: 'off', spellcheck: 'false', placeholder: '0 2 * * *' });
    const tz = h('input', { type: 'text', id: 'ns-tz', autocomplete: 'off', placeholder: 'UTC' });
    const overlap = h('select', { id: 'ns-overlap' }, ['skip', 'allow', 'cancel_previous'].map((o) => h('option', { value: o }, o)));
    const catchup = h('select', { id: 'ns-catchup' }, ['skip_missed', 'run_once'].map((o) => h('option', { value: o }, o)));
    const input = h('textarea', { id: 'ns-input', rows: 4, spellcheck: 'false', value: '{}' });
    const desc = h('input', { type: 'text', id: 'ns-desc', autocomplete: 'off' });
    const paused = h('input', { type: 'checkbox', id: 'ns-paused' });
    kind.addEventListener('change', () => {
      timing.placeholder = kind.value === 'cron' ? '0 2 * * *' : '15m';
      tz.disabled = kind.value !== 'cron';
    });
    const out = h('div', { class: 'result' });
    const submit = h('button', { type: 'submit', class: 'btn btn-primary' }, 'Create schedule');
    const form = h('form', { class: 'panel form-panel', 'data-test': 'schedule-form', novalidate: true }, h('h2', null, 'New schedule'),
      h('div', { class: 'form-grid' },
        f('ns-id', sid, 'Schedule id', 'letters, digits, - _ . (at most 100)'),
        f('ns-workflow', wf, 'Workflow'),
        f('ns-version', version, 'Version', 'empty: the latest version at each fire'),
        f('ns-kind', kind, 'Timing'),
        f('ns-timing', timing, 'Cron or interval', 'cron: minute hour day-of-month month day-of-week; interval: 30s, 15m, 1h30m'),
        f('ns-tz', tz, 'Time zone', 'IANA name for cron, e.g. Europe/Berlin; empty: UTC'),
        f('ns-overlap', overlap, 'Overlap'),
        f('ns-catchup', catchup, 'Catch-up'),
        f('ns-input', input, 'Input', 'JSON, checked against the workflow input'),
        f('ns-desc', desc, 'Description'),
        f('ns-paused', paused, 'Create paused')),
      h('div', { class: 'row' }, submit, link('#/schedules', 'Cancel')), out);
    form.addEventListener('submit', async (ev) => {
      ev.preventDefault();
      let body;
      try {
        body = { schedule_id: sid.value.trim(), workflow: wf.value.trim(), input: JSON.parse(input.value.trim() || '{}'), overlap: overlap.value, catchup: catchup.value, paused: paused.checked };
      } catch (e) {
        replace(out, notice('error', 'Invalid input.', 'Input: not valid JSON (' + e.message + ')'));
        return;
      }
      body[kind.value] = timing.value.trim();
      if (version.value.trim()) body.version = version.value.trim();
      if (kind.value === 'cron' && tz.value.trim()) body.timezone = tz.value.trim();
      if (desc.value.trim()) body.description = desc.value.trim();
      submit.disabled = true;
      try {
        const r = await api('/v1/schedules', { method: 'POST', body });
        location.hash = schedHref(r.schedule_id);
      } catch (e) {
        replace(out, errorNotice(e));
      } finally { submit.disabled = false; }
    });
    append(root, header('New schedule', 'Validated by the server: the workflow must be registered, and the timing, time zone and input must be valid.'), form);
    sid.focus();
  }

  // ---------------------------------------------------------------- workers

  async function viewWorkers(root, gen) {
    const body = h('div');
    append(root, header('Workers', null, autoToggle()), body);
    const load = async () => {
      let r;
      try { r = await api('/v1/workers'); } catch (e) { if (gen === view.gen) replace(body, errorNotice(e)); return; }
      if (gen !== view.gen) return;
      const ws = r.workers.slice().sort((a, b) => (b.online - a.online) || a.worker_id.localeCompare(b.worker_id));
      replace(body, table([{ label: 'Worker' }, { label: 'State' }, { label: 'Runtime' }, { label: 'SDK', cls: 'hide-sm' }, { label: 'Sessions', cls: 'num' }, { label: 'In flight', cls: 'num' }, { label: 'Tasks served' }, { label: 'Registered', cls: 'hide-sm' }, { label: 'Last seen' }],
        ws.map((w) => h('tr', { 'data-worker': w.worker_id },
          h('td', { class: 'mono nowrap' }, w.worker_id),
          h('td', null, h('span', { class: 'badge ' + (w.online ? 's-ok' : 's-idle') }, w.online ? 'online' : 'offline')),
          h('td', null, w.runtime || '—'),
          h('td', { class: 'small nowrap hide-sm' }, w.sdk || '—'),
          h('td', { class: 'num' }, String(w.sessions ?? '—')),
          h('td', { class: 'num' }, String(w.inflight ?? '—')),
          h('td', { class: 'small' }, (w.contracts || []).map((c) => h('span', { class: 'tag', title: 'contract version ' + c.contract_version }, c.task_name))),
          h('td', { class: 'nowrap small hide-sm' }, fmtTime(w.registered_at_ms)),
          h('td', { class: 'nowrap small', title: fmtTime(w.last_seen_ms) }, ago(w.last_seen_ms)))), 'No workers have registered.'));
    };
    await load();
    every(5000, () => { if (autoRefresh()) load(); });
  }

  // ----------------------------------------------------------------- router

  function stopView() {
    view.timers.forEach(clearInterval);
    view = { gen: view.gen + 1, timers: [] };
  }

  function parseHash() {
    const raw = location.hash.replace(/^#/, '') || '/';
    const q = raw.indexOf('?');
    const path = q < 0 ? raw : raw.slice(0, q);
    const params = new URLSearchParams(q < 0 ? '' : raw.slice(q + 1));
    const parts = path.split('/').filter(Boolean).map((p) => { try { return decodeURIComponent(p); } catch (_) { return p; } });
    return { parts, params };
  }

  async function route() {
    stopView();
    const gen = view.gen;
    if (!session.get(MODE_KEY)) {
      // No session yet: an unauthenticated (development) server needs no token.
      try { await signIn(null); } catch (_) { viewLogin(); return; }
      if (gen !== view.gen) return;
    }
    setChrome(true);
    const { parts, params } = parseHash();
    const section = parts[0] || 'overview';
    document.querySelectorAll('#nav a').forEach((a) => {
      if (a.dataset.nav === section) a.setAttribute('aria-current', 'page'); else a.removeAttribute('aria-current');
    });
    const root = h('div', { 'data-view': section });
    replace(main(), root);
    const titles = { overview: 'Overview', executions: 'Executions', workflows: 'Workflows', schedules: 'Schedules', workers: 'Workers' };
    document.title = (parts[1] ? parts[1] + ' · ' : '') + (titles[section] || 'ORBIT') + ' · ORBIT';
    try {
      if (section === 'overview') await viewOverview(root, gen);
      else if (section === 'executions' && parts[1]) await viewExecution(root, gen, parts[1], params);
      else if (section === 'executions') await viewExecutions(root, gen, params);
      else if (section === 'workflows' && parts[1]) await viewWorkflow(root, gen, parts[1], parts[2]);
      else if (section === 'workflows') await viewWorkflows(root, gen);
      else if (section === 'schedules' && parts[1]) await viewSchedule(root, gen, parts[1]);
      else if (section === 'schedules' && params.has('new')) await viewNewSchedule(root, gen);
      else if (section === 'schedules') await viewSchedules(root, gen);
      else if (section === 'workers') await viewWorkers(root, gen);
      else append(root, notice('error', 'Not found.', 'There is no page at ' + location.hash));
    } catch (e) {
      if (gen !== view.gen) return;
      if (e instanceof ApiError && e.status === 401) { signOut('The server rejected the token (401). Sign in again.'); return; }
      append(root, errorNotice(e));
    }
  }

  function boot() {
    document.getElementById('signout').addEventListener('click', () => signOut());
    window.addEventListener('hashchange', route);
    route();
  }

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', boot); else boot();
})();
