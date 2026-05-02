#!/usr/bin/env python3

import argparse
import asyncio
import json
import os
import subprocess
import sys
import time
import urllib.parse
import urllib.request

import websockets


DEFAULT_MATCH = (
    "chatgpt.com/g/g-p-69d5aab6319c8191abe0e3298935c109-canon-mini-agent/project?tab=sources"
)
DEFAULT_TARGET_URL = (
    "https://chatgpt.com/g/g-p-69d5aab6319c8191abe0e3298935c109-canon-mini-agent/project?tab=sources"
)


def parse_args():
    p = argparse.ArgumentParser(description="Upload a local file via CDP into a file input.")
    p.add_argument("--file", help="Path to file to upload")
    p.add_argument(
        "--build-tar",
        action="store_true",
        help="Run tar script first and upload the produced archive",
    )
    p.add_argument(
        "--tar-script",
        default="/workspace/ai_sandbox/canon-mini-agent/tar.sh",
        help="Tar script path used with --build-tar",
    )
    p.add_argument(
        "--tar-output",
        default="canon-mini-agent.tar.gz",
        help="Expected tar output filename or absolute path used with --build-tar",
    )
    p.add_argument("--cdp", default="http://127.0.0.1:9222", help="CDP HTTP endpoint")
    p.add_argument("--match", default=DEFAULT_MATCH, help="Substring to match page URL")
    p.add_argument(
        "--open-target-if-missing",
        action="store_true",
        help="If no matching tab exists, open target URL in a new tab and wait for it",
    )
    p.add_argument(
        "--target-url",
        default=DEFAULT_TARGET_URL,
        help="URL to open when --open-target-if-missing is enabled",
    )
    p.add_argument(
        "--target-wait-timeout-sec",
        type=float,
        default=30.0,
        help="How long to wait for target tab to appear after opening",
    )
    p.add_argument(
        "--page-ready-timeout-sec",
        type=float,
        default=25.0,
        help="How long to wait for page UI (including Sources tab) to become interactive",
    )
    p.add_argument("--selector", default="input[type=file]", help="CSS selector")
    p.add_argument(
        "--scope",
        default="any",
        choices=["any", "sources"],
        help="Restrict input selection. 'sources' avoids chat composer upload.",
    )
    p.add_argument(
        "--open-sources-flow",
        action="store_true",
        help="Try clicking Sources then + Add sources before selecting input",
    )
    p.add_argument(
        "--list-inputs",
        action="store_true",
        help="List discovered file inputs and exit without uploading",
    )
    p.add_argument("--verbose", action="store_true", help="Log CDP traffic")
    p.add_argument(
        "--confirm-loaded",
        action="store_true",
        help="After upload, wait until uploaded filename appears in page text",
    )
    p.add_argument(
        "--confirm-timeout-sec",
        type=float,
        default=20.0,
        help="Timeout for --confirm-loaded",
    )
    p.add_argument(
        "--confirm-settle-sec",
        type=float,
        default=2.0,
        help="Ready state must remain stable for this long before confirming",
    )
    p.add_argument(
        "--force-upload",
        action="store_true",
        help="Upload even if filename already appears in the page",
    )
    p.add_argument(
        "--message",
        help="Message to send in the ChatGPT project composer after the source is ready",
    )
    p.add_argument(
        "--message-file",
        help="Path to a UTF-8 text file containing the message to send after the source is ready",
    )
    p.add_argument(
        "--message-timeout-sec",
        type=float,
        default=20.0,
        help="Timeout for finding/enabling the chat composer when --message is used",
    )
    args = p.parse_args()
    if not args.build_tar and not args.file:
        p.error("either --file or --build-tar is required")
    if args.message and args.message_file:
        p.error("use either --message or --message-file, not both")
    return args


