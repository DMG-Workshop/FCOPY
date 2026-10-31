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
    appearance: { ...FCOPYAppearance.initial.preferences },
    appearanceNotice: FCOPYAppearance.initial.error,
    appearanceTheme: '',
    monitorOpen: false,

    // mode, queue
    mode: 'local',
    queuePaused: false,
    filters: FILTERS,
    filter: 'all',
    search: '',
    selected: {},
    jobs: {},
    history: [],
    historySearch: '',
    historyNotice: '',
    historyBusy: false,
    historyPersistent: true,
    historyRequest: 0,
    recovered: [],
    recoveryPersistent: true,
    recoveryNotice: '',
    recoveryBusy: false,
    recoveryRequest: 0,
    recoverySelected: null,
    recoveryPassword: '',
    logs: [],
    logOpen: false,
    graphHistory: [],
    _lastPaint: 0,

    // modals and forms
    modal: null,
    conflictPolicy: 'ask',
    conflictAll: false,
    conflictAllId: null,
    conflictBusy: false,
    conflictNotice: '',
    addForm: { sources: '', dest: '', label: '', mode: 'local', clientId: '', password: '' },
    addNotice: '',
    remoteBrowser: { path: '', entries: [], busy: false, error: '' },
    settingsForm: { limitMiB: 0, policy: 'retry' },
    settingsNotice: '',
    settingsTab: 'appearance',
    vpnProviders: [],
    vpnProvider: 'nordvpn',
    vpnLoading: false,
    vpnNotice: '',
    vpnTarget: '',
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
      this.previewAppearance();
      FCOPYAppearance.media.addEventListener('change', () => this.previewAppearance());
      document.addEventListener('visibilitychange', () => {
        if (!document.hidden) this.previewAppearance();
      });
      setInterval(() => this.previewAppearance(), 30_000);
      const info = await fetch("/api/info", { credentials: "same-origin" });
      if (info.ok) {
        const body = await info.json();
        this.certificate = body.certificate;
        this.torrentsEnabled = body.torrents;
      }
      const r = await this.api('GET', '/api/me');
      if (r.ok) {
        this.resetHistory();
        this.user = await r.json();
        this.connect();
        if (this.isAdmin()) this.loadSettings();
        this.loadQueueState();
        this.loadRecovery();
      }
      this.paint(0);
    },

    previewAppearance() {
      try {
        this.appearanceTheme = FCOPYAppearance.apply(this.appearance);
      } catch (e) {
        this.appearanceNotice = `Appearance was not applied: ${e.message}`;
      }
    },

    saveAppearance() {
      try {
        FCOPYAppearance.save(this.appearance);
        this.previewAppearance();
        this.appearanceNotice = 'Appearance saved in this browser on this device.';
      } catch (e) {
        this.appearanceNotice = `Appearance was not saved: ${e.message}`;
      }
    },

    queueOverview() {
      const jobs = Object.values(this.jobs);
      const total = jobs.reduce((n, j) => n + (j.bytes_total || 0), 0);
      const processed = jobs.reduce((n, j) => n + (j.bytes_total || 0) * Math.max(0, Math.min(100, j.percent || 0)) / 100, 0);
      const active = jobs.find(j => j.state === 'running' || j.state === 'paused');
      return { percent: total ? Math.round(processed / total * 100) : 0, total, processed,
        jobs: jobs.length, pending: jobs.some(j => j.state === 'pending'),
        active: active?.label || 'No active transfer', eta: active?.state === 'running' ? fmtEta(active.eta_secs) : '--' };
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
      this.resetHistory();
      this.user = await r.json();
      this.connect();
      this.loadRecovery();
      if (this.isAdmin()) this.loadSettings();
    },

    async signOut() {
      await this.api('POST', '/api/auth/logout');
      this.closeModal();
      this.disconnect();
      clearInterval(this.clientTimer);
      this.user = null;
      this.mode = 'local';
      this.jobs = {};
      this.resetHistory();
      this.resetRecovery();
      this.logs = [];
      this.selected = {};
      this.torrents = [];
      this.vpnTarget = '';
      this.conflictPolicy = 'ask';
      this.conflictAll = false;
      this.conflictAllId = null;
      this.conflictNotice = '';
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
            this.resetHistory();
            this.resetRecovery();
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
      if (this.mode === 'history' && !this.historyBusy && (frame.jobs || []).some((job) => !this.isActive(job.state))) {
        this.loadHistory();
      }
      if (!this.recoveryBusy && (frame.jobs || []).some((job) => !this.isActive(job.state))) {
        this.loadRecovery();
      }
    },

    // ---------- finished-job history ----------

    resetRecovery() {
      this.recovered = [];
      this.recoveryNotice = '';
      this.recoveryPassword = '';
      this.recoverySelected = null;
      this.recoveryBusy = false;
      ++this.recoveryRequest;
    },

    async loadRecovery() {
      const request = ++this.recoveryRequest;
      const user = this.user;
      this.recoveryBusy = true;
      this.recoveryNotice = '';
      try {
        const r = await this.api('GET', '/api/recovery');
        if (!r.ok) throw new Error(await errorText(r));
        const body = await r.json();
        if (this.user !== user || request !== this.recoveryRequest) return;
        this.recovered = body.jobs;
        this.recoveryPersistent = body.persistent;
        this.recoveryNotice = body.error || '';
      } catch (e) {
        if (this.user === user && request === this.recoveryRequest) this.recoveryNotice = `Cannot load recovery: ${e.message}`;
      } finally {
        if (this.user === user && request === this.recoveryRequest) this.recoveryBusy = false;
      }
    },

    recoveryRemote(job) {
      return ['upload', 'download'].includes(job.destination.kind);
    },

    recoveryDestination(job) {
      const d = job.destination;
      return d.kind === 'download' ? `${d.path} (from ${d.target})` : (d.target || d.path);
    },

    async recover(job, action) {
      if (this.recoveryBusy) return;
      if (action === 'discard' && !window.confirm(`Discard saved job "${job.label}"? Copied data and partial files are left untouched.`)) return;
      const user = this.user;
      const password = this.recoverySelected === job.id ? this.recoveryPassword : '';
      this.recoveryPassword = '';
      this.recoveryBusy = true;
      this.recoveryNotice = '';
      ++this.recoveryRequest;
      try {
        const r = await this.api('POST', `/api/recovery/${job.id}`, { action, password: action === 'discard' ? '' : password });
        if (!r.ok) throw new Error(await errorText(r));
        if (this.user !== user) return;
        this.recoverySelected = null;
        await this.loadRecovery();
      } catch (e) {
        if (this.user === user) this.recoveryNotice = `${action === 'discard' ? 'Job was not discarded' : 'Job was not started'}: ${e.message}`;
      } finally {
        if (this.user === user) {
          this.recoveryBusy = false;
          this.recoveryPassword = '';
          this.recoverySelected = null;
        }
      }
    },

    resetHistory() {
      this.history = [];
      this.historyNotice = '';
      this.historySearch = '';
      ++this.historyRequest;
      this.historyBusy = false;
      this.historyPersistent = true;
    },

    async loadHistory() {
      const request = ++this.historyRequest;
      const user = this.user;
      this.historyBusy = true;
      this.historyNotice = '';
      try {
        const r = await this.api('GET', '/api/history');
        if (!r.ok) throw new Error(await errorText(r));
        const body = await r.json();
        if (this.user !== user || request !== this.historyRequest) return;
        this.history = body.entries;
        this.historyPersistent = body.persistent;
      } catch (e) {
        if (this.user === user && request === this.historyRequest) this.historyNotice = `Cannot load history: ${e.message}`;
      } finally {
        if (this.user === user && request === this.historyRequest) this.historyBusy = false;
      }
    },

    visibleHistory() {
      const query = this.historySearch.trim().toLowerCase();
      return this.history.filter((entry) => !query ||
        [entry.label, entry.destination, entry.owner || '', ...entry.sources].some((text) => text.toLowerCase().includes(query)));
    },

    historyTime(ms) {
      return new Date(ms).toLocaleString();
    },

    async clearHistory() {
      if (this.historyBusy || !window.confirm(this.isAdmin()
        ? 'Clear all saved transfer history for every account? Files and queued transfers are not changed.'
        : 'Clear your saved transfer history? Files and queued transfers are not changed.')) return;
      const user = this.user;
      this.historyBusy = true;
      this.historyNotice = '';
      // Invalidate a previous refresh before clearing.
      ++this.historyRequest;
      try {
        const r = await this.api('DELETE', '/api/history');
        if (!r.ok) throw new Error(await errorText(r));
        if (this.user === user) await this.loadHistory();
      } catch (e) {
        if (this.user === user) this.historyNotice = `Cannot clear history: ${e.message}`;
      } finally {
        if (this.user === user) this.historyBusy = false;
      }
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
      if (name === 'settings') {
        if (this.isAdmin()) {
          this.loadUsers();
          this.loadVpnProviders();
        } else {
          this.settingsTab = 'appearance';
        }
      }
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
      this.addForm.password = '';
      this.pairForm.password = '';
      this.pairForm.confirm = null;
      this.resetRemoteBrowser();
    },

    resetRemoteBrowser() {
      this.remoteBrowser = { path: '', entries: [], busy: false, error: '' };
    },

    changeTransferMode() {
      this.addForm.sources = '';
      this.addForm.dest = '';
      this.resetRemoteBrowser();
    },

    changeRemoteClient() {
      if (this.addForm.mode === 'download') this.addForm.sources = '';
      this.addForm.password = '';
      this.resetRemoteBrowser();
    },

    async browseRemote(path = '') {
      if (this.remoteBrowser.busy) return;
      const state = this.remoteBrowser;
      state.error = '';
      if (!this.addForm.clientId || !this.addForm.password) {
        state.error = 'Choose a paired PC and enter its password or access key.';
        return;
      }
      state.busy = true;
      try {
        const r = await this.api('POST', `/api/clients/${this.addForm.clientId}/browse`, {
          password: this.addForm.password, path,
        });
        if (state !== this.remoteBrowser) return;
        if (!r.ok) {
          state.error = await errorText(r);
          return;
        }
        const listing = await r.json();
        if (state !== this.remoteBrowser) return;
        state.path = listing.path;
        state.entries = listing.entries;
      } catch (e) {
        if (state === this.remoteBrowser) state.error = `Cannot browse remote PC: ${e.message}`;
      } finally {
        state.busy = false;
      }
    },

    remoteEntryPath(entry) {
      return this.remoteBrowser.path ? `${this.remoteBrowser.path}/${entry.name}` : entry.name;
    },

    addRemoteSource(entry) {
      const path = this.remoteEntryPath(entry);
      const sources = this.addForm.sources.split('\n').filter(Boolean);
      if (!sources.includes(path)) sources.push(path);
      this.addForm.sources = sources.join('\n');
    },

    async setMode(m) {
      this.recoveryPassword = '';
      this.recoverySelected = null;
      this.mode = m;
      clearInterval(this.clientTimer);
      this.clientTimer = null;
      if (m === 'remote') {
        await this.loadClients();
        this.clientTimer = setInterval(() => this.loadClients(), 10000);
      }
      if (m === 'history') await this.loadHistory();
      if (m === 'recovery') await this.loadRecovery();
    },

    async create() {
      this.addNotice = '';
      const sources = this.addForm.sources.split('\n').map((s) => s.trim()).filter(Boolean);
      const body = { sources, dest: this.addForm.dest.trim(), label: this.addForm.label.trim() || null,
        conflict: this.conflictPolicy };
      if (this.addForm.mode === 'remote' || this.addForm.mode === 'download') {
        if (!this.addForm.clientId) {
          this.addNotice = 'Choose a paired PC.';
          return;
        }
        body.remote = { client_id: Number(this.addForm.clientId), password: this.addForm.password,
          direction: this.addForm.mode === 'download' ? 'download' : 'upload' };
      }
      let r;
      try {
        r = await this.api('POST', '/api/transfers', body);
      } catch (e) {
        this.addNotice = `Cannot queue transfer: ${e.message}`;
        return;
      } finally {
        this.addForm.password = '';
      }
      if (r.status === 202) {
        this.addForm = { sources: '', dest: this.addForm.dest, label: '', mode: this.addForm.mode, clientId: this.addForm.clientId, password: '' };
        this.closeModal();
      } else {
        this.addNotice = await errorText(r);
      }
    },

    pendingConflict() {
      return Object.values(this.jobs).find((job) => job.conflict) || null;
    },

    async resolveConflict(action) {
      const job = this.pendingConflict();
      if (!job || this.conflictBusy) {
        this.conflictNotice = 'No conflict is currently available to answer.';
        return;
      }
      const id = job.conflict.id;
      this.conflictBusy = true;
      this.conflictNotice = '';
      try {
        const r = await this.api('POST', `/api/transfers/${job.id}/conflict`, {
          conflict_id: id, action, apply_to_all: this.conflictAll && this.conflictAllId === id,
        });
        if (!r.ok) {
          this.conflictNotice = await errorText(r);
          return;
        }
        if (this.jobs[job.id]?.conflict?.id === id) this.jobs[job.id].conflict = null;
        this.conflictAll = false;
      } catch (e) {
        this.conflictNotice = `Cannot answer conflict: ${e.message}`;
      } finally {
        this.conflictBusy = false;
      }
    },

    // ---------- engine settings ----------

    async loadVpnProviders() {
      if (this.vpnLoading) return;
      this.vpnLoading = true;
      this.vpnNotice = '';
      try {
        const r = await this.api('GET', '/api/settings/vpn');
        if (!r.ok) {
          this.vpnNotice = await errorText(r);
          return;
        }
        this.vpnProviders = await r.json();
      } catch (e) {
        this.vpnNotice = `Cannot load VPN services: ${e.message}`;
      } finally {
        this.vpnLoading = false;
      }
    },

    vpnSelected() {
      return this.vpnProviders.find((p) => p.id === this.vpnProvider);
    },

    pairOverVpn() {
      const provider = this.vpnSelected();
      if (!provider?.mesh || !this.vpnTarget.trim()) {
        this.vpnNotice = 'Select Tailscale or ZeroTier and enter the remote VPN target.';
        return;
      }
      this.pairForm = { name: `${provider.name} PC`, target: this.vpnTarget.trim(), password: '', confirm: null };
      this.openModal('pair');
    },

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
