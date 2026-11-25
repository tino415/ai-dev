(() => {
  const byId = id => document.getElementById(id);
  const root = byId('terminal');
  const screen = byId('screen');
  const keys = byId('keys');
  const encoder = new TextEncoder();
  const SEQUENCES = {
    esc: '\x1b', tab: '\t', up: '\x1b[A', down: '\x1b[B', left: '\x1b[D', right: '\x1b[C',
    'ctrl-c': '\x03', 'ctrl-d': '\x04', pgup: '\x1b[5~',
  };
  let term = null;
  let socket = null;
  let ctrl = false;

  function show(state, text) {
    root.dataset.state = state;
    byId('connection').textContent = text;
  }

  function setCtrl(on) {
    ctrl = on;
    keys.querySelector('[data-key="ctrl"]').classList.toggle('on', on);
  }

  function withStickyCtrl(data) {
    if (!ctrl) return data;
    setCtrl(false);
    if (data.length !== 1) return data;
    const code = data.toUpperCase().charCodeAt(0);
    return code >= 64 && code <= 95 ? String.fromCharCode(code - 64) : data;
  }

  function sendBytes(bytes) {
    if (socket && socket.readyState === WebSocket.OPEN) socket.send(bytes);
  }

  function type(data) {
    sendBytes(encoder.encode(withStickyCtrl(data)));
  }

  function sendSize() {
    if (socket && socket.readyState === WebSocket.OPEN) {
      socket.send(JSON.stringify({ resize: { cols: term.cols, rows: term.rows } }));
    }
  }

  function showRunningBlock(id) {
    const banner = byId('banner');
    banner.hidden = !id;
    banner.textContent = id ? `Block ${id} is running here: what you type goes into its output.` : '';
  }

  function sessionStopped() {
    show('stopped', 'session stopped');
    byId('dot').classList.add('off');
    screen.classList.add('ended');
    keys.hidden = true;
    showRunningBlock(null);
    byId('stopped').hidden = false;
    term.options.disableStdin = true;
    term.blur();
  }

  async function sessionIsStopped() {
    const page = await fetch(location.href).then(response => response.text()).catch(() => '');
    return page.includes('data-state="stopped"');
  }

  function disconnected() {
    show('disconnected', 'disconnected');
    byId('disconnected').hidden = false;
  }

  function openTerminal() {
    term = new Terminal({
      cursorBlink: true,
      fontFamily: 'ui-monospace, monospace',
      fontSize: 13,
      theme: { background: '#111111', foreground: '#d8d8d8' },
    });
    const fit = new FitAddon.FitAddon();
    term.loadAddon(fit);
    term.open(screen);
    fit.fit();
    term.onData(type);
    term.onBinary(data => sendBytes(Uint8Array.from(data, c => c.charCodeAt(0))));
    term.onResize(sendSize);
    new ResizeObserver(() => fit.fit()).observe(screen);
  }

  function connect() {
    const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
    const path = `/sessions/${encodeURIComponent(root.dataset.session)}/terminal/socket`;
    socket = new WebSocket(`${scheme}://${location.host}${path}?cols=${term.cols}&rows=${term.rows}`);
    socket.binaryType = 'arraybuffer';
    let stopped = false;
    let opened = false;
    socket.onopen = () => {
      opened = true;
      show('connected', 'connected');
      sendSize();
      term.focus();
    };
    socket.onmessage = ({ data }) => {
      if (typeof data !== 'string') return term.write(new Uint8Array(data));
      const message = JSON.parse(data);
      if ('running_block' in message) showRunningBlock(message.running_block);
      if (message.session === 'stopped') {
        stopped = true;
        sessionStopped();
      }
    };
    socket.onclose = async () => {
      socket = null;
      if (stopped || root.dataset.state === 'closed') return;
      if (!opened && await sessionIsStopped()) sessionStopped();
      else disconnected();
    };
  }

  keys.addEventListener('pointerdown', event => event.preventDefault());
  keys.addEventListener('click', event => {
    const key = event.target.closest('button')?.dataset.key;
    if (!key) return;
    if (key === 'ctrl') setCtrl(!ctrl);
    else type(SEQUENCES[key] ?? key);
    term.focus();
  });

  byId('close').addEventListener('click', () => {
    root.dataset.state = 'closed';
    socket?.close();
    window.close();
    setTimeout(() => { location.href = '/sessions'; }, 200);
  });

  byId('reconnect').addEventListener('click', () => location.reload());

  byId('start').addEventListener('submit', async event => {
    event.preventDefault();
    const form = event.target;
    form.querySelector('button').disabled = true;
    const response = await fetch(form.action, { method: 'POST', redirect: 'manual' }).catch(error => error);
    if (response.type === 'opaqueredirect' || response.ok) return location.reload();
    form.querySelector('button').disabled = false;
    byId('start-error').textContent = response instanceof Error ? response.message : await response.text();
  });

  if (root.dataset.state !== 'stopped') {
    openTerminal();
    connect();
  }
})();