def resolve_upload_file(args):
    if not args.build_tar:
        file_path = os.path.abspath(args.file)
        if not os.path.exists(file_path):
            raise RuntimeError(f"File not found: {file_path}")
        return file_path

    tar_script = os.path.abspath(args.tar_script)
    if not os.path.exists(tar_script):
        raise RuntimeError(f"Tar script not found: {tar_script}")

    tar_cwd = os.path.dirname(tar_script)
    proc = subprocess.run(
        ["bash", tar_script],
        cwd=tar_cwd,
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        details = (proc.stderr or proc.stdout or "").strip()
        raise RuntimeError(f"tar build failed ({tar_script}): {details}")

    if os.path.isabs(args.tar_output):
        out_path = args.tar_output
    else:
        out_path = os.path.join(tar_cwd, args.tar_output)
    out_path = os.path.abspath(out_path)

    if not os.path.exists(out_path):
        raise RuntimeError(f"Tar output not found after build: {out_path}")
    print(f"Built tar artifact: {out_path}")
    return out_path


def resolve_message(args):
    if args.message_file:
        message_path = os.path.abspath(args.message_file)
        if not os.path.exists(message_path):
            raise RuntimeError(f"Message file not found: {message_path}")
        with open(message_path, "r", encoding="utf-8") as f:
            message = f.read()
    else:
        message = args.message

    if message is None:
        return None
    if not message.strip():
        raise RuntimeError("Message is empty")
    return message


def list_page_targets(cdp_base: str):
    with urllib.request.urlopen(f"{cdp_base}/json/list") as r:
        targets = json.loads(r.read().decode("utf-8"))
    return [t for t in targets if t.get("type") == "page"]


def get_target(cdp_base: str, match: str):
    pages = list_page_targets(cdp_base)
    for t in pages:
        if match in str(t.get("url", "")):
            if "webSocketDebuggerUrl" not in t:
                raise RuntimeError("Matched target has no webSocketDebuggerUrl")
            return t
    urls = "\n- ".join(str(p.get("url", "")) for p in pages)
    raise RuntimeError(f"No page target matched substring:\n{match}\n\nOpen page URLs:\n- {urls}")


def open_new_tab(cdp_base: str, target_url: str):
    encoded = urllib.parse.quote(target_url, safe=":/?&=#%")
    req = urllib.request.Request(f"{cdp_base}/json/new?{encoded}", method="PUT")
    with urllib.request.urlopen(req) as r:
        _ = r.read()


def get_target_with_open_wait(
    cdp_base: str, match: str, open_if_missing: bool, target_url: str, timeout_sec: float
):
    try:
        return get_target(cdp_base, match)
    except RuntimeError as first_err:
        if not open_if_missing:
            raise first_err
        print(f"No matching tab found. Opening target URL: {target_url}")
        open_new_tab(cdp_base, target_url)
        deadline = time.time() + max(1.0, timeout_sec)
        while time.time() < deadline:
            try:
                return get_target(cdp_base, match)
            except RuntimeError:
                time.sleep(0.5)
        raise RuntimeError(
            "Opened target URL but matching tab was not found within "
            f"{timeout_sec}s for match: {match}"
        )


class CdpClient:
    def __init__(self, ws, verbose=False):
        self.ws = ws
        self.verbose = verbose
        self.next_id = 1

    async def send(self, method, params=None):
        if params is None:
            params = {}
        msg_id = self.next_id
        self.next_id += 1
        payload = {"id": msg_id, "method": method, "params": params}
        if self.verbose:
            print(f"=> {json.dumps(payload)}", file=sys.stderr)
        await self.ws.send(json.dumps(payload))
        while True:
            raw = await self.ws.recv()
            resp = json.loads(raw)
            if "id" not in resp:
                continue
            if self.verbose:
                print(f"<= {json.dumps(resp)}", file=sys.stderr)
            if resp["id"] != msg_id:
                continue
            if "error" in resp:
                raise RuntimeError(json.dumps(resp["error"]))
            return resp.get("result", {})


async def evaluate(cdp: CdpClient, expression: str):
    return await cdp.send(
        "Runtime.evaluate",
        {
            "expression": expression,
            "returnByValue": True,
            "awaitPromise": True,
            "silent": True,
        },
    )


async def click_by_text(cdp: CdpClient, text: str):
    js = f"""
    (() => {{
      const target = {json.dumps(text)}.trim().toLowerCase();
      const candidates = Array.from(document.querySelectorAll('button,[role="button"],a,div,span'));
      const visible = (el) => {{
        const style = getComputedStyle(el);
        const r = el.getBoundingClientRect();
        return style.visibility !== 'hidden' && style.display !== 'none' && r.width > 0 && r.height > 0;
      }};
      const norm = (s) => (s || '').replace(/\\s+/g, ' ').trim().toLowerCase();
      for (const el of candidates) {{
        const t = norm(el.innerText || el.textContent);
        if (t === target && visible(el)) {{
          el.click();
          return true;
        }}
      }}
      for (const el of candidates) {{
        const t = norm(el.innerText || el.textContent);
        if (t.includes(target) && visible(el)) {{
          el.click();
          return true;
        }}
      }}
      return false;
    }})()
    """
    result = await evaluate(cdp, js)
    return bool(result.get("result", {}).get("value"))


async def wait_for_sources_ui(cdp: CdpClient, timeout_sec: float):
    started = asyncio.get_event_loop().time()
    js = r"""
    (() => {
      const norm = (s) => (s || '').replace(/\s+/g, ' ').trim().toLowerCase();
      const visible = (el) => {
        const st = getComputedStyle(el);
        const r = el.getBoundingClientRect();
        return st.display !== 'none' && st.visibility !== 'hidden' && r.width > 0 && r.height > 0;
      };
      const ready = document.readyState === 'interactive' || document.readyState === 'complete';
      const candidates = Array.from(document.querySelectorAll('button,[role="tab"],[role="button"],a,div,span'));
      const hasSources = candidates.some(el => visible(el) && norm(el.innerText || el.textContent) === 'sources');
      return { ready, hasSources };
    })()
    """
    while (asyncio.get_event_loop().time() - started) < timeout_sec:
        res = await evaluate(cdp, js)
        val = res.get("result", {}).get("value", {})
        if val.get("ready") and val.get("hasSources"):
            return True
        await asyncio.sleep(0.35)
    return False


async def list_file_inputs(cdp: CdpClient):
    js = r"""
    (() => {
      const visible = (el) => {
        const style = getComputedStyle(el);
        const r = el.getBoundingClientRect();
        return style.visibility !== 'hidden' && style.display !== 'none' && r.width > 0 && r.height > 0;
      };
      const shortText = (el) => {
        const t = (el.innerText || el.textContent || '').replace(/\s+/g, ' ').trim();
        return t.slice(0, 120);
      };
      const all = Array.from(document.querySelectorAll('input[type="file"]'));
      return all.map((el, idx) => {
        const parent = el.closest('div,section,form,main,aside,dialog') || el.parentElement;
        const rect = el.getBoundingClientRect();
        return {
          index: idx,
          id: el.id || '',
          className: el.className || '',
          name: el.getAttribute('name') || '',
          accept: el.getAttribute('accept') || '',
          multiple: !!el.multiple,
          hiddenAttr: el.hidden || false,
          ariaHidden: el.getAttribute('aria-hidden') || '',
          isVisible: visible(el),
          rect: { x: rect.x, y: rect.y, w: rect.width, h: rect.height },
          parentText: parent ? shortText(parent) : '',
          parentHtmlHead: parent ? (parent.outerHTML || '').slice(0, 220) : '',
        };
      });
    })()
    """
    result = await evaluate(cdp, js)
    return result.get("result", {}).get("value", [])


async def is_filename_visible(cdp: CdpClient, filename: str) -> bool:
    js = f"""
    (() => {{
      const needle = {json.dumps(filename)}.toLowerCase();
      const nodes = Array.from(document.querySelectorAll('*'));
      const visible = (el) => {{
        const style = getComputedStyle(el);
        const r = el.getBoundingClientRect();
        return style.visibility !== 'hidden' && style.display !== 'none' && r.width > 0 && r.height > 0;
      }};
      for (const el of nodes) {{
        if (!visible(el)) continue;
        const txt = (el.innerText || el.textContent || '').toLowerCase();
        if (txt.includes(needle)) return true;
      }}
      return false;
    }})()
    """
    result = await evaluate(cdp, js)
    return bool(result.get("result", {}).get("value"))


async def get_source_file_state(cdp: CdpClient, filename: str):
    js = f"""
    (() => {{
      const needle = {json.dumps(filename)}.toLowerCase();
      const root = document.querySelector('section[aria-label="Sources"]') || document;
      const norm = (s) => (s || '').replace(/\\s+/g, ' ').trim();
      const rows = Array.from(root.querySelectorAll('div.group\\\\/file-row'));
      const row = rows.find(r => {{
        const rowText = norm(r.innerText || r.textContent).toLowerCase();
        const label = r.querySelector('[aria-label]');
        const aria = (label?.getAttribute('aria-label') || '').toLowerCase();
        return rowText.includes(needle) || aria.includes(needle);
      }});
      if (!row) {{
        return {{
          exists: false,
          ready: false,
          loading: false,
          rowText: "",
          rowSignature: "",
          reason: "not-found"
        }};
      }}
      const rowText = norm(row.innerText || row.textContent);
      const lower = norm(row.innerText || row.textContent).toLowerCase();
      const loadingTokens = [
        "loading", "processing", "indexing", "uploading", "queued",
        "analyzing", "preparing", "extracting", "reading", "scanning", "in progress"
      ];
      const errorTokens = ["failed", "error"];
      const hasLoadingToken = loadingTokens.some(t => lower.includes(t));
      const hasErrorToken = errorTokens.some(t => lower.includes(t));
      const hasSpinner = !!row.querySelector(
        '[aria-busy="true"], [role="progressbar"], .animate-spin, [data-state="loading"], [data-loading="true"]'
      );
      const hasBusyAncestor = !!row.closest('[aria-busy="true"]');
      const spinnerCount = row.querySelectorAll('.animate-spin,[role="progressbar"],[data-state="loading"],[data-loading="true"]').length;
      const hasWarningOnly = lower.includes("file contents may not be accessible");
      const loading = hasLoadingToken || hasSpinner || hasBusyAncestor;
      const ready = !loading && !hasErrorToken;
      return {{
        exists: true,
        ready,
        loading,
        hasSpinner,
        spinnerCount,
        hasBusyAncestor,
        hasLoadingToken,
        hasErrorToken,
        hasWarningOnly,
        rowText: rowText.slice(0, 500),
        rowSignature: `${{rowText.toLowerCase().slice(0, 300)}}|spin:${{spinnerCount}}|busy:${{hasBusyAncestor ? 1 : 0}}`,
        reason: ready ? "ready" : (loading ? "loading" : (hasErrorToken ? "error" : "unknown"))
      }};
    }})()
    """
    result = await evaluate(cdp, js)
    return result.get("result", {}).get("value", {})


async def wait_for_loaded(cdp: CdpClient, filename: str, timeout_sec: float, settle_sec: float):
    started = asyncio.get_event_loop().time()
    ready_since = None
    ready_signature = None
    while (asyncio.get_event_loop().time() - started) < timeout_sec:
        st = await get_source_file_state(cdp, filename)
        if st.get("exists") and st.get("ready"):
            sig = st.get("rowSignature", "")
            now = asyncio.get_event_loop().time()
            if ready_since is None or sig != ready_signature:
                ready_since = now
                ready_signature = sig
            elif (now - ready_since) >= settle_sec:
                return True
        else:
            ready_since = None
            ready_signature = None
        await asyncio.sleep(0.4)
    return False


async def get_composer_state(cdp: CdpClient):
    js = r"""
    (() => {
      const visible = (el) => {
        if (!el) return false;
        const st = getComputedStyle(el);
        const r = el.getBoundingClientRect();
        return st.display !== 'none' && st.visibility !== 'hidden' && r.width > 0 && r.height > 0;
      };
      const composer =
        document.querySelector('form[data-type="unified-composer"]') ||
        document.querySelector('form.group\\/composer') ||
        document;
      const textbox =
        composer.querySelector('#prompt-textarea') ||
        composer.querySelector('[contenteditable="true"][role="textbox"]') ||
        composer.querySelector('[role="textbox"]') ||
        composer.querySelector('textarea');
      const sendButton =
        composer.querySelector('#composer-submit-button') ||
        composer.querySelector('[data-testid="send-button"]') ||
        Array.from(composer.querySelectorAll('button')).find(btn => {
          const label = (btn.getAttribute('aria-label') || '').toLowerCase();
          return label.includes('send prompt') || label === 'send';
        });
      const disabled = !sendButton || sendButton.disabled || sendButton.getAttribute('aria-disabled') === 'true';
      return {
        hasTextbox: !!textbox,
        textboxVisible: visible(textbox),
        hasSendButton: !!sendButton,
        sendVisible: visible(sendButton),
        sendDisabled: !!disabled,
        text: textbox ? (textbox.innerText || textbox.value || textbox.textContent || '') : ''
      };
    })()
    """
    result = await evaluate(cdp, js)
    return result.get("result", {}).get("value", {})


async def wait_for_composer(cdp: CdpClient, timeout_sec: float):
    started = asyncio.get_event_loop().time()
    while (asyncio.get_event_loop().time() - started) < timeout_sec:
        state = await get_composer_state(cdp)
        if state.get("hasTextbox") and state.get("textboxVisible"):
            return True
        await asyncio.sleep(0.3)
    return False


async def set_chat_message(cdp: CdpClient, message: str):
    js = f"""
    (() => {{
      const message = {json.dumps(message)};
      const visible = (el) => {{
        if (!el) return false;
        const st = getComputedStyle(el);
        const r = el.getBoundingClientRect();
        return st.display !== 'none' && st.visibility !== 'hidden' && r.width > 0 && r.height > 0;
      }};
      const composer =
        document.querySelector('form[data-type="unified-composer"]') ||
        document.querySelector('form.group\\\\/composer') ||
        document;
      const textbox =
        composer.querySelector('#prompt-textarea') ||
        composer.querySelector('[contenteditable="true"][role="textbox"]') ||
        composer.querySelector('[role="textbox"]') ||
        composer.querySelector('textarea');
      if (!textbox || !visible(textbox)) {{
        return {{ ok: false, reason: "textbox-not-found" }};
      }}

      textbox.focus();
      if (textbox.tagName === 'TEXTAREA' || textbox.tagName === 'INPUT') {{
        textbox.value = '';
        textbox.dispatchEvent(new Event('input', {{ bubbles: true }}));
        textbox.value = message;
        textbox.dispatchEvent(new InputEvent('input', {{
          bubbles: true,
          cancelable: true,
          inputType: 'insertText',
          data: message
        }}));
      }} else {{
        const selection = window.getSelection();
        const range = document.createRange();
        range.selectNodeContents(textbox);
        selection.removeAllRanges();
        selection.addRange(range);
        document.execCommand('delete', false, null);
        document.execCommand('insertText', false, message);
        textbox.dispatchEvent(new InputEvent('input', {{
          bubbles: true,
          cancelable: true,
          inputType: 'insertText',
          data: message
        }}));
      }}

      return {{
        ok: true,
        reason: "message-inserted",
        text: (textbox.innerText || textbox.value || textbox.textContent || '').slice(0, 500)
      }};
    }})()
    """
    result = await evaluate(cdp, js)
    return result.get("result", {}).get("value", {})


async def wait_for_send_enabled(cdp: CdpClient, timeout_sec: float):
    started = asyncio.get_event_loop().time()
    while (asyncio.get_event_loop().time() - started) < timeout_sec:
        state = await get_composer_state(cdp)
        if state.get("hasSendButton") and state.get("sendVisible") and not state.get("sendDisabled"):
            return True
        await asyncio.sleep(0.25)
    return False


async def click_send_button(cdp: CdpClient):
    js = r"""
    (() => {
      const visible = (el) => {
        if (!el) return false;
        const st = getComputedStyle(el);
        const r = el.getBoundingClientRect();
        return st.display !== 'none' && st.visibility !== 'hidden' && r.width > 0 && r.height > 0;
      };
      const composer =
        document.querySelector('form[data-type="unified-composer"]') ||
        document.querySelector('form.group\\/composer') ||
        document;
      const sendButton =
        composer.querySelector('#composer-submit-button') ||
        composer.querySelector('[data-testid="send-button"]') ||
        Array.from(composer.querySelectorAll('button')).find(btn => {
          const label = (btn.getAttribute('aria-label') || '').toLowerCase();
          return label.includes('send prompt') || label === 'send';
        });
      if (!sendButton || !visible(sendButton)) {
        return { ok: false, reason: "send-button-not-found" };
      }
      if (sendButton.disabled || sendButton.getAttribute('aria-disabled') === 'true') {
        return { ok: false, reason: "send-button-disabled" };
      }
      const r = sendButton.getBoundingClientRect();
      const cx = r.left + r.width / 2;
      const cy = r.top + r.height / 2;
      const target = document.elementFromPoint(cx, cy) || sendButton;
      const fire = (node, type) => node.dispatchEvent(
        new MouseEvent(type, { bubbles: true, cancelable: true, clientX: cx, clientY: cy })
      );
      fire(target, 'mousedown');
      fire(target, 'mouseup');
      fire(target, 'click');
      try { sendButton.click(); } catch (_e) {}
      return { ok: true, reason: "clicked-send-button" };
    })()
    """
    result = await evaluate(cdp, js)
    return result.get("result", {}).get("value", {})


async def send_chat_message(cdp: CdpClient, message: str, timeout_sec: float):
    composer_ready = await wait_for_composer(cdp, timeout_sec)
    if not composer_ready:
        raise RuntimeError(f"Timed out waiting for chat composer ({timeout_sec}s)")

    inserted = await set_chat_message(cdp, message)
    if not inserted.get("ok"):
        raise RuntimeError(f"Failed to insert chat message: {inserted}")

    enabled = await wait_for_send_enabled(cdp, timeout_sec)
    if not enabled:
        state = await get_composer_state(cdp)
        raise RuntimeError(f"Timed out waiting for send button to enable: {state}")

    sent = await click_send_button(cdp)
    if not sent.get("ok"):
        raise RuntimeError(f"Failed to send chat message: {sent}")
    return sent


async def wait_for_gone(cdp: CdpClient, filename: str, timeout_sec: float):
    started = asyncio.get_event_loop().time()
    while (asyncio.get_event_loop().time() - started) < timeout_sec:
        st = await get_source_file_state(cdp, filename)
        if not st.get("exists"):
            return True
        await asyncio.sleep(0.35)
    return False


async def remove_one_source_file(cdp: CdpClient, filename: str):
    js = f"""
    (() => {{
      const needle = {json.dumps(filename)}.toLowerCase();
      const root = document.querySelector('section[aria-label="Sources"]') || document;
      const norm = (s) => (s || '').replace(/\\s+/g, ' ').trim().toLowerCase();
      const visible = (el) => {{
        const st = getComputedStyle(el);
        const r = el.getBoundingClientRect();
        return st.display !== 'none' && st.visibility !== 'hidden' && r.width > 0 && r.height > 0;
      }};
      const doClick = (el) => {{
        try {{
          el.scrollIntoView({{block: 'center', inline: 'center'}});
        }} catch (_e) {{}}
        const r = el.getBoundingClientRect();
        const cx = r.left + r.width / 2;
        const cy = r.top + r.height / 2;
        const target = document.elementFromPoint(cx, cy) || el;
        const fire = (node, type) => node.dispatchEvent(
          new MouseEvent(type, {{bubbles: true, cancelable: true, clientX: cx, clientY: cy}})
        );
        fire(target, 'mousedown');
        fire(target, 'mouseup');
        fire(target, 'click');
        try {{ el.click(); }} catch (_e) {{}}
      }};
      const rows = Array.from(root.querySelectorAll('div.group\\\\/file-row')).filter(
        row => norm(row.innerText || row.textContent).includes(needle)
      );
      if (!rows.length) return {{ ok: false, reason: "not-found-row" }};
      const row = rows[0];
      const actionBtn =
        row.querySelector('button[aria-label*="Source actions"]') ||
        row.querySelector('button,[role="button"]');
      if (!actionBtn) return {{ ok: false, reason: "no-source-actions-button" }};
      doClick(actionBtn);

      const menuCandidates = Array.from(document.querySelectorAll(
        '[role="menuitem"],button,[role="button"],a,div,span'
      )).filter(el => visible(el));
      const labels = [
        "remove source", "remove", "delete source", "delete", "remove file", "delete file"
      ];
      for (const el of menuCandidates.reverse()) {{
        const txt = norm(el.innerText || el.textContent);
        const aria = norm(el.getAttribute('aria-label') || '');
        const title = norm(el.getAttribute('title') || '');
        const joined = `${{txt}} ${{aria}} ${{title}}`;
        const disabled = el.matches('[disabled],[aria-disabled="true"]');
        if (!disabled && labels.some(l => joined.includes(l))) {{
          doClick(el);
          return {{ ok: true, reason: "clicked-remove-action", target: joined.slice(0, 120) }};
        }}
      }}
      return {{ ok: false, reason: "remove-action-not-found" }};
    }})()
    """
    result = await evaluate(cdp, js)
    return result.get("result", {}).get("value", {})


async def click_any_remove_control(cdp: CdpClient):
    js = r"""
    (() => {
      const norm = (s) => (s || '').replace(/\s+/g, ' ').trim().toLowerCase();
      const visible = (el) => {
        const st = getComputedStyle(el);
        const r = el.getBoundingClientRect();
        return st.display !== 'none' && st.visibility !== 'hidden' && r.width > 0 && r.height > 0;
      };
      const doClick = (el) => {
        const r = el.getBoundingClientRect();
        const cx = r.left + r.width / 2;
        const cy = r.top + r.height / 2;
        const target = document.elementFromPoint(cx, cy) || el;
        const fire = (node, type) => node.dispatchEvent(
          new MouseEvent(type, {bubbles: true, cancelable: true, clientX: cx, clientY: cy})
        );
        fire(target, 'mousedown');
        fire(target, 'mouseup');
        fire(target, 'click');
        try { el.click(); } catch (_e) {}
      };
      const labels = ["remove source", "remove", "delete source", "delete", "remove file", "delete file"];
      const nodes = Array.from(document.querySelectorAll('[role="menuitem"],button,[role="button"],a,div,span'))
        .filter(el => visible(el))
        .reverse();
      for (const el of nodes) {
        const txt = norm(el.innerText || el.textContent);
        const aria = norm(el.getAttribute('aria-label') || '');
        const title = norm(el.getAttribute('title') || '');
        const joined = `${txt} ${aria} ${title}`;
        const disabled = el.matches('[disabled],[aria-disabled="true"]');
        if (!disabled && labels.some(l => joined.includes(l))) {
          doClick(el);
          return { ok: true, target: joined.slice(0, 140) };
        }
      }
      return { ok: false, target: "" };
    })()
    """
    result = await evaluate(cdp, js)
    return result.get("result", {}).get("value", {})


async def ensure_removed(cdp: CdpClient, filename: str, timeout_sec: float):
    attempts = 0
    while attempts < 5:
        attempts += 1
        state = await get_source_file_state(cdp, filename)
        if not state.get("exists"):
            return True
        removal = await remove_one_source_file(cdp, filename)
        await asyncio.sleep(0.4)
        # Some UIs show a second confirm button ("Remove"/"Delete") in a dialog.
        await click_any_remove_control(cdp)
        if not removal.get("ok"):
            await asyncio.sleep(0.6)
        gone = await wait_for_gone(cdp, filename, timeout_sec)
        if gone:
            return True
    return False


def choose_input(inputs, scope: str):
    if not inputs:
        return None

    def score(item):
        s = 0
        lower_parent = (item.get("parentText", "") + " " + item.get("parentHtmlHead", "")).lower()
        if "source" in lower_parent:
            s += 7
        if "add source" in lower_parent or "add sources" in lower_parent:
            s += 6
        if "drag sources here" in lower_parent:
            s += 8
        if "file contents may not be accessible" in lower_parent:
            s += 6
        if "upload-files" == item.get("id", ""):
            s -= 10
        if "composer" in lower_parent or "message" in lower_parent or "chat" in lower_parent:
            s -= 4
        if item.get("isVisible"):
            s += 3
        if item.get("accept"):
            s += 1
        return s

    ranked = sorted(inputs, key=score, reverse=True)
    if scope == "sources":
        filtered = []
        for i in ranked:
            file_id = i.get("id", "")
            parent_blob = (i.get("parentText", "") + " " + i.get("parentHtmlHead", "")).lower()
            if file_id in {"upload-files", "upload-photos", "upload-camera"}:
                continue
            if "drag sources here" in parent_blob or "file contents may not be accessible" in parent_blob:
                filtered.append(i)
        if filtered:
            return filtered[0]
        return None
    return ranked[0]


async def run():
    args = parse_args()
    file_path = resolve_upload_file(args)
    message = resolve_message(args)
    if not os.path.isabs(file_path):
        raise RuntimeError("--file must be an absolute path")

    target = get_target_with_open_wait(
        args.cdp,
        args.match,
        args.open_target_if_missing,
        args.target_url,
        args.target_wait_timeout_sec,
    )
    print(f"Matched target: {target.get('title', '')}")
    print(f"URL: {target.get('url', '')}")

    async with websockets.connect(target["webSocketDebuggerUrl"]) as ws:
        cdp = CdpClient(ws, verbose=args.verbose)
        await cdp.send("Page.bringToFront")
        await cdp.send("Runtime.enable")
        await cdp.send("DOM.enable")
        ui_ready = await wait_for_sources_ui(cdp, args.page_ready_timeout_sec)
        if not ui_ready:
            raise RuntimeError(
                f"Timed out waiting for page Sources UI to load ({args.page_ready_timeout_sec}s)"
            )
        if args.open_sources_flow:
            await click_by_text(cdp, "Sources")
            await asyncio.sleep(0.5)
            clicked = await click_by_text(cdp, "+ Add sources")
            if not clicked:
                await click_by_text(cdp, "Add sources")
            await asyncio.sleep(0.7)

        filename = os.path.basename(file_path)
        pre_state = await get_source_file_state(cdp, filename)
        already_present = pre_state.get("exists", False)
        if already_present and not args.force_upload:
            print(f"File already present in Sources, skipping upload: {filename}")
            print(f"Existing file state: {pre_state.get('reason')} | {pre_state.get('rowText', '')}")
        elif already_present and args.force_upload:
            removed = await ensure_removed(cdp, filename, max(5.0, args.confirm_timeout_sec))
            if not removed:
                raise RuntimeError(f"--force-upload set, but failed to remove existing file: {filename}")
            print(f"Removed existing source file: {filename}")
            if args.open_sources_flow:
                clicked = await click_by_text(cdp, "+ Add sources")
                if not clicked:
                    await click_by_text(cdp, "Add sources")
                await asyncio.sleep(0.6)

        if not already_present or args.force_upload:
            inputs = await list_file_inputs(cdp)
            if args.list_inputs:
                print(json.dumps(inputs, indent=2))
                return

            chosen = choose_input(inputs, args.scope)
            if not chosen:
                raise RuntimeError(
                    "No matching source upload input found. Open '+ Add sources' and retry."
                )

            eval_result = await cdp.send(
                "Runtime.evaluate",
                {
                    "expression": (
                        f"document.querySelectorAll({json.dumps(args.selector)})"
                        f"[{int(chosen.get('index', 0))}]"
                    ),
                    "objectGroup": "file-input",
                    "returnByValue": False,
                    "awaitPromise": False,
                    "silent": True,
                },
            )
            object_id = eval_result.get("result", {}).get("objectId")
            if not object_id:
                raise RuntimeError(
                    f"Could not resolve chosen input index {chosen.get('index')} for selector {args.selector}"
                )

            described = await cdp.send("DOM.describeNode", {"objectId": object_id})
            backend_node_id = described.get("node", {}).get("backendNodeId")
            if not backend_node_id:
                raise RuntimeError("Failed to resolve backendNodeId for file input")

            await cdp.send(
                "DOM.setFileInputFiles", {"files": [file_path], "backendNodeId": backend_node_id}
            )
            await cdp.send(
                "Runtime.callFunctionOn",
                {
                    "objectId": object_id,
                    "functionDeclaration": (
                        "function(){"
                        "this.dispatchEvent(new Event('input',{bubbles:true}));"
                        "this.dispatchEvent(new Event('change',{bubbles:true}));"
                        "}"
                    ),
                    "silent": True,
                },
            )
            await cdp.send("Runtime.releaseObject", {"objectId": object_id})
            print(
                "Uploaded file into input index "
                f"{chosen.get('index')} (id='{chosen.get('id', '')}'): {file_path}"
            )
        elif args.list_inputs:
            inputs = await list_file_inputs(cdp)
            print(json.dumps(inputs, indent=2))
            return

        if args.confirm_loaded or message is not None:
            loaded = await wait_for_loaded(
                cdp, filename, args.confirm_timeout_sec, args.confirm_settle_sec
            )
            if loaded:
                st = await get_source_file_state(cdp, filename)
                print(f"Confirmed loaded in Sources: {filename} ({st.get('reason')})")
            else:
                st = await get_source_file_state(cdp, filename)
                raise RuntimeError(
                    "Upload sent, but source did not reach ready state within "
                    f"{args.confirm_timeout_sec}s: {filename} | state={st}"
                )
        if message is not None:
            sent = await send_chat_message(cdp, message, args.message_timeout_sec)
            print(f"Sent chat message after source was ready: {sent.get('reason')}")


if __name__ == "__main__":
    try:
        asyncio.run(run())
    except Exception as e:
        print(str(e), file=sys.stderr)
        sys.exit(1)
