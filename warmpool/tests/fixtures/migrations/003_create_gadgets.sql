CREATE TABLE gadgets (
    id serial PRIMARY KEY,
    widget_id integer REFERENCES widgets(id),
    description text
);
