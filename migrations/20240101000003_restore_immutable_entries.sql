-- Entries are append-only. Reject UPDATE and DELETE at the DB layer so that
-- even a compromised application or direct DB write cannot silently mutate the
-- ledger without breaking the hash chain.
CREATE OR REPLACE FUNCTION reject_entry_mutation() RETURNS trigger AS $$
BEGIN
  RAISE EXCEPTION 'entries are immutable: % operation not allowed on row %', TG_OP, TG_TABLE_NAME;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS entries_no_update ON entries;
CREATE TRIGGER entries_no_update BEFORE UPDATE ON entries
FOR EACH ROW EXECUTE FUNCTION reject_entry_mutation();

DROP TRIGGER IF EXISTS entries_no_delete ON entries;
CREATE TRIGGER entries_no_delete BEFORE DELETE ON entries
FOR EACH ROW EXECUTE FUNCTION reject_entry_mutation();