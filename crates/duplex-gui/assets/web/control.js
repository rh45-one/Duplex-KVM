const modes = {0: ['0', 'Idle'], 3: ['1', 'PC primary'], 1: ['1+', 'PC + display'], 2: ['2', 'Laptop primary']};
const appearance = document.getElementById('appearance');
let savedTheme;
try { savedTheme = localStorage.getItem('duplex-theme'); } catch (_) {}
appearance.value = savedTheme === 'light' ? 'light' : 'dark';
document.documentElement.dataset.theme = appearance.value;
appearance.addEventListener('change', () => {
  document.documentElement.dataset.theme = appearance.value;
  try { localStorage.setItem('duplex-theme', appearance.value); } catch (_) {}
});

async function postAction(url, body) {
  try {
    const res = await fetch(url, { method: 'POST', headers: body ? {'Content-Type': 'application/json'} : {}, body: body ? JSON.stringify(body) : undefined });
    const data = await res.json();
    document.getElementById('action-status').textContent = data.message || 'Command sent.';
    refreshStatus();
  } catch (e) {
    document.getElementById('action-status').textContent = 'Request failed: ' + e;
  }
}
function setMode(target) { postAction('/api/mode?target=' + encodeURIComponent(target)); }
function triggerPanic() { postAction('/api/panic'); }
function startDaemon() { postAction('/api/start-daemon'); }
function setupNetwork() { postAction('/api/setup-network'); }

window.addEventListener('keydown', (e) => {
  if (e.ctrlKey && e.altKey && e.shiftKey && e.key === 'Backspace') {
    e.preventDefault(); triggerPanic(); return;
  }
  if (e.target.matches('input, textarea, select')) return;
  if (e.ctrlKey && (e.altKey || e.metaKey)) {
    const k = e.key.toLowerCase();
    if (k === 'p') { e.preventDefault(); setMode('cycle'); }
    else if (k === '0') { e.preventDefault(); setMode('idle'); }
    else if (k === '1') { e.preventDefault(); setMode('pc_kvm'); }
    else if (k === '2') { e.preventDefault(); setMode('pc_extend'); }
    else if (k === '3') { e.preventDefault(); setMode('laptop'); }
  }
});

async function refreshStatus() {
  try {
    const res = await fetch('/api/status');
    const s = await res.json();
    const dBadge = document.getElementById('daemon-badge');
    if (s.daemon_online) {
      dBadge.className = 'badge ok';
      dBadge.textContent = 'DAEMON ONLINE (PID ' + s.daemon_pid + ')';
    } else {
      dBadge.className = 'badge err';
      dBadge.textContent = 'DAEMON OFFLINE';
    }
    const pBadge = document.getElementById('peer-badge');
    if (s.peer_connected) {
      pBadge.className = 'badge ok';
      pBadge.textContent = '1GbE PEER CONNECTED';
    } else {
      pBadge.className = 'badge warn';
      pBadge.textContent = s.link && !s.link.address_configured ? 'PC LINK UNAVAILABLE' : 'NO PEER HEARTBEAT';
    }
    document.getElementById('link-detail').textContent = s.link ? s.link.message +
      (s.link.daemon_outdated ? ' The daemon uses an older/different executable. Return to Idle and restart it to apply the current build.' : '') +
      (s.link.last_error ? ' Last error: ' + s.link.last_error : '') : '';
    const mode = modes[s.mode_code] || modes[0];
    document.querySelector('.panic').classList.toggle('armed', s.input_grab_active);
    document.getElementById('val-mode').textContent = mode[0];
    document.getElementById('mode-title').textContent = mode[1];
    document.getElementById('hero-state').textContent = s.daemon_online ? 'CURRENT MODE' : 'DAEMON OFFLINE / LAST MODE';
    document.getElementById('sub-mode').textContent = s.mode_subtitle;
    document.getElementById('val-grab').textContent =
      'Grab: ' + (s.input_grab_active ? 'ACTIVE' : 'OFF') + ' | Displays: ' + s.virtual_displays_active;
    document.getElementById('sub-hb').textContent =
      'HB: ' + s.daemon_age_ms.toFixed(1) + ' / ' + s.peer_age_ms.toFixed(1) + ' ms';
    document.getElementById('val-rtt').textContent =
      s.min_rtt_us.toFixed(1) + ' / ' + s.last_rtt_us.toFixed(1) + ' / ' + s.max_rtt_us.toFixed(1) + ' \u00b5s';
    document.getElementById('val-hid').textContent = s.tx_hid_packets + ' / ' + s.rx_hid_packets;
    document.getElementById('val-vid').textContent = s.tx_video_frames + ' / ' + s.rx_video_frames;
    document.getElementById('val-pli').textContent = String(s.pli_keyframes_requested);

    for (const code of [0, 1, 2, 3]) {
      const btn = document.getElementById('btn-mode-' + code);
      if (btn) {
        const active = s.mode_code === code && s.daemon_online;
        btn.classList.toggle('active', active);
        btn.setAttribute('aria-pressed', String(active));
      }
    }
    document.getElementById('connection-status').textContent = '[LIVE / 250 MS]';
    const segments = document.querySelectorAll('.segments i');
    const count = s.peer_connected ? Math.min(25, Math.max(1, Math.ceil(s.peer_age_ms / 10))) : 0;
    segments.forEach((el, i) => el.classList.toggle('filled', i < count));
    document.getElementById('heartbeat-value').textContent = s.peer_connected ? s.peer_age_ms.toFixed(1) + ' / 250 ms' : 'NO PEER';
  } catch (_) {
    document.querySelector('.panic').classList.remove('armed');
    document.getElementById('connection-status').textContent = '[CONNECTION LOST / RETRYING]';
    document.getElementById('daemon-badge').textContent = 'STATUS UNAVAILABLE';
    document.getElementById('daemon-badge').className = 'badge err';
    document.getElementById('peer-badge').textContent = 'STATUS UNAVAILABLE';
    document.querySelectorAll('.mode-buttons button').forEach(el => {
      el.classList.remove('active'); el.setAttribute('aria-pressed', 'false');
    });
  }
}

