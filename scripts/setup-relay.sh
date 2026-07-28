#!/usr/bin/env bash
# Build and install floptle-relay as a service on a fresh Oracle Cloud
# (Ubuntu 22.04/24.04, ARM or x86) instance. Run as a normal sudo user:
#
#     bash setup-relay.sh https://github.com/Fopull-LLC/Floptle.git
#
# The repo is PRIVATE, so either pass an authenticated URL, or clone it
# yourself first and run this from inside the checkout with no argument.
set -euo pipefail

PORT="${PORT:-7788}"
REPO="${1:-}"
SRC="$HOME/floptle"

echo "==> packages"
sudo apt-get update -qq
sudo apt-get install -y -qq build-essential pkg-config git curl

echo "==> rust"
if ! command -v cargo >/dev/null; then
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  . "$HOME/.cargo/env"
fi
. "$HOME/.cargo/env"

echo "==> source"
if [ -n "$REPO" ]; then
  [ -d "$SRC" ] || git clone --depth 1 "$REPO" "$SRC"
  git -C "$SRC" pull --ff-only || true
else
  SRC="$PWD"
fi

# The relay depends only on floptle-net — no GPU, no windowing, no audio. This
# is why it builds on a bare headless box with nothing but build-essential.
echo "==> build (a few minutes on a free-tier core)"
cargo build --release -p floptle-relay --manifest-path "$SRC/Cargo.toml"
sudo install -m755 "$SRC/target/release/floptle-relay" /usr/local/bin/floptle-relay

echo "==> firewall"
# Oracle's Ubuntu images ship a REJECT-everything iptables policy that survives
# opening the port in the console. Both have to be done; this is the half that
# gets forgotten, and the symptom is a lobby code that nobody can ever join.
sudo iptables -I INPUT 1 -p udp --dport "$PORT" -j ACCEPT
sudo netfilter-persistent save 2>/dev/null || {
  sudo apt-get install -y -qq iptables-persistent && sudo netfilter-persistent save
}

echo "==> service"
sudo tee /etc/systemd/system/floptle-relay.service >/dev/null <<UNIT
[Unit]
Description=Floptle rendezvous relay
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/floptle-relay ${PORT}
Restart=always
RestartSec=2
# It forwards opaque bytes between strangers, so give it nothing to lose:
# no privileges, no writable filesystem, its own empty /tmp.
DynamicUser=yes
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
RestrictAddressFamilies=AF_INET AF_INET6
MemoryMax=256M

[Install]
WantedBy=multi-user.target
UNIT

sudo systemctl daemon-reload
sudo systemctl enable --now floptle-relay
sleep 2
sudo systemctl --no-pager --lines=15 status floptle-relay || true

IP="$(curl -s https://api.ipify.org || echo '<this-box>')"
cat <<DONE

==> done

  Relay:  ${IP}:${PORT}

  In Floptle, set the 🌐 panel's relay field to that, or from Lua:
      net.host{ relay = "${IP}:${PORT}" }
      net.join("relay://${IP}:${PORT}/CODE")

  Still to do in the Oracle web console:
      Networking > VCN > Security List > Add Ingress Rule
      Source 0.0.0.0/0 · IP Protocol UDP · Destination port ${PORT}

  Logs:   sudo journalctl -u floptle-relay -f
DONE
