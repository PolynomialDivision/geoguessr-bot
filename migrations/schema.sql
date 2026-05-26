-- GeoGuessr bot schema (single consolidated file — no migration history needed).

CREATE TABLE IF NOT EXISTS players (
    user_id      TEXT PRIMARY KEY,
    first_seen_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_seen_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS rounds (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    room_id      TEXT NOT NULL,
    n_guesses    INTEGER NOT NULL,
    triggered_by TEXT NOT NULL,
    started_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    ended_at     TEXT
);

CREATE TABLE IF NOT EXISTS guesses (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    round_id            INTEGER NOT NULL REFERENCES rounds(id),
    guess_num           INTEGER NOT NULL,
    country             TEXT NOT NULL,
    region              TEXT NOT NULL,
    city                TEXT,
    source              TEXT NOT NULL,
    attribution         TEXT,
    choices             TEXT NOT NULL DEFAULT '[]',
    correct_index       INTEGER NOT NULL DEFAULT 0,
    answer_timeout_secs INTEGER NOT NULL DEFAULT 90,
    actual_lat          REAL,
    actual_lon          REAL,
    matrix_event_id     TEXT,
    asked_at            TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    n_answers_received  INTEGER,
    n_correct           INTEGER
);

CREATE TABLE IF NOT EXISTS answers (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    guess_id      INTEGER NOT NULL REFERENCES guesses(id),
    round_id      INTEGER NOT NULL REFERENCES rounds(id),
    user_id       TEXT NOT NULL,
    choice_index  INTEGER NOT NULL DEFAULT 0,
    is_correct    INTEGER NOT NULL DEFAULT 0,
    source        TEXT NOT NULL DEFAULT 'reaction',
    submitted_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    changed_answer INTEGER NOT NULL DEFAULT 0,
    guess_text    TEXT,
    guess_lat     REAL,
    guess_lon     REAL,
    distance_km   REAL,
    score         INTEGER,
    UNIQUE(guess_id, user_id)
);

CREATE TABLE IF NOT EXISTS round_scores (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    round_id      INTEGER NOT NULL REFERENCES rounds(id),
    user_id       TEXT NOT NULL,
    correct_count INTEGER NOT NULL DEFAULT 0,
    total_count   INTEGER NOT NULL DEFAULT 0,
    total_score   INTEGER NOT NULL DEFAULT 0,
    UNIQUE(round_id, user_id)
);