function toggleTokenConfig() {
  const el = document.getElementById('token-config-box');
  el.style.display = el.style.display === 'none' ? 'block' : 'none';
}

async function checkUpdate() {
  document.getElementById('update-detail').textContent = 'Checking repository releases...';
  await postAction('/api/update/check');
  refreshUpdateInfo();
}

async function installUpdate() {
  document.getElementById('update-detail').textContent = 'Downloading and installing release...';
  await postAction('/api/update/install');
  refreshUpdateInfo();
}

async function saveUpdatePreferences() {
  await postAction('/api/config/updates', {
    auto_check: document.getElementById('auto-check-updates').checked,
    auto_install: document.getElementById('auto-install-updates').checked
  });
}

async function saveToken() {
  const tok = document.getElementById('input-token').value;
  await postAction('/api/config/token', {token: tok});
  document.getElementById('input-token').value = '';
  document.getElementById('token-config-box').style.display = 'none';
  refreshUpdateInfo();
}

async function refreshUpdateInfo() {
  try {
    const res = await fetch('/api/update');
    const data = await res.json();
    const s = data.status;
    document.getElementById('auto-check-updates').checked = data.auto_check;
    document.getElementById('auto-install-updates').checked = data.auto_install;
    document.getElementById('app-version').textContent = s.current_version;
    const uBadge = document.getElementById('update-badge');
    const btnInstall = document.getElementById('btn-install-update');
    const uDetail = document.getElementById('update-detail');
    if (s.restart_required) {
      uBadge.style.display = 'inline-block';
      uBadge.className = 'badge ok';
      uBadge.textContent = 'RESTART REQUIRED';
      uDetail.textContent = 'New version installed successfully! Please restart Duplex-KVM to run it.';
      btnInstall.style.display = 'none';
    } else if (s.is_downloading) {
      uBadge.style.display = 'inline-block';
      uBadge.className = 'badge warn';
      uBadge.textContent = 'DOWNLOADING...';
      uDetail.textContent = 'Downloading and verifying release from ' + data.repo + '...';
      btnInstall.style.display = 'none';
    } else if (s.is_checking) {
      uBadge.style.display = 'inline-block';
      uBadge.className = 'badge warn';
      uBadge.textContent = 'CHECKING...';
      uDetail.textContent = 'Querying repository releases from ' + data.repo + '...';
      btnInstall.style.display = 'none';
    } else if (s.update_available) {
      uBadge.style.display = 'inline-block';
      uBadge.className = 'badge warn';
      uBadge.textContent = 'UPDATE: ' + s.latest_version;
      uDetail.textContent = 'Newer version available from ' + data.repo + (data.has_token ? ' (Authenticated via token)' : ' (Private repo PAT not configured)');
      btnInstall.style.display = 'inline-block';
      btnInstall.textContent = 'Install ' + s.latest_version;
    } else {
      uBadge.style.display = 'inline-block';
      uBadge.className = 'badge ok';
      uBadge.textContent = 'UP TO DATE';
      uDetail.textContent = 'Repository: ' + data.repo + (data.has_token ? ' (Token: ' + (data.masked_token || 'Saved') + ')' : ' (Private repo token not set)');
      btnInstall.style.display = 'none';
    }
    if (s.last_error) {
      uBadge.className = 'badge err';
      uBadge.textContent = '[CHECK FAILED]';
      uDetail.textContent = 'Update error: ' + s.last_error;
    }
  } catch (_) {}
}

setInterval(refreshStatus, 250);
setInterval(refreshUpdateInfo, 4000);
refreshStatus();
refreshUpdateInfo();

async function refreshPairing() {
  try {
    const response = await fetch('/api/pairing');
    const status = await response.json();
    document.getElementById('pairing-badge').textContent = status.configured ? 'SAVED / ' + status.fingerprint : 'PAIRING REQUIRED';
    if (!status.configured) document.getElementById('pairing-detail').textContent = status.error;
    else if (status.environment_override) document.getElementById('pairing-detail').textContent = 'Using DUPLEX_PSK_HEX from the application environment. Both computers must use the same key.';
  } catch (_) {}
}
async function pairingAction(path, body) {
  try {
    const response = await fetch(path, {method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify(body || {})});
    const result = await response.json();
    document.getElementById('pairing-detail').textContent = result.message;
    if (result.key) document.getElementById('pairing-key').value = result.key;
    if (result.ok) refreshPairing();
  } catch (error) {
    document.getElementById('pairing-detail').textContent = 'Pairing request failed: ' + error;
  }
}
function generatePairingKey() { pairingAction('/api/pairing/generate'); }
function savePairingKey() { pairingAction('/api/pairing/save', {key: document.getElementById('pairing-key').value}); }
async function copyPairingKey() {
  const input = document.getElementById('pairing-key');
  if (!input.value) { document.getElementById('pairing-detail').textContent = 'Generate or paste a key first.'; return; }
  try {
    await navigator.clipboard.writeText(input.value);
    document.getElementById('pairing-detail').textContent = 'Key copied. Paste and save it on the other computer.';
  } catch (_) {
    input.focus(); input.select();
    document.getElementById('pairing-detail').textContent = 'Copy the selected key with Ctrl+C, then paste it on the other computer.';
  }
}
refreshPairing();
