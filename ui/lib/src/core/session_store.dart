import 'dart:convert';

import 'package:fl_lib/fl_lib.dart';
import 'package:fl_pi_llm/fl_pi_llm.dart';
import 'package:flutter/foundation.dart';

/// pi's sessions, kept in the app's encrypted SQLite database.
///
/// A session is an append-only JSONL file to pi, so a file is stored as its
/// appended chunks, one row each: an append is an insert, and a read joins
/// the rows in order. Nothing is rewritten on append.
final class SqlitePiSessionStore implements PiSessionStore {
  SqlitePiSessionStore() {
    SqliteDb.instance.execute('''
CREATE TABLE IF NOT EXISTS pi_chunks (
  path  TEXT    NOT NULL,
  seq   INTEGER NOT NULL,
  text  TEXT    NOT NULL,
  mtime INTEGER NOT NULL,
  PRIMARY KEY (path, seq)
) WITHOUT ROWID;
''');
  }

  static final instance = SqlitePiSessionStore();

  static int get _now => DateTime.now().millisecondsSinceEpoch;

  @override
  Future<String?> read(String path, {int? maxLines}) async {
    final rows = SqliteDb.instance.select(
      'SELECT text FROM pi_chunks WHERE path = ? ORDER BY seq;',
      [path],
    );
    if (rows.isEmpty) return null;
    final sb = StringBuffer();
    var lines = 0;
    for (final r in rows) {
      final t = r['text'] as String;
      sb.write(t);
      if (maxLines != null) {
        lines += '\n'.allMatches(t).length;
        if (lines >= maxLines) break;
      }
    }
    return sb.toString();
  }

  @override
  Future<void> write(String path, String text) async {
    SqliteStore.transact(() {
      final db = SqliteDb.instance;
      db.execute('DELETE FROM pi_chunks WHERE path = ?;', [path]);
      db.execute('INSERT INTO pi_chunks (path, seq, text, mtime) VALUES (?, 0, ?, ?);', [path, text, _now]);
    });
  }

  @override
  Future<void> append(String path, String text) async {
    SqliteDb.instance.execute(
      '''
INSERT INTO pi_chunks (path, seq, text, mtime)
SELECT ?1, COALESCE(MAX(seq) + 1, 0), ?2, ?3 FROM pi_chunks WHERE path = ?1;
''',
      [path, text, _now],
    );
  }

  @override
  Future<void> rename(String from, String to) async {
    SqliteStore.transact(() {
      final db = SqliteDb.instance;
      db.execute('DELETE FROM pi_chunks WHERE path = ?;', [to]);
      db.execute('UPDATE pi_chunks SET path = ? WHERE path = ?;', [to, from]);
    });
  }

  @override
  Future<void> remove(String path, {bool recursive = false}) async {
    final db = SqliteDb.instance;
    db.execute('DELETE FROM pi_chunks WHERE path = ?;', [path]);
    if (recursive) {
      db.execute("DELETE FROM pi_chunks WHERE substr(path, 1, length(?) + 1) = ? || '/';", [path, path]);
    }
  }

  @override
  Future<StoredFile?> stat(String path) async {
    final rows = SqliteDb.instance.select(
      'SELECT SUM(length(text)) AS size, MAX(mtime) AS mtime FROM pi_chunks WHERE path = ?;',
      [path],
    );
    final r = rows.first;
    if (r['mtime'] == null) return null;
    return StoredFile(path, r['size'] as int, DateTime.fromMillisecondsSinceEpoch(r['mtime'] as int));
  }

  @override
  Future<List<StoredFile>> list(String dir) async {
    final prefix = dir == '/' ? '/' : '$dir/';
    final rows = SqliteDb.instance.select(
      '''
SELECT path, SUM(length(text)) AS size, MAX(mtime) AS mtime FROM pi_chunks
WHERE substr(path, 1, length(?1)) = ?1 GROUP BY path;
''',
      [prefix],
    );
    return [
      for (final r in rows)
        StoredFile(r['path'] as String, r['size'] as int, DateTime.fromMillisecondsSinceEpoch(r['mtime'] as int)),
    ];
  }

  /// Every stored file, for a backup.
  Map<String, String> dump() {
    final rows = SqliteDb.instance.select('SELECT path, text FROM pi_chunks ORDER BY path, seq;');
    final out = <String, StringBuffer>{};
    for (final r in rows) {
      (out[r['path'] as String] ??= StringBuffer()).write(r['text'] as String);
    }
    return out.map((k, v) => MapEntry(k, v.toString()));
  }

