// Compose previews in a panel beside the editor, drawn by Droid Lite's preview worker (no Gradle).
// The open .kt file is drawn when the panel opens or shows again, when another .kt file becomes
// active, and on save; never while the panel is hidden.
const vscode = require('vscode');
const path = require('path');
const os = require('os');
const crypto = require('crypto');
const fs = require('fs');

const previewDir = () =>
  process.env.DROID_LITE_PREVIEW_DIR || path.join(os.homedir(), 'Library/Caches/droid-lite/previews');

function html(webview) {
  // only the one script below may run
  const nonce = crypto.randomBytes(16).toString('base64');
  const csp = `default-src 'none'; img-src ${webview.cspSource}; style-src 'unsafe-inline'; script-src 'nonce-${nonce}'`;
  return `<!doctype html><html><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="${csp}">
<style>
  body { font-family: var(--vscode-font-family); color: var(--vscode-foreground); padding: 8px 12px; }
  #status { color: var(--vscode-descriptionForeground); min-height: 1.4em; }
  #bar { height: 2px; background: var(--vscode-progressBar-background); width: 0; transition: width .2s; }
  .card { margin: 14px 0; }
  .card h4 { margin: 0 0 6px; font-weight: 600; font-size: 12px; color: var(--vscode-descriptionForeground); }
  .card img { max-width: 100%; border: 1px solid var(--vscode-panel-border); border-radius: 6px; display: block; }
  .card pre { white-space: pre-wrap; color: var(--vscode-errorForeground); font-size: 12px; margin: 0; }
  .busy .card { opacity: .45; }
</style></head><body>
<div id="status">Open a Kotlin file with @Preview functions.</div><div id="bar"></div><div id="cards"></div>
<script nonce="${nonce}">
  const status = document.getElementById('status'), bar = document.getElementById('bar'), cards = document.getElementById('cards');
  window.addEventListener('message', ({ data: m }) => {
    if (m.type === 'start') { document.body.classList.add('busy'); status.textContent = 'Drawing ' + m.file + '…'; bar.style.width = '5%'; }
    if (m.type === 'status') { status.textContent = m.text + (m.total ? ' (' + m.done + ' of ' + m.total + ')' : ''); bar.style.width = m.total ? (100 * m.done / m.total) + '%' : '30%'; }
    if (m.type === 'cards' || m.type === 'error') {
      document.body.classList.remove('busy'); bar.style.width = '0';
      cards.replaceChildren();
      const list = m.type === 'error' ? [{ name: 'Could not draw the previews', error: m.text }] : m.cards;
      status.textContent = m.type === 'cards' ? (list.length ? m.file : 'No @Preview functions in ' + m.file + '.') : m.file;
      for (const c of list) {
        const d = document.createElement('div'); d.className = 'card';
        const h = document.createElement('h4'); h.textContent = c.name; d.append(h);
        if (c.src) { const i = document.createElement('img'); i.src = c.src; d.append(i); }
        else { const p = document.createElement('pre'); p.textContent = c.error; d.append(p); }
        cards.append(d);
      }
    }
  });
</script></body></html>`;
}

function setup(ctx, project, events) {
  let panel;
  let file; // the file drawn last
  let real; // its real path: the engine names the file it draws by that one

  async function render(editor) {
    if (!panel?.visible || !editor || !/\.kt$/.test(editor.document.uri.fsPath)) return;
    const p = editor.document.uri.fsPath;
    file = p;
    try { real = fs.realpathSync(p); } catch { real = p; }
    const name = path.basename(p);
    panel.webview.postMessage({ type: 'start', file: name });
    try {
      const r = await project(ctx, 'previews', { path: p });
      if (!panel || file !== p) return;
      const cards = r.cards.map((c) => ({
        name: c.name,
        src: c.png && panel.webview.asWebviewUri(vscode.Uri.file(c.png)).toString(),
        error: c.error,
      }));
      panel.webview.postMessage({ type: 'cards', file: name, cards });
    } catch (e) {
      // a newer request for this file answers instead
      if (panel && file === p && e.message !== 'replaced by a newer request') {
        panel.webview.postMessage({ type: 'error', file: name, text: e.message });
      }
    }
  }

  function open() {
    if (panel) {
      panel.reveal(vscode.ViewColumn.Beside, true);
    } else {
      panel = vscode.window.createWebviewPanel('droidLite.preview', 'Compose Preview',
        { viewColumn: vscode.ViewColumn.Beside, preserveFocus: true },
        { enableScripts: true, retainContextWhenHidden: true, localResourceRoots: [vscode.Uri.file(previewDir())] });
      panel.iconPath = vscode.Uri.file(path.join(ctx.extensionPath, 'media', 'icon.png'));
      panel.webview.html = html(panel.webview);
      panel.onDidDispose(() => { panel = undefined; });
      let shown = true;
      panel.onDidChangeViewState(({ webviewPanel: w }) => {
        if (w.visible && !shown) render(vscode.window.activeTextEditor);
        shown = w.visible;
      });
    }
    render(vscode.window.activeTextEditor);
  }

  ctx.subscriptions.push(
    vscode.commands.registerCommand('droidLite.preview', open),
    vscode.window.onDidChangeActiveTextEditor((e) => { if (e && e.document.uri.fsPath !== file) render(e); }),
    vscode.workspace.onDidSaveTextDocument((d) => {
      const e = vscode.window.activeTextEditor;
      if (e && e.document === d) render(e);
    }),
    events.event((m) => {
      // progress of another file's drawing is not shown (older engines send no "file")
      if (panel && m.event === 'preview_status' && (m.file === undefined || m.file === file || m.file === real)) {
        panel.webview.postMessage({ type: 'status', ...m });
      }
    }),
    { dispose: () => panel?.dispose() },
  );
}

module.exports = { setup, html };
