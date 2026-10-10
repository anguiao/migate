CREATE TABLE group_identity (
    source TEXT NOT NULL, device_id TEXT NOT NULL, unique_id TEXT NOT NULL UNIQUE,
    endpoint INTEGER NOT NULL CHECK (endpoint BETWEEN 2 AND 65534), label TEXT,
    PRIMARY KEY (source, device_id)
) STRICT;

CREATE TABLE group_relation (
    uid TEXT NOT NULL, home_id TEXT NOT NULL,
    source TEXT NOT NULL, device_id TEXT NOT NULL,
    member_source TEXT NOT NULL, member_id TEXT NOT NULL,
    PRIMARY KEY (uid, home_id, source, device_id, member_source, member_id)
) STRICT;

CREATE TABLE group_snapshot (
    uid TEXT NOT NULL, home_id TEXT NOT NULL, version INTEGER NOT NULL,
    PRIMARY KEY (uid, home_id)
) STRICT;
