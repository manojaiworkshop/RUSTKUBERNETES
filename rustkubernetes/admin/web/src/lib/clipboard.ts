/**
 * Copies text to the clipboard. `navigator.clipboard` only exists on secure origins (https or localhost), so on a plain
 * http address (the LAN) a hidden textarea and the older `execCommand("copy")` do the job.
 */
export async function copyText(text: string): Promise<boolean> {
  try {
    if (window.isSecureContext && navigator.clipboard) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch { /* fall through to the older way */ }
  const ta = document.createElement("textarea");
  ta.value = text;
  ta.setAttribute("readonly", "");
  ta.style.cssText = "position:fixed;top:0;left:0;opacity:0";
  document.body.appendChild(ta);
  ta.select();
  let ok = false;
  try { ok = document.execCommand("copy"); } catch { /* blocked */ }
  ta.remove();
  return ok;
}
