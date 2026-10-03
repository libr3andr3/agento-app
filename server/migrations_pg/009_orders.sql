-- Product orders: businesses that sell things, not (only) time slots.
CREATE TABLE IF NOT EXISTS orders (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id UUID NOT NULL REFERENCES businesses(id),
    customer_name TEXT NOT NULL,
    -- Canonical conversation identity (harness::canon_phone), like appointments.
    phone TEXT NOT NULL,
    -- [{product, qty, unitPrice}] — priced from the catalog at order time.
    items JSONB NOT NULL,
    total NUMERIC(12,2),
    status TEXT NOT NULL DEFAULT 'pending_payment',
    paid BOOLEAN NOT NULL DEFAULT FALSE,
    notes TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_orders_business ON orders (business_id, created_at DESC);

-- A payment can now settle an order instead of an appointment.
ALTER TABLE payments ADD COLUMN IF NOT EXISTS order_id UUID;
