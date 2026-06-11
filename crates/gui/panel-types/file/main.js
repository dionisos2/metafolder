// file panel: media/text preview of the active path from selected_paths
// (spec-gui "file panel type"). Files are streamed by the GUI server's
// /fsraw endpoint (HTTP Range supported, so audio/video can seek).

const { workspace } = metafolder;

const IMAGE = new Set(['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'bmp', 'avif']);
const AUDIO = new Set(['mp3', 'ogg', 'oga', 'flac', 'wav', 'm4a', 'opus']);
const VIDEO = new Set(['mp4', 'webm', 'mkv', 'mov', 'avi']);
const TEXT = new Set([
  'txt', 'md', 'org', 'json', 'toml', 'yaml', 'yml', 'xml', 'html', 'css', 'js', 'ts',
  'rs', 'py', 'sh', 'c', 'h', 'cpp', 'java', 'log', 'csv', 'ini', 'conf',
]);
const TEXT_PREVIEW_LIMIT = 256 * 1024;

let paths = [];
let activeIndex = 0;

const pathBar = document.getElementById('path-bar');
const viewer = document.getElementById('viewer');

function rawUrl(path) {
  return `${metafolder.guiServer}/fsraw?path=${encodeURIComponent(path)}`;
}

function placeholder(text) {
  const p = document.createElement('p');
  p.className = 'placeholder';
  p.textContent = text;
  viewer.replaceChildren(p);
}

function renderPathBar() {
  pathBar.hidden = paths.length === 0;
  if (paths.length > 1) {
    // Entry reachable at several locations: pick which one to preview.
    const select = document.createElement('select');
    paths.forEach((path, index) => {
      const option = document.createElement('option');
      option.value = String(index);
      option.textContent = path;
      option.selected = index === activeIndex;
      select.appendChild(option);
    });
    select.addEventListener('change', () => {
      activeIndex = Number(select.value);
      void renderViewer();
    });
    pathBar.replaceChildren(select);
  } else if (paths.length === 1) {
    const span = document.createElement('span');
    span.textContent = paths[0];
    pathBar.replaceChildren(span);
  }
}

let mediaSupportCache = null;

async function mediaSupport() {
  if (mediaSupportCache === null) {
    try {
      const response = await fetch(`${metafolder.guiServer}/__media-support`);
      mediaSupportCache = await response.json();
    } catch {
      // Undeterminable (GUI server unreachable): do not degrade silently.
      mediaSupportCache = { audio: true, video: true, missing: [] };
    }
  }
  return mediaSupportCache;
}

async function renderViewer() {
  const path = paths[activeIndex];
  if (!path) {
    placeholder('No file selected');
    return;
  }
  const extension = (path.split('.').pop() ?? '').toLowerCase();
  const url = rawUrl(path);

  if (IMAGE.has(extension)) {
    const img = document.createElement('img');
    img.src = url;
    img.onerror = () => placeholder('cannot load the file');
    viewer.replaceChildren(img);
  } else if (AUDIO.has(extension) || VIDEO.has(extension)) {
    // A media element with no usable GStreamer pipeline does not fail
    // gracefully: it crashes the WebKit web process and freezes the
    // whole GUI. Ask the GUI server first (/__media-support).
    const kind = VIDEO.has(extension) ? 'video' : 'audio';
    const support = await mediaSupport();
    if (!support[kind]) {
      placeholder(
        `media preview disabled: missing GStreamer elements: ` +
          `${support.missing.join(', ')} (install gst-plugins-good)`,
      );
      return;
    }
    const media = document.createElement(kind);
    media.controls = true;
    media.src = url;
    viewer.replaceChildren(media);
  } else if (TEXT.has(extension)) {
    try {
      const response = await fetch(url, {
        headers: { range: `bytes=0-${TEXT_PREVIEW_LIMIT - 1}` },
      });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const text = await response.text();
      const pre = document.createElement('pre');
      pre.textContent = text;
      viewer.replaceChildren(pre);
      if (response.status === 206 && text.length >= TEXT_PREVIEW_LIMIT) {
        pre.textContent += '\n… (truncated preview)';
      }
    } catch (error) {
      placeholder(`cannot load the file: ${error.message ?? error}`);
    }
  } else {
    placeholder('no preview available for this format');
  }
}

function update(newPaths) {
  paths = Array.isArray(newPaths) ? newPaths : [];
  activeIndex = 0;
  renderPathBar();
  void renderViewer();
}

await metafolder.ready;
workspace.onChange('selected_paths', update);
update(await workspace.get('selected_paths'));
