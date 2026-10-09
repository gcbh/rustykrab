/* Shared browser access. Tailscale identity never creates or stores a token. */
'use strict';
window.RustyKrabAccess = (() => {
  const key = 'rustykrab_access_token', pauseKey = 'rustykrab_access_paused';
  let current = null, token = '';
  const read = (storage, name) => { try { return window[storage].getItem(name) || ''; } catch (_) { return ''; } };
  const write = (name, value) => { try { if (value) sessionStorage.setItem(name, value); else sessionStorage.removeItem(name); } catch (_) {} };
  function clearTokens() {
    token = '';
    for (const [storage, name] of [['sessionStorage', key], ['sessionStorage', 'rustykrab_monitor_token'], ['localStorage', 'rustykrab_token']]) {
      try { window[storage].removeItem(name); } catch (_) {}
    }
  }
  function message(status) {
    if (status?.authenticated) return status.method === 'tailscale'
      ? 'Connected with Tailscale · ' + status.identity : 'Connected · ' + status.identity;
    if (status?.disconnected) return 'This view is disconnected. Choose Connect to reopen your systems.';
    return status?.tailscale_enabled
      ? 'Turn on Tailscale on this device, using your authorised account, then choose Connect. Use this same address on your phone or computer.'
      : 'Connect this browser with a one-time pairing code or an existing access token.';
  }
  async function status(value = '') {
    const response = await fetch('/api/access', {
      headers: value ? {Authorization: 'Bearer ' + value} : {},
      cache: 'no-store', signal: AbortSignal.timeout(12000)
    });
    if (!response.ok) throw new Error('Could not check access (' + response.status + '). Check Tailscale and try again.');
    return response.json();
  }
  async function connect(automatic = false) {
    if (automatic && read('sessionStorage', pauseKey)) return {authenticated: false, disconnected: true};
    write(pauseKey, '');
    const identity = await status();
    if (identity.authenticated) {
      clearTokens(); current = identity; return identity;
    }
    const saved = token || read('sessionStorage', key) || read('sessionStorage', 'rustykrab_monitor_token') || read('localStorage', 'rustykrab_token');
    if (saved) {
      const result = await status(saved);
      if (result.authenticated) {
        clearTokens(); token = saved; write(key, token); current = result; return result;
      }
      clearTokens();
    }
    current = null; return identity;
  }
  async function connectToken(value) {
    value = value.trim();
    if (!value) throw new Error('Enter an access token.');
    const result = await status(value);
    if (!result.authenticated) throw new Error('This token was not accepted. Try Tailscale or a new pairing code.');
    clearTokens(); token = value; write(key, token); write(pauseKey, ''); current = result; return result;
  }
  async function pair(code, deviceName) {
    const response = await fetch('/api/pair', {
      method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({code: code.trim().toUpperCase(), deviceName: deviceName.trim()}),
      cache: 'no-store', signal: AbortSignal.timeout(12000)
    });
    if (!response.ok) throw new Error('Pairing was refused. The code may have expired or been used; request a new code.');
    const paired = await response.json();
    if (!/^[0-9a-f]{64}$/.test(paired.deviceToken || '')) throw new Error('The pairing response was unreadable.');
    // Save before a subsequent read can fail: a consumed code cannot be retried.
    clearTokens(); token = paired.deviceToken; write(key, token);
    return connectToken(token);
  }
  function disconnect() { current = null; clearTokens(); write(pauseKey, '1'); }
  return {connect, connectToken, pair, disconnect, message,
    get connected() { return Boolean(current?.authenticated); },
    get method() { return current?.method; },
    get identity() { return current?.identity; },
    headers() { return token ? {Authorization: 'Bearer ' + token} : {}; }
  };
})();
