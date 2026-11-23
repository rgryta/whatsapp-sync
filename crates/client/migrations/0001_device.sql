-- The linked device's own state: its private keys, which are this WhatsApp account, and the
-- Signal sessions it has with every contact device.
--
-- In its own schema, out of the MCP role's reach. `public` is not safe for this: the live
-- database has default privileges granting the MCP role SELECT on every table the ingest
-- role creates there, and the MCP is reachable through MCPHub. A new schema grants nothing
-- to anyone but its owner; the REVOKE says so out loud.
REVOKE ALL ON SCHEMA device FROM PUBLIC;

CREATE TABLE device.account (
    id                          smallint PRIMARY KEY CHECK (id = 1),
    registration_id             integer NOT NULL,
    noise_private               bytea NOT NULL,
    identity_private            bytea NOT NULL,
    signed_pre_key_id           integer NOT NULL,
    signed_pre_key_private      bytea NOT NULL,
    signed_pre_key_signature    bytea NOT NULL,
    adv_secret                  bytea NOT NULL,
    -- Pairing's record and the two signatures over it; absent for a never-paired device.
    adv_details                 bytea,
    adv_account_signature_key   bytea,
    adv_account_signature       bytea,
    adv_device_signature        bytea,
    me_id                       text,
    me_lid                      text,
    me_name                     text,
    platform                    text,
    routing_info                bytea,
    next_pre_key_id             integer NOT NULL,
    first_unuploaded_pre_key_id integer NOT NULL
);

CREATE TABLE device.pre_key (
    id      integer PRIMARY KEY,
    private bytea NOT NULL
);

-- libsignal's stored session record, one per contact device.
CREATE TABLE device.session (
    name   text NOT NULL,
    device integer NOT NULL,
    record bytea NOT NULL,
    PRIMARY KEY (name, device)
);

-- The identity key each contact device last used, 33 bytes with the type byte.
CREATE TABLE device.identity (
    name   text NOT NULL,
    device integer NOT NULL,
    key    bytea NOT NULL,
    PRIMARY KEY (name, device)
);

CREATE TABLE device.sender_key (
    group_id text NOT NULL,
    name     text NOT NULL,
    device   integer NOT NULL,
    record   bytea NOT NULL,
    PRIMARY KEY (group_id, name, device)
);

CREATE TABLE device.app_state_key (
    key_id text PRIMARY KEY,
    data   bytea NOT NULL
);

CREATE TABLE device.lid_mapping (
    pn  text PRIMARY KEY,
    lid text NOT NULL
);
