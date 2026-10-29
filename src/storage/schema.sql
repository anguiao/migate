CREATE TABLE identity (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    bridge_id TEXT NOT NULL CHECK (length(bridge_id) = 32)
) STRICT;

CREATE TABLE blobs (
    key INTEGER PRIMARY KEY CHECK (key BETWEEN 0 AND 65535),
    value BLOB NOT NULL
) STRICT;

CREATE TABLE xiaomi_auth (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    uid TEXT NOT NULL,
    region TEXT NOT NULL,
    oauth_client_uuid TEXT NOT NULL,
    redirect_uri TEXT NOT NULL,
    access_token TEXT NOT NULL,
    refresh_token TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    refresh_at INTEGER NOT NULL,
    virtual_did TEXT NOT NULL,
    private_key_pem TEXT NOT NULL,
    certificate_pem TEXT NOT NULL
) STRICT;

-- The complete committed bridge configuration in one private JSON record.
-- Canonical native facts, confirmed group relations, global historical
-- identities, names, endpoints and versions live here; published and hidden
-- sets, logical capabilities, group indexes and repair plans are derived.
CREATE TABLE bridge_configuration (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    json TEXT NOT NULL
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

INSERT INTO bridge_configuration (id, json)
VALUES (1, '{"scope":null,"root_version":1,"devices":[],"descriptions":[],"groups":[]}');

PRAGMA user_version = 10;