  /// When anything was last written: a cheap "has something changed".
  int latestMtime() =>
      SqliteDb.instance.select('SELECT MAX(mtime) AS m FROM pi_chunks;').firstOrNull?['m'] as int? ?? 0;

  /// The chat among [ids] the file at [path] belongs to: a chat keeps its
  /// storage in `/chats/<encoded id>/`. A session from before pi-durable is
  /// `/sessions/<cwd>/<created>_<encoded id>.jsonl`, and an id may itself
  /// contain `_`, so it is matched rather than split out (TODO: remove with
  /// `js/src/legacy.js`).
  static String? chatIdOf(String path, Iterable<String> ids) {
    final parts = path.split('/');
    if (parts.length >= 3 && parts[1] == 'chats') {
      final id = Uri.decodeComponent(parts[2]);
      return ids.contains(id) ? id : null;
    }
    final name = parts.last;
    if (!name.endsWith('.jsonl')) return null;
    for (final id in ids) {
      if (name.endsWith('_${Uri.encodeComponent(id)}.jsonl')) return id;
    }
    return null;
  }

  /// Whether [path] holds a chat's entries: its storage's main log, or a
  /// session from before pi-durable. The rest (documents such as a partial
  /// answer) says nothing a search should find.
  static bool _holdsEntries(String path) =>
      (path.startsWith('/chats/') && path.endsWith('/main.jsonl')) ||
      (path.startsWith('/sessions/') && path.endsWith('.jsonl'));

  /// What was said in each file, lowercased, with the mtime and size it was
  /// read at: an append within the same millisecond still counts.
  final _said = <String, (String, String)>{};

  /// The files whose messages contain [needle], ignoring case.
  ///
  /// Searches what the user and the model said, not the stored JSON (its
  /// keys, ids, base64 images). That text is kept per file and read again
  /// only when the file changes, off this isolate.
  Future<List<String>> search(String needle) async {
    final q = needle.toLowerCase();
    final rows = SqliteDb.instance.select(
      'SELECT path, MAX(mtime) AS m, SUM(length(text)) AS n FROM pi_chunks GROUP BY path;',
    );
    final mtimes = {
      for (final r in rows)
        if (_holdsEntries(r['path'] as String)) r['path'] as String: '${r['m']}/${r['n']}',
    };
    _said.removeWhere((k, _) => !mtimes.containsKey(k));
    final stale = [
      for (final MapEntry(:key, :value) in mtimes.entries)
        if (_said[key]?.$1 != value) key,
    ];
    if (stale.isNotEmpty) {
      final texts = <String, String>{
        for (final p in stale) p: await read(p) ?? '',
      };
      final said = await compute(_saidIn, texts);
      for (final p in stale) {
        _said[p] = (mtimes[p]!, said[p] ?? '');
      }
    }
    return [
      for (final MapEntry(:key, :value) in _said.entries)
        if (value.$2.contains(q)) key,
    ];
  }

  /// The user's and the model's text in each session log, lowercased.
  static Map<String, String> _saidIn(Map<String, String> files) => {
    for (final MapEntry(:key, :value) in files.entries) key: _said1(value),
  };

  static String _said1(String log) {
    final sb = StringBuffer();
    void text(Object? content) {
      if (content is String) {
        sb.writeln(content);
      } else if (content is List) {
        for (final p in content) {
          if (p is Map && p['type'] == 'text' && p['text'] is String) sb.writeln(p['text']);
        }
      }
    }

    void said(Object? m) {
      if (m is Map && (m['role'] == 'user' || m['role'] == 'assistant')) text(m['content']);
    }

    for (final line in const LineSplitter().convert(log)) {
      if (line.isEmpty) continue;
      try {
        final v = json.decode(line);
        // pi-durable: one commit per line, its entries among the writes.
        if (v is Map && v['type'] == 'commit') {
          for (final w in v['writes'] as List? ?? const []) {
            if (w is! Map || w['type'] != 'entry') continue;
            final model = (w['value'] as Map?)?['model'];
            if (model is List) model.forEach(said);
          }
          continue;
        }
        // TODO: remove with `js/src/legacy.js`: pi-agent-core 0.87's log.
        for (final w in v is List ? v : [v]) {
          if (w is Map && w['kind'] == 'entry') said(w['message']);
        }
      } catch (_) {
        // A header, or a torn line.
      }
    }
    return sb.toString().toLowerCase();
  }
}
