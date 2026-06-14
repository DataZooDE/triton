# dz-triton-gateway image: the dz-triton image (same binary + the baked env://
# WhatsApp manifest at /etc/triton/adapter.yaml) re-stamped with
# LABEL service="dz-triton-gateway" so Kamal accepts it for the
# dz-triton-gateway service (Kamal asserts the image's `service` label equals
# the deploy service name). The public WhatsApp ingress (substrate
# kamal/dz-triton-gateway) shares triton's signing key + iss — no cascade.
FROM ghcr.io/datazoode/dz-triton:main
LABEL service="dz-triton-gateway"
