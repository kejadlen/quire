DROP TABLE sh_events;
DROP TABLE jobs;

CREATE TABLE events (
    run_id  TEXT    NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    seq     INTEGER NOT NULL,
    event   JSON    NOT NULL,
    PRIMARY KEY (run_id, seq)
);
