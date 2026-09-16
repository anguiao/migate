CREATE TABLE identity (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    bridge_id TEXT NOT NULL CHECK (length(bridge_id) = 32),
    light_id TEXT NOT NULL CHECK (length(light_id) = 32 AND light_id <> bridge_id)
) STRICT;

CREATE TABLE blobs (
    key INTEGER PRIMARY KEY CHECK (key BETWEEN 0 AND 65535),
    value BLOB NOT NULL
) STRICT;

CREATE TABLE matter_topology (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    signature TEXT NOT NULL CHECK (signature = '' OR length(signature) = 40)
) STRICT;
INSERT INTO matter_topology (id, signature) VALUES (1, '');

CREATE TABLE matter_feature_labels (
    endpoint INTEGER PRIMARY KEY CHECK (endpoint BETWEEN 2 AND 65534),
    label TEXT NOT NULL CHECK (length(CAST(label AS BLOB)) <= 32),
    FOREIGN KEY (endpoint) REFERENCES feature_identities(endpoint) ON DELETE CASCADE
) STRICT;
CREATE TABLE matter_endpoint_scenes (
    endpoint INTEGER PRIMARY KEY CHECK (endpoint BETWEEN 2 AND 65534),
    value BLOB NOT NULL CHECK (length(value) <= 4096),
    FOREIGN KEY (endpoint) REFERENCES feature_identities(endpoint) ON DELETE CASCADE
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
    certificate_pem TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0)
) STRICT;

CREATE TABLE auth_revision (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    revision INTEGER NOT NULL CHECK (revision >= 0)
) STRICT;
INSERT INTO auth_revision (id, revision) VALUES (1, 0);
CREATE TABLE auth_session_generation (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    generation INTEGER NOT NULL CHECK (generation >= 0)
) STRICT;
INSERT INTO auth_session_generation (id, generation) VALUES (1, 0);
CREATE TABLE home_binding (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    account_uid TEXT NOT NULL,
    home_id TEXT NOT NULL,
    display_name TEXT NOT NULL
) STRICT;
CREATE TABLE devices (
    account_uid TEXT NOT NULL,
    home_id TEXT NOT NULL,
    parent_did TEXT NOT NULL,
    model TEXT NOT NULL,
    name TEXT NOT NULL,
    room_id TEXT,
    admitted INTEGER NOT NULL CHECK (admitted IN (0, 1)),
    PRIMARY KEY (account_uid, home_id, parent_did)
) STRICT;
CREATE TABLE catalog_homes (
    account_uid TEXT NOT NULL,
    home_id TEXT NOT NULL,
    name TEXT NOT NULL,
    group_id TEXT NOT NULL CHECK (length(group_id) = 16),
    PRIMARY KEY (account_uid, home_id)
) STRICT;
CREATE TABLE catalog_rooms (
    account_uid TEXT NOT NULL,
    home_id TEXT NOT NULL,
    room_id TEXT NOT NULL,
    name TEXT NOT NULL,
    PRIMARY KEY (account_uid, home_id, room_id),
    FOREIGN KEY (account_uid, home_id) REFERENCES catalog_homes(account_uid, home_id) ON DELETE CASCADE
) STRICT;
CREATE TABLE catalog_device_metadata (
    account_uid TEXT NOT NULL,
    home_id TEXT NOT NULL,
    parent_did TEXT NOT NULL,
    spec_type TEXT,
    pid INTEGER,
    local_ip TEXT,
    parent_id TEXT,
    online INTEGER CHECK (online IS NULL OR online IN (0, 1)),
    feature_document TEXT NOT NULL,
    PRIMARY KEY (account_uid, home_id, parent_did),
    FOREIGN KEY (account_uid, home_id, parent_did) REFERENCES devices(account_uid, home_id, parent_did) ON DELETE CASCADE
) STRICT;
CREATE TABLE device_tokens (
    account_uid TEXT NOT NULL,
    home_id TEXT NOT NULL,
    parent_did TEXT NOT NULL,
    token BLOB NOT NULL CHECK (length(token) = 16),
    PRIMARY KEY (account_uid, home_id, parent_did),
    FOREIGN KEY (account_uid, home_id, parent_did)
        REFERENCES devices (account_uid, home_id, parent_did) ON DELETE CASCADE
) STRICT;
CREATE TABLE miot_specs (
    type_urn TEXT PRIMARY KEY,
    document TEXT NOT NULL,
    fetched_at INTEGER NOT NULL
) STRICT;
CREATE TABLE feature_allocator (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    next_endpoint INTEGER NOT NULL CHECK (next_endpoint >= 2)
) STRICT;
INSERT INTO feature_allocator (id, next_endpoint) VALUES (1, 2);
CREATE TABLE feature_identities (
    account_uid TEXT NOT NULL,
    home_id TEXT NOT NULL,
    parent_did TEXT NOT NULL,
    service_instance INTEGER NOT NULL CHECK (service_instance >= 0),
    role TEXT NOT NULL,
    endpoint INTEGER NOT NULL UNIQUE CHECK (endpoint BETWEEN 2 AND 65534),
    public_id TEXT NOT NULL UNIQUE CHECK (length(public_id) = 32),
    active INTEGER NOT NULL CHECK (active IN (0, 1)),
    PRIMARY KEY (account_uid, home_id, parent_did, service_instance, role)
) STRICT;
CREATE TABLE published_feature_definitions (
    account_uid TEXT NOT NULL,
    home_id TEXT NOT NULL,
    parent_did TEXT NOT NULL,
    service_instance INTEGER NOT NULL,
    role TEXT NOT NULL,
    model TEXT NOT NULL,
    spec_document TEXT NOT NULL,
    name TEXT NOT NULL,
    PRIMARY KEY (account_uid, home_id, parent_did, service_instance, role),
    FOREIGN KEY (account_uid, home_id, parent_did, service_instance, role)
        REFERENCES feature_identities (
            account_uid, home_id, parent_did, service_instance, role
        )
) STRICT;
CREATE TABLE feature_states (
    account_uid TEXT NOT NULL,
    home_id TEXT NOT NULL,
    parent_did TEXT NOT NULL,
    service_instance INTEGER NOT NULL,
    role TEXT NOT NULL,
    property TEXT NOT NULL,
    value TEXT NOT NULL,
    source TEXT NOT NULL,
    observed_at INTEGER NOT NULL,
    report_version INTEGER NOT NULL CHECK (report_version >= 0),
    PRIMARY KEY (account_uid, home_id, parent_did, service_instance, role, property),
    FOREIGN KEY (account_uid, home_id, parent_did, service_instance, role)
        REFERENCES feature_identities (
            account_uid, home_id, parent_did, service_instance, role
        )
) STRICT;
PRAGMA user_version = 8;
