(() => {
  htmx.config.defaultSettleDelay = 0;
  const waiting = new Map();
  let pointerIn = null;
  let opened = false;
  let stale = false;

  const versionOf = (html) => Number((/data-v="(\d+)"/.exec(html) || [])[1] || 0);
  const idOf = (html) => (/id="([^"]+)"/.exec(html) || [])[1];
  const later = () => setTimeout(applyWaiting);

  function edited(root) {
    return [...root.querySelectorAll('input, textarea, select')].some((field) => {
      if (field.type === 'checkbox') return field.checked !== field.defaultChecked;
      if (field.tagName === 'SELECT') {
        const initial = [...field.options].find((o) => o.defaultSelected) || field.options[0];
        return field.value !== (initial ? initial.value : '');
      }
      return field.value !== field.defaultValue;
    });
  }

  function busy(root) {
    const focused = document.activeElement;
    return (focused && root.contains(focused) && focused.matches('input, textarea, select'))
      || (pointerIn && root.contains(pointerIn))
      || root.matches('.htmx-request') || !!root.querySelector('.htmx-request')
      || edited(root);
  }

  function keepFor(id, html) {
    const kept = waiting.get(id);
    if (!kept || versionOf(html) >= versionOf(kept)) waiting.set(id, html);
  }

  function applyWaiting() {
    for (const [id, html] of [...waiting]) {
      const target = document.getElementById(id);
      if (!target) { waiting.delete(id); continue; }
      if (busy(target)) continue;
      waiting.delete(id);
      if (versionOf(html) >= Number(target.dataset.v || 0)) htmx.swap(target, html, { swapStyle: 'outerHTML' });
    }
    const guarded = [...document.querySelectorAll('[data-guard]')];
    if (stale && guarded.every((el) => !busy(el))) location.reload();
  }

  function showError(text) {
    const flash = document.getElementById('flash');
    flash.querySelector('pre').textContent = text;
    flash.hidden = false;
  }

  function working(button) {
    if (!button || button.tagName !== 'BUTTON' || button.disabled) return;
    button.dataset.label = button.textContent;
    button.textContent = button.dataset.working || 'Working…';
    button.disabled = true;
  }

  function restore(button) {
    if (!button || button.dataset.label === undefined) return;
    button.textContent = button.dataset.label;
    delete button.dataset.label;
    button.disabled = false;
  }

  document.addEventListener('htmx:sseBeforeMessage', (event) => {
    const target = event.target;
    const html = event.detail.data;
    if (target.hasAttribute('data-stale')) {
      event.preventDefault();
      stale = true;
      later();
      return;
    }
    if (target.hasAttribute('data-append')) {
      const id = idOf(html);
      if (id && document.getElementById(id)) event.preventDefault();
      return;
    }
    if (!target.hasAttribute('data-guard')) return;
    if (versionOf(html) < Number(target.dataset.v || 0)) {
      event.preventDefault();
    } else if (busy(target)) {
      event.preventDefault();
      keepFor(target.id, html);
    }
  });

  document.addEventListener('htmx:sseOpen', () => {
    if (opened) { stale = true; later(); }
    opened = true;
    document.documentElement.dataset.live = 'open';
  });

  document.addEventListener('htmx:beforeSwap', (event) => {
    const target = event.detail.target;
    if (event.detail.shouldSwap && target?.dataset?.v
      && versionOf(event.detail.serverResponse) < Number(target.dataset.v)) {
      event.detail.shouldSwap = false;
    }
  });

  document.addEventListener('htmx:beforeRequest', (event) => working(event.detail.elt));
  document.addEventListener('htmx:afterRequest', (event) => { restore(event.detail.elt); later(); });
  document.addEventListener('htmx:responseError', (event) => showError(event.detail.xhr.responseText || event.detail.xhr.statusText));
  document.addEventListener('htmx:sendError', () => showError('the server did not answer'));
  document.addEventListener('submit', (event) => working(event.submitter));

  document.addEventListener('pointerdown', (event) => { pointerIn = event.target; }, true);
  for (const name of ['pointerup', 'pointercancel']) {
    document.addEventListener(name, () => { pointerIn = null; later(); }, true);
  }
  document.addEventListener('focusout', later);
  document.addEventListener('input', later);
  document.addEventListener('click', (event) => {
    if (event.target.matches('#flash button')) event.target.closest('#flash').hidden = true;
  });
})();
