// LNURLcash Run: the landing page's beta sign-up.
//
// With a NIP-07 signer (window.nostr), joining is a NIP-98 HTTP-auth event
// signed for this exact request: the URL, the method and a hash of the body.
// The server checks the signature, so the key is proven to be the signer's.
// Without one, an npub can be typed in; it is listed as unverified.

document.documentElement.classList.add('js');

const $ = (sel, root = document) => root.querySelector(sel);
const api = `${location.origin}/api/beta`;

// ---- reveal on scroll ----
const reveals = document.querySelectorAll('.reveal');
if ('IntersectionObserver' in window) {
  const io = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        if (e.isIntersecting) {
          e.target.classList.add('is-visible');
          io.unobserve(e.target);
        }
      }
    },
    {rootMargin: '0px 0px -8% 0px'},
  );
  for (const el of reveals) io.observe(el);
} else {
  for (const el of reveals) el.classList.add('is-visible');
}

// ---- how many are on the list ----
async function refreshCount() {
  try {
    const res = await fetch(api);
    const {count} = await res.json();
    if (count > 0) {
      for (const el of document.querySelectorAll('[data-count-value]')) el.textContent = count.toLocaleString('en');
      for (const el of document.querySelectorAll('[data-count]')) el.hidden = false;
    }
  } catch {
    // the page works without it
  }
}
refreshCount();

// ---- sign-up ----
const form = $('#beta-form');
const result = $('[data-result]', form);
const signButton = $('[data-sign]', form);

const sha256Hex = async (text) => {
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, '0')).join('');
};
const base64 = (text) => {
  const bytes = new TextEncoder().encode(text);
  let binary = '';
  for (const b of bytes) binary += String.fromCharCode(b);
  return btoa(binary);
};

/** NIP-07 signers inject window.nostr shortly after load: wait a moment. */
async function signer(waitMs = 1500) {
  const started = Date.now();
  while (!window.nostr && Date.now() - started < waitMs) await new Promise((r) => setTimeout(r, 100));
  return window.nostr;
}

function show(kind, ...children) {
  result.className = `beta__result beta__result--${kind}`;
  result.replaceChildren(...children);
  result.hidden = false;
}

function el(tag, text, attrs = {}) {
  const node = document.createElement(tag);
  if (text) node.textContent = text;
  Object.assign(node, attrs);
  return node;
}

function joined(res) {
  show(
    'ok',
    el('strong', `You're #${res.place} on the list.`),
    el('span', "We'll reach you on nostr at "),
    el('code', res.npub),
    el('span', res.verified ? '.' : '. It is listed as unverified: sign in once with your extension to verify it.'),
  );
  refreshCount();
  // approved members name their mint right here
  if (res.approved && res.provisioning) showMints(res.mints || []);
}

// ---- an approved member's mint ----
const panel = $('[data-mint-panel]');
const mintOut = $('[data-mint-out]');

function mintCard(m) {
  const card = el('div', '', {className: 'mint-card'});
  const link = el('a', m.url, {
    href: m.url,
    target: '_blank',
    rel: 'noopener',
  });
  card.append(el('b', m.name), link, el('span', `Lightning Address: ${m.lightning_address}`));
  card.append(
    el('span', m.commit ? `Committed in ${m.commit}: Argo CD is rolling it out.` : 'Being committed…', {className: 'muted'}),
  );
  return card;
}

function showMints(mints) {
  panel.hidden = false;
  if (mints.length) {
    $('h3', panel).textContent = mints.length === 1 ? 'Your mint' : 'Your mints';
    $('.mint-panel__row', panel).hidden = true;
    $('.small', panel).hidden = true;
    mintOut.replaceChildren(...mints.map(mintCard));
  }
}

const nameInput = $('input[name=mint]', panel);
nameInput.addEventListener('input', () => {
  nameInput.value = nameInput.value.toLowerCase().replace(/[^a-z0-9-]/g, '');
  $('[data-mint-preview]').textContent = nameInput.value || 'yourname';
});

$('[data-create-mint]').addEventListener('click', async (event) => {
  const button = event.currentTarget;
  const name = nameInput.value.trim();
  if (!nameInput.checkValidity() || !name) {
    mintOut.replaceChildren(
      el('span', 'Use 3 to 30 characters: a-z, 0-9 and dashes, not starting or ending with a dash.', {className: 'bad'}),
    );
    nameInput.focus();
    return;
  }
  const nostr = await signer();
  if (!nostr) return;
  button.disabled = true;
  mintOut.replaceChildren(el('span', 'Committing your mint…', {className: 'muted'}));
  try {
    const url = `${location.origin}/api/mints`;
    const body = JSON.stringify({name});
    const auth = await httpAuth(nostr, url, 'POST', body);
    const res = await fetch(url, {
      method: 'POST',
      headers: {'Content-Type': 'application/json', Authorization: auth},
      body,
    });
    const data = await res.json().catch(() => ({reason: `the server answered ${res.status}`}));
    if (!res.ok || data.status !== 'OK') throw new Error(data.reason || 'provisioning failed');
    showMints([data]);
  } catch (err) {
    mintOut.replaceChildren(
      el('span', `Couldn't create it: ${err.message || err}`, {
        className: 'bad',
      }),
    );
  } finally {
    button.disabled = false;
  }
});

/** A NIP-98 Authorization header for one request. */
async function httpAuth(nostr, url, method, body) {
  const tags = [
    ['u', url],
    ['method', method],
  ];
  if (body) tags.push(['payload', await sha256Hex(body)]);
  const event = await nostr.signEvent({
    kind: 27235,
    created_at: Math.floor(Date.now() / 1000),
    tags,
    content: '',
  });
  return `Nostr ${base64(JSON.stringify(event))}`;
}

async function submit(body, headers = {}) {
  const res = await fetch(api, {
    method: 'POST',
    headers: {'Content-Type': 'application/json', ...headers},
    body,
  });
  const data = await res.json().catch(() => ({reason: `the server answered ${res.status}`}));
  if (!res.ok || data.status !== 'OK') throw new Error(data.reason || 'sign-up failed');
  return data;
}

form.addEventListener('submit', async (event) => {
  event.preventDefault();
  const nostr = await signer();
  if (!nostr) {
    $('[data-no-extension]', form).hidden = false;
    $('[data-fallback]', form).open = true;
    $('input[name=npub]', form).focus();
    return;
  }
  signButton.disabled = true;
  try {
    const body = JSON.stringify({note: form.note.value.trim()});
    joined(
      await submit(body, {
        Authorization: await httpAuth(nostr, api, 'POST', body),
      }),
    );
  } catch (err) {
    show('error', el('span', `Couldn't sign you up: ${err.message || err}`));
  } finally {
    signButton.disabled = false;
  }
});

$('[data-typed]', form).addEventListener('click', async () => {
  const npub = form.npub.value.trim();
  if (!npub) {
    form.npub.focus();
    return;
  }
  try {
    joined(await submit(JSON.stringify({note: form.note.value.trim(), npub})));
  } catch (err) {
    show('error', el('span', `Couldn't sign you up: ${err.message || err}`));
  }
});
