CREATE TABLE native_group (
    uid TEXT NOT NULL,
    home_id TEXT NOT NULL,
    group_did TEXT NOT NULL,
    record_did TEXT NOT NULL,
    model TEXT NOT NULL,
    instance_type TEXT NOT NULL,
    is_physical INTEGER NOT NULL CHECK (is_physical IN (0,1)),
    PRIMARY KEY (uid, home_id, group_did)
) STRICT;

CREATE TABLE native_group_member (
    uid TEXT NOT NULL,
    home_id TEXT NOT NULL,
    group_did TEXT NOT NULL,
    member_did TEXT NOT NULL,
    PRIMARY KEY (uid, home_id, group_did, member_did)
) STRICT;

-- The root endpoint belongs to exactly one bridge. Its ConfigurationVersion is
-- bridge-global and must never decrease across account or home changes.
CREATE TABLE publication_root (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    configuration_version INTEGER NOT NULL CHECK (configuration_version > 0)
) STRICT;

INSERT INTO publication_root (id, configuration_version) VALUES (1, 1);
