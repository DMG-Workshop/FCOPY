// fcopy console. Alpine.js renders the page; one WebSocket feeds the queue table.
// Text always goes through x-text, never innerHTML, so names and paths can't inject markup.

const CSRF_HEADERS = { 'X-Fcopy-Request': '1' };
const MiB = 1024 * 1024;
const GRAPH_SAMPLES = 600; // 60 s at 10 Hz
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

function fmtBytes(b) {
  if (!b) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  let v = b;
  while (v >= 1000 && i < units.length - 1) {
    v /= 1000;
    i++;
  }
  return `${v.toFixed(v < 10 && i > 0 ? 2 : i > 0 ? 1 : 0)} ${units[i]}`;
}

function fmtEta(secs) {
  if (secs == null) return '—';
  const s = Math.round(secs);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, '0')}s`;
  return `${Math.floor(s / 3600)}h ${String(Math.floor((s % 3600) / 60)).padStart(2, '0')}m`;
}

const STATE_LABELS = {
  pending: 'Queued',
  running: 'Copying',
  paused: 'Paused',
  completed: 'Done',
  completed_with_errors: 'Done with errors',
  failed: 'Failed',
  cancelled: 'Cancelled',
};

const STATUS_LABELS = { online: 'online', offline: 'offline', identity_changed: 'identity changed' };

const TORRENT_PHASE_LABELS = {
  waiting: 'Waiting for peers',
  checking: 'Checking',
  downloading: 'Downloading',
  seeding: 'Seeding',
  paused: 'Paused',
  failed: 'Failed',
};

const FILTERS = [
  { id: 'all', label: 'All items' },
  { id: 'running', label: 'Copying' },
  { id: 'pending', label: 'Queued' },
  { id: 'paused', label: 'Paused' },
  { id: 'done', label: 'Done' },
  { id: 'errors', label: 'Errors' },
];

function fcopy() {
  return {
    // session
    user: null,
    form: { user: '', password: '' },
    error: "",
    busy: false,
    certificate: null,
    socket: null,
    retry: 1000,
    live: false,

    // mode, queue
    mode: 'local',
    queuePaused: false,
    filters: FILTERS,
    filter: 'all',
    search: '',
    selected: {},
    jobs: {},
    logs: [],
    logOpen: false,
    graphHistory: [],
    _lastPaint: 0,

    // modals and forms
    modal: null,
    addForm: { sources: '', dest: '', label: '', mode: 'local', clientId: '', password: '' },
    addNotice: '',
    settingsForm: { limitMiB: 0, policy: 'retry' },
    settingsNotice: '',
    users: [],
    userForm: { name: '', password: '', role: 'user' },
    userNotice: '',
    clients: [],
    pairForm: { name: '', target: '', password: '', confirm: null },
    pairNotice: '',
    clientTimer: null,

    // BitTorrent (admins only; rows arrive in the telemetry frames)
    torrentsEnabled: false,
    torrents: [],
    torrentForm: { magnet: '', peers: '', file: null },
    torrentNotice: '',

    async start() {
      const info = await fetch("/api/info", { credentials: "same-origin" });
      if (info.ok) {
        const body = await info.json();
        this.certificate = body.certificate;
        this.torrentsEnabled = body.torrents;
      }
      const r = await this.api('GET', '/api/me');
      if (r.ok) {
        this.user = await r.json();
        this.connect();
        if (this.isAdmin()) this.loadSettings();
        this.loadQueueState();
      }
      this.paint(0);
    },

    isAdmin() {
      return this.user?.role === 'admin';
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

    // ---------- session ----------

    async login() {
      this.busy = true;
      this.error = '';
      const r = await this.api('POST', '/api/auth/login', { user: this.form.user, password: this.form.password });
      this.busy = false;
      this.form.password = '';
      if (!r.ok) {
        this.error = r.status === 401 ? 'Wrong account or password.' : await errorText(r);
        return;
      }
      this.user = await r.json();
      this.connect();
      if (this.isAdmin()) this.loadSettings();
    },

    async signOut() {
      await this.api('POST', '/api/auth/logout');
      this.disconnect();
      clearInterval(this.clientTimer);
      this.user = null;
      this.mode = 'local';
      this.jobs = {};
      this.logs = [];
      this.selected = {};
      this.torrents = [];
    },

    // ---------- live queue ----------

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
          // The session may have ended while we were away; check before reconnecting.
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

    apply(frame) {
      for (const job of frame.jobs || []) this.jobs[job.id] = job;
      for (const id of frame.removed || []) {
        delete this.jobs[id];
        delete this.selected[id];
      }
      if (frame.logs && frame.logs.length) {
        this.logs = [...frame.logs.reverse(), ...this.logs].slice(0, LOG_LIMIT);
      }
      // The whole list arrives whenever it changes; absent means unchanged.
      if (frame.torrents) this.torrents = frame.torrents;
    },

    // ---------- BitTorrent ----------

    torrentPhaseLabel(phase) {
      return TORRENT_PHASE_LABELS[phase] || phase;
    },

    torrentPhaseClass(phase) {
      if (phase === 'failed') return 'text-bad';
      if (phase === 'seeding') return 'text-ok';
      if (phase === 'downloading') return 'text-brand-light';
      if (phase === 'paused') return 'text-warn';
      return 'text-dim';
    },

    async torrentAct(t, verb) {
      const r = await this.api('POST', `/api/torrents/${t.id}/${verb}`);
      if (!r.ok) this.notify(await errorText(r));
    },

    async removeTorrent(t, deleteFiles) {
      const msg = deleteFiles
        ? `Remove ${t.name} and delete its downloaded files? This cannot be undone.`
        : `Remove ${t.name}? It stops; the downloaded files stay on disk.`;
      if (!window.confirm(msg)) return;
      const r = await this.api('DELETE', `/api/torrents/${t.id}?delete_files=${deleteFiles}`);
      if (!r.ok) this.notify(await errorText(r));
    },

    async addTorrent() {
      this.torrentNotice = '';
      const magnet = this.torrentForm.magnet.trim();
      const file = this.torrentForm.file;
      let r;
      const peers = this.torrentForm.peers.trim();
      if (magnet) {
        r = await this.api('POST', '/api/torrents/magnet', { magnet, peers: peers || null });
      } else if (file) {
        const query = peers ? `?peers=${encodeURIComponent(peers)}` : '';
        // The body is the raw file, not JSON.
        r = await fetch(`/api/torrents/file${query}`, {
          method: 'POST',
          headers: { ...CSRF_HEADERS, 'Content-Type': 'application/x-bittorrent' },
          body: file,
          credentials: 'same-origin',
        });
      } else {
        this.torrentNotice = 'Paste a magnet link or choose a .torrent file.';
        return;
      }

      if (r.status === 201) {
        // A magnet link is listed at once as waiting for peers, then downloads under the same row.
        this.resetTorrentForm();
        this.closeModal();
      } else {
        this.torrentNotice = await errorText(r);
      }
    },

    resetTorrentForm() {
      this.torrentForm = { magnet: '', peers: '', file: null };
      if (this.$refs.torrentFile) this.$refs.torrentFile.value = '';
    },

    allJobs() {
      return Object.values(this.jobs).sort((a, b) => b.id - a.id);
    },

    matchesFilter(job, id) {
      switch (id) {
        case 'running': return job.state === 'running';
        case 'pending': return job.state === 'pending';
        case 'paused': return job.state === 'paused';
        case 'done': return job.state === 'completed' || job.state === 'completed_with_errors';
        case 'errors': return job.state === 'failed' || job.state === 'completed_with_errors' || job.files_failed > 0;
        default: return true;
      }
    },

    visibleJobs() {
      const q = this.search.trim().toLowerCase();
      return this.allJobs().filter((j) => this.matchesFilter(j, this.filter)
        && (!q || j.label.toLowerCase().includes(q) || (j.current_file || '').toLowerCase().includes(q)));
    },

    countFor(id) {
      return this.allJobs().filter((j) => this.matchesFilter(j, id)).length;
    },

    isActive(state) {
      return state === 'pending' || state === 'running' || state === 'paused';
    },

    activeCount() {
      return this.allJobs().filter((j) => this.isActive(j.state)).length;
    },

    doneCount() {
      return this.countFor('done');
    },

    totalRate() {
      return this.allJobs().reduce((sum, j) => sum + (j.state === 'running' ? j.rate_bps : 0), 0);
    },

    totalBytes() {
      return this.allJobs().reduce((sum, j) => sum + (j.bytes_total || 0), 0);
    },

    selectedIds() {
      return Object.keys(this.selected).filter((id) => this.selected[id] && this.jobs[id]).map(Number);
    },

    allSelected() {
      const ids = this.visibleJobs().map((j) => j.id);
      return ids.length > 0 && ids.every((id) => this.selected[id]);
    },

    toggleAll(on) {
      for (const j of this.visibleJobs()) this.selected[j.id] = on;
    },

    stateLabel(state) {
      return STATE_LABELS[state] || state;
    },

    stateClass(state) {
      if (state === 'running') return 'text-brand-light';
      if (state === 'failed' || state === 'completed_with_errors') return 'text-bad';
      if (state === 'completed') return 'text-ok';
      if (state === 'paused') return 'text-warn';
      return 'text-dim';
    },

    fmtRate,
    fmtBytes,
    fmtEta,

    async act(job, verb) {
      const r = await this.api('POST', `/api/transfers/${job.id}/${verb}`);
      if (!r.ok) this.notify(await errorText(r));
    },

    async cancel(job) {
      const r = await this.api('DELETE', `/api/transfers/${job.id}`);
      if (r.ok) delete this.jobs[job.id];
      else this.notify(await errorText(r));
    },

    async loadQueueState() {
      const r = await this.api('GET', '/api/queue');
      if (r.ok) this.queuePaused = (await r.json()).paused;
    },

    async pauseAll() {
      const r = await this.api('POST', '/api/queue/pause');
      if (r.ok) this.queuePaused = true;
      else this.notify(await errorText(r));
    },

    async resumeAll() {
      const r = await this.api('POST', '/api/queue/resume');
      if (r.ok) this.queuePaused = false;
      else this.notify(await errorText(r));
    },

    notify(msg) {
      this.settingsNotice = msg;
    },

    // ---------- modals and modes ----------

    openModal(name) {
      this.modal = name;
      if (name === 'settings') this.loadUsers();
      if (name === 'add') {
        this.addNotice = '';
        if (this.isAdmin()) this.loadClients();
      }
      if (name === 'pair') {
        this.pairNotice = '';
        this.pairForm.confirm = null;
      }
      if (name === 'torrent') this.torrentNotice = '';
    },

    closeModal() {
      this.modal = null;
    },

    async setMode(m) {
      this.mode = m;
      clearInterval(this.clientTimer);
      this.clientTimer = null;
      if (m === 'remote') {
        await this.loadClients();
        this.clientTimer = setInterval(() => this.loadClients(), 10000);
      }
    },

    async create() {
      this.addNotice = '';
      const sources = this.addForm.sources.split('\n').map((s) => s.trim()).filter(Boolean);
      const body = { sources, dest: this.addForm.dest.trim(), label: this.addForm.label.trim() || null };
      if (this.addForm.mode === 'remote') {
        if (!this.addForm.clientId) {
          this.addNotice = 'Choose a paired PC.';
          return;
        }
        body.remote = { client_id: Number(this.addForm.clientId), password: this.addForm.password };
      }
      const r = await this.api('POST', '/api/transfers', body);
      // The password is cleared as soon as it has been sent, whatever the answer.
      this.addForm.password = '';
      if (r.status === 202) {
        this.addForm = { sources: '', dest: this.addForm.dest, label: '', mode: this.addForm.mode, clientId: this.addForm.clientId, password: '' };
        this.closeModal();
      } else {
        this.addNotice = await errorText(r);
      }
    },

    // ---------- engine settings ----------

    limitLabel() {
      const m = this.settingsForm.limitMiB;
      return m ? `${m} MiB/s` : 'Unlimited';
    },

    async loadSettings() {
      const r = await this.api('GET', '/api/settings');
      if (!r.ok) return;
      const s = await r.json();
      this.settingsForm = {
        limitMiB: s.speed_limit_bps ? Math.round(s.speed_limit_bps / MiB) : 0,
        policy: s.error_policy === 'skip' ? 'skip' : 'retry',
      };
    },

    async saveSettings() {
      const m = Number(this.settingsForm.limitMiB) || 0;
      const r = await this.api('PUT', '/api/settings', {
        speed_limit_bps: m ? m * MiB : null,
        error_policy: this.settingsForm.policy,
      });
      this.settingsNotice = r.ok ? 'Saved. Applies to the running queue.' : await errorText(r);
    },

    // ---------- accounts ----------

    async loadUsers() {
      const r = await this.api('GET', '/api/users');
      if (r.ok) this.users = await r.json();
    },

    async addUser() {
      this.userNotice = '';
      const r = await this.api('POST', '/api/users', { ...this.userForm });
      if (r.status === 201) {
        this.userNotice = `Added ${this.userForm.name}.`;
        this.userForm = { name: '', password: '', role: 'user' };
        await this.loadUsers();
      } else {
        this.userNotice = await errorText(r);
      }
    },

    async removeUser(u) {
      if (!window.confirm(`Remove account ${u.name}? Its sessions end immediately.`)) return;
      const r = await this.api('DELETE', `/api/users/${encodeURIComponent(u.name)}`);
      if (r.ok) await this.loadUsers();
      else this.userNotice = await errorText(r);
    },

    // ---------- remote nodes ----------

    async loadClients() {
      const r = await this.api('GET', '/api/clients');
      if (r.ok) this.clients = await r.json();
    },

    statusDot(status) {
      if (status === 'online') return 'bg-ok';
      if (status === 'identity_changed') return 'bg-bad';
      return 'bg-faint';
    },

    statusText(status) {
      if (status === 'online') return 'text-ok';
      if (status === 'identity_changed') return 'text-bad';
      return 'text-faint';
    },

    statusLabel(status) {
      return STATUS_LABELS[status] || status;
    },

    // Two steps: the first call returns the fingerprint to confirm, the second
    // pins it. The password is cleared on success and never kept.
    async pair(confirmed) {
      this.pairNotice = '';
      const body = { name: this.pairForm.name, target: this.pairForm.target, password: this.pairForm.password };
      if (typeof confirmed === 'string') body.expected_fingerprint = confirmed;
      const r = await this.api('POST', '/api/clients', body);
      if (r.status === 409) {
        const j = await r.json();
        if (j.needs_confirmation) {
          this.pairForm.confirm = j.fingerprint;
          this.pairNotice = 'Confirm the fingerprint below.';
          return;
        }
        this.pairNotice = j.error;
        return;
      }
      if (r.status === 201) {
        this.pairNotice = `Paired ${this.pairForm.name}.`;
        this.pairForm = { name: '', target: '', password: '', confirm: null };
        await this.loadClients();
        this.closeModal();
        return;
      }
      this.pairNotice = await errorText(r);
      this.pairForm.confirm = null;
    },

    async unpair(c) {
      if (!window.confirm(`Unpair ${c.name}? Its saved fingerprint is removed.`)) return;
      const r = await this.api('DELETE', `/api/clients/${c.id}`);
      if (r.ok) await this.loadClients();
    },

    // ---------- throughput graph ----------

    graphPath() {
      const h = this.graphHistory;
      if (h.length < 2) return '';
      const max = Math.max(MiB, ...h);
      return h
        .map((v, i) => {
          const x = (i / (GRAPH_SAMPLES - 1)) * 300;
          const y = 78 - (v / max) * 74;
          return `${i === 0 ? 'M' : 'L'}${x.toFixed(1)},${y.toFixed(1)}`;
        })
        .join(' ');
    },

    // Sample total throughput at 10 Hz, independent of frame arrival.
    paint(now) {
      if (now - this._lastPaint >= 100) {
        this._lastPaint = now;
        this.graphHistory.push(this.totalRate());
        if (this.graphHistory.length > GRAPH_SAMPLES) this.graphHistory.shift();
      }
      requestAnimationFrame((t) => this.paint(t));
    },
  };
}
