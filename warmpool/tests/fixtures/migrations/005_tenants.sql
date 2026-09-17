CREATE SCHEMA tenant_a;
CREATE SCHEMA tenant_b;

CREATE TABLE tenant_a.orders (id SERIAL PRIMARY KEY);
CREATE TABLE tenant_b.orders (id SERIAL PRIMARY KEY);
