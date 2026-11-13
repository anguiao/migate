CREATE TABLE identity (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    bridge_id TEXT NOT NULL CHECK (length(bridge_id) = 32)
) STRICT;

CREATE TABLE blobs (
    key INTEGER PRIMARY KEY CHECK (key BETWEEN 0 AND 65535),
    value BLOB NOT NULL
) STRICT;

CREATE TABLE xiaomi_account (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    uid TEXT NOT NULL,
    region TEXT NOT NULL
) STRICT;

CREATE TABLE xiaomi_account_recovery (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    user_id TEXT NOT NULL,
    c_user_id TEXT NOT NULL,
    pass_token TEXT NOT NULL,
    device_id TEXT NOT NULL,
    user_agent TEXT NOT NULL,
    stopped TEXT CHECK (stopped IN ('rejected', 'verification_required'))
) STRICT;

CREATE TABLE xiaomi_mijia_session (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    user_id TEXT NOT NULL,
    service_token TEXT NOT NULL,
    ssecurity TEXT NOT NULL,
    user_agent TEXT NOT NULL
) STRICT;

CREATE TABLE xiaomi_ha_client (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    client_uuid TEXT NOT NULL,
    stopped INTEGER NOT NULL CHECK (stopped IN (0, 1))
) STRICT;

CREATE TABLE xiaomi_ha_oauth (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    redirect_uri TEXT NOT NULL,
    access_token TEXT NOT NULL,
    refresh_token TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    refresh_at INTEGER NOT NULL
) STRICT;

CREATE TABLE xiaomi_hub (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    virtual_did TEXT NOT NULL,
    private_key_pem TEXT NOT NULL,
    certificate_pem TEXT
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
VALUES (1, '{"scope":null,"root_version":1,"devices":[],"descriptions":[],"groups":[],"split_devices":[],"temperature_bindings":[]}');

-- Confirmed LAN credentials are separate from the published configuration.
CREATE TABLE xiaomi_lan (
    did TEXT PRIMARY KEY,
    uid TEXT NOT NULL,
    home_id TEXT NOT NULL,
    model TEXT NOT NULL,
    address TEXT NOT NULL,
    token BLOB NOT NULL CHECK (length(token) = 16)
) STRICT;

PRAGMA user_version = 13;
