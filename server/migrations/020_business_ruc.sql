-- Peru RUC (Registro Único de Contribuyentes), optional and owner-typed at
-- registration. Needed to issue a boleta/factura through the SUNAT rail: a
-- business without one can still sell, it just cannot be invoiced.
--
-- Nullable on purpose. Most micro-businesses registering from a phone do not
-- have a RUC to hand, and blocking registration on it would cost far more
-- signups than the invoicing add-on is worth.
ALTER TABLE businesses ADD COLUMN ruc TEXT;
