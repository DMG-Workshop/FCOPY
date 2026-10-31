// Browser-device preferences only. Never sent to the console host.
const FCOPYAppearance = (() => {
  const key = 'fcopy.appearance.v1';
  const defaults = () => ({ theme: 'system', warmth: 0, night: 'off', nightWarmth: 35, start: '20:00', end: '07:00' });
  const themes = ['system', 'light', 'dark', 'low_contrast', 'high_contrast'];
  const minutes = value => {
    if (typeof value !== 'string' || !/^\d{2}:\d{2}$/.test(value)) return null;
    const [hour, minute] = value.split(':').map(Number);
    return hour < 24 && minute < 60 ? hour * 60 + minute : null;
  };
  function validate(p) {
    if (!p || typeof p !== 'object' || Array.isArray(p) || Object.keys(p).some(k => !Object.hasOwn(defaults(), k))) throw new Error('Unrecognized appearance settings.');
    if (!themes.includes(p.theme) || !['off', 'on', 'scheduled'].includes(p.night)) throw new Error('Choose a supported theme and Night Shift mode.');
    if (![p.warmth, p.nightWarmth].every(n => Number.isInteger(n) && n >= 0 && n <= 60)) throw new Error('Warmth must be between 0 and 60%.');
    const start = minutes(p.start), end = minutes(p.end);
    if (start === null || end === null) throw new Error('Schedule times must use HH:MM (24-hour).');
    if (start === end) throw new Error('Night Shift start and end must be different.');
    return p;
  }
  function warmth(p, now) {
    const start = minutes(p.start), end = minutes(p.end);
    const scheduled = start < end ? now >= start && now < end : now >= start || now < end;
    return Math.max(p.warmth, p.night === 'on' || (p.night === 'scheduled' && scheduled) ? p.nightWarmth : 0);
  }
  const media = matchMedia('(prefers-color-scheme: dark)');
  function apply(p, date = new Date()) {
    validate(p);
    const theme = p.theme === 'system' ? (media.matches ? 'dark' : 'light') : p.theme;
    document.documentElement.dataset.theme = theme;
    document.documentElement.style.setProperty('--warmth', warmth(p, date.getHours() * 60 + date.getMinutes()) / 100);
    return theme;
  }
  function load() {
    try {
      const saved = localStorage.getItem(key);
      return { preferences: saved === null ? defaults() : validate(JSON.parse(saved)), error: '' };
    } catch (e) {
      return { preferences: defaults(), error: `Appearance could not be loaded: ${e.message}. Defaults are previewed; saved settings have not been changed.` };
    }
  }
  function save(p) {
    validate(p);
    localStorage.setItem(key, JSON.stringify(p));
  }
  const initial = load();
  apply(initial.preferences);
  return { defaults, minutes, validate, warmth, apply, load, save, media, initial };
})();
