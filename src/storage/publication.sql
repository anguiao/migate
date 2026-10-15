CREATE TABLE native_group (
    uid TEXT NOT NULL,
    home_id TEXT NOT NULL,
    group_did TEXT NOT NULL,
    record_did TEXT NOT NULL,
    model TEXT NOT NULL,
    instance_type TEXT,
    is_physical INTEGER NOT NULL CHECK (is_physical IN (0,1)),
    active INTEGER NOT NULL CHECK (active IN (0,1)),
    error TEXT,
    PRIMARY KEY (uid, home_id, group_did)
) STRICT;

CREATE TABLE native_group_member (
    uid TEXT NOT NULL,
    home_id TEXT NOT NULL,
    group_did TEXT NOT NULL,
    member_did TEXT NOT NULL,
    PRIMARY KEY (uid, home_id, group_did, member_did)
) STRICT;

-- Focused durable deletion/permission revocation tombstones. Written before
-- the main configuration commit and cleared only by a newer confirmed
-- authorized re-admission inside that commit. No timestamps or history.
CREATE TABLE publication_revocation (
    source TEXT NOT NULL,
    device_id TEXT NOT NULL,
    reason TEXT NOT NULL,
    PRIMARY KEY (source, device_id)
) STRICT;

-- The root endpoint belongs to exactly one bridge. Its ConfigurationVersion is
-- bridge-global and must never decrease across account or home changes.
CREATE TABLE publication_root (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    configuration_version INTEGER NOT NULL CHECK (configuration_version > 0)
) STRICT;

INSERT INTO publication_root (id, configuration_version) VALUES (1, 1);
