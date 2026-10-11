-- The public API (spec §16.2): a key's daily page budget is counted from
-- the requests it made since midnight, and a key reads back only its own
-- results. Both look requests up by key and time.

CREATE INDEX requests_key_created_idx ON requests (api_key_id, created_at)
    WHERE api_key_id IS NOT NULL;

-- The audit trail is read newest first by one key's actions as well as
-- whole (GET /v1/admin/audit).
CREATE INDEX audit_events_key_idx ON audit_events (api_key_id, id)
    WHERE api_key_id IS NOT NULL;
