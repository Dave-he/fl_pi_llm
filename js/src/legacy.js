// TODO: remove once no install keeps a session written before pi-durable
// (fl_pi_llm on pi-agent-core 0.87, `/sessions/<cwd>/<created>_<id>.jsonl`).
//
// Such a session is a tree of entries in one file, with the branch tip the
// harness last stood on. It is moved into a chat's pi-durable storage once,
// the first time the chat is opened: every root-to-leaf path becomes a
// conversation, forked from the one it shares the longest prefix with, so
// the versions an edit or a regeneration left stay where they were. The old
// file is deleted once the chat holds everything it said.

import { hostFileSystem as fs } from './fs.js';

const host = globalThis.__host;
const call = async (name, payload) => JSON.parse(await host.call(name, JSON.stringify(payload)));

export const LEGACY_ROOT = '/sessions';

/** pi named a session `<created>_<id>.jsonl`, the id URI-encoded. */
export function legacyIdOf(path) {
  const name = path.slice(path.lastIndexOf('/') + 1);
  if (!name.endsWith('.jsonl')) return null;
  const stem = name.slice(0, -'.jsonl'.length);
  const at = stem.indexOf('_');
  if (at < 0) return null;
  try {
    return decodeURIComponent(stem.slice(at + 1));
  } catch {
    return null;
  }
}

/** Every legacy session file still waiting to be moved, by chat id. */
export async function legacyFiles() {
  const { files } = await call('fs.list', { dir: LEGACY_ROOT });
  const out = new Map();
  for (const f of files) {
    const id = legacyIdOf(f.path);
    if (id != null) out.set(id, f);
  }
  return out;
}

/** The parts of a legacy file a chat needs: its entries, tip and name. */
export function parseLegacy(text) {
  const entries = new Map();
  let tip;
  let name = null;
  for (const line of text.split('\n')) {
    if (!line.trim()) continue;
    let value;
    try {
      value = JSON.parse(line);
    } catch {
      continue; // A torn last line: what came before it is whole.
    }
    for (const w of Array.isArray(value) ? value : [value]) {
      if (w == null || typeof w !== 'object') continue;
      if (w.kind === 'entry' || (w.kind === undefined && typeof w.id === 'string' && typeof w.type === 'string' && w.type !== 'session')) {
        entries.set(w.id, w);
      } else if (w.kind === 'value' && w.namespace === 'pi.branch.tip' && w.key === 'main') {
        tip = w.op === 'delete' ? null : w.value;
      } else if (w.kind === 'value' && w.namespace === 'pi.session.name') {
        name = w.op === 'delete' ? null : (w.value ?? null);
      }
    }
  }
  return { entries, tip, name };
}

const millis = (t) => (typeof t === 'number' ? t : Date.parse(t) || Date.now());

/** One legacy entry as the pi-durable entry the chat keeps in its place. */
function draft(e, mapped) {
  const timestamp = millis(e.timestamp);
  switch (e.type) {
    case 'message':
      return { kind: 'fl.message', model: [e.message] };
    case 'custom':
      return { kind: 'fl.custom', data: { customType: e.customType ?? '', data: e.data ?? null, timestamp } };
    case 'compaction':
    case 'branch_summary': {
      const summary = e.summary ?? '';
      // The kept tail starts at the first entry the summary did not replace.
      const kept = e.firstKeptEntryId ?? firstKept(e, mapped);
      return {
        kind: 'fl.compaction',
        data: { summary, timestamp, ...(e.tokensBefore != null ? { tokensBefore: e.tokensBefore } : {}) },
        model: [{ role: 'user', content: [{ type: 'text', text: summary }], timestamp }],
        head: kept != null && mapped.has(kept) ? mapped.get(kept).newId : 'self',
      };
    }
    default:
      return null;
  }
}

/** pi 0.87 kept the tail as messages; the entry holding the first is the head. */
function firstKept(e, mapped) {
  const first = e.retainedTail?.[0];
  if (!first) return null;
  for (const [oldId, m] of mapped) {
    if (m.message?.timestamp === first.timestamp && m.message?.role === first.role) return oldId;
  }
  return null;
}

/**
 * Moves the legacy session in [file] into [harness]'s empty storage. Returns
 * the conversation and tip the chat should open on, and its name.
 */
export async function migrate(harness, file, BG) {
  const { text } = await call('fs.read', { path: file.path });
  if (text == null) return null;
  const { entries, tip, name } = parseLegacy(text);

  const children = new Map();
  for (const e of entries.values()) {
    const p = e.parentId ?? null;
    if (!children.has(p)) children.set(p, []);
    children.get(p).push(e);
  }
  const leaves = [...entries.values()].filter((e) => !children.has(e.id)).sort((a, b) => a.seq - b.seq);

  /** old id -> {newId, conv, message} */
  const mapped = new Map();
  /** conversation id -> the old id of its last entry */
  const lastOf = new Map();
  let root;

  for (const leaf of leaves) {
    const path = [];
    for (let e = leaf; e; e = e.parentId != null ? entries.get(e.parentId) : undefined) path.unshift(e);
    let i = 0;
    while (i < path.length && mapped.has(path[i].id)) i++;
    let conv;
    if (i === 0) {
      conv = root ? await harness.createConversation({ ownership: { kind: 'ownerless' } }, BG) : await harness.root(BG);
      root ??= conv;
    } else {
      const at = mapped.get(path[i - 1].id);
      // A branch ends where another goes on: continue that conversation
      // when this one is its own tip, fork it otherwise.
      conv = lastOf.get(at.conv) === path[i - 1].id
        ? await harness.conversation(at.conv, BG)
        : await (await harness.conversation(at.conv, BG)).fork(at.newId, { ownership: { kind: 'ownerless' } }, BG);
    }
    for (; i < path.length; i++) {
      const e = path[i];
      const entry = draft(e, mapped);
      if (!entry) continue;
      const settled = await (await conv.submit({ type: 'write', entry }, BG)).wait(BG);
      if (settled.status !== 'done') throw new Error(`Could not move entry ${e.id}: ${settled.reason}`);
      mapped.set(e.id, { newId: settled.entry, conv: conv.id, message: e.message });
      lastOf.set(conv.id, e.id);
    }
  }
  root ??= await harness.root(BG);

  let current = root.id;
  let newTip = null;
  if (tip === null) {
    newTip = -1;
  } else if (tip != null && mapped.has(tip)) {
    const m = mapped.get(tip);
    current = m.conv;
    if (lastOf.get(m.conv) !== tip) newTip = m.newId;
  } else if (leaves.length) {
    const m = mapped.get(leaves[leaves.length - 1].id);
    if (m) current = m.conv;
  }
  return { current, tip: newTip, name, remove: () => fs.remove(file.path, { force: true }, BG) };
}
