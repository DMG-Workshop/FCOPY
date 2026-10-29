// fcopy dashboard. Alpine.js renders the page; one WebSocket feeds it.
// Text always goes through x-text, never innerHTML, so file names can't inject markup.

const CSRF_HEADERS = { 'X-Fcopy-Request': '1' };
const RATE_SAMPLES = 60; // about 6 s of history at 10 Hz
const LOG_LIMIT = 200;

async function errorText(r) {
  try {
    return (await r.json()).error || `Request failed (${r.status})`;
  } catch {
    return `Request failed (${r.status})`;
  }
}

function fmtRate(bps) {
  if (!bps || bps < 1) return '0 B/s';
  const units = ['B/s', 'KiB/s', 'MiB/s', 'GiB/s'];
  let i = 0;
  let v = bps;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v < 10 && i > 0 ? 1 : 0)} ${units[i]}`;
}

function fmtEta(secs) {
  if (secs == null) return '—';
  const s = Math.round(secs);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, '0')}s`;
  return `${Math.floor(s / 3600)}h ${String(Math.floor((s % 3600) / 60)).padStart(2, '0')}m`;
}

const STATE_LABELS = {
  pending: 'Waiting',
  running: 'Copying',
  paused: 'Paused',
  completed: 'Done',
  completed_with_errors: 'Done with errors',
  failed: 'Failed',
  cancelled: 'Cancelled',
};

function drawSpark(canvas, values) {
  const ctx = canvas.getContext('2d');
  const { width: w, height: h } = canvas;
  ctx.clearRect(0, 0, w, h);
  if (values.length < 2) return;
  const max = Math.max(1, ...values);
  ctx.beginPath();
  values.forEach((v, i) => {
    const x = (i / (RATE_SAMPLES - 1)) * w;
    const y = h - (v / max) * (h - 4) - 2;
    if (i === 0) ctx.moveTo(x, y);
    else ctx.lineTo(x, y);
  });
  ctx.strokeStyle = '#10b981';
  ctx.lineWidth = 2;
  ctx.stroke();
}

function fcopy() {
  return {
    user: null,
    live: false,
    busy: false,
    error: '',
    notice: '',
    form: { user: '', password: '' },
    newJob: { sources: '', dest: '', label: '' },
    jobs: {},
    logs: [],
    // Plain (non-reactive) map so 10 Hz samples don't re-render the page.
    history: {},
    socket: null,
    retry: 1000,

    async start() {
      const r = await this.api('GET', '/api/me');
      if (r.ok) {
        this.user = await r.json();
        this.connect();
      }
      this.paint(0);
    },

    api(method, url, body) {
      const headers = method === 'GET' ? {} : { ...CSRF_HEADERS };
      if (body !== undefined) headers['Content-Type'] = 'application/json';
      return fetch(url, {
        method,
        headers,
        body: body === undefined ? undefined : JSON.stringify(body),
        credentials: 'same-origin',
      });
    },

    async login() {
      this.busy = true;
      this.error = '';
      const r = await this.api('POST', '/api/auth/login', { user: this.form.user, password: this.form.password });
      this.busy = false;
      this.form.password = '';
      if (!r.ok) {
        this.error = r.status === 401 ? 'Wrong username or password.' : await errorText(r);
        return;
      }
      this.user = await r.json();
      this.connect();
    },

    async logout() {
      await this.api('POST', '/api/auth/logout');
      this.disconnect();
      this.user = null;
      this.jobs = {};
      this.logs = [];
      this.history = {};
    },

    connect() {
      this.disconnect();
      const proto = location.protocol === 'https:' ? 'wss' : 'ws';
      const ws = new WebSocket(`${proto}://${location.host}/ws/telemetry`);
      this.socket = ws;
      ws.onopen = () => {
        this.live = true;
        this.retry = 1000;
      };
      ws.onmessage = (e) => this.apply(JSON.parse(e.data));
      ws.onclose = () => {
        this.live = false;
        if (!this.user || this.socket !== ws) return;
        setTimeout(async () => {
          // Session may have expired while we were away; check before reconnecting.
          const r = await this.api('GET', '/api/me');
          if (!r.ok) {
            this.user = null;
            return;
          }
          this.connect();
        }, this.retry);
        this.retry = Math.min(this.retry * 2, 15000);
      };
    },

    disconnect() {
      const ws = this.socket;
      this.socket = null;
      this.live = false;
      if (ws) ws.close();
    },

    // One frame: changed jobs, removed ids, and new log lines.
    apply(frame) {
      for (const job of frame.jobs || []) {
        this.jobs[job.id] = job;
        const h = this.history[job.id] || [];
        h.push(job.rate_bps);
        if (h.length > RATE_SAMPLES) h.shift();
        this.history[job.id] = h;
      }
      for (const id of frame.removed || []) {
        delete this.jobs[id];
        delete this.history[id];
      }
      if (frame.logs && frame.logs.length) {
        this.logs = [...frame.logs.reverse(), ...this.logs].slice(0, LOG_LIMIT);
      }
    },

    jobList() {
      return Object.values(this.jobs).sort((a, b) => b.id - a.id);
    },

    async create() {
      this.notice = '';
      const sources = this.newJob.sources.split('\n').map((s) => s.trim()).filter(Boolean);
      const body = {
        sources,
        dest: this.newJob.dest.trim(),
        label: this.newJob.label.trim() || null,
      };
      const r = await this.api('POST', '/api/transfers', body);
      if (r.status === 202) {
        this.notice = 'Transfer started.';
        this.newJob = { sources: '', dest: this.newJob.dest, label: '' };
      } else {
        this.notice = await errorText(r);
      }
    },

    async act(job, verb) {
      const r = await this.api('POST', `/api/transfers/${job.id}/${verb}`);
      if (!r.ok) this.notice = await errorText(r);
    },

    async cancel(job) {
      const r = await this.api('DELETE', `/api/transfers/${job.id}`);
      if (r.ok) {
        delete this.jobs[job.id];
        delete this.history[job.id];
      } else {
        this.notice = await errorText(r);
      }
    },

    stateLabel(state) {
      return STATE_LABELS[state] || state;
    },
    fmtRate,
    fmtEta,

    // Repaint sparklines at 10 Hz, independent of frame arrival.
    paint(now) {
      if (now - (this._lastPaint || 0) >= 100) {
        this._lastPaint = now;
        for (const c of document.querySelectorAll('canvas[data-job]')) {
          drawSpark(c, this.history[c.dataset.job] || []);
        }
      }
      requestAnimationFrame((t) => this.paint(t));
    },
  };
}
