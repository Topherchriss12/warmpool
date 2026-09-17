CREATE FUNCTION noop_trigger() RETURNS trigger AS $$
BEGIN
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER orders_audit_a
    AFTER INSERT ON tenant_a.orders
    FOR EACH ROW
    EXECUTE FUNCTION noop_trigger();

CREATE TRIGGER orders_audit_b
    AFTER INSERT ON tenant_b.orders
    FOR EACH ROW
    EXECUTE FUNCTION noop_trigger();
