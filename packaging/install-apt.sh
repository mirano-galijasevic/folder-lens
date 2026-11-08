#!/bin/sh
# Configures the signed SharpLogica APT repository on supported amd64 systems.

set -eu

# The repository currently publishes packages only for 64-bit Intel/AMD Linux.
if [ "$(dpkg --print-architecture)" != "amd64" ]; then
    echo "SharpLogica packages currently support only amd64 systems." >&2
    exit 1
fi

# Keep this repository key separate from the system-wide APT trust store.
install -d -m 0755 /etc/apt/keyrings

curl -fsSL \
    https://packages.sharplogica.com/sharplogica-archive-key.asc \
    -o /etc/apt/keyrings/sharplogica-archive-key.asc

chmod 0644 /etc/apt/keyrings/sharplogica-archive-key.asc

# Restrict the key to SharpLogica's repository and the supported architecture.
cat > /etc/apt/sources.list.d/sharplogica.sources <<'EOF'
Types: deb
URIs: https://packages.sharplogica.com/apt/
Suites: stable
Components: main
Architectures: amd64
Signed-By: /etc/apt/keyrings/sharplogica-archive-key.asc
EOF

apt-get update

echo "SharpLogica repository installed successfully."
