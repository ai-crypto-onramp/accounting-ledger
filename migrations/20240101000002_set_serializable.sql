-- Set the default transaction isolation level to SERIALIZABLE for the
-- current database. The service asserts this at startup and refuses to boot
-- if weaker. current_database() is used so the migration is portable across
-- dev (ledger_accounting) and prod (ledger) database names.
DO $$ BEGIN
    EXECUTE format('ALTER DATABASE %I SET default_transaction_isolation = %L',
                   current_database(), 'serializable');
END $$;