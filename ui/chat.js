/* Chat panel: conversations, model switching, attachments, tool activity.
 *
 * The agent loop runs in the backend (chat.rs) because only that process holds
 * the WebView2 controllers the tools drive. This file is therefore a renderer
 * plus one blocking `chat_send` per turn. It never sees a provider key: keys
 * arrive masked and are only ever displayed as masks.
 */
'use strict';
(function () {
  const { invoke } = window.__TAURI__.core;

  const esc = (s) => String(s ?? '').replace(/[&<>"']/g, c => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  }[c]));

  const state = {
    open: false,
    historyOpen: false,
    conversationId: null,
    conversation: null,
    providers: [],
    attachments: [],
    sending: false,
    error: '',
    // Set when the backend reconciled a stale model, so the panel can explain the
    // switch instead of silently showing a different model than last time.
    modelNotice: '',
    // Name of the tool the agent is running right now, or '' between calls. The
    // composer says it, because "waiting" for ten seconds with no detail is
    // indistinguishable from a hang.
    runningTool: '',
  };

  const $ = (sel) => document.querySelector(sel);

  // ---- markdown-lite -------------------------------------------------------

  /* A model answers in Markdown. Rendering it as plain text loses the structure
     that makes an answer readable, and a full parser is not worth the weight for
     a 380px panel. This covers what a chat answer actually uses: fenced code,
     inline code, bold, links and lists. Everything is escaped first, so no model
     output can inject markup. */

  function inline(text) {
    return text
      .replace(/`([^`]+)`/g, (_, code) => `<code>${code}</code>`)
      .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>')
      .replace(/\[([^\]]+)\]\((https?:[^)\s]+)\)/g,
        '<a href="$2" target="_blank" rel="noreferrer">$1</a>');
  }

  function renderMarkdown(text) {
    const blocks = [];
    // Fenced code first, so its contents are never touched by the inline rules.
    const withTokens = String(text || '').replace(/```[\w-]*\r?\n?([\s\S]*?)```/g, (_, code) => {
      blocks.push(`<pre><code>${esc(code.replace(/\s+$/, ''))}</code></pre>`);
      return `\u0000${blocks.length - 1}\u0000`;
    });

    const out = [];
    let listType = null;
    const closeList = () => {
      if (listType) { out.push(`</${listType}>`); listType = null; }
    };

    for (const line of withTokens.split(/\r?\n/)) {
      const bullet = line.match(/^\s*[-*]\s+(.*)$/);
      const numbered = line.match(/^\s*\d+[.)]\s+(.*)$/);
      if (bullet || numbered) {
        const want = bullet ? 'ul' : 'ol';
        if (listType !== want) { closeList(); out.push(`<${want}>`); listType = want; }
        out.push(`<li>${inline(esc(bullet ? bullet[1] : numbered[1]))}</li>`);
        continue;
      }
      closeList();
      if (!line.trim()) continue;
      // A line that is only a placeholder carries a code block through.
      if (/^\u0000\d+\u0000$/.test(line.trim())) { out.push(line.trim()); continue; }
      out.push(`<p>${inline(esc(line))}</p>`);
    }
    closeList();

    return out.join('').replace(/\u0000(\d+)\u0000/g, (_, i) => blocks[Number(i)] || '');
  }

  // ---- helpers -------------------------------------------------------------

  function fmtTime(ms) {
    const value = Number(ms || 0);
    if (!value) return '';
    const date = new Date(value);
    if (Number.isNaN(date.getTime())) return '';
    const diff = Date.now() - value;
    if (diff < 60_000) return 'just now';
    if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}m ago`;
    if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)}h ago`;
    return date.toLocaleDateString();
  }

  /** Human label for a tool call, so the log reads as actions rather than RPC. */
  const TOOL_LABELS = {
    list_tabs: 'Listed tabs',
    list_apps: 'Listed apps',
    read_page: 'Read page',
    eval: 'Ran script',
    navigate: 'Navigated',
    screenshot: 'Took screenshot',
    console_logs: 'Read console',
    network_logs: 'Read network log',
    add_app: 'Added app',
    update_app: 'Updated app',
    delete_app: 'Deleted app',
  };

  function toolSummary(call) {
    const label = TOOL_LABELS[call.name] || call.name;
    const args = call.arguments || {};
    const target = args.appId || args.url || args.expression || '';
    return target ? `${label} · ${String(target).slice(0, 56)}` : label;
  }

  function shortJson(value) {
    let text;
    try { text = JSON.stringify(value); } catch (_) { text = String(value); }
    if (!text) return '';
    return text.length > 420 ? `${text.slice(0, 420)}…` : text;
  }

  // ---- render --------------------------------------------------------------

  function render() {
    if (!state.open) return;
    renderHead();
    renderModelBar();
    renderLog();
    renderComposer();
  }

  function renderHead() {
    const title = $('#ch-title');
    if (!title) return;
    title.textContent = state.conversation && state.conversation.title
      ? state.conversation.title
      : 'Chat with AI';
  }

  function renderModelBar() {
    const bar = $('#ch-modelbar');
    if (!bar) return;
    if (!state.providers.length) {
      bar.innerHTML = '<span class="ch-tools">no provider</span>';
      return;
    }
    const current = state.conversation
      ? (state.conversation.provider_id || '')
      : '';
    const provider = state.providers.find(p => p.id === current) || null;
    const models = provider ? (provider.models || []) : [];
    const chosen = state.conversation ? (state.conversation.model || '') : '';

    bar.innerHTML = `
      <select id="ch-provider">
        ${current ? '' : '<option value="">— pick a provider —</option>'}
        ${state.providers.map(p => `
          <option value="${esc(p.id)}" ${p.id === current ? 'selected' : ''}>
            ${esc(p.name)}${p.model ? ` · ${esc(p.model)}` : ''}${p.ready ? '' : ' (incomplete)'}
          </option>`).join('')}
      </select>
      ${models.length > 1 ? `
        <select id="ch-model" title="Model for this conversation">
          ${models.map(m => `<option value="${esc(m)}" ${m === chosen ? 'selected' : ''}>${esc(m)}</option>`).join('')}
        </select>` : `<span class="ch-tools ${current ? 'on' : ''}">${chosen ? esc(chosen) : 'tools'}</span>`}`;

    bar.querySelector('#ch-provider').addEventListener('change', (e) => switchProvider(e.target.value));
    const modelSelect = bar.querySelector('#ch-model');
    if (modelSelect) modelSelect.addEventListener('change', (e) => switchModel(e.target.value));
  }

  function renderLog() {
    const log = $('#ch-log');
    if (!log) return;
    const messages = state.conversation ? (state.conversation.messages || []) : [];

    if (!messages.length) {
      log.innerHTML = `<div class="ch-empty">
        <strong>Ask about what is on screen</strong>
        The agent can read the page in your open tab, follow links, and run a
        script when reading is not enough. It asks before changing anything.
      </div>`;
      renderNotice();
      return;
    }

    log.innerHTML = messages.map(m => {
      // A bare tool-result turn is folded into the call that produced it.
      if (m.role === 'tool') return '';

      const isUser = m.role === 'user';
      const body = isUser
        ? esc(m.content || '').replace(/\r?\n/g, '<br>')
        : renderMarkdown(m.content || '');

      const sent = (m.attachments || []).length
        ? `<div class="ch-sent">${m.attachments.map(a => (
          a.mime && a.mime.startsWith('image/')
            ? `<img src="data:${esc(a.mime)};base64,${esc(a.data)}" alt="${esc(a.name)}" title="${esc(a.name)}">`
            : `<span class="ch-chip">${esc(a.name)}</span>`
        )).join('')}</div>`
        : '';

      const calls = (m.tool_calls || []).length
        ? m.tool_calls.map(c => `
          <div class="ch-tool">
            <span class="t-name">${esc(toolSummary(c))}</span>
            <span class="t-state ${c.error ? 'bad' : c.result ? 'ok' : ''}">${c.error ? 'failed' : c.result ? 'done' : 'running'}</span>
            ${c.error ? `<div class="t-result">${esc(c.error)}</div>` : ''}
            ${c.result && !c.error ? `<div class="t-result">${esc(shortJson(c.result))}</div>` : ''}
          </div>`).join('')
        : '';

      return `<div class="ch-msg ${isUser ? 'user' : 'assistant'}">
        <div class="ch-role">${isUser ? 'You' : 'Agent'}</div>
        ${m.content ? `<div class="ch-body">${body}</div>` : ''}
        ${calls}
        ${sent}
      </div>`;
    }).join('');

    if (state.sending) {
      log.insertAdjacentHTML('beforeend', `<div class="ch-msg">
        <div class="ch-role">Agent</div>
        <div class="ch-thinking"><span></span><span></span><span></span></div>
        ${state.runningTool ? `<div class="ch-tool"><span class="t-name">${esc(state.runningTool)}</span><span class="t-state">running</span></div>` : ''}
      </div>`);
    }

    // Follow the bottom only when the user has not scrolled away from it.
    const nearBottom = log.scrollHeight - log.scrollTop - log.clientHeight < 140;
    if (nearBottom) log.scrollTop = log.scrollHeight;
    renderNotice();
  }

  function renderNotice() {
    const notice = $('#ch-notice');
    if (!notice) return;
    // The model notice is informational and the error is actionable, so the error
    // wins when both are set; a reconciliation that happened alongside a failure
    // would only be noise next to the failure.
    const text = state.error || state.modelNotice;
    notice.classList.toggle('hidden', !text);
    if (!text) { notice.innerHTML = ''; return; }
    if (!state.error) {
      notice.innerHTML = `${esc(text)}<button id="ch-notice-dismiss">Got it</button>`;
      const dismiss = $('#ch-notice-dismiss');
      if (dismiss) {
        dismiss.addEventListener('click', () => {
          state.modelNotice = '';
          renderNotice();
        });
      }
      return;
    }
    const offersSettings = /provider|settings|configured/i.test(state.error);
    notice.innerHTML = `${esc(state.error)}${offersSettings
      ? '<br><button id="ch-open-settings">Open AI provider settings</button>'
      : ''}`;
    const open = $('#ch-open-settings');
    if (open) {
      open.addEventListener('click', () => {
        if (window.RustBoxSettings) window.RustBoxSettings.open('ai');
      });
    }
  }

  function renderComposer() {
    const host = $('#ch-attach');
    if (host) {
      host.innerHTML = state.attachments.map((a, i) => `
        <span class="ch-chip">
          ${a.mime && a.mime.startsWith('image/') ? `<img src="data:${esc(a.mime)};base64,${esc(a.data)}" alt="">` : ''}
          <span class="c-name">${esc(a.name)}</span>
          <button data-attach-del="${i}" title="Remove">&times;</button>
        </span>`).join('');
      host.querySelectorAll('[data-attach-del]').forEach(btn => {
        btn.addEventListener('click', () => {
          state.attachments.splice(Number(btn.dataset.attachDel), 1);
          renderComposer();
        });
      });
    }
    const send = $('#ch-send');
    if (send) send.disabled = state.sending;
    const input = $('#ch-input');
    if (input) {
      input.disabled = state.sending;
      input.placeholder = state.sending
        ? (state.runningTool ? `Running ${state.runningTool}…` : 'Waiting for the agent…')
        : 'Ask anything about this page';
    }
  }

  // ---- data ----------------------------------------------------------------

  async function loadProviders() {
    try {
      const view = await invoke('get_settings');
      state.providers = (view.provider_rows || []).map(row => ({
        id: row.id,
        name: row.name,
        model: row.model,
        models: (row.models && row.models.length) ? row.models : (row.model ? [row.model] : []),
        ready: row.ready,
      }));
    } catch (_) {
      state.providers = [];
    }
  }

  async function loadConversation(id) {
    if (!id) {
      state.conversation = null;
      state.conversationId = null;
      return;
    }
    try {
      const before = state.conversation && state.conversation.id === id
        ? state.conversation.model
        : null;
      state.conversation = await invoke('chat_get', { id });
      state.conversationId = id;
      state.error = '';
      // The backend reconciles a model the provider no longer serves back to the
      // provider's default, and saves that. Without a word the picker would just
      // look different the next time the panel opens, so say it happened and why.
      if (before && state.conversation.model && before !== state.conversation.model) {
        state.modelNotice = `'${before}' is no longer offered by this provider, so this conversation now uses '${state.conversation.model}'.`;
      }
    } catch (e) {
      state.conversation = null;
      state.conversationId = null;
      state.error = String(e?.message || e);
    }
  }

  async function loadHistory() {
    try {
      const res = await invoke('chat_list');
      return res.conversations || [];
    } catch (_) {
      return [];
    }
  }

  async function newConversation() {
    const provider = state.providers.find(p => p.ready) || state.providers[0] || null;
    if (!provider) {
      state.error = 'No AI provider is configured. Add one in Settings, then come back.';
      render();
      return;
    }
    try {
      const conversation = await invoke('chat_new', {
        providerId: provider.id,
        model: provider.model || (provider.models && provider.models[0]) || null,
      });
      state.conversation = conversation;
      state.conversationId = conversation.id;
      state.error = '';
      render();
    } catch (e) {
      state.error = String(e?.message || e);
      render();
    }
  }

  /** Point the current conversation at another provider.
   *
   *  Written to the conversation, not just to the UI: the next request reads it
   *  from there, and reopening the conversation later has to remember it. */
  async function switchProvider(providerId) {
    if (!state.conversation || !state.conversationId) return;
    const provider = state.providers.find(p => p.id === providerId);
    if (!provider) return;
    try {
      const updated = await invoke('chat_update', {
        id: state.conversationId,
        providerId,
        model: provider.model || '',
      });
      state.conversation = updated;
      state.error = '';
    } catch (e) {
      state.error = String(e?.message || e);
    }
    render();
  }

  /** Point the current conversation at another model of the same provider.
   *
   *  Same reason as switching provider: the choice has to live on the
   *  conversation so the next request uses it and reopening remembers it. */
  async function switchModel(model) {
    if (!state.conversation || !state.conversationId) return;
    try {
      const updated = await invoke('chat_update', {
        id: state.conversationId,
        model,
      });
      state.conversation = updated;
      state.error = '';
    } catch (e) {
      state.error = String(e?.message || e);
    }
    render();
  }

  async function send() {
    const input = $('#ch-input');
    const text = (input ? input.value : '').trim();
    if (state.sending) return;
    if (!text && !state.attachments.length) return;

    if (!state.conversationId) {
      await newConversation();
      if (!state.conversationId) return;
    }

    state.sending = true;
    state.runningTool = '';
    state.error = '';
    const attachments = state.attachments.slice();
    state.attachments = [];
    if (input) { input.value = ''; autosize(); }

    // Show the turn immediately: waiting for the round trip to echo it back
    // makes the panel feel dead for as long as the model takes.
    const optimistic = {
      id: `local-${Date.now()}`,
      role: 'user',
      content: text,
      attachments,
      tool_calls: [],
      at: Date.now(),
    };
    state.conversation = state.conversation || { messages: [] };
    state.conversation.messages = (state.conversation.messages || []).concat([optimistic]);
    render();

    try {
      const result = await invoke('chat_send', {
        conversationId: state.conversationId,
        text,
        attachments: attachments.map(a => ({ name: a.name, mime: a.mime, data: a.data })),
      });
      const keepTitle = state.conversation.title;
      state.conversation = {
        ...state.conversation,
        messages: result.messages || [],
        title: keepTitle,
      };
      // The backend titles a conversation from its first user message, so a
      // refetch is what carries the new title back.
      if (!keepTitle || keepTitle === 'New conversation') {
        await loadConversation(state.conversationId);
      }
    } catch (e) {
      state.error = String(e?.message || e);
      // Drop the optimistic turn: it never reached the model, and leaving it in
      // would make the transcript lie about what was sent.
      state.conversation.messages = state.conversation.messages.filter(m => m.id !== optimistic.id);
    } finally {
      state.sending = false;
      state.runningTool = '';
      render();
      const box = $('#ch-input');
      if (box && !box.disabled) box.focus();
    }
  }

  // ---- attachments ---------------------------------------------------------

  /* Images are sent inline; anything else is passed by name, because there is no
     portable way to hand arbitrary bytes to a chat model. The cap matches what
     the backend accepts, so an oversized file is refused here with a sentence
     instead of disappearing silently on save. */
  const MAX_BYTES = 6 * 1024 * 1024;

  function attachFiles(files) {
    Array.from(files || []).forEach(file => {
      if (file.size > MAX_BYTES) {
        state.error = `"${file.name}" is ${(file.size / 1048576).toFixed(1)} MB. The limit is 6 MB per file.`;
        render();
        return;
      }
      const reader = new FileReader();
      reader.onload = () => {
        const result = String(reader.result || '');
        const comma = result.indexOf(',');
        state.attachments.push({
          name: file.name,
          mime: file.type || 'application/octet-stream',
          data: comma >= 0 ? result.slice(comma + 1) : result,
        });
        renderComposer();
      };
      reader.onerror = () => {
        state.error = `Could not read "${file.name}".`;
        render();
      };
      reader.readAsDataURL(file);
    });
  }

  // ---- history overlay -----------------------------------------------------

  async function toggleHistory(force) {
    const panel = $('#ch-history');
    if (!panel) return;
    const want = force === undefined ? !state.historyOpen : force;
    state.historyOpen = want;
    panel.classList.toggle('open', want);
    if (want) await renderHistory();
  }

  async function renderHistory() {
    const list = $('#ch-h-list');
    if (!list) return;
    const conversations = await loadHistory();
    if (!conversations.length) {
      list.innerHTML = '<div class="ch-h-empty">No conversations yet. The first message you send creates one.</div>';
      return;
    }
    list.innerHTML = conversations.map(c => `
      <div class="ch-conv ${c.id === state.conversationId ? 'sel' : ''}" data-conv="${esc(c.id)}">
        <div class="c-main">
          <div class="c-title">${esc(c.title || 'New conversation')}</div>
          <div class="c-meta">${c.message_count} message${c.message_count === 1 ? '' : 's'} · ${esc(fmtTime(c.updated_at))}</div>
        </div>
        <div class="c-act">
          <button data-conv-rename="${esc(c.id)}" title="Rename">&#9998;</button>
          <button class="danger" data-conv-del="${esc(c.id)}" title="Delete">&times;</button>
        </div>
      </div>`).join('');

    list.querySelectorAll('[data-conv]').forEach(row => {
      row.addEventListener('click', async (e) => {
        if (e.target.closest('button')) return;
        await loadConversation(row.dataset.conv);
        await toggleHistory(false);
        render();
      });
    });

    list.querySelectorAll('[data-conv-del]').forEach(btn => {
      btn.addEventListener('click', async (e) => {
        e.stopPropagation();
        const id = btn.dataset.convDel;
        const ok = await window.RustBoxUI.askConfirm({
          title: 'Delete conversation?',
          text: 'Every message in it will be removed. This cannot be undone.',
          okLabel: 'Delete',
        });
        if (!ok) return;
        try {
          await invoke('chat_delete', { id });
          if (state.conversationId === id) {
            state.conversation = null;
            state.conversationId = null;
          }
          await renderHistory();
          render();
        } catch (err) {
          state.error = String(err?.message || err);
          render();
        }
      });
    });

    list.querySelectorAll('[data-conv-rename]').forEach(btn => {
      btn.addEventListener('click', async (e) => {
        e.stopPropagation();
        const id = btn.dataset.convRename;
        const current = conversations.find(c => c.id === id);
        const title = window.prompt('Rename conversation', current ? current.title : '');
        if (title === null) return;
        try {
          await invoke('chat_rename', { id, title });
          if (state.conversationId === id) await loadConversation(id);
          await renderHistory();
          render();
        } catch (err) {
          state.error = String(err?.message || err);
          render();
        }
      });
    });
  }

  // ---- open / close --------------------------------------------------------

  function autosize() {
    const input = $('#ch-input');
    if (!input) return;
    input.style.height = 'auto';
    input.style.height = `${Math.min(input.scrollHeight, 140)}px`;
  }

  async function open() {
    if (state.open) { $('#ch-input')?.focus(); return; }
    state.open = true;
    $('#chat-panel').classList.add('open');
    announce(true);
    // The panel takes width from #content, so the tab windows have to be told
    // the new rect or they will sit underneath it.
    if (window.RustBoxUI) window.RustBoxUI.relayout();
    await loadProviders();
    if (!state.conversationId) {
      // Resume the most recent conversation instead of starting blank: a panel
      // that forgets what you were doing is worse than one extra click.
      const conversations = await loadHistory();
      if (conversations.length) await loadConversation(conversations[0].id);
    } else {
      await loadConversation(state.conversationId);
    }
    render();
    setTimeout(() => $('#ch-input')?.focus(), 30);
  }

  function close() {
    if (!state.open) return;
    state.open = false;
    state.historyOpen = false;
    $('#chat-panel').classList.remove('open');
    $('#ch-history')?.classList.remove('open');
    announce(false);
    if (window.RustBoxUI) window.RustBoxUI.relayout();
  }

  /** Tell the shell whether this panel is up, so its sidebar button can reflect
   *  the state without polling. */
  function announce(open) {
    document.dispatchEvent(new CustomEvent('rustbox:overlay', {
      detail: { name: 'chat', open },
    }));
  }

  function toggle() {
    if (state.open) close(); else open();
  }

  // ---- boot ----------------------------------------------------------------

  function boot() {
    const panel = $('#chat-panel');
    if (!panel) return;

    panel.querySelector('#ch-close').addEventListener('click', close);
    panel.querySelector('#ch-history-btn').addEventListener('click', () => toggleHistory());
    panel.querySelector('#ch-h-close').addEventListener('click', () => toggleHistory(false));
    panel.querySelector('#ch-new').addEventListener('click', async () => {
      await toggleHistory(false);
      await newConversation();
    });
    panel.querySelector('#ch-h-new').addEventListener('click', async () => {
      await toggleHistory(false);
      await newConversation();
    });
    panel.querySelector('#ch-send').addEventListener('click', send);

    // The backend emits this before and after every tool call. Subscribing is
    // what turns "Waiting for the agent…" into the name of the tool actually
    // running — the difference between a panel that looks hung and one that is
    // visibly working through a slow page load.
    if (window.__TAURI__.event) {
      window.__TAURI__.event.listen('chat-tool', ({ payload }) => {
        if (!state.sending) return;
        const name = payload && payload.tool ? String(payload.tool) : '';
        state.runningTool = payload && payload.phase === 'start' ? name : '';
        renderComposer();
      });
    }
    panel.querySelector('#ch-attach-btn').addEventListener('click', () => {
      panel.querySelector('#ch-file').click();
    });
    panel.querySelector('#ch-file').addEventListener('change', (e) => {
      attachFiles(e.target.files);
      e.target.value = '';
    });

    const input = panel.querySelector('#ch-input');
    input.addEventListener('input', autosize);
    input.addEventListener('keydown', (e) => {
      // Enter sends, Shift+Enter breaks the line: the convention every chat
      // window uses, and the one a long prompt needs.
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        send();
      }
    });

    // Dropping a file on the panel attaches it, because the alternative is a
    // file dialog for something already under the cursor.
    panel.addEventListener('dragover', (e) => e.preventDefault());
    panel.addEventListener('drop', (e) => {
      e.preventDefault();
      if (e.dataTransfer && e.dataTransfer.files.length) attachFiles(e.dataTransfer.files);
    });

    document.addEventListener('keydown', (e) => {
      if (!state.open) return;
      if (e.key === 'Escape') {
        e.stopPropagation();
        if (state.historyOpen) { toggleHistory(false); return; }
        close();
      }
    });
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot);
  } else {
    boot();
  }

  window.RustBoxChat = { open, close, toggle, isOpen: () => state.open };
})();
