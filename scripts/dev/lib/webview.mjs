// Reaches into the GUI's WebView through WebKit's remote inspector, for
// scripts/dev/gui-e2e.sh (doc "Testing the GUI end to end"): what only a mouse
// reaches is clicked by selector, never by coordinates, so a change of style
// cannot move a click.
//
// The GUI must run with WEBKIT_INSPECTOR_HTTP_SERVER=<host:port>; its page is
// the inspector's first target (/socket/1/1/WebPage), whose protocol is
// multiplexed: a command for the page goes through Target.sendMessageToTarget.
//
// Usage: node webview.mjs <host:port> <action> <args…>
//   ready                                 the page answers
//   click <selector> [<text> [<inner selector> [<inner text>]]]
//       clicks the one visible element matching <selector> whose text
//       contains <text> — or, with <inner selector>, the one visible element
//       inside it matching that (and containing <inner text>). Selectors are
//       matched in the document and in every shadow root (the panels').
//   contextmenu <selector> [<text> [<inner selector> [<inner text>]]]
//       right-clicks the middle of that element (the event, not the mouse:
//       the selection is left as it is)
//   bounds <selector> [<text> [<inner selector> [<inner text>]]]
//       prints that element's left, top, right and bottom, in the page's
//       coordinates
//   text <selector> [<text> [<inner selector> [<inner text>]]]
//       prints that element's text (an input's: its value)
//   zoom <selector> [<text> [<inner selector> [<inner text>]]]
//       prints the CSS zoom of the panel body holding that element
//   scroll <selector> [<text> [<inner selector> [<inner text>]]]
//       prints how far that element is scrolled down (its scrollTop, rounded)
// Exit status: 0 done, 1 not exactly one element matches (nothing clicked),
// 2 no page or a protocol error.

const [address, action, ...args] = process.argv.slice(2);
const TIMEOUT_MS = 5000;

// Runs in the page: `args` are this script's, passed as JSON.
function inPage(action, args) {
  const [selector, text, inner, innerText] = args;
  /** @param {ParentNode} root @returns {Element[]} */
  const deep = (root, sel) => {
    const found = [...root.querySelectorAll(sel)];
    for (const e of root.querySelectorAll('*')) if (e.shadowRoot) found.push(...deep(e.shadowRoot, sel));
    return found;
  };
  const visible = (e) => e.getClientRects().length > 0;
  const holds = (e, t) => t === undefined || t === '' || (e.textContent ?? '').includes(t);
  if (action === 'ready') return 'ready';
  let hits = deep(document, selector).filter((e) => visible(e) && holds(e, text));
  if (inner !== undefined) {
    hits = hits
      .flatMap((e) => [...e.querySelectorAll(inner)])
      .filter((e) => visible(e) && holds(e, innerText));
  }
  if (hits.length !== 1)
    return {
      miss: `${hits.length} visible elements match ${JSON.stringify(args)}`,
    };
  if (action === 'click') {
    /** @type {HTMLElement} */ (hits[0]).click();
    return 'clicked';
  }
  const hit = hits[0];
  if (action === 'bounds') {
    const rect = hit.getBoundingClientRect();
    return [rect.left, rect.top, rect.right, rect.bottom].map(Math.round).join(' ');
  }
  if (action === 'contextmenu') {
    const rect = hit.getBoundingClientRect();
    hit.dispatchEvent(
      new MouseEvent('contextmenu', {
        bubbles: true,
        composed: true,
        cancelable: true,
        clientX: rect.left + rect.width / 2,
        clientY: rect.top + rect.height / 2,
      }),
    );
    return 'opened';
  }
  if (action === 'zoom') {
    return getComputedStyle(hit.closest('.mf-panel-body') ?? hit).zoom;
  }
  if (action === 'scroll') return Math.round(hit.scrollTop);
  if (hit instanceof HTMLInputElement || hit instanceof HTMLTextAreaElement) return hit.value;
  return (hit.textContent ?? '').trim();
}

function fail(code, message) {
  process.stderr.write(`webview: ${message}\n`);
  process.exit(code);
}

if (!['ready', 'click', 'bounds', 'contextmenu', 'text', 'zoom', 'scroll'].includes(action)) fail(2, `unknown action ${action}`);
if (action !== 'ready' && !args[0]) fail(2, `${action} needs a selector`);

const timer = setTimeout(() => fail(2, `no answer from the page within ${TIMEOUT_MS} ms`), TIMEOUT_MS);
const socket = new WebSocket(`ws://${address}/socket/1/1/WebPage`);
socket.onerror = () => fail(2, `no inspector at ${address}`);
socket.onmessage = (event) => {
  const message = JSON.parse(String(event.data));
  if (message.method === 'Target.targetCreated' && message.params.targetInfo.type === 'page') {
    const expression = `(${inPage.toString()})(${JSON.stringify(action)}, ${JSON.stringify(args)})`;
    const command = {
      id: 1,
      method: 'Runtime.evaluate',
      params: { expression, returnByValue: true },
    };
    socket.send(
      JSON.stringify({
        id: 1,
        method: 'Target.sendMessageToTarget',
        params: {
          targetId: message.params.targetInfo.targetId,
          message: JSON.stringify(command),
        },
      }),
    );
  } else if (message.method === 'Target.dispatchMessageFromTarget') {
    const reply = JSON.parse(message.params.message);
    if (reply.id !== 1) return;
    clearTimeout(timer);
    if (reply.error) fail(2, reply.error.message);
    const { result, wasThrown } = reply.result;
    if (wasThrown) fail(2, `thrown in the page: ${result.description ?? JSON.stringify(result)}`);
    if (result.value && typeof result.value === 'object' && 'miss' in result.value)
      fail(1, result.value.miss);
    process.stdout.write(`${result.value}\n`);
    process.exit(0);
  } else if (message.error) {
    fail(2, message.error.message);
  }
};
